//! The application's own memory, one SQLite file: how the user arranged sessions (folders,
//! order, pins, renames), the summaries of every harness's sessions so a start shows the
//! tree at once, a full-text index of sessions, and notes. The harnesses' own files are
//! never written here.

use std::collections::HashMap;
use std::path::Path;

use model::{SessionKey, SessionSummary};
use rusqlite::{Connection, OptionalExtension, params};

mod tree;

pub use tree::{Grouping, PINNED_GROUP, SortOrder, TreeNode, TreeNodeKind, build_tree, flatten_visible};

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

pub type FolderId = i64;

#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    pub id: FolderId,
    pub parent: Option<FolderId>,
    pub name: String,
    pub position: f64,
    pub collapsed: bool,
}

/// Where the user put a session and what they changed about it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Placement {
    pub folder: Option<FolderId>,
    pub position: f64,
    pub pinned: bool,
    pub title: Option<String>,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub id: i64,
    pub session: Option<SessionKey>,
    pub project: Option<String>,
    pub text: String,
    pub created_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub key: SessionKey,
    pub title: String,
    pub snippet: String,
}

const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE folders (
        id INTEGER PRIMARY KEY,
        parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        position REAL NOT NULL,
        collapsed INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE placements (
        session_key TEXT PRIMARY KEY,
        folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
        position REAL NOT NULL DEFAULT 0,
        pinned INTEGER NOT NULL DEFAULT 0,
        title TEXT,
        hidden INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE summaries (
        session_key TEXT PRIMARY KEY,
        source_path TEXT NOT NULL,
        env TEXT NOT NULL,
        harness TEXT NOT NULL,
        data TEXT NOT NULL
    );
    CREATE INDEX summaries_source ON summaries(source_path);
    CREATE VIRTUAL TABLE search USING fts5(
        session_key UNINDEXED, title, prompt, cwd, body,
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TABLE notes (
        id INTEGER PRIMARY KEY,
        session_key TEXT,
        project TEXT,
        text TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    );
    CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE environments (
        id TEXT PRIMARY KEY,
        label TEXT NOT NULL,
        host TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1
    );
    "#,
    "ALTER TABLE environments ADD COLUMN identity TEXT;",
    // Which version of a session file the text in the index comes from.
    "CREATE TABLE indexed (session_key TEXT PRIMARY KEY, stamp TEXT NOT NULL);",
];

/// The version of a session's source the index holds the text of.
fn stamp_of(summary: &SessionSummary) -> String {
    format!("{}:{}", summary.source.size, summary.source.modified_ms)
}

/// An SSH host the user added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshHostRecord {
    pub host: String,
    pub label: String,
    /// A private key file for this host only.
    pub identity: Option<String>,
    pub enabled: bool,
}

pub struct Library {
    connection: Connection,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::with_connection(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(connection: Connection) -> Result<Self> {
        // The window and `bowyer mcp` share the file: wait for a lock instead of failing.
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            connection.execute_batch(migration)?;
            connection.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        Ok(Self { connection })
    }

    // Summaries --------------------------------------------------------------------------

    /// Replaces the cached summaries of one harness in one environment with a fresh scan,
    /// and indexes the new ones for search.
    /// Stores the summaries of one scan; returns whether any of them changed.
    pub fn store_scan(&mut self, env: &str, harness: &str, summaries: &[SessionSummary]) -> Result<bool> {
        let mut changed = false;
        let transaction = self.connection.transaction()?;
        let old: HashMap<String, String> = {
            let mut statement = transaction.prepare("SELECT session_key, data FROM summaries WHERE env = ?1 AND harness = ?2")?;
            statement.query_map(params![env, harness], |row| Ok((row.get(0)?, row.get(1)?)))?.flatten().collect()
        };
        let mut fresh = std::collections::HashSet::new();
        for summary in summaries {
            let key = summary.key.encode();
            let data = serde_json::to_string(summary).unwrap_or_default();
            fresh.insert(key.clone());
            if old.get(&key) == Some(&data) {
                continue;
            }
            changed = true;
            transaction.execute(
                "INSERT OR REPLACE INTO summaries (session_key, source_path, env, harness, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, summary.source.path, env, harness, data],
            )?;
            // The text of the conversation stays: the indexer reads it again when the
            // file changed, not when only the title or the state did.
            let prompt = summary.first_prompt.clone().unwrap_or_default();
            let cwd = summary.cwd.clone().unwrap_or_default();
            let updated = transaction.execute(
                "UPDATE search SET title = ?2, prompt = ?3, cwd = ?4 WHERE session_key = ?1",
                params![key, summary.title, prompt, cwd],
            )?;
            if updated == 0 {
                transaction.execute(
                    "INSERT INTO search (session_key, title, prompt, cwd, body) VALUES (?1, ?2, ?3, ?4, '')",
                    params![key, summary.title, prompt, cwd],
                )?;
            }
        }
        for key in old.keys().filter(|key| !fresh.contains(*key)) {
            changed = true;
            transaction.execute("DELETE FROM summaries WHERE session_key = ?1", [key])?;
            transaction.execute("DELETE FROM search WHERE session_key = ?1", [key])?;
            transaction.execute("DELETE FROM indexed WHERE session_key = ?1", [key])?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    /// Every cached summary, for the tree at start-up before any scan finishes.
    pub fn summaries(&self) -> Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare("SELECT data FROM summaries")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|data| serde_json::from_str(&data).ok()).collect())
    }

    /// The cached summary read from a source file, to skip files that did not change.
    pub fn summary_by_source(&self, source_path: &str) -> Option<SessionSummary> {
        self.connection
            .query_row("SELECT data FROM summaries WHERE source_path = ?1", [source_path], |row| row.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .and_then(|data| serde_json::from_str(&data).ok())
    }

    /// Adds the text of a whole transcript to the index of a session.
    pub fn index_body(&self, key: &SessionKey, body: &str) -> Result<()> {
        self.connection.execute("UPDATE search SET body = ?2 WHERE session_key = ?1", params![key.encode(), body])?;
        Ok(())
    }

    /// The text of the conversation of `summary`, read from the version of its file the
    /// summary names; the indexer does not read that version again.
    pub fn index_session(&self, summary: &SessionSummary, body: &str) -> Result<()> {
        self.index_body(&summary.key, body)?;
        self.connection.execute(
            "INSERT OR REPLACE INTO indexed (session_key, stamp) VALUES (?1, ?2)",
            params![summary.key.encode(), stamp_of(summary)],
        )?;
        Ok(())
    }

    /// Sessions of `env` whose text the index lacks or holds from an older version of the
    /// file, newest first; sessions written after `quiet_before_ms` wait.
    pub fn bodies_to_index(&self, env: &str, quiet_before_ms: i64, limit: usize) -> Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.data, i.stamp FROM summaries s LEFT JOIN indexed i ON i.session_key = s.session_key WHERE s.env = ?1",
        )?;
        let rows = statement.query_map([env], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?;
        let mut stale: Vec<SessionSummary> = rows
            .flatten()
            .filter_map(|(data, stamp)| {
                let summary: SessionSummary = serde_json::from_str(&data).ok()?;
                let stale = stamp.as_deref() != Some(stamp_of(&summary).as_str());
                (stale && summary.updated_ms <= quiet_before_ms).then_some(summary)
            })
            .collect();
        stale.sort_by_key(|summary| std::cmp::Reverse(summary.updated_ms));
        stale.truncate(limit);
        Ok(stale)
    }

    // Search -----------------------------------------------------------------------------

    /// Sessions whose title, prompt, folder or text match every word, best first. Words
    /// match as prefixes, so "pars" finds "parser".
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|word| !word.is_empty())
            .map(|word| format!("\"{}\"*", word.replace('"', "")))
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT session_key, title, snippet(search, -1, '[', ']', '…', 12)
             FROM search WHERE search MATCH ?1 ORDER BY bm25(search, 0.0, 8.0, 3.0, 2.0, 1.0) LIMIT ?2",
        )?;
        let rows = statement.query_map(params![words.join(" AND "), limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        Ok(rows
            .flatten()
            .filter_map(|(key, title, snippet)| Some(SearchHit { key: SessionKey::decode(&key)?, title, snippet }))
            .collect())
    }

    // Folders and placements --------------------------------------------------------------

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut statement = self.connection.prepare("SELECT id, parent_id, name, position, collapsed FROM folders")?;
        let rows = statement.query_map([], |row| {
            Ok(Folder { id: row.get(0)?, parent: row.get(1)?, name: row.get(2)?, position: row.get(3)?, collapsed: row.get(4)? })
        })?;
        rows.collect()
    }

    pub fn create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId> {
        let position: f64 = self.connection.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM folders WHERE parent_id IS ?1",
            [parent],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO folders (parent_id, name, position) VALUES (?1, ?2, ?3)",
            params![parent, name, position],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn rename_folder(&self, id: FolderId, name: &str) -> Result<()> {
        self.connection.execute("UPDATE folders SET name = ?2 WHERE id = ?1", params![id, name])?;
        Ok(())
    }

    /// Removes a folder; its sessions go back to the top level, its subfolders go with it.
    pub fn delete_folder(&self, id: FolderId) -> Result<()> {
        self.connection.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn set_folder_collapsed(&self, id: FolderId, collapsed: bool) -> Result<()> {
        self.connection.execute("UPDATE folders SET collapsed = ?2 WHERE id = ?1", params![id, collapsed])?;
        Ok(())
    }

    /// Moves a folder under `parent` at `position`. A folder never goes inside itself.
    pub fn move_folder(&self, id: FolderId, parent: Option<FolderId>, position: f64) -> Result<bool> {
        let mut cursor = parent;
        while let Some(current) = cursor {
            if current == id {
                return Ok(false);
            }
            cursor = self
                .connection
                .query_row("SELECT parent_id FROM folders WHERE id = ?1", [current], |row| row.get(0))
                .optional()?
                .flatten();
        }
        self.connection.execute(
            "UPDATE folders SET parent_id = ?2, position = ?3 WHERE id = ?1",
            params![id, parent, position],
        )?;
        Ok(true)
    }

    pub fn placements(&self) -> Result<HashMap<SessionKey, Placement>> {
        let mut statement = self.connection.prepare("SELECT session_key, folder_id, position, pinned, title, hidden FROM placements")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Placement { folder: row.get(1)?, position: row.get(2)?, pinned: row.get(3)?, title: row.get(4)?, hidden: row.get(5)? },
            ))
        })?;
        Ok(rows.flatten().filter_map(|(key, placement)| Some((SessionKey::decode(&key)?, placement))).collect())
    }

    fn placement(&self, key: &SessionKey) -> Result<Placement> {
        Ok(self.placements_of(key)?.unwrap_or_default())
    }

    fn placements_of(&self, key: &SessionKey) -> Result<Option<Placement>> {
        self.connection
            .query_row(
                "SELECT folder_id, position, pinned, title, hidden FROM placements WHERE session_key = ?1",
                [key.encode()],
                |row| Ok(Placement { folder: row.get(0)?, position: row.get(1)?, pinned: row.get(2)?, title: row.get(3)?, hidden: row.get(4)? }),
            )
            .optional()
    }

    pub fn set_placement(&self, key: &SessionKey, placement: &Placement) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO placements (session_key, folder_id, position, pinned, title, hidden) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key.encode(), placement.folder, placement.position, placement.pinned, placement.title, placement.hidden],
        )?;
        Ok(())
    }

    /// Puts a session into a folder (or the top level) at a position among its siblings.
    pub fn move_session(&self, key: &SessionKey, folder: Option<FolderId>, position: f64) -> Result<()> {
        let mut placement = self.placement(key)?;
        placement.folder = folder;
        placement.position = position;
        self.set_placement(key, &placement)
    }

    pub fn rename_session(&self, key: &SessionKey, title: Option<&str>) -> Result<()> {
        let mut placement = self.placement(key)?;
        placement.title = title.map(str::to_owned).filter(|t| !t.trim().is_empty());
        self.set_placement(key, &placement)?;
        if let Some(title) = &placement.title {
            self.connection.execute("UPDATE search SET title = ?2 WHERE session_key = ?1", params![key.encode(), title])?;
        }
        Ok(())
    }

    pub fn set_pinned(&self, key: &SessionKey, pinned: bool) -> Result<()> {
        let mut placement = self.placement(key)?;
        placement.pinned = pinned;
        self.set_placement(key, &placement)
    }

    pub fn set_hidden(&self, key: &SessionKey, hidden: bool) -> Result<()> {
        let mut placement = self.placement(key)?;
        placement.hidden = hidden;
        self.set_placement(key, &placement)
    }

    // Notes (the memory) -----------------------------------------------------------------

    pub fn add_note(&self, session: Option<&SessionKey>, project: Option<&str>, text: &str, now_ms: i64) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO notes (session_key, project, text, created_ms) VALUES (?1, ?2, ?3, ?4)",
            params![session.map(SessionKey::encode), project, text, now_ms],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn notes(&self, project: Option<&str>) -> Result<Vec<Note>> {
        let mut statement = self.connection.prepare(
            "SELECT id, session_key, project, text, created_ms FROM notes WHERE ?1 IS NULL OR project = ?1 ORDER BY created_ms DESC",
        )?;
        let rows = statement.query_map([project], |row| {
            Ok(Note {
                id: row.get(0)?,
                session: row.get::<_, Option<String>>(1)?.as_deref().and_then(SessionKey::decode),
                project: row.get(2)?,
                text: row.get(3)?,
                created_ms: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_note(&self, id: i64) -> Result<()> {
        self.connection.execute("DELETE FROM notes WHERE id = ?1", [id])?;
        Ok(())
    }

    // Settings and environments ----------------------------------------------------------

    pub fn setting(&self, key: &str) -> Option<String> {
        self.connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    /// SSH hosts the user added.
    pub fn ssh_environments(&self) -> Result<Vec<SshHostRecord>> {
        let mut statement = self.connection.prepare("SELECT host, label, identity, enabled FROM environments ORDER BY label")?;
        let rows = statement.query_map([], |row| {
            Ok(SshHostRecord { host: row.get(0)?, label: row.get(1)?, identity: row.get(2)?, enabled: row.get(3)? })
        })?;
        rows.collect()
    }

    pub fn add_ssh_environment(&self, host: &str, label: &str, identity: Option<&str>) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO environments (id, label, host, identity, enabled) VALUES (?1, ?2, ?3, ?4, 1)",
            params![format!("ssh:{host}"), label, host, identity],
        )?;
        Ok(())
    }

    pub fn remove_ssh_environment(&self, host: &str) -> Result<()> {
        self.connection.execute("DELETE FROM environments WHERE host = ?1", [host])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{EnvId, HarnessKind, SourceStamp};

    pub(crate) fn summary(id: &str, title: &str, cwd: &str, updated: i64) -> SessionSummary {
        SessionSummary {
            key: SessionKey { env: EnvId::local(), harness: HarnessKind::Claude, id: id.into() },
            title: title.into(),
            first_prompt: Some(format!("prompt of {title}")),
            cwd: Some(cwd.into()),
            git_branch: None,
            model: None,
            created_ms: updated,
            updated_ms: updated,
            turn: None,
            parent: None,
            source: SourceStamp { path: format!("/h/{id}.jsonl"), size: 1, modified_ms: updated },
        }
    }

    #[test]
    fn scans_replace_and_forget() {
        let mut library = Library::in_memory().unwrap();
        library.store_scan("local", "claude", &[summary("a", "Parser fix", "/w/app", 1), summary("b", "Docs", "/w/app", 2)]).unwrap();
        assert_eq!(library.summaries().unwrap().len(), 2);
        assert!(library.store_scan("local", "claude", &[summary("b", "Docs", "/w/app", 3)]).unwrap());
        assert!(!library.store_scan("local", "claude", &[summary("b", "Docs", "/w/app", 3)]).unwrap(), "the same scan changes nothing");
        let left = library.summaries().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].updated_ms, 3);
        assert!(library.summary_by_source("/h/b.jsonl").is_some());
        assert!(library.search("parser", 10).unwrap().is_empty(), "a forgotten session leaves the index");
    }

    #[test]
    fn the_indexer_reads_a_session_again_only_when_its_file_changed() {
        let mut library = Library::in_memory().unwrap();
        let mut first = summary("a", "Parser work", "/w", 10);
        first.source.size = 100;
        library.store_scan("local", "claude", &[first.clone()]).unwrap();
        assert_eq!(library.bodies_to_index("local", i64::MAX, 10).unwrap().len(), 1);
        library.index_session(&first, "we discussed the websocket reconnect").unwrap();
        assert!(library.bodies_to_index("local", i64::MAX, 10).unwrap().is_empty());

        // A new title keeps the text of the conversation.
        let mut renamed = first.clone();
        renamed.title = "Parser and sockets".into();
        library.store_scan("local", "claude", &[renamed.clone()]).unwrap();
        assert!(library.bodies_to_index("local", i64::MAX, 10).unwrap().is_empty());
        assert_eq!(library.search("websock", 10).unwrap().len(), 1, "the body stayed");

        // The file grew: read it again.
        let mut grown = renamed;
        grown.source.size = 200;
        library.store_scan("local", "claude", &[grown]).unwrap();
        assert_eq!(library.bodies_to_index("local", i64::MAX, 10).unwrap().len(), 1);
        assert!(library.bodies_to_index("ssh:host", i64::MAX, 10).unwrap().is_empty(), "per environment");
        assert!(library.bodies_to_index("local", 5, 10).unwrap().is_empty(), "written lately: later");
    }

    #[test]
    fn search_matches_prefixes_and_bodies() {
        let mut library = Library::in_memory().unwrap();
        library.store_scan("local", "claude", &[summary("a", "Parser fix", "/w/app", 1), summary("b", "Сборка релиза", "/w/app", 2)]).unwrap();
        assert_eq!(library.search("pars", 10).unwrap()[0].key.id, "a");
        assert_eq!(library.search("релиз", 10).unwrap()[0].key.id, "b");
        let key = SessionKey { env: EnvId::local(), harness: HarnessKind::Claude, id: "a".into() };
        library.index_body(&key, "we discussed the websocket reconnect").unwrap();
        assert_eq!(library.search("websock", 10).unwrap()[0].key, key);
    }

    #[test]
    fn folders_never_go_inside_themselves() {
        let library = Library::in_memory().unwrap();
        let outer = library.create_folder(None, "Work").unwrap();
        let inner = library.create_folder(Some(outer), "Api").unwrap();
        assert!(!library.move_folder(outer, Some(inner), 1.0).unwrap());
        assert!(library.move_folder(inner, None, 0.5).unwrap());
        library.delete_folder(outer).unwrap();
        assert_eq!(library.folders().unwrap().len(), 1);
    }

    #[test]
    fn placements_keep_renames_and_survive_folder_removal() {
        let library = Library::in_memory().unwrap();
        let key = SessionKey { env: EnvId::local(), harness: HarnessKind::Codex, id: "x".into() };
        let folder = library.create_folder(None, "Inbox").unwrap();
        library.move_session(&key, Some(folder), 2.0).unwrap();
        library.rename_session(&key, Some("Renamed")).unwrap();
        library.delete_folder(folder).unwrap();
        let placement = &library.placements().unwrap()[&key];
        assert_eq!(placement.folder, None);
        assert_eq!(placement.title.as_deref(), Some("Renamed"));
    }

    #[test]
    fn ssh_hosts_keep_their_key() {
        let library = Library::in_memory().unwrap();
        library.add_ssh_environment("dev@build-box", "server", Some("C:/keys/id")).unwrap();
        let hosts = library.ssh_environments().unwrap();
        assert_eq!(hosts[0].identity.as_deref(), Some("C:/keys/id"));
        library.remove_ssh_environment("dev@build-box").unwrap();
        assert!(library.ssh_environments().unwrap().is_empty());
    }

    #[test]
    fn notes_filter_by_project() {
        let library = Library::in_memory().unwrap();
        library.add_note(None, Some("/w/app"), "use cargo nextest", 1).unwrap();
        library.add_note(None, Some("/w/other"), "other", 2).unwrap();
        assert_eq!(library.notes(Some("/w/app")).unwrap().len(), 1);
        assert_eq!(library.notes(None).unwrap().len(), 2);
    }
}
