//! The sidebar tree: the user's folders with the sessions they put there, then every other
//! session grouped by project, environment or harness. Pure data, so the order rules are
//! tested without a window.

use std::collections::{HashMap, HashSet};

use model::{SessionKey, SessionSummary};
use serde::{Deserialize, Serialize};

use crate::{Folder, FolderId, Placement};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// The order the user dragged sessions into; new ones on top.
    Manual,
    #[default]
    Updated,
    Created,
    Title,
}

impl SortOrder {
    pub const ALL: [SortOrder; 4] = [Self::Manual, Self::Updated, Self::Created, Self::Title];

    pub fn id(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Updated => "updated",
            Self::Created => "created",
            Self::Title => "title",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL.into_iter().find(|order| order.id() == id).unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    None,
    /// By working folder.
    #[default]
    Project,
    Environment,
    Harness,
}

impl Grouping {
    pub const ALL: [Grouping; 4] = [Self::None, Self::Project, Self::Environment, Self::Harness];

    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Project => "project",
            Self::Environment => "environment",
            Self::Harness => "harness",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL.into_iter().find(|grouping| grouping.id() == id).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TreeNodeKind {
    Folder(FolderId),
    /// An automatic group; the string is its id in the collapsed set.
    Group(String),
    Session(SessionKey),
    /// The rest of a long list: the collapse id of the node it belongs to and the number of
    /// sessions it holds back.
    More(String, usize),
}

/// How many rows a group, folder or session shows before "Show more".
pub const SHOWN_ROWS: usize = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub kind: TreeNodeKind,
    pub label: String,
    /// A second line or a hint: the folder of a project group, the harness of a session.
    pub detail: Option<String>,
    pub collapsed: bool,
    pub pinned: bool,
    /// The newest session inside, for sorting groups.
    pub updated_ms: i64,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// Sessions inside, at any depth.
    pub fn session_count(&self) -> usize {
        let own = match self.kind {
            TreeNodeKind::Session(_) => 1,
            TreeNodeKind::More(_, hidden) => hidden,
            _ => 0,
        };
        own + self.children.iter().map(TreeNode::session_count).sum::<usize>()
    }

    /// The id this node uses in the set of collapsed nodes. Groups and folders are open until
    /// the user closes them; a session with sessions under it is closed until the user opens
    /// it, so its id names it open (`o:`).
    pub fn collapse_id(&self) -> String {
        match &self.kind {
            TreeNodeKind::Folder(id) => format!("f:{id}"),
            TreeNodeKind::Group(id) => format!("g:{id}"),
            TreeNodeKind::Session(key) => format!("o:{}", key.encode()),
            TreeNodeKind::More(id, _) => format!("m:{id}"),
        }
    }
}

fn project_label(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(cwd).to_owned()
}

/// Folders where agents and apps put the worktrees of a repository: `<repo>/<marker><name>`.
const WORKTREE_MARKERS: [&str; 2] = ["/.claude/worktrees/", "/.worktrees/"];

/// The project a working folder belongs to: the folder itself, or the repository whose
/// worktree it is, with the name of the worktree.
pub fn project_root(cwd: &str) -> (&str, Option<&str>) {
    // `\` and `/` are one byte each, so positions in the plain form hold in the original.
    let plain = cwd.replace('\\', "/");
    for marker in WORKTREE_MARKERS {
        if let Some(at) = plain.find(marker) {
            let rest = &cwd[at + marker.len()..];
            let name = rest.split(['/', '\\']).next().unwrap_or_default();
            if at > 0 && !name.is_empty() {
                return (&cwd[..at], Some(name));
            }
        }
    }
    (cwd, None)
}

/// A folder in one spelling: `/` for separators, no trailing one, and lowercase for Windows
/// paths, whose case does not matter.
fn folder_key(path: &str) -> String {
    let plain = path.replace('\\', "/");
    let plain = plain.trim_end_matches('/');
    let windows = plain.as_bytes().get(1) == Some(&b':');
    if windows { plain.to_lowercase() } else { plain.to_owned() }
}

/// How long after the last write of a session a worktree session it started may begin.
const STARTED_BY_SLACK_MS: i64 = 5 * 60 * 1000;

/// The session that most likely started `summary`: a session in its worktree's repository on
/// the same host that was at work when `summary` began (the newest one, when several were).
/// Agents that hand work to other harnesses in worktrees leave no other trace of it.
fn started_by<'a>(summary: &SessionSummary, in_roots: &HashMap<(String, String), Vec<&'a SessionSummary>>) -> Option<&'a SessionSummary> {
    let cwd = summary.cwd.as_deref()?;
    let (root, Some(_)) = project_root(cwd) else { return None };
    let candidates = in_roots.get(&(summary.key.env.0.clone(), folder_key(root)))?;
    candidates
        .iter()
        .filter(|parent| {
            parent.key != summary.key
                && parent.created_ms <= summary.created_ms
                && summary.created_ms <= parent.updated_ms + STARTED_BY_SLACK_MS
        })
        .max_by_key(|parent| parent.created_ms)
        .copied()
}

/// Cuts long lists of rows to [`SHOWN_ROWS`] and a "Show more" row, except where the user
/// asked for all of them.
fn cut_long_lists(nodes: &mut [TreeNode], show_all: &HashSet<String>) {
    for node in nodes {
        cut_long_lists(&mut node.children, show_all);
        let id = node.collapse_id();
        if node.children.len() > SHOWN_ROWS + 2 && !show_all.contains(&id) {
            let hidden: usize = node.children[SHOWN_ROWS..].iter().map(TreeNode::session_count).sum();
            node.children.truncate(SHOWN_ROWS);
            node.children.push(TreeNode {
                kind: TreeNodeKind::More(id, hidden),
                label: String::new(),
                detail: None,
                collapsed: false,
                pinned: false,
                updated_ms: 0,
                children: Vec::new(),
            });
        }
    }
}

fn session_node(summary: &SessionSummary, placement: Option<&Placement>, collapsed: &HashSet<String>) -> TreeNode {
    let label = placement
        .and_then(|placement| placement.title.clone())
        .unwrap_or_else(|| summary.display_title());
    let mut node = TreeNode {
        kind: TreeNodeKind::Session(summary.key.clone()),
        label,
        detail: Some(summary.key.harness.label().to_owned()),
        collapsed: false,
        pinned: placement.is_some_and(|p| p.pinned),
        updated_ms: summary.updated_ms,
        children: Vec::new(),
    };
    node.collapsed = !collapsed.contains(&node.collapse_id());
    node
}

fn sort_sessions(nodes: &mut [TreeNode], order: SortOrder, summaries: &HashMap<&SessionKey, &SessionSummary>, placements: &HashMap<SessionKey, Placement>) {
    let key_of = |node: &TreeNode| match &node.kind {
        TreeNodeKind::Session(key) => Some(key.clone()),
        _ => None,
    };
    nodes.sort_by(|a, b| {
        let (ka, kb) = (key_of(a), key_of(b));
        let (sa, sb) = (ka.as_ref().and_then(|k| summaries.get(k)), kb.as_ref().and_then(|k| summaries.get(k)));
        b.pinned.cmp(&a.pinned).then_with(|| match order {
            SortOrder::Manual => {
                let position = |key: &Option<SessionKey>| key.as_ref().and_then(|k| placements.get(k)).map(|p| p.position);
                match (position(&ka), position(&kb)) {
                    (Some(pa), Some(pb)) => pa.total_cmp(&pb),
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (None, None) => b.updated_ms.cmp(&a.updated_ms),
                }
            }
            SortOrder::Updated => b.updated_ms.cmp(&a.updated_ms),
            SortOrder::Created => {
                let created = |s: Option<&&SessionSummary>| s.map_or(0, |s| s.created_ms);
                created(sb).cmp(&created(sa))
            }
            SortOrder::Title => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
        })
    });
}

/// The id of the group of pinned sessions, always first.
pub const PINNED_GROUP: &str = "pinned";

/// The whole tree. `collapsed` holds the collapse ids of automatic groups the user closed
/// and of sessions with children the user opened; folders keep their own state. Lists in
/// `show_all` (collapse ids) are shown whole, the others stop at [`SHOWN_ROWS`].
pub fn build_tree(
    summaries: &[SessionSummary],
    folders: &[Folder],
    placements: &HashMap<SessionKey, Placement>,
    order: SortOrder,
    grouping: Grouping,
    collapsed: &HashSet<String>,
    show_all: &HashSet<String>,
) -> Vec<TreeNode> {
    let by_key: HashMap<&SessionKey, &SessionSummary> = summaries.iter().map(|s| (&s.key, s)).collect();
    let visible: Vec<&SessionSummary> = summaries
        .iter()
        .filter(|s| !placements.get(&s.key).is_some_and(|p| p.hidden))
        .collect();

    // Child sessions hang under their parent when the parent is shown: subagents and forks
    // under the session they name, sessions in a worktree under the session of the
    // repository that was at work when they began.
    let ids: HashSet<(&model::EnvId, &model::HarnessKind, &str)> =
        visible.iter().map(|s| (&s.key.env, &s.key.harness, s.key.id.as_str())).collect();
    let mut in_roots: HashMap<(String, String), Vec<&SessionSummary>> = HashMap::new();
    for summary in &visible {
        if let Some(cwd) = summary.cwd.as_deref()
            && project_root(cwd).1.is_none()
            && summary.parent.is_none()
        {
            in_roots.entry((summary.key.env.0.clone(), folder_key(cwd))).or_default().push(summary);
        }
    }
    let mut children: HashMap<SessionKey, Vec<&SessionSummary>> = HashMap::new();
    let mut roots: Vec<&SessionSummary> = Vec::new();
    for summary in &visible {
        match &summary.parent {
            Some(parent) if ids.contains(&(&summary.key.env, &summary.key.harness, parent.as_str())) => {
                let parent = SessionKey { env: summary.key.env.clone(), harness: summary.key.harness.clone(), id: parent.clone() };
                children.entry(parent).or_default().push(summary);
            }
            // A session the user pinned or filed stays where the user put it.
            _ => match placements.get(&summary.key).filter(|p| p.pinned || p.folder.is_some()).map_or_else(|| started_by(summary, &in_roots), |_| None) {
                Some(parent) => children.entry(parent.key.clone()).or_default().push(summary),
                None => roots.push(summary),
            },
        }
    }
    let make = |summary: &SessionSummary| {
        let mut node = session_node(summary, placements.get(&summary.key), collapsed);
        if let Some(kids) = children.get(&summary.key) {
            node.children = kids.iter().map(|kid| session_node(kid, placements.get(&kid.key), collapsed)).collect();
            // Newest first: with many workers the recent ones matter, and the list is cut.
            sort_sessions(&mut node.children, SortOrder::Created, &by_key, placements);
        }
        node
    };

    let folder_ids: HashSet<FolderId> = folders.iter().map(|f| f.id).collect();
    let mut in_folder: HashMap<FolderId, Vec<TreeNode>> = HashMap::new();
    let mut unfiled: Vec<&SessionSummary> = Vec::new();
    let mut pinned: Vec<TreeNode> = Vec::new();
    for summary in roots {
        let placement = placements.get(&summary.key);
        match placement.and_then(|p| p.folder).filter(|id| folder_ids.contains(id)) {
            Some(folder) => in_folder.entry(folder).or_default().push(make(summary)),
            // A pinned session outside the user's folders goes to the group on top.
            None if placement.is_some_and(|p| p.pinned) => pinned.push(make(summary)),
            None => unfiled.push(summary),
        }
    }

    fn folder_nodes(
        parent: Option<FolderId>,
        folders: &[Folder],
        in_folder: &mut HashMap<FolderId, Vec<TreeNode>>,
        sort: &dyn Fn(&mut Vec<TreeNode>),
    ) -> Vec<TreeNode> {
        let mut level: Vec<&Folder> = folders.iter().filter(|f| f.parent == parent).collect();
        level.sort_by(|a, b| a.position.total_cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
        level
            .into_iter()
            .map(|folder| {
                let mut sessions = in_folder.remove(&folder.id).unwrap_or_default();
                sort(&mut sessions);
                let mut kids = folder_nodes(Some(folder.id), folders, in_folder, sort);
                kids.extend(sessions);
                let updated_ms = kids.iter().map(|k| k.updated_ms).max().unwrap_or(0);
                TreeNode {
                    kind: TreeNodeKind::Folder(folder.id),
                    label: folder.name.clone(),
                    detail: None,
                    collapsed: folder.collapsed,
                    pinned: false,
                    updated_ms,
                    children: kids,
                }
            })
            .collect()
    }
    let sort = |nodes: &mut Vec<TreeNode>| sort_sessions(nodes, order, &by_key, placements);
    let mut tree = Vec::new();
    if !pinned.is_empty() {
        sort(&mut pinned);
        let mut node = TreeNode {
            kind: TreeNodeKind::Group(PINNED_GROUP.to_owned()),
            label: "Pinned".to_owned(),
            detail: None,
            collapsed: false,
            pinned: false,
            updated_ms: pinned.iter().map(|s| s.updated_ms).max().unwrap_or(0),
            children: pinned,
        };
        node.collapsed = collapsed.contains(&node.collapse_id());
        tree.push(node);
    }
    tree.extend(folder_nodes(None, folders, &mut in_folder, &sort));

    let loose: Vec<TreeNode> = if grouping == Grouping::None {
        let mut loose: Vec<TreeNode> = unfiled.iter().map(|s| make(s)).collect();
        sort(&mut loose);
        loose
    } else {
        let mut groups: HashMap<String, (String, Option<String>, Vec<TreeNode>)> = HashMap::new();
        for summary in unfiled {
            let (id, label, detail) = match grouping {
                Grouping::Project => {
                    // A worktree belongs to its repository's group.
                    let cwd = summary.cwd.clone().unwrap_or_default();
                    let root = project_root(&cwd).0.to_owned();
                    let label = if root.is_empty() { "No folder".to_owned() } else { project_label(&root) };
                    (format!("{}|{}", summary.key.env, folder_key(&root)), label, Some(root).filter(|c| !c.is_empty()))
                }
                Grouping::Environment => (summary.key.env.0.clone(), summary.key.env.0.clone(), None),
                Grouping::Harness => (summary.key.harness.id().to_owned(), summary.key.harness.label().to_owned(), None),
                Grouping::None => unreachable!(),
            };
            groups.entry(id).or_insert_with(|| (label, detail, Vec::new())).2.push(make(summary));
        }
        let mut group_nodes: Vec<TreeNode> = groups
            .into_iter()
            .map(|(id, (label, detail, mut sessions))| {
                sort(&mut sessions);
                let mut node = TreeNode {
                    kind: TreeNodeKind::Group(id),
                    label,
                    detail,
                    collapsed: false,
                    pinned: false,
                    updated_ms: sessions.iter().map(|s| s.updated_ms).max().unwrap_or(0),
                    children: sessions,
                };
                node.collapsed = collapsed.contains(&node.collapse_id());
                node
            })
            .collect();
        group_nodes.sort_by(|a, b| match order {
            SortOrder::Title => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
            _ => b.updated_ms.cmp(&a.updated_ms),
        });
        group_nodes
    };
    tree.extend(loose);
    cut_long_lists(&mut tree, show_all);
    tree
}

/// The rows a list draws: each node with its depth, children of collapsed nodes left out.
pub fn flatten_visible(nodes: &[TreeNode]) -> Vec<(usize, &TreeNode)> {
    fn walk<'a>(nodes: &'a [TreeNode], depth: usize, out: &mut Vec<(usize, &'a TreeNode)>) {
        for node in nodes {
            out.push((depth, node));
            if !node.collapsed {
                walk(&node.children, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::summary;

    #[test]
    fn folders_come_first_and_the_rest_groups_by_project() {
        let summaries = vec![
            summary("a", "Old api work", "/w/api", 1),
            summary("b", "New api work", "/w/api", 5),
            summary("c", "Web", "C:\\w\\web", 3),
            summary("d", "Filed", "/w/api", 2),
        ];
        let folders = vec![Folder { id: 1, parent: None, name: "Important".into(), position: 1.0, collapsed: false }];
        let mut placements = HashMap::new();
        placements.insert(summaries[3].key.clone(), Placement { folder: Some(1), ..Default::default() });
        let tree = build_tree(&summaries, &folders, &placements, SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["Important", "api", "web"]);
        assert_eq!(tree[1].children[0].label, "New api work");
        assert_eq!(tree[0].session_count(), 1);
    }

    #[test]
    fn manual_order_and_pins() {
        let summaries = vec![summary("a", "A", "/w", 1), summary("b", "B", "/w", 2), summary("c", "C", "/w", 3)];
        let mut placements = HashMap::new();
        placements.insert(summaries[0].key.clone(), Placement { position: 1.0, ..Default::default() });
        placements.insert(summaries[1].key.clone(), Placement { position: 2.0, ..Default::default() });
        placements.insert(summaries[2].key.clone(), Placement { position: 3.0, pinned: true, ..Default::default() });
        let tree = build_tree(&summaries, &[], &placements, SortOrder::Manual, Grouping::None, &HashSet::new(), &HashSet::new());
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["Pinned", "A", "B"], "the pinned group first, then the dragged order");
        assert_eq!(tree[0].kind, TreeNodeKind::Group(PINNED_GROUP.into()));
        assert_eq!(tree[0].children[0].label, "C");
    }

    #[test]
    fn collapsed_groups_hide_their_rows_and_children_follow_parents() {
        let mut child = summary("kid", "Subagent", "/w", 4);
        child.parent = Some("a".into());
        let summaries = vec![summary("a", "Parent", "/w", 1), child];
        let tree = build_tree(&summaries, &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].children[0].label, "Subagent");
        assert_eq!(flatten_visible(&tree).len(), 2, "a session with children starts closed");
        let opened: HashSet<String> = [tree[0].children[0].collapse_id()].into();
        let tree = build_tree(&summaries, &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &opened, &HashSet::new());
        assert_eq!(flatten_visible(&tree).len(), 3);
        let collapsed: HashSet<String> = [tree[0].collapse_id()].into();
        let tree = build_tree(&summaries, &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &collapsed, &HashSet::new());
        assert_eq!(flatten_visible(&tree).len(), 1);
    }

    #[test]
    fn worktrees_belong_to_their_repository() {
        assert_eq!(project_root("C:/w/ide/.claude/worktrees/worker-405"), ("C:/w/ide", Some("worker-405")));
        assert_eq!(project_root("C:\\w\\ide\\.claude\\worktrees\\qa-1\\crates"), ("C:\\w\\ide", Some("qa-1")));
        assert_eq!(project_root("/srv/app/.worktrees/fix"), ("/srv/app", Some("fix")));
        assert_eq!(project_root("/srv/app"), ("/srv/app", None));
        assert_eq!(project_root("/srv/app/.claude/worktrees/"), ("/srv/app/.claude/worktrees/", None));
        assert_eq!(folder_key("C:\\W\\Ide\\"), folder_key("c:/w/ide"));
        assert_ne!(folder_key("/srv/App"), folder_key("/srv/app"));
    }

    #[test]
    fn workers_in_worktrees_hang_under_the_session_that_ran_then() {
        let mut lead = summary("lead", "Plan", "C:\\w\\ide", 100);
        lead.created_ms = 10;
        let mut other = summary("other", "Earlier work", "C:\\w\\ide", 8);
        other.created_ms = 1;
        let mut worker = summary("t1", "T-0001", "C:/w/ide/.claude/worktrees/worker-1", 60);
        worker.created_ms = 50;
        worker.key.harness = model::HarnessKind::OpenCode;
        // Began long after every session of the repository stopped: stays on its own.
        let mut late = summary("t2", "T-0002", "C:/w/ide/.claude/worktrees/worker-2", 10_000_000);
        late.created_ms = 10_000_000;
        let summaries = vec![lead, other, worker, late];
        let tree = build_tree(&summaries, &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        assert_eq!(tree.len(), 1, "one group for the repository and its worktrees");
        assert_eq!(tree[0].label, "ide");
        let labels: Vec<&str> = tree[0].children.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["T-0002", "Plan", "Earlier work"]);
        assert_eq!(tree[0].children[1].children[0].label, "T-0001");
        assert!(tree[0].children[1].collapsed);
        assert_eq!(tree[0].session_count(), 4);

        // Pinned or filed, a worker stays where the user put it.
        let mut placements = HashMap::new();
        placements.insert(summaries[2].key.clone(), Placement { pinned: true, ..Default::default() });
        let tree = build_tree(&summaries, &[], &placements, SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        assert_eq!(tree[0].kind, TreeNodeKind::Group(PINNED_GROUP.into()));
        assert_eq!(tree[0].children[0].label, "T-0001");
        let folders = vec![Folder { id: 7, parent: None, name: "Kept".into(), position: 1.0, collapsed: false }];
        placements.insert(summaries[2].key.clone(), Placement { folder: Some(7), ..Default::default() });
        let tree = build_tree(&summaries, &folders, &placements, SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        assert_eq!(tree[0].label, "Kept");
        assert_eq!(tree[0].children[0].label, "T-0001");
    }

    #[test]
    fn long_lists_stop_at_a_show_more_row() {
        let summaries: Vec<SessionSummary> = (0..25).map(|i| summary(&format!("s{i}"), &format!("S{i}"), "/w", i)).collect();
        let tree = build_tree(&summaries, &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        let group = &tree[0];
        assert_eq!(group.children.len(), SHOWN_ROWS + 1);
        assert_eq!(group.children[SHOWN_ROWS].kind, TreeNodeKind::More(group.collapse_id(), 15));
        assert_eq!(group.session_count(), 25);
        let all: HashSet<String> = [group.collapse_id()].into();
        let tree = build_tree(&summaries, &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &HashSet::new(), &all);
        assert_eq!(tree[0].children.len(), 25);
        // Twelve rows are shown whole: a "Show 2 more" row would save nothing.
        let tree = build_tree(&summaries[..12], &[], &HashMap::new(), SortOrder::Updated, Grouping::Project, &HashSet::new(), &HashSet::new());
        assert_eq!(tree[0].children.len(), 12);
    }
}
