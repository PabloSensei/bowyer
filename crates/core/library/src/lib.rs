//! The application's own memory, one SQLite file: how the user arranged sessions (folders,
//! order, pins, renames), the summaries of every harness's sessions so a start shows the
//! tree at once, a full-text index of sessions, and notes. The harnesses' own files are
//! never written here.

use std::collections::HashMap;
use std::path::Path;

use model::{SessionKey, SessionSummary};
use rusqlite::{Connection, OptionalExtension, params};

mod tree;

pub use tree::{Grouping, PINNED_GROUP, SessionStart, SortOrder, TreeNode, TreeNodeKind, build_tree, flatten_visible, link_started, project_root};

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
    /// A piece of the text around the match, the matched words between [`MATCH_START`]
    /// and [`MATCH_END`]: see [`snippet_pieces`] and [`SearchHit::bracketed`].
    pub snippet: String,
}

/// Where a matched word starts in a snippet: a control character, which text never holds.
pub const MATCH_START: char = '\u{2}';
/// Where a matched word ends in a snippet.
pub const MATCH_END: char = '\u{3}';

impl SearchHit {
    /// The snippet on one line, the matched words in brackets: for a model to read.
    pub fn bracketed(&self) -> String {
        self.snippet.replace(MATCH_START, "[").replace(MATCH_END, "]").replace('\n', " ")
    }
}

/// A snippet on one line in pieces, each with whether it is a matched word.
pub fn snippet_pieces(snippet: &str) -> Vec<(String, bool)> {
    let mut pieces: Vec<(String, bool)> = Vec::new();
    let mut matched = false;
    let mut piece = String::new();
    for ch in snippet.chars() {
        match ch {
            MATCH_START | MATCH_END => {
                if !piece.is_empty() {
                    pieces.push((std::mem::take(&mut piece), matched));
                }
                matched = ch == MATCH_START;
            }
            '\n' | '\r' | '\t' => piece.push(' '),
            _ => piece.push(ch),
        }
    }
    if !piece.is_empty() {
        pieces.push((piece, matched));
    }
    pieces
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
    // Claude Code sessions took the folder of their last line, where a `cd` of the agent's
    // shell may have left them; they are read again for the folder they began in.
    "DELETE FROM summaries WHERE harness = 'claude';",
    // A short account of each session for the memory server: problem, fix, commits,
    // tickets, files. `stamp` names the version of the session file it was made from;
    // `by_model` says a model wrote the problem, fix and search words.
    r#"
    CREATE TABLE digests (session_key TEXT PRIMARY KEY, stamp TEXT NOT NULL, text TEXT NOT NULL, by_model INTEGER NOT NULL DEFAULT 0);
    CREATE VIRTUAL TABLE digest_search USING fts5(session_key UNINDEXED, text, tokenize = 'unicode61 remove_diacritics 2');
    "#,
    // The titles of the sessions a session started in separate sessions, to hang them under
    // it. Claude Code sessions are indexed again so the ones from before are found.
    r#"
    CREATE TABLE started (parent TEXT NOT NULL, title TEXT NOT NULL, PRIMARY KEY (parent, title));
    DELETE FROM indexed WHERE session_key LIKE '%|claude|%';
    "#,
    // Where each session's row is in the full-text tables: a column of an FTS table has no
    // index, and finding a row by it read the whole table for every row written.
    r#"
    CREATE TABLE search_rows (session_key TEXT PRIMARY KEY, row INTEGER NOT NULL);
    INSERT OR REPLACE INTO search_rows (session_key, row) SELECT session_key, rowid FROM search ORDER BY rowid;
    DELETE FROM search WHERE rowid NOT IN (SELECT row FROM search_rows);
    CREATE TABLE digest_rows (session_key TEXT PRIMARY KEY, row INTEGER NOT NULL);
    INSERT OR REPLACE INTO digest_rows (session_key, row) SELECT session_key, rowid FROM digest_search ORDER BY rowid;
    DELETE FROM digest_search WHERE rowid NOT IN (SELECT row FROM digest_rows);
    "#,
    // How full each window of a subscription's limits was when the window looked, per
    // account (`<env>|<harness>`): the history of the five-hour and weekly windows.
    "CREATE TABLE limit_samples (account TEXT NOT NULL, at_ms INTEGER NOT NULL, kind TEXT NOT NULL, percent REAL NOT NULL, PRIMARY KEY (account, at_ms, kind));",
];

/// How long samples of the limits are kept.
const LIMIT_SAMPLES_KEPT_MS: i64 = 60 * 86_400_000;

/// The row of a session in a full-text table, through its table of rows (`search_rows`,
/// `digest_rows`).
fn fts_row(connection: &Connection, rows: &str, key: &str) -> Result<Option<i64>> {
    connection
        .query_row(&format!("SELECT row FROM {rows} WHERE session_key = ?1"), [key], |row| row.get(0))
        .optional()
}

/// Removes a session's row from a full-text table and its table of rows.
/// The name the user gave a session, if any.
fn given_title(connection: &Connection, key: &str) -> Result<Option<String>> {
    let title: Option<Option<String>> = connection.query_row("SELECT title FROM placements WHERE session_key = ?1", [key], |row| row.get(0)).optional()?;
    Ok(title.flatten().filter(|title| !title.trim().is_empty()))
}

fn fts_delete(connection: &Connection, table: &str, rows: &str, key: &str) -> Result<()> {
    if let Some(row) = fts_row(connection, rows, key)? {
        connection.execute(&format!("DELETE FROM {table} WHERE rowid = ?1"), [row])?;
        connection.execute(&format!("DELETE FROM {rows} WHERE session_key = ?1"), [key])?;
    }
    Ok(())
}

/// The version of a session's source the index holds the text of.
pub fn stamp_of(summary: &SessionSummary) -> String {
    format!("{}:{}", summary.source.size, summary.source.modified_ms)
}

/// Words that carry meaning in a question: no short words, no common English ones.
pub fn meaningful_words(query: &str) -> Vec<String> {
    const COMMON: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "what", "when", "where", "which", "who", "how", "why", "did", "does",
        "was", "were", "are", "you", "we", "our", "us", "there", "then", "than", "from", "into", "about", "have", "had",
        "has", "not", "but", "can", "could", "should", "would", "remember", "recent", "session", "sessions", "earlier",
        "before", "last", "time", "some", "similar", "problem", "issue", "solved", "fixed", "fix",
    ];
    let mut words: Vec<String> = Vec::new();
    for word in query.split(|c: char| !c.is_alphanumeric() && c != '_').map(str::to_lowercase) {
        if word.chars().count() >= 3 && !COMMON.contains(&word.as_str()) && !words.contains(&word) {
            words.push(word);
        }
    }
    words
}

/// An FTS query that matches any of the meaningful words, as prefixes.
fn any_words(query: &str) -> Option<String> {
    let words = meaningful_words(query);
    (!words.is_empty()).then(|| words.iter().map(|word| format!("\"{word}\"*")).collect::<Vec<_>>().join(" OR "))
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

    fn with_connection(mut connection: Connection) -> Result<Self> {
        // The window and `bowyer mcp` share the file: wait for a lock instead of failing.
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        // Switching a new file to WAL does not wait for a lock by itself.
        let started = std::time::Instant::now();
        loop {
            match connection.pragma_update(None, "journal_mode", "WAL") {
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy && started.elapsed() < std::time::Duration::from_secs(5) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => break result?,
            }
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if (version as usize) < MIGRATIONS.len() {
            // The window, `bowyer mcp` and the memory server may open the file at once after
            // an update: one takes the write lock and migrates, the others wait for it and
            // find the work done. All the steps go in together or not at all.
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
            for migration in MIGRATIONS.iter().skip(version as usize) {
                transaction.execute_batch(migration)?;
            }
            transaction.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
            transaction.commit()?;
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
            // A session the user renamed is found by its name.
            let title = given_title(&transaction, &key)?.unwrap_or_else(|| summary.title.clone());
            match fts_row(&transaction, "search_rows", &key)? {
                // FTS5 writes a row anew whole, the text of the conversation too: a running
                // session changes on every poll, its title, prompt and folder seldom.
                Some(row) => {
                    let indexed: Option<(String, String, String)> = transaction
                        .query_row("SELECT title, prompt, cwd FROM search WHERE rowid = ?1", [row], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                        .optional()?;
                    if indexed.as_ref().is_none_or(|(old_title, old_prompt, old_cwd)| (old_title, old_prompt, old_cwd) != (&title, &prompt, &cwd)) {
                        transaction.execute("UPDATE search SET title = ?2, prompt = ?3, cwd = ?4 WHERE rowid = ?1", params![row, title, prompt, cwd])?;
                    }
                }
                None => {
                    transaction.execute(
                        "INSERT INTO search (session_key, title, prompt, cwd, body) VALUES (?1, ?2, ?3, ?4, '')",
                        params![key, title, prompt, cwd],
                    )?;
                    let row = transaction.last_insert_rowid();
                    transaction.execute("INSERT OR REPLACE INTO search_rows (session_key, row) VALUES (?1, ?2)", params![key, row])?;
                }
            }
        }
        for key in old.keys().filter(|key| !fresh.contains(*key)) {
            changed = true;
            transaction.execute("DELETE FROM summaries WHERE session_key = ?1", [key])?;
            fts_delete(&transaction, "search", "search_rows", key)?;
            transaction.execute("DELETE FROM indexed WHERE session_key = ?1", [key])?;
            transaction.execute("DELETE FROM digests WHERE session_key = ?1", [key])?;
            fts_delete(&transaction, "digest_search", "digest_rows", key)?;
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
        if let Some(row) = fts_row(&self.connection, "search_rows", &key.encode())? {
            self.connection.execute("UPDATE search SET body = ?2 WHERE rowid = ?1", params![row, body])?;
        }
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

    /// Keeps the titles of the sessions `parent` started, in place of the ones kept before.
    pub fn set_started(&self, parent: &SessionKey, titles: &[String]) -> Result<()> {
        let key = parent.encode();
        self.connection.execute("DELETE FROM started WHERE parent = ?1", [&key])?;
        for title in titles {
            self.connection.execute("INSERT OR IGNORE INTO started (parent, title) VALUES (?1, ?2)", params![key, title])?;
        }
        Ok(())
    }

    /// Every session that started others, with the title of each one it started.
    pub fn started(&self) -> Result<Vec<(SessionKey, String)>> {
        let mut statement = self.connection.prepare("SELECT parent, title FROM started")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        Ok(rows.flatten().filter_map(|(parent, title)| Some((SessionKey::decode(&parent)?, title))).collect())
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
            "SELECT session_key, title, snippet(search, -1, char(2), char(3), '…', 12)
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

    /// Sessions whose title, prompt, folder or text hold any of the words, the ones that
    /// hold more of them and rarer ones first. For questions in a sentence, where
    /// [`Library::search`] needs every word.
    pub fn search_any(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(words) = any_words(query) else { return Ok(Vec::new()) };
        let mut statement = self.connection.prepare(
            "SELECT session_key, title, snippet(search, -1, char(2), char(3), '…', 12)
             FROM search WHERE search MATCH ?1 ORDER BY bm25(search, 0.0, 8.0, 3.0, 2.0, 1.0) LIMIT ?2",
        )?;
        let rows = statement.query_map(params![words, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        Ok(rows
            .flatten()
            .filter_map(|(key, title, snippet)| Some(SearchHit { key: SessionKey::decode(&key)?, title, snippet }))
            .collect())
    }

    // Digests ----------------------------------------------------------------------------

    /// Keeps the digest of a session, made from the version of its file `stamp` names.
    pub fn set_digest(&self, key: &SessionKey, stamp: &str, text: &str, by_model: bool) -> Result<()> {
        let key = key.encode();
        // The digest and its index change together: a failure between them would leave one
        // without the other.
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR REPLACE INTO digests (session_key, stamp, text, by_model) VALUES (?1, ?2, ?3, ?4)",
            params![key, stamp, text, by_model],
        )?;
        fts_delete(&transaction, "digest_search", "digest_rows", &key)?;
        transaction.execute("INSERT INTO digest_search (session_key, text) VALUES (?1, ?2)", params![key, text])?;
        let row = transaction.last_insert_rowid();
        transaction.execute("INSERT INTO digest_rows (session_key, row) VALUES (?1, ?2)", params![key, row])?;
        transaction.commit()
    }

    /// The digest of a session: the stamp it was made from, its text, whether a model wrote it.
    pub fn digest(&self, key: &SessionKey) -> Option<(String, String, bool)> {
        self.connection
            .query_row("SELECT stamp, text, by_model FROM digests WHERE session_key = ?1", [key.encode()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Sessions whose digest holds any of the words, best first.
    pub fn search_digests(&self, query: &str, limit: usize) -> Result<Vec<SessionKey>> {
        let Some(words) = any_words(query) else { return Ok(Vec::new()) };
        let mut statement =
            self.connection.prepare("SELECT session_key FROM digest_search WHERE digest_search MATCH ?1 ORDER BY bm25(digest_search) LIMIT ?2")?;
        let rows = statement.query_map(params![words, limit as i64], |row| row.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|key| SessionKey::decode(&key)).collect())
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
        // The search finds the new name, or again the harness's title once the name is gone.
        let encoded = key.encode();
        let harness_title = || {
            self.connection
                .query_row("SELECT data FROM summaries WHERE session_key = ?1", [&encoded], |row| row.get::<_, String>(0))
                .optional()
                .map(|data| data.and_then(|data| serde_json::from_str::<SessionSummary>(&data).ok()).map(|summary| summary.title))
        };
        let shown = match &placement.title {
            Some(title) => Some(title.clone()),
            None => harness_title()?,
        };
        if let Some(title) = shown
            && let Some(row) = fts_row(&self.connection, "search_rows", &encoded)?
        {
            self.connection.execute("UPDATE search SET title = ?2 WHERE rowid = ?1", params![row, title])?;
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

    /// Keeps how full the windows of an account's limits are at `at_ms`: `(kind, percent)`.
    /// Samples older than two months go.
    pub fn add_limit_samples(&self, account: &str, at_ms: i64, windows: &[(String, f64)]) -> Result<()> {
        for (kind, percent) in windows {
            self.connection.execute(
                "INSERT OR REPLACE INTO limit_samples (account, at_ms, kind, percent) VALUES (?1, ?2, ?3, ?4)",
                params![account, at_ms, kind, percent],
            )?;
        }
        self.connection.execute("DELETE FROM limit_samples WHERE at_ms < ?1", [at_ms - LIMIT_SAMPLES_KEPT_MS])?;
        Ok(())
    }

    /// The samples of an account's limits from `since_ms` on, oldest first: `(at, kind, percent)`.
    pub fn limit_samples(&self, account: &str, since_ms: i64) -> Result<Vec<(i64, String, f64)>> {
        let mut statement = self.connection.prepare("SELECT at_ms, kind, percent FROM limit_samples WHERE account = ?1 AND at_ms >= ?2 ORDER BY at_ms")?;
        let rows = statement.query_map(params![account, since_ms], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    pub fn remove_setting(&self, key: &str) -> Result<()> {
        self.connection.execute("DELETE FROM settings WHERE key = ?1", [key])?;
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

    /// Samples of the limits come back oldest first from a time on; old ones go.
    #[test]
    fn limit_samples_are_kept_per_account() {
        let library = Library::in_memory().unwrap();
        let day = 86_400_000;
        library.add_limit_samples("local|claude", 100 * day, &[("five_hours".into(), 10.0), ("week".into(), 3.0)]).unwrap();
        library.add_limit_samples("local|claude", 101 * day, &[("five_hours".into(), 40.0)]).unwrap();
        library.add_limit_samples("ssh:edge|claude", 101 * day, &[("five_hours".into(), 99.0)]).unwrap();
        let samples = library.limit_samples("local|claude", 0).unwrap();
        assert_eq!(samples, vec![(100 * day, "five_hours".to_owned(), 10.0), (100 * day, "week".to_owned(), 3.0), (101 * day, "five_hours".to_owned(), 40.0)]);
        assert_eq!(library.limit_samples("local|claude", 101 * day).unwrap().len(), 1);
        library.add_limit_samples("local|claude", 170 * day, &[("five_hours".into(), 1.0)]).unwrap();
        assert_eq!(library.limit_samples("local|claude", 0).unwrap().len(), 1, "two months on, the old ones are gone");
    }

    #[test]
    fn a_setting_goes_away_when_removed() {
        let library = Library::in_memory().unwrap();
        library.set_setting("memory.token.dev@box", "secret").unwrap();
        assert_eq!(library.setting("memory.token.dev@box").as_deref(), Some("secret"));
        library.remove_setting("memory.token.dev@box").unwrap();
        assert_eq!(library.setting("memory.token.dev@box"), None);
        library.remove_setting("never.there").unwrap();
    }

    /// A library from before the tables of rows gets them, one row per session, and goes on
    /// finding, renaming and dropping sessions through them.
    #[test]
    fn the_full_text_rows_are_found_by_their_table_of_rows() {
        let connection = Connection::open_in_memory().unwrap();
        let rows_at = MIGRATIONS.iter().position(|migration| migration.contains("CREATE TABLE search_rows")).unwrap();
        for migration in &MIGRATIONS[..rows_at] {
            connection.execute_batch(migration).unwrap();
        }
        connection.pragma_update(None, "user_version", rows_at as i64).unwrap();
        let key = summary("a", "Parser fix", "/w", 1).key.encode();
        for title in ["stale twin", "Parser fix"] {
            connection
                .execute("INSERT INTO search (session_key, title, prompt, cwd, body) VALUES (?1, ?2, '', '/w', '')", params![key, title])
                .unwrap();
        }
        let mut library = Library::with_connection(connection).unwrap();
        let rows: i64 = library.connection.query_row("SELECT count(*) FROM search", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 1, "the older twin is gone");
        assert!(library.search("stale", 10).unwrap().is_empty());

        library.store_scan("local", "claude", &[summary("a", "Parser fix", "/w", 1), summary("b", "Lexer", "/w", 2)]).unwrap();
        let b = summary("b", "Lexer", "/w", 2).key;
        library.index_body(&b, "tokens and spans").unwrap();
        assert_eq!(library.search("spans", 10).unwrap()[0].key, b);
        library.rename_session(&b, Some("Tokenizer")).unwrap();
        assert_eq!(library.search("tokenizer", 10).unwrap()[0].key, b);
        library.set_digest(&b, "1:2", "first digest", false).unwrap();
        library.set_digest(&b, "1:3", "second digest", false).unwrap();
        assert_eq!(library.search_digests("first", 10).unwrap(), Vec::<SessionKey>::new());
        assert_eq!(library.search_digests("second", 10).unwrap(), vec![b.clone()]);
        library.store_scan("local", "claude", &[summary("a", "Parser fix", "/w", 1)]).unwrap();
        assert!(library.search("tokenizer", 10).unwrap().is_empty());
        assert!(library.search_digests("second", 10).unwrap().is_empty());
        let left: i64 = library.connection.query_row("SELECT count(*) FROM search_rows", [], |row| row.get(0)).unwrap();
        assert_eq!(left, 1);
    }

    /// Processes that open a new library at once all find it whole: one migrates, the
    /// others wait and find it done.
    #[test]
    fn a_library_opened_at_once_is_migrated_once() {
        for round in 0..5 {
            let path = std::env::temp_dir().join(format!("bowyer-library-race-{}-{round}.db", std::process::id()));
            let opened: Vec<Result<Library>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..6).map(|_| scope.spawn(|| Library::open(&path))).collect();
                handles.into_iter().map(|handle| handle.join().unwrap()).collect()
            });
            for library in &opened {
                let library = library.as_ref().expect("every opener gets the library");
                let version: i64 = library.connection.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
                assert_eq!(version as usize, MIGRATIONS.len());
            }
            drop(opened);
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }

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

    /// A running session changes on every poll; the full-text index is written only when
    /// what it holds changed, not with the whole text of the conversation each time.
    #[test]
    fn a_running_session_does_not_rewrite_its_text_in_the_index() {
        let mut library = Library::in_memory().unwrap();
        let mut running = summary("a", "Parser work", "/w", 10);
        library.store_scan("local", "claude", &[running.clone()]).unwrap();
        library.index_session(&running, &"the websocket reconnect ".repeat(5_000)).unwrap();
        let written = |library: &Library| -> (i64, i64) {
            library.connection.query_row("SELECT count(*), total(length(block)) FROM search_data", [], |row| Ok((row.get(0)?, row.get::<_, f64>(1)? as i64))).unwrap()
        };
        let before = written(&library);
        for poll in 1..=5 {
            running.updated_ms += poll;
            running.source.size += 100;
            running.source.modified_ms += poll;
            running.turn = Some(model::TurnState::Working);
            assert!(library.store_scan("local", "claude", &[running.clone()]).unwrap(), "the row in the tree changed");
        }
        assert_eq!(written(&library), before, "the index was not written");
        assert_eq!(library.search("websock", 10).unwrap().len(), 1);

        running.title = "Parser and sockets".into();
        library.store_scan("local", "claude", &[running]).unwrap();
        assert_ne!(written(&library), before, "a new title is written");
        assert_eq!(library.search("sockets", 10).unwrap().len(), 1);
        assert_eq!(library.search("websock", 10).unwrap().len(), 1, "the text stays");
    }

    /// A renamed session is found by its name after the next scan too, and by the
    /// harness's title again once the name is taken away.
    #[test]
    fn a_renamed_session_is_found_by_its_name() {
        let mut library = Library::in_memory().unwrap();
        let first = summary("a", "Parser work", "/w", 1);
        library.store_scan("local", "claude", &[first.clone()]).unwrap();
        library.rename_session(&first.key, Some("Quokka")).unwrap();
        assert_eq!(library.search("quokka", 10).unwrap().len(), 1);
        let mut later = first.clone();
        later.updated_ms = 2;
        library.store_scan("local", "claude", &[later]).unwrap();
        assert_eq!(library.search("quokka", 10).unwrap().len(), 1, "a scan keeps the name");
        library.rename_session(&first.key, None).unwrap();
        assert!(library.search("quokka", 10).unwrap().is_empty());
        assert_eq!(library.search("parser", 10).unwrap().len(), 1, "the harness's title again");
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
    fn a_snippet_marks_its_matched_words_apart_from_brackets_in_the_text() {
        let mut library = Library::in_memory().unwrap();
        library.store_scan("local", "claude", &[summary("a", "Retries", "/w/app", 1)]).unwrap();
        let key = SessionKey { env: EnvId::local(), harness: HarnessKind::Claude, id: "a".into() };
        library.index_body(&key, "the [draft] webhook retry queue\nkeeps failed deliveries").unwrap();
        let hit = &library.search("webhook", 10).unwrap()[0];
        let pieces = snippet_pieces(&hit.snippet);
        assert!(pieces.contains(&("webhook".into(), true)), "{pieces:?}");
        assert!(pieces.iter().any(|(text, matched)| !matched && text.contains("[draft]")), "{pieces:?}");
        assert!(pieces.iter().all(|(text, _)| !text.contains('\n')));
        assert!(hit.bracketed().contains("[webhook]"), "{}", hit.bracketed());
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

    #[test]
    fn digests_are_kept_found_by_any_word_and_go_with_their_session() {
        let mut library = Library::in_memory().unwrap();
        let session = summary("a", "Double charge", "/w", 10);
        library.store_scan("local", "claude", &[session.clone(), summary("b", "Docs", "/w", 11)]).unwrap();
        assert!(library.digest(&session.key).is_none());
        library.set_digest(&session.key, "1:10", "Problem: customers charged twice. Commits: a3f9c21", false).unwrap();
        library.set_digest(&session.key, "2:10", "Problem: customers charged twice on timeouts. Commits: a3f9c21", true).unwrap();
        assert_eq!(library.digest(&session.key).unwrap(), ("2:10".into(), "Problem: customers charged twice on timeouts. Commits: a3f9c21".into(), true));
        let found = library.search_digests("were people charged two times?", 10).unwrap();
        assert_eq!(found, vec![session.key.clone()], "any word, and only once per session");
        assert!(library.search_digests("how did we", 10).unwrap().is_empty(), "no meaningful words");
        library.store_scan("local", "claude", &[summary("b", "Docs", "/w", 11)]).unwrap();
        assert!(library.digest(&session.key).is_none());
        assert!(library.search_digests("charged", 10).unwrap().is_empty());
    }

    #[test]
    fn any_word_search_ranks_sessions_that_hold_more_words_first() {
        let mut library = Library::in_memory().unwrap();
        library
            .store_scan("local", "claude", &[summary("a", "Refund webhook", "/w", 1), summary("b", "Refund webhook retries twice", "/w", 2)])
            .unwrap();
        assert!(library.search("how do refund retries work", 10).unwrap().is_empty(), "every word");
        let hits = library.search_any("how do refund retries work", 10).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.key.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(meaningful_words("Remember the session where we fixed the refund?"), ["refund"]);
    }

    #[test]
    fn the_sessions_a_session_started_are_kept_by_title() {
        let library = Library::in_memory().unwrap();
        let parent = SessionKey { env: model::EnvId::local(), harness: model::HarnessKind::Claude, id: "p".into() };
        library.set_started(&parent, &["Measure".to_owned(), "Fix".to_owned()]).unwrap();
        library.set_started(&parent, &["Measure".to_owned()]).unwrap();
        assert_eq!(library.started().unwrap(), vec![(parent, "Measure".to_owned())], "the newest list replaces the one before");
    }
}
