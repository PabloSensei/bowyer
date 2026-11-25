//! The window: the session tree on the left, the open session in the middle, what the
//! session is and did on the right. Disk and network work runs on background threads;
//! this file only arranges state and draws it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use envs::{Environment, GitInfo, LocalEnvironment, ReadRange, SharedEnv, SshEnvironment};
use library::{
    Folder, FolderId, Grouping, Library, Placement, SearchHit, SortOrder, TreeNode, TreeNodeKind, build_tree,
    flatten_visible,
};
use model::{
    ContextBreakdown, ContextPart, FileChange, HarnessKind, Role, SessionKey, SessionSummary, TaskRun, TaskStatus,
    Transcript, TurnState,
};
use ui_kit::prelude::*;
use ui_kit::*;

use live::{Permission, StopHandle};

use crate::display::{self, Item, Piece, ToolView};
use crate::extensions::{self, Extension, ExtensionKind};
use crate::highlight;
use crate::i18n::t;
use crate::launch;
use crate::rerank;
use crate::markdown::{Inline, MdBlock, Rich};

mod composer;
pub(crate) use composer::picture_of_base64;
mod change_tree;
mod changes_view;
mod cost_view;
mod files_view;
mod live_view;
mod memory_view;
mod drop_view;
mod plan_view;
mod quit_view;
mod awake_view;
mod context_menu;
mod selection;
mod settings_view;
mod theme_editor;
mod theme_import;
mod titlebar;
mod term_grid;
mod terminal_view;
mod update_view;

pub use titlebar::window_options;
mod ssh_view;
mod new_session;
mod harness_view;
mod turn_view;
mod spend_view;
mod memory_tunnel;

pub const APP_NAME: &str = "Bowyer";

const LEFT_WIDTH: f32 = 300.;
const RIGHT_WIDTH: f32 = 330.;
/// How often the local history is looked at for sessions running elsewhere.
const POLL_SECONDS: u64 = 6;

/// When to try again something that failed: 10 s after the first failure, twice as long
/// after each next one, at most every 5 minutes.
#[derive(Clone, Copy, Debug)]
struct Retry {
    next: std::time::Instant,
    delay: std::time::Duration,
}

impl Retry {
    const FIRST: std::time::Duration = std::time::Duration::from_secs(10);
    const LONGEST: std::time::Duration = std::time::Duration::from_secs(300);

    /// The retry after a failure, `before` being the one of the failure before it.
    fn after(before: Option<&Retry>) -> Retry {
        let delay = before.map_or(Self::FIRST, |before| (before.delay * 2).min(Self::LONGEST));
        Retry { next: std::time::Instant::now() + delay, delay }
    }

    fn due(&self) -> bool {
        std::time::Instant::now() >= self.next
    }
}
/// Session files bigger than this are not read for the search index in the background.
const INDEX_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// The text of one session in the search index: its start and its end.
const INDEX_TEXT_BYTES: usize = 120_000;
/// A session written to in this time is left for later: a running one would be read again
/// on every poll.
const INDEX_QUIET_MS: i64 = 10 * 60_000;
/// The limits a dragged pane keeps to.
const LEFT_RANGE: (f32, f32) = (200., 640.);
const RIGHT_RANGE: (f32, f32) = (240., 720.);
/// The padding of the window around the panes, and the handle between two panes.
const EDGE: f32 = 6.;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
}
const TREE_ROW: f32 = 30.;
/// The width of the stripe a folded side pane leaves.
const STRIPE_WIDTH: f32 = 40.;
/// The height of the terminal panel under the conversation, and how far it goes.
const TERMINAL_HEIGHT: f32 = 300.;
const TERMINAL_RANGE: (f32, f32) = (120., 2000.);
pub(crate) const MONO: &str = "JetBrains Mono";

fn hex(color: u32) -> Hsla {
    rgb(color).into()
}

/// Puts the results a model ranked first back in its order, the others after them as the
/// search gave them.
fn keep_ranking(hits: &mut [SearchHit], order: &[SessionKey]) {
    let rank = |hit: &SearchHit| order.iter().position(|key| *key == hit.key).unwrap_or(usize::MAX);
    hits.sort_by_key(rank);
}

fn harness_color(harness: &HarnessKind) -> Hsla {
    match harness {
        HarnessKind::Claude => hex(0xD97757),
        HarnessKind::Codex => hex(0x10A37F),
        HarnessKind::OpenCode => hex(0x6E8BFF),
        HarnessKind::DeepSeek => hex(0x4D6BFE),
        HarnessKind::Other(_) => hex(0x9AA0A6),
    }
}

fn part_color(part: ContextPart) -> Hsla {
    match part {
        ContextPart::System => hex(0x8B8FA3),
        ContextPart::Memory => hex(0x4FB8A8),
        ContextPart::Prompts => hex(0xF2A65A),
        ContextPart::Answers => hex(0x7BC67E),
        ContextPart::Thinking => hex(0xB38CF2),
        ContextPart::ToolCalls => hex(0x5AB0F2),
        ContextPart::ToolResults => hex(0x3E7CC9),
        ContextPart::Images => hex(0xE36D8F),
        ContextPart::Summary => hex(0xE8D35A),
    }
}

/// A category of Claude Code's own count of its context, in the language of the window.
fn claude_category_label(name: &str) -> String {
    let key = match name {
        "System prompt" => "claude_part.system_prompt",
        "System tools" => "claude_part.system_tools",
        "MCP tools" => "claude_part.mcp_tools",
        "MCP server instructions" => "claude_part.mcp_instructions",
        "Custom agents" => "claude_part.agents",
        "Memory files" => "claude_part.memory",
        "Skills" => "claude_part.skills",
        "Messages" => "claude_part.messages",
        _ => return name.to_owned(),
    };
    t(key).to_owned()
}

/// Colours of Claude's categories, in their order.
fn claude_category_color(name: &str, index: usize) -> Hsla {
    match name {
        "System prompt" => hex(0x8A8F98),
        "System tools" => hex(0x6B7B8C),
        "MCP tools" | "MCP server instructions" => hex(0xA07CD6),
        "Memory files" => part_color(ContextPart::Memory),
        "Messages" => part_color(ContextPart::Prompts),
        "Skills" => hex(0x5FB3A1),
        "Custom agents" => hex(0xD68A5F),
        _ => [hex(0xC9A227), hex(0x5F8FD6), hex(0xD65F7C)][index % 3],
    }
}

fn part_label(part: ContextPart) -> &'static str {
    t(match part {
        ContextPart::System => "part.system",
        ContextPart::Memory => "part.memory",
        ContextPart::Prompts => "part.prompts",
        ContextPart::Answers => "part.answers",
        ContextPart::Thinking => "part.thinking",
        ContextPart::ToolCalls => "part.tool_calls",
        ContextPart::ToolResults => "part.tool_results",
        ContextPart::Images => "part.images",
        ContextPart::Summary => "part.summary",
    })
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// "now", "5m", "3h", "2d", "6w", "4mo".
fn ago(ms: i64) -> String {
    let minutes = (now_ms() - ms).max(0) / 60_000;
    match minutes {
        0 => "now".into(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 24 => format!("{}h", m / 60),
        m if m < 60 * 24 * 14 => format!("{}d", m / (60 * 24)),
        m if m < 60 * 24 * 60 => format!("{}w", m / (60 * 24 * 7)),
        m => format!("{}mo", m / (60 * 24 * 30)),
    }
}

fn tokens_text(tokens: u64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{:.2}M", t as f64 / 1e6),
        t if t >= 10_000 => format!("{}k", t / 1_000),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1e3),
        t => t.to_string(),
    }
}

/// A row of the flattened tree or of the search results.
#[derive(Clone)]
struct Row {
    depth: usize,
    kind: TreeNodeKind,
    label: SharedString,
    detail: Option<SharedString>,
    collapsed: bool,
    expandable: bool,
    pinned: bool,
    count: usize,
    updated_ms: i64,
    turn: Option<model::TurnState>,
    /// Where the + of a project row starts a new session.
    start: Option<library::SessionStart>,
    /// The environment and harness of a harness group: its version and updates.
    harness: Option<(model::EnvId, HarnessKind)>,
}

/// What a session is doing now, by where its last turn stands and when it last wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowStatus {
    Working,
    /// A tool call waits: a long command, or a permission prompt in a terminal.
    Waiting,
    /// The model answered a short while ago: the user's move.
    YourTurn,
}

fn row_status(turn: Option<model::TurnState>, updated_ms: i64, now: i64) -> Option<RowStatus> {
    let age = now - updated_ms;
    match turn? {
        model::TurnState::Working if age < 90_000 => Some(RowStatus::Working),
        model::TurnState::ToolPending if age < 20_000 => Some(RowStatus::Working),
        model::TurnState::ToolPending if age < 2 * 3_600_000 => Some(RowStatus::Waiting),
        model::TurnState::Idle if age < 20 * 60_000 => Some(RowStatus::YourTurn),
        _ => None,
    }
}

#[derive(Clone)]
enum DragItem {
    Session(SessionKey, SharedString),
    Folder(FolderId, SharedString),
}

struct Ghost(SharedString);

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        div()
            .px(px(10.))
            .py(px(5.))
            .rounded(px(8.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.accent)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

#[derive(Clone, PartialEq)]
enum RenameTarget {
    Folder(FolderId),
    Session(SessionKey),
}

/// A session loaded for the centre and right panes.
struct OpenSession {
    summary: SessionSummary,
    /// Only what the panes show: the transcript itself is dropped after loading.
    model: Option<String>,
    usage: model::Usage,
    cost_usd: Option<f64>,
    items: Arc<Vec<Item>>,
    list: ListState,
    breakdown: ContextBreakdown,
    changes: Vec<FileChange>,
    tasks: Vec<TaskRun>,
    /// The files each prompt's turn changed, from Claude Code's file history.
    turns: Arc<std::collections::HashMap<String, Vec<model::TurnFile>>>,
    git: Option<GitInfo>,
    /// When the last model call answered, for the state of the prompt cache.
    last_call_ms: Option<i64>,
    /// Tokens per price bucket, for the cost.
    usage_by_model: Vec<(String, model::Usage)>,
    /// A Claude session that grew while open keeps its reader, so the next growth reads
    /// only the new lines. Sessions that never grow do not pay for keeping it.
    follower: Arc<Mutex<Option<history::ClaudeFollower>>>,
    /// The bytes of the file the panes show, so a slower read cannot undo a newer one.
    shown_offset: u64,
    /// Tokens of the memory files the harness loads, read once when the session opens.
    memory_tokens: u64,
    /// The context as Claude Code counts it, asked for by the user, with the bytes of the
    /// file it was asked at: once the session grows it is out of date and not shown.
    exact: Option<(u64, model::ClaudeContext)>,
    exact_busy: bool,
    /// The turn started from this view, before the harness names a new session.
    run: Option<u64>,
    /// The last prompt, when the model never answered it.
    failed: Option<model::FailedTurn>,
}

/// A prompt waiting for the end of a turn, and the files sent with it.
struct Queued {
    text: String,
    attached: Vec<composer::Attached>,
}

/// A turn the window runs. Each belongs to its session: several sessions can run at once,
/// and the composer and the conversation show the one of the open session.
struct LiveRun {
    /// Names the run for its events and for the draft session it was started from.
    id: u64,
    /// The session the turn runs in; the id is empty until the harness names a new one.
    key: SessionKey,
    /// How many items of the history the conversation had when the turn began: the
    /// history read while the turn runs already has part of it. `usize::MAX` when unknown.
    base_items: usize,
    /// What the turn was started with, kept for the session once the harness names it.
    model: Option<String>,
    effort: Option<String>,
    /// The turn as it was asked for: a prompt sent while it runs goes the same way.
    spec: live::TurnSpec,
    /// Prompts sent while the turn runs, with their files: they go as the next turn of the
    /// session when this one ends well, and back into the composer when it fails or is
    /// stopped.
    queued: Vec<Queued>,
    stop: StopHandle,
    items: Arc<Vec<Item>>,
    turn: Option<Transcript>,
    started: std::time::Instant,
    /// Questions of the harness before it acts, waiting for the user's answer.
    asks: Vec<live::PermissionAsk>,
    /// Commands being stopped one by one, by tool call.
    stopping: Vec<String>,
    /// Choices picked so far in questions with several answers, by tool call: per question,
    /// the picked choices.
    picks: std::collections::HashMap<String, Vec<Vec<usize>>>,
    /// Answers of the user's own to questions of the harness, by tool call: per question.
    texts: std::collections::HashMap<String, Vec<TextField>>,
    /// The answer field being typed in: tool call and question.
    typing: Option<(String, usize)>,
    answer_focus: FocusHandle,
    finished: bool,
    /// The harness answered and runs on only for what it left in the background (a command,
    /// an agent, a wakeup): the user's move, though its process lives.
    resting: bool,
    /// What the harness runs in the background, and since when the window knows of each.
    background: Vec<(live::BackgroundTask, std::time::Instant)>,
    /// Prompts sent into the running harness that it has not taken yet: they go as the
    /// next turn, like queued ones, if the turn ends first.
    said: Vec<Queued>,
    /// A prompt is on its way into the running harness (its files going up, a round trip
    /// to the host): the next one waits for it.
    saying: bool,
    error: Option<String>,
    stderr: String,
}

/// Models a turn can ask for, per harness; `None` is the harness default.
fn model_presets(harness: &HarnessKind) -> &'static [Option<&'static str>] {
    match harness {
        HarnessKind::Claude => &[None, Some("haiku"), Some("sonnet"), Some("opus")],
        HarnessKind::OpenCode => &[None, Some("opencode-go/deepseek-v4.1-flash"), Some("opencode-go/deepseek-v4-pro")],
        _ => &[None],
    }
}

/// The efforts a harness takes for a turn, the harness default first; none for a harness
/// whose effort Bowyer cannot set (OpenCode over ACP, DeepSeek Harness).
fn effort_presets(harness: &HarnessKind) -> &'static [Option<&'static str>] {
    match harness {
        HarnessKind::Claude => &[None, Some("low"), Some("medium"), Some("high"), Some("xhigh"), Some("max")],
        HarnessKind::Codex => &[None, Some("minimal"), Some("low"), Some("medium"), Some("high")],
        _ => &[],
    }
}

/// The list of a conversation. It follows its end while the reader is there: new text
/// scrolls it, and a scroll up leaves the end until the reader comes back to it.
fn conversation_list(items: usize) -> ListState {
    let list = ListState::new(items, ListAlignment::Top, px(1200.)).with_uniform_item_height(ROW_HEIGHT_GUESS);
    list.set_follow_mode(FollowMode::Tail);
    list
}

/// The height a row of a conversation is taken to have until it is drawn: GPUI counts a
/// row it has not measured as empty, and the scrollbar of a long session would lie.
const ROW_HEIGHT_GUESS: Pixels = px(120.);

/// Replaces rows of a conversation where the reader may be. GPUI moves a place inside
/// the replaced rows to the first of them; a reader who is not at the end keeps theirs.
///
/// GPUI counts a new row as empty until it is drawn: in a long session the scrollbar then
/// shrank with every read of a growing file and grew back once the row was drawn, and rows
/// never drawn stayed empty. New rows take the guess instead; rows already drawn keep the
/// height they were drawn at (and are measured again when next drawn).
fn splice_in_place(list: &ListState, range: std::ops::Range<usize>, count: usize) {
    let top = list.logical_scroll_top();
    list.splice(range, count);
    let _ = list.clone().with_uniform_item_height(ROW_HEIGHT_GUESS);
    if !list.is_following_tail() {
        list.scroll_to(top);
    }
}

/// The rows to replace when a conversation of `old` rows becomes one of `new`: the last old
/// row (it may have grown) and everything after it. Rows before it are the same.
fn changed_tail(old: usize, new: usize) -> (std::ops::Range<usize>, usize) {
    let start = old.min(new).saturating_sub(1);
    (start..old, new - start)
}

fn can_run_live(harness: &HarnessKind) -> bool {
    matches!(harness, HarnessKind::Claude | HarnessKind::Codex | HarnessKind::OpenCode | HarnessKind::DeepSeek)
}

struct Loaded {
    transcript: Transcript,
    memory_tokens: u64,
    items: Vec<Item>,
    breakdown: ContextBreakdown,
    changes: Vec<FileChange>,
    tasks: Vec<TaskRun>,
    turns: std::collections::HashMap<String, Vec<model::TurnFile>>,
    git: Option<GitInfo>,
}

pub struct Shell {
    library: Arc<Mutex<Library>>,
    envs: Vec<SharedEnv>,
    cache_dir: PathBuf,
    catalog: ThemeCatalog,

    summaries: Vec<SessionSummary>,
    /// Sessions that started others in separate sessions, with the titles they gave them.
    started: Vec<(SessionKey, String)>,
    folders: Vec<Folder>,
    placements: HashMap<SessionKey, Placement>,
    collapsed: HashSet<String>,
    /// Lists the user asked to see whole (collapse ids), for this run of the window.
    show_all: HashSet<String>,
    /// Folders of the changed files the user opened; the others show one line each.
    opened_change_folders: HashSet<String>,
    /// The settings page shown over the panes, if any.
    settings: Option<settings_view::SettingsPage>,
    settings_scroll: ScrollHandle,
    settings_dir: PathBuf,
    /// The shells of this computer, found when the terminal settings first open.
    shells: Option<Vec<terminal::Shell>>,
    updates: update_view::UpdateState,
    sort: SortOrder,
    grouping: Grouping,
    rows: Vec<Row>,
    tree_scroll: UniformListScrollHandle,
    scanning: usize,
    /// Scans that failed, by environment and harness: the error and when to try again. A
    /// host that dropped comes back by itself, and its error goes once a scan answers.
    scan_failures: HashMap<(model::EnvId, HarnessKind), (String, Retry)>,
    /// Scans running now, so a retry never runs next to one.
    scans_running: HashSet<(model::EnvId, HarnessKind)>,

    search: TextField,
    search_focus: FocusHandle,
    hits: Option<Vec<SearchHit>>,
    /// The order a model ranked the results of a query in: the same query searched again
    /// (after a scan, say) keeps it.
    ranked: Option<(String, Vec<SessionKey>)>,
    /// The number of the latest search: an older one that finishes later is dropped.
    search_seq: u64,

    renaming: Option<(RenameTarget, TextField)>,
    rename_focus: FocusHandle,
    /// The dialog of a new session, when open.
    new_session: Option<new_session::NewSession>,

    selected: Option<SessionKey>,
    open: Option<OpenSession>,
    loading: bool,
    load_generation: u64,
    load_error: Option<String>,
    /// The session that failed to load and when to load it again.
    load_retry: Option<(SessionKey, Retry)>,
    open_runs: HashSet<(usize, usize)>,
    open_calls: HashSet<(usize, usize, usize)>,
    open_thinking: HashSet<(usize, usize)>,
    /// Compaction summaries the reader unfolded; the rest show only their title.
    open_summaries: HashSet<usize>,
    show_tools: bool,
    info_scroll: ScrollHandle,

    composer: TextField,
    composer_focus: FocusHandle,
    /// The scroll of the composer's text, taller than the field when long.
    composer_scroll: ScrollHandle,
    /// A drag of the mouse selects in the composer.
    composer_selecting: bool,
    ssh_hosts: Vec<SshEnvironment>,
    ssh_form: Option<ssh_view::SshForm>,
    ssh_focus: FocusHandle,
    /// The commit message field of the Changes tab.
    commit_focus: FocusHandle,
    attachments: Vec<composer::Attached>,
    completion: Option<composer::Completion>,
    /// The text before the caret when Escape closed the suggestions, so they stay closed.
    completion_dismissed: Option<String>,
    folder_index: composer::FolderIndex,
    /// A picture shown over the window, with its size.
    viewer: Option<(Arc<Image>, u32, u32)>,
    /// Attachments are being copied to the host of the session.
    uploading: bool,
    /// When the next repaint for a moving timer is due, so a render schedules one, not one more.
    repaint_at: std::cell::Cell<Option<std::time::Instant>>,
    left_width: f32,
    right_width: f32,
    /// Side panes the user hid: the conversation takes their room.
    left_hidden: bool,
    right_hidden: bool,
    /// The pane whose edge is being dragged.
    resizing: Option<Side>,
    /// A look at the local history for changes is under way.
    polling: bool,
    /// A look at the file of the open remote session is under way.
    polling_remote: bool,
    /// The IDE that has the open session's folder open, by that folder: Claude Code on
    /// this computer started from the window connects to it. Looked for at each poll.
    ide: Option<(String, live::IdeLink)>,
    looking_for_ide: bool,
    /// The text of local sessions is being read into the search index.
    indexing: bool,
    /// The menu of a tree row, open after a right click.
    context_menu: Option<context_menu::ContextMenu>,
    /// Text selected in an answer, with its bubble.
    selection: Option<selection::TextSelection>,
    /// The blocks of text drawn in the last frame, for a selection whose end is over none.
    drawn_text: selection::DrawnText,
    /// Where the pointer is while a selection is dragged at the edge of the conversation.
    autoscroll: Option<Point<Pixels>>,
    autoscrolling: bool,
    /// The search results are being ranked by Haiku.
    reranking: bool,
    /// Who orders the search results: Haiku or a local model, remembered.
    ranker: rerank::Ranker,
    /// The menu of rankers under the search: `Some(None)` while the local servers are asked.
    ranker_menu: Option<Option<Vec<rerank::LocalModel>>>,
    /// Ticks of the poll, to look at a remote session every other one.
    poll_ticks: u64,
    /// What the poll of pinned remote sessions learnt since the last scan of their host.
    remote_watch: HashMap<SessionKey, WatchedRemote>,
    polling_pinned: bool,
    /// Remote session files a poll found and asked a scan for.
    remote_unknown: std::collections::HashSet<String>,
    /// When Codex, dsh and OpenCode last wrote on each host, as the poll saw it.
    remote_written: HashMap<(model::EnvId, String), i64>,
    /// The code block copied last, for its "Copied" mark.
    copied: Option<(usize, std::time::Instant)>,
    /// The last status of each watched session (pinned or open), to see it change.
    statuses: HashMap<SessionKey, Option<RowStatus>>,
    statuses_seeded: bool,
    /// The terminals of the window, as tabs of the panel under the conversation.
    terminals: Vec<Entity<terminal_view::TermView>>,
    /// The tab each session's panel shows, and the sessions whose panel is open: switching
    /// sessions switches the terminals with them.
    active_terminal: HashMap<Option<SessionKey>, EntityId>,
    /// The height of the terminal panel, kept between runs.
    terminal_height: f32,
    /// The edge between the conversation and the terminal panel is being dragged.
    resizing_terminal: bool,
    terminal_shown: HashSet<Option<SessionKey>>,
    /// The list of shells under the ▾ of the terminal panel, and where its button is.
    terminal_menu: bool,
    terminal_anchor: MenuAnchor,
    /// The sections of the right pane folded to their titles, by id, kept between runs.
    folded: HashSet<&'static str>,
    /// How many of the open session's commands and agents the right pane lists, newest first;
    /// another session starts again from `TASKS_SHOWN`.
    tasks_shown: Option<(SessionKey, usize)>,
    /// The same for its agents.
    agents_shown: Option<(SessionKey, usize)>,
    /// The plan of each account the open sessions used, and an API account's month.
    plans: HashMap<(model::EnvId, HarnessKind), plan_view::PlanState>,
    months: HashMap<(model::EnvId, HarnessKind), plan_view::MonthState>,
    month_cache: plan_view::MonthCache,
    /// The question before quitting while turns or terminals run.
    quit_ask: Option<quit_view::QuitAsk>,
    /// The next message of this session starts a new one from the line `at`, before the
    /// prompt shown at item `cut`.
    branch: Option<(SessionKey, String, usize)>,
    /// The computer kept from sleeping while agents work.
    awake: crate::awake::Awake,
    /// A watched session wants its user (no key: the open one): on the next draw the
    /// window flashes and the system shows a notification, if the window is not in front.
    attention: Option<(Option<SessionKey>, Call)>,
    right_tab: files_view::RightTab,
    files: files_view::FilesState,
    /// A file shown in the centre in place of the conversation.
    preview: Option<files_view::Preview>,
    changes: changes_view::ChangesState,
    /// The git diff of a file shown in the centre.
    diff_view: Option<changes_view::DiffView>,
    /// A commit of the history opened in the centre.
    commit_view: Option<changes_view::CommitView>,
    /// The files a turn changed, opened from its prompt.
    turn_view: Option<turn_view::TurnView>,
    /// What an account spent over the last weeks, opened from the plan.
    spend_view: Option<spend_view::SpendView>,
    spend_cache: spend_view::SpendCache,
    /// Turns the window runs, of any session; finished ones stay until their session is
    /// read again.
    runs: Vec<LiveRun>,
    /// The harnesses' versions in each environment and the newest releases.
    harness_versions: harness_view::HarnessVersions,
    /// The memory server over HTTP and the tunnels that bring it to SSH hosts.
    memory_link: memory_tunnel::MemoryLink,
    next_run: u64,
    /// What was typed in the composer of each session the user left, with its attachments.
    drafts: std::collections::HashMap<SessionKey, (TextField, Vec<composer::Attached>)>,
    permission: Permission,
    /// The list of the composer that is open: models, efforts or permissions, and the
    /// buttons the lists open under (in that order).
    composer_menu: Option<live_view::ComposerMenu>,
    composer_anchors: [MenuAnchor; 3],
    /// The open list of the sessions pane's footer, and where its buttons are.
    footer_menu: Option<FooterMenu>,
    footer_anchors: [MenuAnchor; 2],
    /// A session to open as soon as a scan has it: the one a new turn just created.
    pending_open: Option<SessionKey>,
    extensions_open: bool,
    theme_editor: Option<theme_editor::ThemeEditor>,
    extensions: Option<Result<Vec<Extension>, String>>,
    /// The session place `extensions` is (or is being read) for: its environment, harness
    /// and folder. Another session open reads them again.
    extensions_of: Option<(model::EnvId, HarnessKind, Option<String>)>,
    status: Option<String>,
    pub(crate) focus: FocusHandle,
}

impl Shell {
    pub fn new(library: Library, settings_dir: PathBuf, cache_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut envs: Vec<SharedEnv> = vec![Arc::new(LocalEnvironment::new())];
        let mut ssh_hosts = Vec::new();
        for record in library.ssh_environments().unwrap_or_default() {
            if record.enabled {
                let ssh = SshEnvironment::new(&record.host)
                    .with_identity(record.identity.map(PathBuf::from))
                    .with_password(envs::secret::exists(&record.host));
                envs.push(Arc::new(ssh.clone()));
                ssh_hosts.push(ssh);
            }
        }
        let width = |key: &str, default: f32, (min, max): (f32, f32)| {
            library.setting(key).and_then(|w| w.parse::<f32>().ok()).unwrap_or(default).clamp(min, max)
        };
        let left_width = width("layout.left", LEFT_WIDTH, LEFT_RANGE);
        let right_width = width("layout.right", RIGHT_WIDTH, RIGHT_RANGE);
        let hidden = |key: &str| library.setting(key).is_some_and(|value| value == "1");
        let (left_hidden, right_hidden) = (hidden("layout.left_hidden"), hidden("layout.right_hidden"));
        let terminal_height = width("layout.terminal", TERMINAL_HEIGHT, TERMINAL_RANGE);
        let mut folded: HashSet<&'static str> = library
            .setting("layout.folded")
            .map(|ids| ids.split(',').filter_map(|id| FOLDING.iter().find(|known| **known == id).copied()).collect())
            .unwrap_or_default();
        if hidden("layout.tasks_collapsed") {
            folded.insert("tasks");
        }
        let sort = library.setting("sort").map_or_else(SortOrder::default, |id| SortOrder::from_id(&id));
        // Nested groups became the default after users had saved a grouping: they get it
        // once, and keep whatever they choose after.
        let grouping = if library.setting("grouping.nested_default").is_none() {
            let _ = library.set_setting("grouping", Grouping::Nested.id());
            let _ = library.set_setting("grouping.nested_default", "1");
            Grouping::Nested
        } else {
            library.setting("grouping").map_or_else(Grouping::default, |id| Grouping::from_id(&id))
        };
        let ranker = library.setting("rerank.model").map_or(rerank::Ranker::Haiku, |value| rerank::Ranker::from_setting(&value));
        let collapsed = library
            .setting("collapsed")
            .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_default();
        let catalog = ThemeCatalog::load(&settings_dir);
        launch::set_local_shell(library.setting("terminal.shell").filter(|line| !line.is_empty()));
        let mut shell = Self {
            library: Arc::new(Mutex::new(library)),
            envs,
            cache_dir,
            catalog,
            summaries: Vec::new(),
            started: Vec::new(),
            folders: Vec::new(),
            placements: HashMap::new(),
            collapsed,
            show_all: HashSet::new(),
            opened_change_folders: HashSet::new(),
            settings: None,
            settings_scroll: ScrollHandle::new(),
            settings_dir: settings_dir.clone(),
            shells: None,
            updates: Default::default(),
            sort,
            grouping,
            rows: Vec::new(),
            tree_scroll: UniformListScrollHandle::new(),
            scanning: 0,
            scan_failures: HashMap::new(),
            scans_running: HashSet::new(),
            search: TextField::from_text(""),
            search_focus: cx.focus_handle(),
            hits: None,
            ranked: None,
            search_seq: 0,
            renaming: None,
            rename_focus: cx.focus_handle(),
            new_session: None,
            selected: None,
            open: None,
            loading: false,
            load_generation: 0,
            load_error: None,
            load_retry: None,
            open_runs: HashSet::new(),
            open_calls: HashSet::new(),
            open_thinking: HashSet::new(),
            open_summaries: HashSet::new(),
            show_tools: false,
            info_scroll: ScrollHandle::new(),
            composer: TextField::from_text(""),
            composer_focus: cx.focus_handle(),
            composer_scroll: ScrollHandle::new(),
            composer_selecting: false,
            ssh_hosts,
            ssh_form: None,
            ssh_focus: cx.focus_handle(),
            commit_focus: cx.focus_handle(),
            attachments: Vec::new(),
            completion: None,
            completion_dismissed: None,
            folder_index: Default::default(),
            viewer: None,
            uploading: false,
            repaint_at: Default::default(),
            left_width,
            right_width,
            left_hidden,
            right_hidden,
            resizing: None,
            polling: false,
            polling_remote: false,
            ide: None,
            looking_for_ide: false,
            remote_watch: HashMap::new(),
            polling_pinned: false,
            remote_unknown: Default::default(),
            remote_written: HashMap::new(),
            indexing: false,
            context_menu: None,
            selection: None,
            drawn_text: Default::default(),
            autoscroll: None,
            autoscrolling: false,
            reranking: false,
            ranker,
            ranker_menu: None,
            poll_ticks: 0,
            copied: None,
            statuses: HashMap::new(),
            statuses_seeded: false,
            attention: None,
            terminals: Vec::new(),
            active_terminal: HashMap::new(),
            terminal_height,
            resizing_terminal: false,
            terminal_shown: HashSet::new(),
            terminal_menu: false,
            terminal_anchor: Default::default(),
            folded,
            tasks_shown: None,
            agents_shown: None,
            plans: HashMap::new(),
            months: HashMap::new(),
            month_cache: Default::default(),
            quit_ask: None,
            branch: None,
            awake: Default::default(),
            right_tab: Default::default(),
            files: Default::default(),
            preview: None,
            changes: Default::default(),
            diff_view: None,
            commit_view: None,
            turn_view: None,
            spend_view: None,
            spend_cache: Default::default(),
            runs: Vec::new(),
            harness_versions: Default::default(),
            memory_link: Default::default(),
            next_run: 1,
            drafts: Default::default(),
            // Turns from the window run with everything allowed unless the user picks otherwise.
            permission: Permission::Bypass,
            composer_menu: None,
            composer_anchors: Default::default(),
            footer_menu: None,
            footer_anchors: Default::default(),
            pending_open: None,
            extensions_open: false,
            theme_editor: None,
            extensions: None,
            extensions_of: None,
            status: None,
            focus: cx.focus_handle(),
        };
        shell.reload(cx);
        shell.rescan(cx);
        shell.schedule_update_checks(cx);
        // Sessions that run in a terminal: their state and new turns, every few seconds.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(POLL_SECONDS)).await;
                let polled = this.update(cx, |this, cx| {
                    this.poll_local(cx);
                    this.look_for_ide(cx);
                    this.look_at_plan(false, cx);
                    this.poll_open_remote(cx);
                    this.poll_pinned_remote(cx);
                    this.retry_failures(cx);
                    this.keep_awake(cx);
                    this.look_at_logins(cx);
                });
                if polled.is_err() {
                    break;
                }
            }
        })
        .detach();
        shell
    }

    fn lib(&self) -> std::sync::MutexGuard<'_, Library> {
        self.library.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Redraws the window when the reader leaves the end of a conversation or comes back
    /// to it, for the button that takes them down. Only those changes redraw: a scroll
    /// otherwise draws the list alone.
    pub(super) fn watch_scroll(&self, list: &ListState, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        let seen = std::rc::Rc::new(std::cell::Cell::new((list.is_following_tail(), false)));
        list.set_scroll_handler(move |event, _, cx| {
            let now = (event.is_following_tail, event.visible_range.end >= event.count);
            if seen.replace(now) != now {
                shell.update(cx, |_, cx| cx.notify()).ok();
            }
        });
    }

    /// The round button over the end of a conversation the reader scrolled up from: it
    /// takes them to the end, which the list then follows again.
    fn render_jump_down(&self, list: &ListState, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Not following, yet at the bottom (the end was reached some other way than by the
        // wheel): nothing to go down to.
        let to_end = list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y;
        if list.is_following_tail() || list.max_offset_for_scrollbar().y <= px(0.) || to_end <= px(4.) {
            return None;
        }
        let list = list.clone();
        Some(
            div()
                .absolute()
                .bottom(px(14.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("jump-down")
                        .size(px(34.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        // Solid over the text it sits on.
                        .bg(Hsla { a: 1.0, ..theme.popup_bg })
                        .border_1()
                        .border_color(theme.border)
                        .shadow_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.hover))
                        .child(icon("general/arrowDown", theme))
                        .tooltip(|_, cx| tooltip(t("conversation.to_end"), None, cx))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            list.set_follow_mode(FollowMode::Tail);
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn env_for(&self, id: &model::EnvId) -> Option<SharedEnv> {
        self.envs.iter().find(|env| &env.id() == id).cloned()
    }

    fn summary(&self, key: &SessionKey) -> Option<&SessionSummary> {
        self.summaries.iter().find(|s| &s.key == key)
    }

    // State ------------------------------------------------------------------------------

    /// Reads the cached tree from the library and rebuilds the rows.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let (summaries, folders, placements, started) = {
            let library = self.lib();
            (
                library.summaries().unwrap_or_default(),
                library.folders().unwrap_or_default(),
                library.placements().unwrap_or_default(),
                library.started().unwrap_or_default(),
            )
        };
        self.summaries = summaries;
        self.started = started;
        self.apply_remote_watch(true);
        // The open remote session knows its state better than the last scan of its host.
        if let Some(open) = &self.open
            && !open.summary.key.env.is_local()
            && let Some(row) = self.summaries.iter_mut().find(|s| s.key == open.summary.key)
            && row.source.modified_ms < open.summary.source.modified_ms
        {
            row.turn = open.summary.turn;
            row.updated_ms = row.updated_ms.max(open.summary.updated_ms);
        }
        self.folders = folders;
        self.placements = placements;
        // A search that is open finds the sessions the scan just brought.
        if !self.search.text.trim().is_empty() {
            self.start_search(cx);
        }
        self.rebuild_rows();
        if self.scanning == 0 {
            self.index_bodies(cx);
        }
        let mut grew = false;
        if let Some(open) = &mut self.open
            && let Some(fresh) = self.summaries.iter().find(|s| s.key == open.summary.key)
        {
            grew = fresh.source != open.summary.source && fresh.source.modified_ms >= open.summary.source.modified_ms;
            // A remote session the window followed on its own knows its file better than
            // the last scan of the host.
            let followed = (fresh.source.modified_ms < open.summary.source.modified_ms).then(|| open.summary.source.clone());
            open.summary = fresh.clone();
            if let Some(source) = followed {
                open.summary.source = source;
            }
        }
        // The open session was written to elsewhere, in a terminal: show the new turns.
        if grew && !self.live_running() && !self.loading {
            self.refresh_open(cx);
        }
        self.check_attention();
        if let Some(key) = self.pending_open.clone()
            && self.summaries.iter().any(|s| s.key == key)
        {
            self.pending_open = None;
            self.selected = None;
            self.open_session(key, cx);
        }
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        if let Some(hits) = &self.hits {
            self.rows = hits
                .iter()
                .filter_map(|hit| {
                    let summary = self.summaries.iter().find(|s| s.key == hit.key)?;
                    let title = self.placements.get(&hit.key).and_then(|p| p.title.clone()).unwrap_or_else(|| summary.display_title());
                    Some(Row {
                        depth: 0,
                        kind: TreeNodeKind::Session(hit.key.clone()),
                        label: title.into(),
                        detail: Some(hit.snippet.clone().into()),
                        collapsed: false,
                        expandable: false,
                        pinned: false,
                        count: 0,
                        updated_ms: summary.updated_ms,
                        turn: summary.turn,
                        start: None,
                        harness: None,
                    })
                })
                .collect();
            return;
        }
        let tree =
            build_tree(&library::link_started(&self.summaries, &self.started), &self.folders, &self.placements, self.sort, self.grouping, &self.collapsed, &self.show_all);
        let turns: HashMap<&SessionKey, model::TurnState> =
            self.summaries.iter().filter_map(|s| s.turn.map(|turn| (&s.key, turn))).collect();
        self.rows = flatten_visible(&tree)
            .into_iter()
            .map(|(depth, node): (usize, &TreeNode)| Row {
                depth,
                kind: node.kind.clone(),
                label: match &node.kind {
                    TreeNodeKind::Group(id) if id == library::PINNED_GROUP => t("group.pinned").into(),
                    // The group of the local environment, alone or around its harnesses.
                    TreeNodeKind::Group(id) if id == model::EnvId::LOCAL => t("group.local").into(),
                    TreeNodeKind::More(_, hidden) => t("tree.show_more").replace("{n}", &hidden.to_string()).into(),
                    _ => node.label.clone().into(),
                },
                detail: node.detail.clone().map(Into::into),
                collapsed: node.collapsed,
                expandable: !node.children.is_empty() || matches!(node.kind, TreeNodeKind::Folder(_)),
                pinned: node.pinned,
                count: node.session_count(),
                updated_ms: node.updated_ms,
                turn: match &node.kind {
                    TreeNodeKind::Session(key) => turns.get(key).copied(),
                    _ => None,
                },
                // Only a harness the window can run takes a new session from the tree.
                start: node.start.clone().filter(|start| can_run_live(&start.harness)),
                harness: node.harness.clone(),
            })
            .collect();
    }

    /// Closes every folder, group and opened session; opens the folders and groups again when
    /// all of them are closed already.
    fn toggle_collapse_all(&mut self, cx: &mut Context<Self>) {
        let tree =
            build_tree(&self.summaries, &self.folders, &self.placements, self.sort, self.grouping, &HashSet::new(), &HashSet::new());
        let groups: Vec<String> = tree
            .iter()
            .filter(|node| matches!(node.kind, TreeNodeKind::Group(_)))
            .map(TreeNode::collapse_id)
            .collect();
        let all_closed = groups.iter().all(|id| self.collapsed.contains(id)) && self.folders.iter().all(|f| f.collapsed);
        // Opened sessions close in both cases: their children are the long lists.
        self.collapsed.retain(|id| !id.starts_with("o:"));
        self.show_all.clear();
        let open = all_closed;
        for id in groups {
            if open {
                self.collapsed.remove(&id);
            } else {
                self.collapsed.insert(id);
            }
        }
        let folders: Vec<FolderId> = self.folders.iter().filter(|f| f.collapsed == open).map(|f| f.id).collect();
        for id in folders {
            let _ = self.lib().set_folder_collapsed(id, !open);
        }
        self.save_collapsed();
        self.reload(cx);
    }

    fn save_collapsed(&self) {
        let ids: Vec<&String> = self.collapsed.iter().collect();
        let _ = self.lib().set_setting("collapsed", &serde_json::to_string(&ids).unwrap_or_default());
    }

    /// Reads every environment's history again, each harness on a thread of its own.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.scan_failures.clear();
        for env in self.envs.clone() {
            for source in history::builtin_sources() {
                self.scan_source(env.clone(), source, cx);
            }
        }
        cx.notify();
    }

    /// Scans that failed are tried again when their time comes, and a session that failed
    /// to load is loaded again: a host that dropped (a network gone for a while, a rebuilt
    /// askpass program) comes back without a restart.
    fn retry_failures(&mut self, cx: &mut Context<Self>) {
        let due: Vec<(model::EnvId, HarnessKind)> = self
            .scan_failures
            .iter()
            .filter(|(key, (_, retry))| retry.due() && !self.scans_running.contains(*key))
            .map(|(key, _)| key.clone())
            .collect();
        for (env_id, harness) in due {
            match (self.env_for(&env_id), history::source_for(&harness)) {
                (Some(env), Some(source)) => self.scan_source(env, source, cx),
                // A host the user forgot: nothing to try again.
                _ => {
                    self.scan_failures.remove(&(env_id, harness));
                }
            }
        }
        if let Some((key, retry)) = &self.load_retry
            && retry.due()
            && !self.loading
            && self.load_error.is_some()
            && self.selected.as_ref() == Some(key)
        {
            let key = key.clone();
            self.open_session(key, cx);
        }
    }

    /// Reads the history of one harness in one environment again, on a thread of its own.
    fn scan_source(&mut self, env: SharedEnv, source: Box<dyn history::HistorySource>, cx: &mut Context<Self>) {
        let scan_key = (env.id(), source.harness());
        if !self.scans_running.insert(scan_key.clone()) {
            return;
        }
        self.scanning += 1;
        let library = self.library.clone();
        let cache = self.cache_dir.join("remote");
        let task = cx.background_spawn(async move {
            let known = |path: &str| library.lock().ok()?.summary_by_source(path);
            let result = source.scan(&*env, &known, &cache);
            match result {
                Ok(list) => {
                    let stored = library
                        .lock()
                        .map_err(|_| "library lock".to_owned())
                        .and_then(|mut library| {
                            library.store_scan(&env.id().0, source.harness().id(), &list).map_err(|e| e.to_string())
                        });
                    stored.map(|_| ())
                }
                Err(error) => Err(format!("{} · {}: {error}", env.label(), source.harness())),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.scanning -= 1;
                this.scans_running.remove(&scan_key);
                match result {
                    Err(error) => {
                        let retry = Retry::after(this.scan_failures.get(&scan_key).map(|(_, retry)| retry));
                        log::info!("scan failed, again in {:?}: {error}", retry.delay);
                        this.scan_failures.insert(scan_key, (error, retry));
                    }
                    Ok(()) => {
                        this.scan_failures.remove(&scan_key);
                    }
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Pinned sessions and the open one call for their user when they start to wait or
    /// finish: the window flashes in the taskbar if it is not in front. Nothing fires on
    /// the first look after the start.
    fn check_attention(&mut self) {
        let now = now_ms();
        let open = self.open.as_ref().map(|open| open.summary.key.clone());
        let watched: Vec<(SessionKey, Option<RowStatus>)> = self
            .summaries
            .iter()
            .filter(|s| self.placements.get(&s.key).is_some_and(|p| p.pinned) || open.as_ref() == Some(&s.key))
            .map(|s| (s.key.clone(), row_status(s.turn, s.updated_ms, now)))
            .collect();
        for (key, status) in watched {
            let before = self.statuses.insert(key.clone(), status);
            let call = match status {
                Some(RowStatus::Waiting) => Some(Call::Waiting),
                Some(RowStatus::YourTurn) => Some(Call::YourTurn),
                _ => None,
            };
            if let Some(call) = call
                && self.statuses_seeded
                && before.flatten() != status
            {
                self.call_attention(Some(key), call);
            }
        }
        self.statuses_seeded = true;
    }

    /// Calls the user back: the next drawing flashes the window and notifies when it is not
    /// in front; with the window away in the tray nothing is drawn, so the notification goes
    /// at once.
    fn call_attention(&mut self, key: Option<SessionKey>, call: Call) {
        if crate::tray::window_away() {
            self.notify_attention(key, call);
        } else {
            self.attention = Some((key, call));
        }
    }

    /// The system notification of a call: the session's title and what happened.
    fn notify_attention(&self, key: Option<SessionKey>, call: Call) {
        if !self.notifications_enabled() {
            return;
        }
        let key = key.or_else(|| self.open.as_ref().map(|open| open.summary.key.clone()));
        let title = key
            .and_then(|key| {
                let summary = self.summaries.iter().find(|s| s.key == key).or(self.open.as_ref().map(|open| &open.summary).filter(|s| s.key == key))?;
                Some(self.placements.get(&key).and_then(|p| p.title.clone()).unwrap_or_else(|| summary.display_title()))
            })
            .unwrap_or_else(|| APP_NAME.to_owned());
        crate::notify::show(&model::shorten(&title, 120), call.text());
    }

    /// Reads the open session again and adds what is new, keeping the place in the list.
    fn refresh_open(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let summary = open.summary.clone();
        let Some(env) = self.env_for(&summary.key.env) else { return };
        let cache = self.cache_dir.join("remote");
        let generation = self.load_generation;
        let follower = open.follower.clone();
        let dropped = self.dropped_prompt(&summary.key);
        let task = cx.background_spawn(async move {
            let (mut transcript, offset, turn) = if summary.key.harness == HarnessKind::Claude {
                // Read on from where the last refresh stopped; the first one reads it all.
                let mut follower = follower.lock().map_err(|_| "reader poisoned".to_owned())?;
                let path = &summary.source.path;
                match follower.as_mut().filter(|f| f.path() == path.as_str()) {
                    Some(reader) => {
                        if !reader.read_on(&*env).map_err(|e| e.to_string())? {
                            return Err("nothing new".to_owned());
                        }
                    }
                    None => *follower = Some(history::ClaudeFollower::open(&*env, path).map_err(|e| e.to_string())?),
                }
                let reader = follower.as_ref().ok_or_else(|| "no reader".to_owned())?;
                (reader.transcript(), reader.offset(), reader.turn())
            } else {
                let source = history::source_for(&summary.key.harness).ok_or_else(|| "no reader".to_owned())?;
                (source.load(&*env, &summary, &cache).map_err(|e| e.to_string())?, 0, None)
            };
            if let Some(prompt) = &dropped {
                transcript.hide_failed_turn(prompt);
            }
            let items = display::items(&transcript);
            let breakdown = ContextBreakdown::of(&transcript);
            let changes = transcript.changes();
            let tasks = transcript.tasks(false);
            let turns = transcript.turn_files();
            Ok::<_, String>((summary.key, transcript, offset, turn, items, breakdown, changes, tasks, turns))
        });
        cx.spawn(async move |this, cx| {
            let Ok((key, transcript, offset, turn, items, breakdown, changes, tasks, turns)) = task.await else { return };
            this.update(cx, |this, cx| {
                if this.load_generation != generation || this.live_running() {
                    return;
                }
                let Some(open) = &mut this.open else { return };
                if open.summary.key != key || offset < open.shown_offset {
                    return;
                }
                open.shown_offset = offset;
                // A remote session is not scanned while it runs: its row takes the state
                // the reader saw.
                if !key.env.is_local() && turn.is_some() {
                    open.summary.turn = turn;
                    let updated = open.summary.source.modified_ms;
                    if let Some(row) = this.summaries.iter_mut().find(|s| s.key == key) {
                        row.turn = turn;
                        row.updated_ms = row.updated_ms.max(updated);
                    }
                    this.rebuild_rows();
                    this.check_attention();
                }
                let Some(open) = &mut this.open else { return };
                let (old, new) = (open.items.len(), items.len());
                // The list follows the end by itself while the reader is there. Only the end
                // changes as a session grows: the last item may have grown (a reply that went
                // on), or folded into the one before; the rows before keep their heights.
                let (range, count) = changed_tail(old, new);
                splice_in_place(&open.list, range, count);
                open.items = Arc::new(items);
                open.model = transcript.model.clone();
                open.usage = transcript.total_usage;
                open.cost_usd = transcript.cost_usd;
                open.last_call_ms = transcript.last_call_ms;
                open.usage_by_model = transcript.usage_by_model.clone();
                open.breakdown = breakdown;
                open.breakdown.take_memory(open.memory_tokens);
                open.changes = changes;
                open.tasks = tasks;
                open.turns = Arc::new(turns);
                open.failed = transcript.failed_turn.clone();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Looks at the history on this computer again, every few seconds: sessions that run in
    /// a terminal show their state in the tree, and the open one follows its new turns.
    /// Only changed files are read; nothing changed, nothing is redrawn but the labels.
    fn poll_local(&mut self, cx: &mut Context<Self>) {
        if self.scanning > 0 || self.polling {
            return;
        }
        let Some(env) = self.envs.iter().find(|env| env.id().is_local()).cloned() else { return };
        self.polling = true;
        let library = self.library.clone();
        let cache = self.cache_dir.join("remote");
        let task = cx.background_spawn(async move {
            let mut changed = false;
            for source in history::builtin_sources() {
                let known = |path: &str| library.lock().ok()?.summary_by_source(path);
                if let Ok(list) = source.scan(&*env, &known, &cache)
                    && let Ok(mut library) = library.lock()
                {
                    changed |= library.store_scan(&env.id().0, source.harness().id(), &list).unwrap_or(false);
                }
            }
            changed
        });
        cx.spawn(async move |this, cx| {
            let changed = task.await;
            this.update(cx, |this, cx| {
                this.polling = false;
                if changed {
                    this.reload(cx);
                } else {
                    // The labels of the tree move with the clock.
                    this.check_attention();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Opens the menu of rankers and asks the local servers for their models, or closes it.
    fn toggle_ranker_menu(&mut self, cx: &mut Context<Self>) {
        if self.ranker_menu.take().is_some() {
            cx.notify();
            return;
        }
        self.ranker_menu = Some(None);
        let task = cx.background_spawn(async move { rerank::local_models() });
        cx.spawn(async move |this, cx| {
            let models = task.await;
            this.update(cx, |this, cx| {
                if this.ranker_menu.is_some() {
                    this.ranker_menu = Some(Some(models));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn choose_ranker(&mut self, ranker: rerank::Ranker, cx: &mut Context<Self>) {
        let _ = self.lib().set_setting("rerank.model", &ranker.to_setting());
        self.ranker = ranker;
        self.ranker_menu = None;
        cx.notify();
    }

    /// The menu of rankers: Haiku and the models of the local servers that run.
    fn render_ranker_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.ranker_menu.as_ref()?;
        let row = |id: SharedString, label: String, detail: String, chosen: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(6.))
                .hover(|style| style.bg(theme.hover))
                .child(div().w(px(10.)).flex_none().text_color(theme.accent).child(if chosen { "✓" } else { "" }))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(div().flex_none().text_color(theme.text_muted).child(detail))
        };
        let haiku = row("ranker-haiku".into(), "Haiku".to_owned(), t("search.ranker_account").to_owned(), self.ranker == rerank::Ranker::Haiku)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.choose_ranker(rerank::Ranker::Haiku, cx)));
        let locals: Vec<AnyElement> = match menu {
            None => vec![div().px(px(8.)).py(px(4.)).text_color(theme.text_muted).child(t("search.ranker_looking")).into_any_element()],
            Some(models) if models.is_empty() => {
                vec![div().px(px(8.)).py(px(4.)).text_color(theme.text_muted).child(t("search.ranker_none")).into_any_element()]
            }
            Some(models) => models
                .iter()
                .enumerate()
                .map(|(ix, model)| {
                    let ranker = rerank::Ranker::Local(model.clone());
                    let chosen = self.ranker == ranker;
                    row(format!("ranker-local-{ix}").into(), model.name.clone(), model.server.label().to_owned(), chosen)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_ranker(ranker.clone(), cx)))
                        .into_any_element()
                })
                .collect(),
        };
        Some(
            div()
                .mx(px(10.))
                .mb(px(8.))
                .p(px(4.))
                .rounded(px(10.))
                .bg(theme.popup_bg)
                .border_1()
                .border_color(theme.popup_border)
                .text_size(px(12.))
                .child(haiku)
                .children(locals)
                .into_any_element(),
        )
    }

    /// Asks the chosen model (Haiku or a local one) to order the first search results by
    /// what the query means.
    fn rerank_search(&mut self, cx: &mut Context<Self>) {
        let Some(hits) = self.hits.clone() else { return };
        if self.reranking || hits.len() < 2 {
            return;
        }
        let query = self.search.text.trim().to_owned();
        let items: Vec<(String, String)> = hits
            .iter()
            .take(rerank::TOP)
            .map(|hit| {
                let title = self.summaries.iter().find(|s| s.key == hit.key).map(|s| s.display_title()).unwrap_or_else(|| hit.title.clone());
                (title, hit.snippet.clone())
            })
            .collect();
        let count = items.len();
        let prompt = rerank::prompt(&query, &items);
        self.reranking = true;
        self.ranker_menu = None;
        self.status = Some(t("search.ranking").to_owned());
        let ranker = self.ranker.clone();
        let label = ranker.label();
        let task = cx.background_spawn(async move {
            match ranker {
                rerank::Ranker::Haiku => {
                    let folder = std::env::temp_dir();
                    let out = rerank::run_claude(&rerank::arguments(), folder.to_str(), prompt.as_bytes()).map_err(|e| e.to_string())?;
                    rerank::read_answer(&out).ok_or_else(|| model::shorten(out.trim(), 200))
                }
                rerank::Ranker::Local(model) => rerank::ask_local(&model, &prompt).map(|text| (text, None)),
            }
        });
        cx.spawn(async move |this, cx| {
            let answer = task.await;
            this.update(cx, |this, cx| {
                this.reranking = false;
                match answer {
                    Ok((text, cost)) => {
                        // The query may have changed while the model thought.
                        if this.search.text.trim() == query
                            && let Some(hits) = &mut this.hits
                            && hits.len() >= count
                        {
                            let order = rerank::parse_order(&text, count);
                            let top: Vec<SearchHit> = order.iter().map(|&i| hits[i].clone()).collect();
                            let keys = top.iter().map(|hit| hit.key.clone()).collect();
                            hits.splice(0..count, top);
                            this.ranked = Some((query.clone(), keys));
                            this.rebuild_rows();
                        }
                        let cost = cost.map(|c| format!(" · ${c:.4}")).unwrap_or_default();
                        this.status = Some(format!("{} {label}{cost}", t("search.ranked")));
                    }
                    Err(error) => this.status = Some(format!("{} ({label}): {error}", t("search.rank_failed"))),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Reads the conversations of local sessions into the search index, newest first, one
    /// at a time and with pauses, so the window stays quiet. A session is read again only
    /// when its file changed. Remote sessions are indexed when they are opened.
    fn index_bodies(&mut self, cx: &mut Context<Self>) {
        if self.indexing {
            return;
        }
        let Some(env) = self.envs.iter().find(|env| env.id().is_local()).cloned() else { return };
        self.indexing = true;
        let library = self.library.clone();
        let cache = self.cache_dir.join("remote");
        cx.spawn(async move |this, cx| {
            loop {
                let lib = library.clone();
                let batch = cx
                    .background_spawn(async move { lib.lock().ok().and_then(|lib| lib.bodies_to_index(model::EnvId::LOCAL, now_ms() - INDEX_QUIET_MS, 16).ok()).unwrap_or_default() })
                    .await;
                if batch.is_empty() {
                    break;
                }
                for summary in batch {
                    let (env, lib, cache) = (env.clone(), library.clone(), cache.clone());
                    cx.background_spawn(async move {
                        // A file too big to read whole in the background keeps its title,
                        // prompt and folder in the index; opening it indexes its text.
                        let transcript = if summary.source.size > INDEX_MAX_BYTES {
                            None
                        } else {
                            history::source_for(&summary.key.harness).and_then(|source| source.load(&*env, &summary, &cache).ok())
                        };
                        let body = transcript.as_ref().map(|transcript| transcript.search_text_ends(INDEX_TEXT_BYTES)).unwrap_or_default();
                        if let Ok(lib) = lib.lock() {
                            let _ = lib.index_session(&summary, &body);
                            if let Some(transcript) = &transcript {
                                let _ = lib.set_started(&summary.key, &transcript.started_sessions());
                            }
                        }
                    })
                    .await;
                    cx.background_executor().timer(std::time::Duration::from_millis(30)).await;
                }
            }
            // The sessions read may have started others: the tree hangs those under them.
            this.update(cx, |this, cx| {
                this.indexing = false;
                let started = this.lib().started().unwrap_or_default();
                this.started = started;
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Looks for an IDE with the folder of the open session, if it is a Claude Code session
    /// on this computer: Arro, VS Code and the JetBrains plugin leave a lock file for it.
    fn look_for_ide(&mut self, cx: &mut Context<Self>) {
        let folder = self.open.as_ref().and_then(|open| {
            let summary = &open.summary;
            (summary.key.env.is_local() && summary.key.harness == HarnessKind::Claude).then(|| summary.cwd.clone()).flatten()
        });
        let Some(folder) = folder else {
            if self.ide.take().is_some() {
                cx.notify();
            }
            return;
        };
        if self.looking_for_ide {
            return;
        }
        self.looking_for_ide = true;
        let task = cx.background_spawn({
            let folder = folder.clone();
            async move { live::ide_for(&folder) }
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            this.update(cx, |this, cx| {
                this.looking_for_ide = false;
                let found = found.map(|link| (folder, link));
                if this.ide != found {
                    this.ide = found;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The IDE of `folder`, when the last look found one there.
    fn ide_of(&self, folder: &str) -> Option<live::IdeLink> {
        self.ide.as_ref().filter(|(found, _)| found == folder).map(|(_, link)| link.clone())
    }

    /// The open session of an SSH host, looked at every other poll (a round trip each
    /// time): when its file grew, the new lines are read on. Only Claude sessions, whose
    /// history is a file that grows; the rest of the host is scanned when asked.
    fn poll_open_remote(&mut self, cx: &mut Context<Self>) {
        self.poll_ticks += 1;
        if self.polling_remote || self.poll_ticks % 2 != 0 || self.loading || self.live_running() {
            return;
        }
        let Some(open) = &self.open else { return };
        let summary = &open.summary;
        if summary.key.env.is_local() || summary.key.harness != HarnessKind::Claude || summary.source.path.is_empty() {
            return;
        }
        let Some(env) = self.env_for(&summary.key.env) else { return };
        let (key, path) = (summary.key.clone(), summary.source.path.clone());
        self.polling_remote = true;
        let task = cx.background_spawn(async move { env.stat(&path) });
        cx.spawn(async move |this, cx| {
            let meta = task.await;
            this.update(cx, |this, cx| {
                this.polling_remote = false;
                let Ok(meta) = meta else { return };
                let Some(open) = &mut this.open else { return };
                if open.summary.key != key {
                    return;
                }
                let source = &mut open.summary.source;
                if source.size == meta.size && source.modified_ms == meta.modified_ms {
                    return;
                }
                source.size = meta.size;
                source.modified_ms = meta.modified_ms;
                open.summary.updated_ms = open.summary.updated_ms.max(meta.modified_ms);
                this.refresh_open(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Sessions on SSH hosts, other than the open one, tell their state without a scan
    /// of the host. Every few polls, per host: one command stats the pinned Claude
    /// sessions and every Claude session file written in the last minutes, then the tails
    /// of the files that grew are read; a file the window does not know yet is a new
    /// session, and the host's Claude history is scanned again. A second command tells when
    /// Codex, dsh and OpenCode last wrote there; a harness that wrote since the last look
    /// has its history on the host scanned again (only changed files are read).
    fn poll_pinned_remote(&mut self, cx: &mut Context<Self>) {
        if self.polling_pinned || self.poll_ticks % PINNED_POLL_TICKS != 1 {
            return;
        }
        let open = self.open.as_ref().map(|open| open.summary.key.clone());
        let mut hosts: HashMap<model::EnvId, WatchedHost> = HashMap::new();
        for env in self.envs.iter().filter(|env| !env.id().is_local()) {
            hosts.insert(env.id(), WatchedHost { env: env.clone(), root: None, pinned: Vec::new(), known: HashMap::new() });
        }
        for summary in &self.summaries {
            if summary.key.env.is_local() || summary.key.harness != HarnessKind::Claude || summary.source.path.is_empty() {
                continue;
            }
            let Some(host) = hosts.get_mut(&summary.key.env) else { continue };
            if host.root.is_none() {
                host.root = claude_projects_root(&summary.source.path);
            }
            // The open session has a poll of its own; its file is still a known one.
            let followed = open.as_ref() == Some(&summary.key);
            if !followed && self.placements.get(&summary.key).is_some_and(|p| p.pinned) {
                host.pinned.push(summary.source.path.clone());
            }
            host.known.insert(summary.source.path.clone(), (summary.key.clone(), (!followed).then_some(summary.source.size)));
        }
        if hosts.is_empty() {
            return;
        }
        self.polling_pinned = true;
        let task = cx.background_spawn(async move {
            let mut seen = Vec::new();
            let mut new_sessions = Vec::new();
            let mut written = Vec::new();
            for host in hosts.into_values() {
                if let Ok(stamps) = other_stamps(&*host.env) {
                    written.extend(stamps.into_iter().map(|(harness, ms)| (host.env.clone(), harness, ms)));
                }
                if host.root.is_none() && host.pinned.is_empty() {
                    continue;
                }
                let paths: Vec<&str> = host.pinned.iter().map(String::as_str).collect();
                let Ok(stats) = remote_stats(&*host.env, host.root.as_deref(), &paths) else { continue };
                let unknown: Vec<String> = stats.keys().filter(|path| !host.known.contains_key(*path)).cloned().collect();
                if !unknown.is_empty() {
                    new_sessions.push((host.env.clone(), unknown));
                }
                let grown: Vec<(SessionKey, String, u64, i64)> = stats
                    .iter()
                    .filter_map(|(path, &(now_size, modified_ms))| {
                        let (key, size) = host.known.get(path)?;
                        (size.is_some_and(|size| size != now_size)).then(|| (key.clone(), path.clone(), now_size, modified_ms))
                    })
                    .collect();
                if grown.is_empty() {
                    continue;
                }
                let reads: Vec<(String, ReadRange)> = grown.iter().map(|(_, path, _, _)| (path.clone(), ReadRange::Tail(PINNED_TAIL_BYTES))).collect();
                for ((key, _, size, modified_ms), tail) in grown.into_iter().zip(host.env.read_many(&reads)) {
                    let Ok(tail) = tail else { continue };
                    seen.push((key, WatchedRemote { size, modified_ms, turn: history::turn_of_tail(&tail) }));
                }
            }
            (seen, new_sessions, written)
        });
        cx.spawn(async move |this, cx| {
            let (seen, new_sessions, written) = task.await;
            this.update(cx, |this, cx| {
                this.polling_pinned = false;
                // A harness that wrote since the last look: its history on the host again.
                for (env, harness, ms) in written {
                    let key = (env.id(), harness.id().to_owned());
                    if this.remote_written.get(&key).is_some_and(|&before| before >= ms) {
                        continue;
                    }
                    this.remote_written.insert(key, ms);
                    if let Some(source) = history::source_for(&harness) {
                        this.scan_source(env, source, cx);
                    }
                }
                // A file the scan leaves out (a session without messages) asks only once.
                for (env, paths) in new_sessions {
                    let mut fresh = false;
                    for path in paths {
                        fresh |= this.remote_unknown.insert(path);
                    }
                    if fresh && let Some(source) = history::source_for(&HarnessKind::Claude) {
                        this.scan_source(env, source, cx);
                    }
                }
                if seen.is_empty() {
                    return;
                }
                this.remote_watch.extend(seen);
                this.apply_remote_watch(false);
                this.rebuild_rows();
                this.check_attention();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Puts what the poll learnt into the rows, where it is newer than the last scan.
    /// `fresh` rows come straight from the library: one that caught up with the poll ends
    /// its entry. Other rows may already carry what the poll saw.
    fn apply_remote_watch(&mut self, fresh: bool) {
        let watch = &mut self.remote_watch;
        for summary in &mut self.summaries {
            let Some(seen) = watch.get(&summary.key) else { continue };
            if summary.source.size >= seen.size {
                if fresh {
                    // A scan caught up with it.
                    watch.remove(&summary.key);
                }
                continue;
            }
            summary.turn = seen.turn.or(summary.turn);
            summary.source.size = seen.size;
            summary.source.modified_ms = summary.source.modified_ms.max(seen.modified_ms);
            summary.updated_ms = summary.updated_ms.max(seen.modified_ms);
        }
    }

    fn open_session(&mut self, key: SessionKey, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&key) && (self.open.is_some() || self.loading) {
            return;
        }
        // A branch begun in another session is given up.
        self.branch = self.branch.take().filter(|(branched, ..)| *branched == key);
        self.turn_view = self.turn_view.take().filter(|view| view.key() == &key);
        self.spend_view = None;
        self.selected = Some(key.clone());
        // The user went to another session: the one a finished turn wanted to show again
        // stays where it is.
        if self.pending_open.as_ref().is_some_and(|pending| *pending != key) {
            self.pending_open = None;
        }
        self.runs.retain(|run| !run.finished);
        let (Some(summary), Some(env)) = (self.summary(&key).cloned(), self.env_for(&key.env)) else {
            cx.notify();
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        self.loading = true;
        self.load_error = None;
        let cache = self.cache_dir.join("remote");
        let library = self.library.clone();
        let dropped = self.dropped_prompt(&summary.key);
        let task = cx.background_spawn(async move {
            let source = history::source_for(&summary.key.harness).ok_or_else(|| "no reader for this harness".to_owned())?;
            let mut transcript = source.load(&*env, &summary, &cache).map_err(|e| e.to_string())?;
            // A prompt the user dropped is left out until the next turn resumes before it.
            if let Some(prompt) = &dropped {
                transcript.hide_failed_turn(prompt);
            }
            let items = display::items(&transcript);
            let mut breakdown = ContextBreakdown::of(&transcript);
            let memory_tokens = history::memory_tokens(&*env, &summary);
            breakdown.take_memory(memory_tokens);
            let changes = transcript.changes();
            let tasks = transcript.tasks(false);
            let turns = transcript.turn_files();
            let git = summary.cwd.as_deref().and_then(|cwd| env.git_info(cwd));
            if let Ok(library) = library.lock() {
                let _ = library.index_session(&summary, &transcript.search_text_ends(INDEX_TEXT_BYTES));
                let _ = library.set_started(&summary.key, &transcript.started_sessions());
            }
            Ok::<_, String>((summary, Loaded { transcript, items, breakdown, memory_tokens, changes, tasks, turns, git }))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok((summary, mut loaded)) => {
                        this.load_retry = None;
                        this.ide = None;
                        // A turn the window still runs in this session shows after the history
                        // it began on; the history on disk already has part of it.
                        let running = this.runs.iter().find(|run| !run.key.id.is_empty() && run.key == summary.key);
                        let live_items = running.map_or(0, |run| run.items.len());
                        if let Some(run) = running {
                            loaded.items.truncate(run.base_items);
                        }
                        this.switch_composer(Some(&summary), cx);
                        let list = conversation_list(loaded.items.len() + live_items);
                        this.watch_scroll(&list, cx);
                        // The same session read again after a turn of the window keeps the
                        // place the reader was at; a session opened anew shows its end and
                        // follows it, as the list does from the start.
                        let before = this.open.as_ref().filter(|open| open.summary.key == summary.key).map(|open| &open.list);
                        let same = before.is_some();
                        if let Some(before) = before
                            && !before.is_following_tail()
                        {
                            list.scroll_to(before.logical_scroll_top());
                        }
                        // The same session read again keeps its blocks open and its selection.
                        if !same {
                            this.forget_view_state();
                        }
                        this.open = Some(OpenSession {
                            summary,
                            model: loaded.transcript.model.clone(),
                            usage: loaded.transcript.total_usage,
                            cost_usd: loaded.transcript.cost_usd,
                            last_call_ms: loaded.transcript.last_call_ms,
                            usage_by_model: loaded.transcript.usage_by_model.clone(),
                            items: Arc::new(loaded.items),
                            list,
                            breakdown: loaded.breakdown,
                            changes: loaded.changes,
                            tasks: loaded.tasks,
                            turns: Arc::new(loaded.turns),
                            git: loaded.git,
                            follower: Arc::default(),
                            shown_offset: 0,
                            memory_tokens: loaded.memory_tokens,
                            exact: None,
                            exact_busy: false,
                            failed: loaded.transcript.failed_turn.clone(),
                            run: None,
                        });
                        // Each session read (a turn may have added a server) reads the
                        // extensions again, while the panel is open.
                        this.extensions_of = None;
                        this.look_for_ide(cx);
                        this.look_at_plan(false, cx);
                    }
                    Err(error) => {
                        // What was typed for the session left stays with it.
                        this.switch_composer(None, cx);
                        this.open = None;
                        this.load_error = Some(error);
                        // Loaded again later by itself; the same session's failures space
                        // the tries out.
                        let key = this.selected.clone();
                        if let Some(key) = key {
                            let before = this.load_retry.as_ref().filter(|(failed, _)| *failed == key).map(|(_, retry)| retry);
                            this.load_retry = Some((key, Retry::after(before)));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn toggle_row(&mut self, row: &Row, cx: &mut Context<Self>) {
        match &row.kind {
            TreeNodeKind::Folder(id) => {
                let _ = self.lib().set_folder_collapsed(*id, !row.collapsed);
                self.reload(cx);
            }
            kind => {
                let id = TreeNode {
                    kind: kind.clone(),
                    label: String::new(),
                    detail: None,
                    collapsed: false,
                    pinned: false,
                    updated_ms: 0,
                    children: Vec::new(),
                    start: None,
                    harness: None,
                }
                .collapse_id();
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id);
                }
                self.save_collapsed();
                self.rebuild_rows();
                cx.notify();
            }
        }
    }

    fn set_sort(&mut self, sort: SortOrder, cx: &mut Context<Self>) {
        self.sort = sort;
        let _ = self.lib().set_setting("sort", sort.id());
        self.rebuild_rows();
        cx.notify();
    }

    /// A button of the sessions pane's footer that opens its list.
    fn footer_button(&self, menu: FooterMenu, label: &str, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = *cx.global::<Theme>();
        let (id, tip) = match menu {
            FooterMenu::Sort => ("sort", t("menu.sort")),
            FooterMenu::Group => ("group", t("menu.group")),
        };
        let open = self.footer_menu == Some(menu);
        // A long choice is cut short with an ellipsis; the arrow stays.
        let full = format!("{tip}: {label}");
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(24.))
            .px(px(8.))
            .min_w_0()
            .when(menu == FooterMenu::Sort, |this| this.flex_none())
            .overflow_hidden()
            .rounded(px(6.))
            .text_size(px(12.))
            .text_color(theme.text_secondary)
            .hover(|style| style.bg(theme.hover))
            .when(open, |this| this.bg(theme.hover))
            .child(div().min_w_0().truncate().child(label.to_owned()))
            .child(div().flex_none().text_size(px(10.)).child("▾"))
            .tooltip(move |_, cx| tooltip(full.clone(), None, cx))
            .child(self.footer_anchors[menu as usize].canvas())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.footer_menu = if this.footer_menu == Some(menu) { None } else { Some(menu) };
                cx.notify();
            }))
    }

    /// The open list of the footer, above its button.
    fn render_footer_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let menu = self.footer_menu?;
        let bounds = self.footer_anchors[menu as usize].bounds()?;
        let rows: Vec<(String, bool)> = match menu {
            FooterMenu::Sort => SortOrder::ALL.iter().map(|sort| (t(&format!("sort.{}", sort.id())).to_owned(), *sort == self.sort)).collect(),
            FooterMenu::Group => {
                Grouping::ALL.iter().map(|grouping| (t(&format!("group.{}", grouping.id())).to_owned(), *grouping == self.grouping)).collect()
            }
        };
        let title = match menu {
            FooterMenu::Sort => t("menu.sort"),
            FooterMenu::Group => t("menu.group"),
        };
        let list = div()
            .id("footer-menu")
            .min_w(bounds.size.width.max(px(200.)))
            .p(px(4.))
            .rounded(px(9.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.5))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.footer_menu = None;
                cx.notify();
            }))
            .child(div().px(px(10.)).pt(px(4.)).pb(px(6.)).text_size(px(11.)).text_color(theme.text_muted).child(title.to_owned()))
            .children(rows.into_iter().enumerate().map(|(ix, (label, chosen))| {
                div()
                    .id(("footer-menu-row", ix))
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .when(chosen, |this| this.bg(theme.selection).text_color(theme.text))
                    .when(!chosen, |this| this.text_color(theme.text_secondary).hover(|style| style.bg(theme.selection_hover)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.footer_menu = None;
                        match menu {
                            FooterMenu::Sort => this.set_sort(SortOrder::ALL[ix], cx),
                            FooterMenu::Group => this.set_grouping(Grouping::ALL[ix], cx),
                        }
                    }))
            }));
        // The footer sits at the bottom: the list opens upwards.
        let at = point(bounds.left(), bounds.top() - px(4.));
        Some(deferred(anchored().anchor(Anchor::BottomLeft).position(at).snap_to_window_with_margin(px(8.)).child(list)).with_priority(2).into_any_element())
    }

    fn new_folder(&mut self, cx: &mut Context<Self>, window: &mut Window) {
        let id = self.lib().create_folder(None, t("new_folder"));
        if let Ok(id) = id {
            self.reload(cx);
            self.renaming = Some((RenameTarget::Folder(id), TextField::from_text(t("new_folder"))));
            if let Some((_, field)) = &mut self.renaming {
                field.select_all();
            }
            self.rename_focus.focus(window, cx);
        }
    }

    fn start_rename(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let target = match &row.kind {
            TreeNodeKind::Folder(id) => RenameTarget::Folder(*id),
            TreeNodeKind::Session(key) => RenameTarget::Session(key.clone()),
            TreeNodeKind::Group(_) | TreeNodeKind::More(..) => return,
        };
        let mut field = TextField::from_text(row.label.to_string());
        field.select_all();
        self.renaming = Some((target, field));
        self.rename_focus.focus(window, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((target, field)) = self.renaming.take() {
            let name = field.text.trim().to_owned();
            {
                let library = self.lib();
                match target {
                    RenameTarget::Folder(id) if !name.is_empty() => {
                        let _ = library.rename_folder(id, &name);
                    }
                    RenameTarget::Session(key) => {
                        let _ = library.rename_session(&key, Some(&name));
                    }
                    _ => {}
                }
            }
            self.reload(cx);
        }
    }

    fn rename_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "enter" => self.commit_rename(cx),
            "escape" => {
                self.renaming = None;
                cx.notify();
            }
            _ => {
                if let Some((_, field)) = &mut self.renaming
                    && field.key_with_clipboard(event, false, cx)
                {
                    cx.notify();
                }
            }
        }
        cx.stop_propagation();
    }

    fn search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.search = TextField::from_text("");
                self.focus.focus(window, cx);
            }
            "enter" | "down" => {
                if let Some(Row { kind: TreeNodeKind::Session(key), .. }) = self.rows.first().cloned() {
                    self.open_session(key, cx);
                }
            }
            _ => {
                if !self.search.key_with_clipboard(event, false, cx) {
                    return;
                }
            }
        }
        self.start_search(cx);
        cx.stop_propagation();
        cx.notify();
    }

    /// Searches the library for the text of the search field in the background: the
    /// library may be busy storing a scan, and typing must not wait for it. The results
    /// shown stay until the new ones come.
    fn start_search(&mut self, cx: &mut Context<Self>) {
        self.search_seq += 1;
        let seq = self.search_seq;
        let query = self.search.text.trim().to_owned();
        if query.is_empty() {
            self.hits = None;
            self.rebuild_rows();
            return;
        }
        let library = self.library.clone();
        let asked = query.clone();
        let task = cx.background_spawn(async move { library.lock().ok().and_then(|lib| lib.search(&asked, 200).ok()).unwrap_or_default() });
        cx.spawn(async move |this, cx| {
            let mut hits = task.await;
            this.update(cx, |this, cx| {
                if this.search_seq == seq {
                    if let Some((ranked, order)) = &this.ranked
                        && *ranked == query
                    {
                        keep_ranking(&mut hits, order);
                    }
                    this.hits = Some(hits);
                    this.rebuild_rows();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Window shortcuts that work wherever the focus is, before text fields see the key.
    fn root_capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        if stroke.modifiers.control && !stroke.modifiers.alt && !stroke.modifiers.shift && stroke.key == "`" {
            self.toggle_terminal(window, cx);
            cx.stop_propagation();
            return;
        }
        // Ctrl+B hides the left pane or shows it, Ctrl+Alt+B the right one. The terminal
        // keeps Ctrl+B for its programs.
        if stroke.modifiers.control && !stroke.modifiers.shift && stroke.key == "b" && !self.terminal_focused(window, cx) {
            self.toggle_pane(if stroke.modifiers.alt { Side::Right } else { Side::Left }, cx);
            cx.stop_propagation();
            return;
        }
        // Ctrl+C copies what is selected in the conversation. The composer keeps the
        // focus while the reader selects: its own selection, if any, goes first. Other
        // fields and the terminal copy their own text.
        let primary = stroke.modifiers.control || stroke.modifiers.platform;
        let here = (self.composer_focus.is_focused(window) && !self.composer.has_selection()) || self.focus.is_focused(window);
        if primary
            && !stroke.modifiers.alt
            && !stroke.modifiers.shift
            && stroke.key == "c"
            && here
            && self.copy_selection(cx)
        {
            cx.stop_propagation();
        }
    }

    fn root_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let primary = stroke.modifiers.control || stroke.modifiers.platform;
        if stroke.key == "escape" && self.viewer.take().is_some() {
            cx.notify();
            return;
        }
        if stroke.key == "escape" && self.settings.is_some() {
            self.close_settings(cx);
            return;
        }
        if primary && stroke.key == "," {
            self.open_settings(settings_view::SettingsPage::General, window, cx);
            return;
        }
        if stroke.key == "escape"
            && (self.commit_view.take().is_some()
                || self.diff_view.take().is_some()
                || self.preview.is_some()
                || self.turn_view.take().is_some()
                || self.spend_view.take().is_some())
        {
            self.preview = None;
            cx.notify();
            return;
        }
        if primary && (stroke.key == "k" || stroke.key == "f") {
            self.show_pane(Side::Left, cx);
            self.search_focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Moves a dragged session or folder onto a row: into a folder, before a session, or
    /// out to the top level over a group.
    fn drop_on(&mut self, item: &DragItem, target: &Row, cx: &mut Context<Self>) {
        // A pinned session stays on top wherever it is filed: dragged out of the pinned
        // group, it is unpinned so it shows where it was dropped.
        let into_pinned = match &target.kind {
            TreeNodeKind::Group(id) => id == library::PINNED_GROUP,
            TreeNodeKind::Session(other) => self.placements.get(other).is_some_and(|p| p.pinned),
            _ => false,
        };
        if let DragItem::Session(key, _) = item
            && !into_pinned
            && self.placements.get(key).is_some_and(|p| p.pinned)
        {
            let _ = self.lib().set_pinned(key, false);
        }
        match (item, &target.kind) {
            (DragItem::Session(key, _), TreeNodeKind::Folder(folder)) => {
                let position = now_ms() as f64 * -1.0; // On top of the folder in manual order.
                let _ = self.lib().move_session(key, Some(*folder), position);
            }
            (DragItem::Session(key, _), TreeNodeKind::Group(_)) => {
                let position = self.placements.get(key).map_or(0., |p| p.position);
                let _ = self.lib().move_session(key, None, position);
            }
            (DragItem::Session(key, _), TreeNodeKind::Session(before)) if key != before => {
                let folder = self.placements.get(before).and_then(|p| p.folder);
                // Renumber the siblings of the target in their shown order, the dragged one
                // just before the target.
                let mut order: Vec<SessionKey> = Vec::new();
                let target_depth = target.depth;
                if let Some(start) = self.rows.iter().position(|row| row.kind == target.kind) {
                    let mut begin = start;
                    while begin > 0 && self.rows[begin - 1].depth >= target_depth {
                        begin -= 1;
                    }
                    for row in &self.rows[begin..] {
                        if row.depth < target_depth {
                            break;
                        }
                        if row.depth == target_depth
                            && let TreeNodeKind::Session(k) = &row.kind
                            && k != key
                        {
                            if k == before {
                                order.push(key.clone());
                            }
                            order.push(k.clone());
                        }
                    }
                }
                {
                    let library = self.lib();
                    for (index, k) in order.iter().enumerate() {
                        let _ = library.move_session(k, folder, index as f64);
                    }
                }
                if self.sort != SortOrder::Manual {
                    self.set_sort(SortOrder::Manual, cx);
                }
            }
            (DragItem::Folder(id, _), TreeNodeKind::Folder(parent)) if id != parent => {
                let _ = self.lib().move_folder(*id, Some(*parent), now_ms() as f64);
            }
            (DragItem::Folder(id, _), TreeNodeKind::Group(_)) => {
                let _ = self.lib().move_folder(*id, None, now_ms() as f64);
            }
            _ => return,
        }
        self.reload(cx);
    }

    /// Forgets which blocks of the conversation are open and what is selected in it: they
    /// name items by their place, which in another session are other items.
    pub(super) fn forget_view_state(&mut self) {
        self.open_runs.clear();
        self.open_calls.clear();
        self.open_thinking.clear();
        self.open_summaries.clear();
        self.selection = None;
    }

    fn toggle_extensions(&mut self, cx: &mut Context<Self>) {
        self.extensions_open = !self.extensions_open;
        // Opened again, the panel reads again: a server added since then shows.
        if self.extensions_open {
            self.extensions_of = None;
        }
        self.want_extensions(cx);
        cx.notify();
    }

    /// Reads the extensions of the open session while the panel is open, unless they are
    /// read (or being read) for its place already.
    fn want_extensions(&mut self, cx: &mut Context<Self>) {
        if !self.extensions_open {
            return;
        }
        let place = self.open.as_ref().map(|open| (open.summary.key.env.clone(), open.summary.key.harness.clone(), open.summary.cwd.clone()));
        if place != self.extensions_of {
            self.extensions = None;
            self.extensions_of = place;
            self.load_extensions(cx);
        }
    }

    fn load_extensions(&mut self, cx: &mut Context<Self>) {
        let Some(asked) = self.extensions_of.clone() else { return };
        let Some(env) = self.env_for(&asked.0) else { return };
        let (harness, cwd) = (asked.1.clone(), asked.2.clone());
        let task = cx.background_spawn(async move {
            extensions::list(&*env, &harness, cwd.as_deref()).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // The answer for a session left since then is not this one's.
                if this.extensions_of.as_ref() == Some(&asked) {
                    this.extensions = Some(result);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_extension(&mut self, extension: &Extension, cx: &mut Context<Self>) {
        if extension.kind != ExtensionKind::Plugin || !extension.toggleable {
            return;
        }
        let home = LocalEnvironment::new().home().unwrap_or_default();
        match extensions::set_claude_plugin(&home, &extension.name, !extension.enabled) {
            Ok(()) => {
                self.status = Some(format!("{} → {}", extension.name, if extension.enabled { "off" } else { "on" }));
                self.load_extensions(cx);
            }
            Err(error) => self.status = Some(format!("{}: {error}", t("error"))),
        }
        cx.notify();
    }

    /// Asks for a repaint after `delay`, unless one comes sooner already. Renders call it for
    /// moving timers; each render scheduling its own repaint would multiply them without end.
    pub(super) fn repaint_after(&self, delay: std::time::Duration, cx: &mut Context<Self>) {
        let at = std::time::Instant::now() + delay;
        if self.repaint_at.get().is_some_and(|due| due <= at) {
            return;
        }
        self.repaint_at.set(Some(at));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                if this.repaint_at.get() == Some(at) {
                    this.repaint_at.set(None);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether the window runs a turn of this session now.
    fn runs_turn_of(&self, key: &SessionKey) -> bool {
        self.runs.iter().any(|run| !run.finished && !run.key.id.is_empty() && run.key == *key)
    }

    /// Whether the window's turn of this session has answered and waits only for what it
    /// runs in the background.
    fn runs_in_background(&self, key: &SessionKey) -> bool {
        self.runs.iter().any(|run| !run.finished && run.resting && !run.background.is_empty() && !run.key.id.is_empty() && run.key == *key)
    }

    /// Pins a session to the group on top, or puts it back where it was. A pinned session
    /// is shown: its group opens and the list goes to it.
    fn toggle_pin(&mut self, key: &SessionKey, cx: &mut Context<Self>) {
        let pinned = self.placements.get(key).is_some_and(|p| p.pinned);
        let _ = self.lib().set_pinned(key, !pinned);
        let placements = self.lib().placements().unwrap_or_default();
        self.placements = placements;
        if !pinned && self.collapsed.remove(&format!("g:{}", library::PINNED_GROUP)) {
            self.save_collapsed();
        }
        self.rebuild_rows();
        if let Some(ix) = self.rows.iter().position(|row| row.kind == TreeNodeKind::Session(key.clone())) {
            self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// The strip between two panes: drag it to resize the pane, double-click for the default.
    fn render_handle(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let active = self.resizing == Some(side);
        div()
            .id(match side {
                Side::Left => "left-handle",
                Side::Right => "right-handle",
            })
            .flex_none()
            .w(px(EDGE))
            .h_full()
            .flex()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .rounded_full()
                    .when(active, |this| this.bg(theme.accent))
                    .when(!active, |this| this.hover(|style| style.bg(theme.border))),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        match side {
                            Side::Left => this.left_width = LEFT_WIDTH,
                            Side::Right => this.right_width = RIGHT_WIDTH,
                        }
                        this.save_layout();
                    } else {
                        this.resizing = Some(side);
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
    }

    fn resize_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.resizing_terminal {
            if event.pressed_button != Some(MouseButton::Left) {
                self.end_resize(cx);
                return;
            }
            let height = f32::from(window.viewport_size().height);
            let wanted = height - f32::from(event.position.y) - EDGE;
            self.terminal_height = wanted.clamp(TERMINAL_RANGE.0, (height - 220.).max(TERMINAL_RANGE.0));
            cx.notify();
            return;
        }
        let Some(side) = self.resizing else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            // The button came up outside the window.
            self.end_resize(cx);
            return;
        }
        let x = f32::from(event.position.x);
        let width = f32::from(window.viewport_size().width);
        match side {
            Side::Left => self.left_width = (x - EDGE).clamp(LEFT_RANGE.0, LEFT_RANGE.1),
            Side::Right => self.right_width = (width - x - EDGE).clamp(RIGHT_RANGE.0, RIGHT_RANGE.1),
        }
        cx.notify();
    }

    fn end_resize(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.resizing_terminal) {
            let _ = self.lib().set_setting("layout.terminal", &format!("{:.0}", self.terminal_height));
            cx.notify();
        }
        if self.resizing.take().is_some() {
            self.save_layout();
            cx.notify();
        }
    }

    /// Hides a side pane or shows it again; the choice outlives the window.
    fn toggle_pane(&mut self, side: Side, cx: &mut Context<Self>) {
        let (hidden, key) = match side {
            Side::Left => (&mut self.left_hidden, "layout.left_hidden"),
            Side::Right => (&mut self.right_hidden, "layout.right_hidden"),
        };
        *hidden = !*hidden;
        let value = if *hidden { "1" } else { "0" };
        let _ = self.lib().set_setting(key, value);
        cx.notify();
    }

    /// Shows a hidden side pane, for what needs it: the search, the theme editor.
    fn show_pane(&mut self, side: Side, cx: &mut Context<Self>) {
        let hidden = match side {
            Side::Left => self.left_hidden,
            Side::Right => self.right_hidden,
        };
        if hidden {
            self.toggle_pane(side, cx);
        }
    }

    /// The button in a pane's header that folds the pane into its stripe.
    fn hide_pane_button(&self, side: Side, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = *cx.global::<Theme>();
        let (id, keys) = match side {
            Side::Left => ("hide-left", "Ctrl+B"),
            Side::Right => ("hide-right", "Ctrl+Alt+B"),
        };
        icon_button(id, "general/hide", t("panel.hide"), Some(keys.into()), &theme).on_click(cx.listener(
            move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.toggle_pane(side, cx);
            },
        ))
    }

    /// A folded pane: a narrow stripe at the edge of the window with the button that
    /// unfolds it, as the tool window stripes of an IDE.
    fn render_stripe(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let (id, name, label, keys) = match side {
            Side::Left => ("show-left", "own/aiSessions", t("panel.left"), "Ctrl+B"),
            Side::Right => ("show-right", "own/panelRight", t("panel.right"), "Ctrl+Alt+B"),
        };
        island(&theme)
            .flex_none()
            .h_full()
            .w(px(STRIPE_WIDTH))
            .items_center()
            .pt(px(6.))
            .child(stripe_button(id, name, label.into(), Some(keys.into()), false, &theme).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| this.toggle_pane(side, cx),
            )))
    }

    fn save_layout(&self) {
        let _ = self.lib().set_setting("layout.left", &format!("{:.0}", self.left_width));
        let _ = self.lib().set_setting("layout.right", &format!("{:.0}", self.right_width));
    }

    fn report<T>(&mut self, result: std::io::Result<T>, cx: &mut Context<Self>) {
        if let Err(error) = result {
            self.status = Some(format!("{}: {error}", t("error")));
            cx.notify();
        }
    }

    // Left pane ------------------------------------------------------------------------

    fn render_left(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let focused = self.search_focus.is_focused(window);
        let search_box = div()
            .id("search")
            .track_focus(&self.search_focus)
            .on_key_down(cx.listener(Self::search_key))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.search_focus.focus(window, cx);
                cx.notify();
            }))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(theme.editor_bg)
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .child(icon("general/search", &theme))
            .child(if self.search.text.is_empty() && !focused {
                div().text_color(theme.text_muted).child(format!("{}  ·  Ctrl+K", t("search"))).into_any_element()
            } else {
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .children(text_field_children(&self.search, theme.text, theme.over_accent, theme.selection, theme.caret, 16., focused))
                    .into_any_element()
            });

        let rank_bar = self.hits.as_ref().filter(|hits| hits.len() > 1).map(|hits| {
            let busy = self.reranking;
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(px(12.))
                .pb(px(6.))
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(format!("{} {}", hits.len(), t("search.results")))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .child(
                            chip("rerank", if busy { t("search.ranking").to_owned() } else { format!("{} · {}", t("search.rerank"), self.ranker.label()) }, &theme)
                                .when(busy, |this| this.opacity(0.6))
                                .tooltip(|_, cx| tooltip(t("search.rerank_tip"), None, cx))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rerank_search(cx))),
                        )
                        .child(
                            chip("ranker", "▾", &theme)
                                .tooltip(|_, cx| tooltip(t("search.ranker_tip"), None, cx))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_ranker_menu(cx))),
                        ),
                )
        });
        let ranker_menu = rank_bar.is_some().then(|| self.render_ranker_menu(&theme, cx)).flatten();
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(40.))
            .px(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(14.)).child(t("sessions"))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(icon_button("new-session", "general/add", t("new_session"), None, &theme).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.open_new_session(window, cx),
                    )))
                    .child(icon_button("new-folder", "general/newFolder", t("new_folder"), None, &theme).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.new_folder(cx, window)),
                    ))
                    .child(icon_button("collapse-all", "general/collapseAll", t("tree.collapse_all"), None, &theme).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_collapse_all(cx)),
                    ))
                    .child(icon_button("rescan", "general/refresh", t("refresh"), None, &theme).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| this.rescan(cx),
                    )))
                    .child(self.hide_pane_button(Side::Left, cx)),
            );

        let ssh_form = self.render_ssh_form(window, cx);
        let rows = self.rows.len();
        let tree = uniform_list("tree", rows, cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
            range.map(|ix| this.render_row(ix, window, cx)).collect::<Vec<_>>()
        }))
        .track_scroll(&self.tree_scroll)
        .size_full();

        let count = self.summaries.len();
        let empty = (rows == 0).then(|| {
            div()
                .p(px(16.))
                .text_color(theme.text_muted)
                .child(if self.hits.is_some() { t("no_results") } else if self.scanning > 0 { t("scanning") } else { t("no_results") })
        });

        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(34.))
            .px(px(8.))
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(12.))
            .text_color(theme.text_secondary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .min_w_0()
                    .child(icon_button("settings", "general/settings", t("settings"), Some("Ctrl+,".into()), &theme).flex_none().on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| match this.settings {
                            Some(_) => this.close_settings(cx),
                            None => this.open_settings(settings_view::SettingsPage::General, window, cx),
                        },
                    )))
                    .child(self.footer_button(FooterMenu::Sort, t(&format!("sort.{}", self.sort.id())), cx))
                    .child(self.footer_button(FooterMenu::Group, t(&format!("group.{}", self.grouping.id())), cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(2.))
                    .children(self.render_update_chip(&theme, cx))
                    .child(chip("ssh", "SSH +", &theme).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.toggle_ssh_form(window, cx),
                    ))),
            );
        let footer_menu = self.render_footer_menu(cx);

        let status_line = if self.scanning > 0 {
            Some(format!("{}  {count} {}", t("scanning"), t("sessions_count")))
        } else if !self.scan_failures.is_empty() {
            let mut errors: Vec<&str> = self.scan_failures.values().map(|(error, _)| error.as_str()).collect();
            errors.sort_unstable();
            Some(format!("{}\n{}", errors.join("\n"), t("retry.soon")))
        } else {
            self.status.clone()
        };

        island(&theme)
            .w(px(self.left_width))
            .flex_none()
            .h_full()
            .child(header)
            .child(div().px(px(10.)).pb(px(8.)).child(search_box))
            .children(rank_bar)
            .children(ranker_menu)
            .children(ssh_form)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(6.))
                    .children(empty)
                    .child(scrollable("tree-scroll", &self.tree_scroll, tree, &theme, window, cx).size_full()),
            )
            .children(status_line.map(|line| {
                div()
                    .px(px(12.))
                    .py(px(6.))
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .border_t_1()
                    .border_color(theme.border)
                    .child(line)
            }))
            .child(footer)
            .children(footer_menu)
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let Some(row) = self.rows.get(ix).cloned() else { return div().into_any_element() };
        let selected = matches!(&row.kind, TreeNodeKind::Session(key) if self.selected.as_ref() == Some(key));
        let renaming = self.renaming.as_ref().filter(|(target, _)| match (target, &row.kind) {
            (RenameTarget::Folder(a), TreeNodeKind::Folder(b)) => a == b,
            (RenameTarget::Session(a), TreeNodeKind::Session(b)) => a == b,
            _ => false,
        });
        let label: AnyElement = match renaming {
            Some((_, field)) => div()
                .id("rename")
                .track_focus(&self.rename_focus)
                .on_key_down(cx.listener(Self::rename_key))
                .flex()
                .items_center()
                .flex_1()
                .min_w_0()
                .px(px(4.))
                .rounded(px(4.))
                .bg(theme.editor_bg)
                .border_1()
                .border_color(theme.accent)
                .children(text_field_children(field, theme.text, theme.over_accent, theme.selection, theme.caret, 16., self.rename_focus.is_focused(window)))
                .into_any_element(),
            None => div().flex_1().min_w_0().truncate().child(row.label.clone()).into_any_element(),
        };

        let row_for_chevron = row.clone();
        let chevron = div().id(("chevron", ix)).size(px(16.)).flex_none().when(row.expandable, |this| {
            this.child(icon(if row.collapsed { "general/chevronRight" } else { "general/chevronDown" }, &theme))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle_row(&row_for_chevron, cx);
                }))
        });

        let (lead, trailing, drag): (AnyElement, AnyElement, Option<DragItem>) = match &row.kind {
            TreeNodeKind::Folder(id) => (
                icon("general/folder", &theme).into_any_element(),
                div().text_color(theme.text_muted).text_size(px(11.)).child(row.count.to_string()).into_any_element(),
                Some(DragItem::Folder(*id, row.label.clone())),
            ),
            TreeNodeKind::Group(_) => {
                // A harness row shows its version there, and an arrow when a newer one is out.
                let version = row
                    .harness
                    .clone()
                    .and_then(|(env, harness)| self.version_mark(&env, &harness, SharedString::from(format!("tree-update-{ix}")), true, false, cx));
                // A project row starts a new session in its folder.
                let new_session = row.start.clone().map(|start| {
                    let tip = t("tree.new_session").replace("{harness}", start.harness.label());
                    div()
                        .id(("new-in-project", ix))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.))
                        .rounded(px(4.))
                        .text_size(px(14.))
                        .text_color(theme.text_muted)
                        .hover(|style| style.bg(theme.hover).text_color(theme.text))
                        .child("+")
                        .tooltip(move |_, cx| tooltip(tip.clone(), None, cx))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.start_draft_at(start.clone(), window, cx);
                        }))
                });
                (
                    div().into_any_element(),
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .children(version)
                        .children(new_session)
                        .child(div().text_color(theme.text_muted).text_size(px(11.)).child(row.count.to_string()))
                        .into_any_element(),
                    None,
                )
            }
            TreeNodeKind::More(..) => (div().into_any_element(), div().into_any_element(), None),
            TreeNodeKind::Session(key) => {
                let running = self.runs_turn_of(key);
                let pinned = row.pinned;
                let pin_key = key.clone();
                let pin = div()
                    .id(("pin", ix))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(18.))
                    .rounded(px(4.))
                    .when(!pinned, |this| this.opacity(0.).group_hover("tree-row", |style| style.opacity(0.7)))
                    .hover(|style| style.bg(theme.hover).opacity(1.))
                    .child(icon_sized("general/pin", 12., &theme))
                    .tooltip(move |_, cx| tooltip(if pinned { t("unpin") } else { t("pin") }, None, cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_pin(&pin_key, cx);
                    }));
                (
                    div()
                        .size(px(8.))
                        .flex_none()
                        .rounded_full()
                        .bg(harness_color(&key.harness))
                        .when(running, |this| this.border_2().border_color(theme.accent).size(px(10.)))
                        .into_any_element(),
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_color(theme.text_muted)
                        .text_size(px(11.))
                        .when(!key.env.is_local(), |this| this.child(key.env.ssh_host().unwrap_or("").to_owned()))
                        .when(key.harness == HarnessKind::Claude && self.pays_by_key(&key.env), |this| {
                            this.child(
                                div()
                                    .id(SharedString::from(format!("api-badge-{ix}")))
                                    .px(px(4.))
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(theme.warning_mark)
                                    .text_color(theme.warning_mark)
                                    .text_size(px(10.))
                                    .child(t("plan.api_badge"))
                                    .tooltip(|_, cx| tooltip(t("plan.api_badge_tip"), None, cx)),
                            )
                        })
                        .child(match (running, row_status(row.turn, row.updated_ms, now_ms())) {
                            (true, _) if self.runs_in_background(key) => {
                                div().text_color(theme.text_secondary).child(t("status.background")).into_any_element()
                            }
                            (true, _) | (false, Some(RowStatus::Working)) => {
                                div().text_color(theme.accent).child(t("status.working")).into_any_element()
                            }
                            (false, Some(RowStatus::Waiting)) => {
                                div().text_color(theme.warning_mark).child(t("status.waiting")).into_any_element()
                            }
                            (false, Some(RowStatus::YourTurn)) => {
                                div().text_color(theme.success).child(t("status.your_turn")).into_any_element()
                            }
                            (false, None) => div().child(ago(row.updated_ms)).into_any_element(),
                        })
                        .child(pin)
                        .into_any_element(),
                    Some(DragItem::Session(key.clone(), row.label.clone())),
                )
            }
        };
        let is_group = matches!(row.kind, TreeNodeKind::Group(_));
        let is_more = matches!(row.kind, TreeNodeKind::More(..));
        let search_detail = (self.hits.is_some()).then(|| row.detail.clone()).flatten();
        let hover = theme.selection_hover;
        let drop_color = theme.accent.opacity(0.18);
        let row_for_click = row.clone();
        let row_for_drop = row.clone();
        let row_for_menu = row.clone();
        let mut element = div()
            .id(("row", ix))
            .group("tree-row")
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(TREE_ROW))
            .w_full()
            .pl(px(6. + row.depth as f32 * 14.))
            // Room for the scrollbar, which widens over the right edge under the mouse.
            .pr(px(TREE_ROW_END))
            .rounded(px(6.))
            .text_size(px(if is_group { 12. } else { 13. }))
            .when(is_group, |this| this.text_color(theme.text_secondary).font_weight(FontWeight::MEDIUM))
            .when(is_more, |this| this.text_size(px(12.)).text_color(theme.accent))
            .when(selected, |this| this.bg(theme.selection_inactive))
            .when(!selected, |this| this.hover(move |style| style.bg(hover)))
            .child(chevron)
            .child(lead)
            .child(label)
            .when(search_detail.is_none(), |this| this.child(trailing))
            .when_some(search_detail, |this, detail| {
                this.child(div().max_w(px(110.)).truncate().text_size(px(11.)).text_color(theme.text_muted).child(detail))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.start_rename(&row_for_click, window, cx);
                    return;
                }
                match &row_for_click.kind {
                    TreeNodeKind::More(id, _) => {
                        this.show_all.insert(id.clone());
                        this.rebuild_rows();
                        cx.notify();
                    }
                    TreeNodeKind::Session(key) => {
                        if row_for_click.expandable && event.click_count() == 1 && this.selected.as_ref() == Some(key) {
                            this.toggle_row(&row_for_click, cx);
                        }
                        this.open_session(key.clone(), cx);
                    }
                    _ => this.toggle_row(&row_for_click, cx),
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| this.open_context_menu(row_for_menu.clone(), event.position, cx)),
            )
            .drag_over::<DragItem>(move |style, _, _, _| style.bg(drop_color))
            .on_drop(cx.listener(move |this, item: &DragItem, _, cx| this.drop_on(item, &row_for_drop, cx)));
        if let Some(drag) = drag {
            element = element.on_drag(drag, |item: &DragItem, _, _, cx| {
                let label = match item {
                    DragItem::Session(_, label) | DragItem::Folder(_, label) => label.clone(),
                };
                cx.new(|_| Ghost(label))
            });
        }
        element.into_any_element()
    }

    // Centre pane ----------------------------------------------------------------------

    /// The conversation, and the terminal panel under it when it is shown.
    fn render_center(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.render_conversation_pane(window, cx).into_any_element();
        let Some(panel) = self.render_terminal_panel(cx) else { return pane };
        let edge = div()
            .id("terminal-edge")
            .flex_none()
            .h(px(EDGE))
            .cursor_row_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.resizing_terminal = true;
                    cx.stop_propagation();
                }),
            );
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(div().flex().flex_1().min_h_0().child(pane))
            .child(edge)
            .child(panel)
            .into_any_element()
    }

    fn render_conversation_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let pane = island(&theme).flex_1().min_w_0().h_full().bg(theme.editor_bg);
        let Some(open) = &self.open else {
            let message = if self.loading {
                t("loading").to_owned()
            } else if let Some(error) = &self.load_error {
                format!("{}: {error}\n{}", t("error"), t("retry.soon"))
            } else {
                t("no_session").to_owned()
            };
            return pane.child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(16.))
                    .child(logo(64.))
                    .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(APP_NAME))
                    .child(div().text_color(theme.text_muted).child(message)),
            );
        };
        let summary = &open.summary;
        let title = self.placements.get(&summary.key).and_then(|p| p.title.clone()).unwrap_or_else(|| summary.display_title());
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .min_w_0()
                    .child(badge(summary.key.harness.label(), harness_color(&summary.key.harness)))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(14.)).truncate().child(title))
                    .when(self.loading, |this| this.child(div().text_color(theme.text_muted).child(t("loading")))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        chip("tools-toggle", if self.show_tools { t("tools.hide") } else { t("tools.show") }, &theme)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.show_tools = !this.show_tools;
                                // What the reader opens grows down from where it is, not up from the end.
                                if let Some(open) = &this.open {
                                    open.list.pause_following_tail();
                                    open.list.remeasure();
                                }
                                cx.notify();
                            })),
                    )
                    .child(icon_button("extensions", "nodes/plugin", t("mcp"), None, &theme).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| this.toggle_extensions(cx),
                    )))
                    .child(icon_button("terminal", "toolwindows/terminal", t("open_terminal"), Some("Ctrl+`".into()), &theme).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_terminal(window, cx)),
                    ))
                    .child(icon_button("resume", "run/run", t("resume"), None, &theme).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.resume_in_terminal(window, cx),
                    ))),
            );
        let list = list(open.list.clone(), cx.processor(|this, ix: usize, window, cx| this.render_item(ix, window, cx)))
            .size_full();
        let list = scrollable("conversation-scroll", &open.list, list, &theme, window, cx).size_full();
        let jump_down = self.render_jump_down(&open.list, &theme, cx);
        let extensions = self.extensions_open.then(|| self.render_extensions(cx));
        let preview = self
            .render_commit_view(cx)
            .or_else(|| self.render_diff(cx))
            .or_else(|| self.render_preview(window, cx))
            .or_else(|| self.render_turn_view(cx))
            .or_else(|| self.render_spend_view(cx));
        let turn_shown = self.turn_view_shown() || self.spend_view.is_some();
        let composer = self.render_composer(window, cx);
        pane.drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(theme.accent))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.attach_paths(paths.paths().to_vec(), cx);
            }))
            .child(header)
            .when_some(preview, |this, preview| this.child(preview))
            .when(self.preview.is_none() && self.diff_view.is_none() && self.commit_view.is_none() && !turn_shown, |this| {
                this.child(div().relative().flex_1().min_h_0().child(list).children(jump_down).children(extensions))
            })
            .child(composer)
    }

    fn render_extensions(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.want_extensions(cx);
        let theme = *cx.global::<Theme>();
        let connected = matches!(&self.extensions, Some(Ok(list)) if list.iter().any(|e| e.kind == ExtensionKind::Mcp && e.name == "bowyer"));
        let harness = self.open.as_ref().map(|open| open.summary.key.harness.clone());
        let memory = harness.and_then(|harness| self.render_memory_block(&harness, connected, cx));
        let body: AnyElement = match &self.extensions {
            None => div().p(px(12.)).text_color(theme.text_muted).child(t("loading")).into_any_element(),
            Some(Err(error)) => div().p(px(12.)).text_color(theme.error).child(error.clone()).into_any_element(),
            Some(Ok(list)) if list.is_empty() => {
                div().p(px(12.)).text_color(theme.text_muted).child(t("no_results")).into_any_element()
            }
            Some(Ok(list)) => div()
                .flex()
                .flex_col()
                .children(list.iter().enumerate().map(|(ix, extension)| {
                    let item = extension.clone();
                    div()
                        .id(("ext", ix))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(6.))
                        .hover(|style| style.bg(theme.selection_hover))
                        .child(switch(extension.enabled, extension.toggleable, &theme))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .flex_1()
                                .child(div().truncate().child(extension.name.clone()))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme.text_muted)
                                        .truncate()
                                        .child(format!(
                                            "{} · {}{}",
                                            if extension.kind == ExtensionKind::Mcp { "MCP" } else { "plugin" },
                                            extension.scope,
                                            if extension.detail.is_empty() { String::new() } else { format!(" · {}", extension.detail) }
                                        )),
                                ),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_extension(&item, cx)))
                }))
                .into_any_element(),
        };
        div()
            .id("extensions")
            .absolute()
            .top(px(8.))
            .right(px(12.))
            .w(px(380.))
            .max_h(px(460.))
            .overflow_y_scroll()
            .p(px(6.))
            .rounded(px(12.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .child(div().px(px(10.)).py(px(6.)).font_weight(FontWeight::SEMIBOLD).child(t("mcp")))
            .children(memory)
            .child(body)
            .into_any_element()
    }

    fn render_item(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let Some(open) = &self.open else { return div().into_any_element() };
        let base = open.items.clone();
        let live_items = self.live_items();
        let item = match base.get(ix) {
            Some(item) => item,
            None => match live_items.as_ref().and_then(|items| items.get(ix - base.len())) {
                Some(item) => item,
                None => return div().into_any_element(),
            },
        };
        let user = item.role == Role::User && !item.compact_summary;
        // A prompt of the history can start a branch, a new session before it, and shows
        // the files its turn changed.
        let can_branch = user
            && ix < base.len()
            && item.branch.is_some()
            && open.summary.key.harness == HarnessKind::Claude
            && !self.live_running();
        let turn_files = (user && ix < base.len()).then(|| self.turn_files_of(ix)).flatten();
        let action = |id: &'static str, label: String| {
            div().id((id, ix)).px(px(6.)).rounded(px(5.)).hover(|style| style.bg(theme.hover).text_color(theme.text)).child(label)
        };
        let branch = (can_branch || turn_files.is_some()).then(|| {
            div()
                .id(("branch", ix))
                .flex()
                .justify_end()
                .gap(px(4.))
                .mt(px(4.))
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .opacity(0.)
                .group_hover("user-message", |style| style.opacity(1.))
                .when_some(turn_files, |this, n| {
                    this.child(
                        action("turn-action", t("turn.action").replace("{n}", &n.to_string()))
                            .tooltip(|_, cx| tooltip(t("turn.tip"), None, cx))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_turn(ix, cx))),
                    )
                })
                .when(can_branch, |this| {
                    this.child(
                        action("branch-action", t("branch.action").to_owned())
                            .tooltip(|_, cx| tooltip(t("branch.tip"), None, cx))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.start_branch(ix, window, cx))),
                    )
                })
        });
        // A compaction summary is long and repeats what came before: folded until asked.
        let summary_open = item.compact_summary && self.open_summaries.contains(&ix);
        let mut column = div().flex().flex_col().gap(px(8.)).min_w_0();
        if !item.compact_summary || summary_open {
            for (pi, piece) in item.pieces.iter().enumerate() {
                column = column.child(self.render_piece(ix, pi, piece, user, cx));
            }
        }
        let content = if user {
            div()
                .group("user-message")
                .flex()
                .flex_col()
                .child(
                    div().flex().justify_end().child(
                        div()
                            .max_w(relative(0.82))
                            .px(px(14.))
                            .py(px(10.))
                            .rounded(px(14.))
                            .bg(theme.selection_muted)
                            .child(column),
                    ),
                )
                .children(branch)
                .into_any_element()
        } else if item.role == Role::System {
            column.into_any_element()
        } else {
            div()
                .flex()
                .gap(px(12.))
                .child(
                    div()
                        .mt(px(3.))
                        .size(px(22.))
                        .flex_none()
                        .rounded_full()
                        .bg(open.summary.key.harness_color())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(hex(0xFFFFFF))
                        .child(open.summary.key.harness.label().chars().next().unwrap_or('?').to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(item.compact_summary, |this| {
                            this.px(px(10.)).py(px(6.)).rounded(px(10.)).border_1().border_color(theme.border).child(
                                div()
                                    .id(("compact-summary", ix))
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .text_size(px(12.))
                                    .text_color(theme.text_muted)
                                    .hover(|style| style.text_color(theme.text_secondary))
                                    .when(summary_open, |this| this.mb(px(6.)))
                                    .child(icon_sized(if summary_open { "general/chevronDown" } else { "general/chevronRight" }, 12., &theme))
                                    .child(t("compacted"))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        if !this.open_summaries.remove(&ix) {
                                            this.open_summaries.insert(ix);
                                        }
                                        if let Some(open) = &this.open {
                                            open.list.pause_following_tail();
                                            open.list.remeasure_items(ix..ix + 1);
                                        }
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(column),
                )
                .into_any_element()
        };
        // A row of the list is laid out on its own, where `mx_auto` has no parent to centre
        // in: a full-width row centres the column instead.
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(div().w_full().max_w(px(980.)).min_w_0().px(px(24.)).py(px(10.)).child(content))
            .into_any_element()
    }

    fn render_piece(&self, ix: usize, pi: usize, piece: &Piece, user: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        match piece {
            Piece::Markdown(blocks) => self.markdown_view(blocks, ix * 1000 + pi, cx).into_any_element(),
            // One line, as wide as the column at most: a long one ends in an ellipsis.
            Piece::Notice(text) => div()
                .flex()
                .justify_center()
                .min_w_0()
                .child(
                    div()
                        .max_w_full()
                        .min_w_0()
                        .px(px(10.))
                        .py(px(3.))
                        .rounded_full()
                        .bg(theme.hover)
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(text.clone()),
                )
                .into_any_element(),
            Piece::Image(_, Some((image, width, height))) => {
                let (w, h) = composer::fit(*width, *height, 420., 300.);
                let shown = (image.clone(), *width, *height);
                div()
                    .id(("picture", ix * 1000 + pi))
                    .w(px(w))
                    .h(px(h))
                    .rounded(px(8.))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .child(img(image.clone()).size_full())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.viewer = Some(shown.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Piece::Image(media, None) => div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(format!("🖼 {media}"))
                .into_any_element(),
            Piece::Thinking(text) => {
                let open = self.open_thinking.contains(&(ix, pi));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id(("thinking", ix * 1000 + pi))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .hover(|style| style.text_color(theme.text_secondary))
                            .child(icon_sized(if open { "general/chevronDown" } else { "general/chevronRight" }, 12., &theme))
                            .child(t("thinking"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if !this.open_thinking.remove(&(ix, pi)) {
                                    this.open_thinking.insert((ix, pi));
                                }
                                if let Some(open) = &this.open {
                                    open.list.pause_following_tail();
                                    open.list.remeasure_items(ix..ix + 1);
                                }
                                cx.notify();
                            })),
                    )
                    .when(open, |this| {
                        this.child(
                            div()
                                .pl(px(10.))
                                .border_l_2()
                                .border_color(theme.border)
                                .text_size(px(12.5))
                                .text_color(theme.text_secondary)
                                .italic()
                                .child(text.clone()),
                        )
                    })
                    .into_any_element()
            }
            Piece::Tools(run) => {
                let _ = user;
                let unfolded = self.show_tools || self.open_runs.contains(&(ix, pi));
                let header = div()
                    .id(("run", ix * 1000 + pi))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(26.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .bg(theme.hover.opacity(0.5))
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.hover))
                    .child(icon_sized(if unfolded { "general/chevronDown" } else { "general/chevronRight" }, 12., &theme))
                    .child(div().font_weight(FontWeight::MEDIUM).child(format!("{} ", run.len())))
                    .child(div().truncate().child(display::run_summary(run)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if !this.open_runs.remove(&(ix, pi)) {
                            this.open_runs.insert((ix, pi));
                        }
                        if let Some(open) = &this.open {
                            open.list.pause_following_tail();
                            open.list.remeasure_items(ix..ix + 1);
                        }
                        cx.notify();
                    }));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(header)
                    .when(unfolded, |this| {
                        this.children(run.iter().enumerate().map(|(ci, tool)| self.render_tool(ix, pi, ci, tool, cx)))
                    })
                    .into_any_element()
            }
        }
    }

    fn render_tool(&self, ix: usize, pi: usize, ci: usize, tool: &ToolView, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let open = self.open_calls.contains(&(ix, pi, ci));
        let status_color = match &tool.result {
            Some(result) if result.is_error => theme.error,
            Some(_) => theme.success,
            None => theme.text_disabled,
        };
        let row = div()
            .id(("call", (ix * 1000 + pi) * 1000 + ci))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(24.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(5.))
            .text_size(px(12.))
            .hover(|style| style.bg(theme.hover))
            .child(div().size(px(6.)).flex_none().rounded_full().bg(status_color))
            .child(div().flex_none().font_weight(FontWeight::MEDIUM).child(tool.name.clone()))
            .child(div().truncate().font_family(MONO).text_color(theme.text_muted).child(tool.summary.clone()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !this.open_calls.remove(&(ix, pi, ci)) {
                    this.open_calls.insert((ix, pi, ci));
                }
                if let Some(open) = &this.open {
                    open.list.pause_following_tail();
                    open.list.remeasure_items(ix..ix + 1);
                }
                cx.notify();
            }));
        let detail = open.then(|| {
            let mut body = div().ml(px(22.)).flex().flex_col().gap(px(6.));
            if let Some((old, new)) = &tool.diff {
                body = body.child(diff_view(old, new, &theme));
            } else if !tool.input.is_empty() {
                body = body.child(code_box(&tool.input, &theme));
            }
            if let Some(result) = &tool.result
                && !result.text.trim().is_empty()
            {
                body = body.child(
                    code_box(&result.text, &theme).when(result.is_error, |this| this.text_color(theme.error)),
                );
            }
            body
        });
        div().flex().flex_col().gap(px(4.)).child(row).children(detail).into_any_element()
    }

    // Right pane -----------------------------------------------------------------------

    fn render_right(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let pane = island(&theme).w(px(self.right_width)).flex_none().h_full();
        if let Some(editor) = self.render_theme_editor(window, cx) {
            return pane.child(editor);
        }
        let Some(open) = &self.open else {
            return pane.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(16.))
                    .pr(px(8.))
                    .pt(px(8.))
                    .text_color(theme.text_muted)
                    .child(t("info"))
                    .child(self.hide_pane_button(Side::Right, cx)),
            );
        };
        let summary = &open.summary;
        let env_label = self.env_for(&summary.key.env).map_or_else(|| summary.key.env.0.clone(), |env| {
            if env.id().is_local() { t("local").to_owned() } else { env.label() }
        });
        let branch = open
            .git
            .as_ref()
            .map(|git| format!("{}{}", git.branch, if git.worktree { "  (worktree)" } else { "" }))
            .or_else(|| summary.git_branch.clone())
            .unwrap_or_else(|| "—".into());
        let facts = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(fact(t("environment"), env_label, &theme))
            .child(fact(t("folder"), summary.cwd.clone().unwrap_or_else(|| "—".into()), &theme))
            .child(fact(t("branch"), branch, &theme))
            .child(fact(
                t("model"),
                open.model.clone().or_else(|| summary.model.clone()).unwrap_or_else(|| "—".into()),
                &theme,
            ));

        let breakdown = &open.breakdown;
        // Claude's own count, while the session has not grown since it was asked.
        let exact = open.exact.as_ref().filter(|(offset, _)| *offset == open.shown_offset).map(|(_, exact)| exact);
        // The window of the model; Claude's /context divides by a smaller working limit.
        let window_size = breakdown.window.or_else(|| exact.and_then(|exact| exact.window));
        let total = exact.map_or(breakdown.total, |exact| exact.total);
        let measured = breakdown.measured || exact.is_some();
        // Parts with their label and colour: Claude's categories, or the estimate.
        let parts: Vec<(String, Hsla, u64)> = match exact {
            Some(exact) => exact
                .categories
                .iter()
                .enumerate()
                .map(|(ix, (name, tokens))| (claude_category_label(name), claude_category_color(name, ix), *tokens))
                .collect(),
            None => breakdown.parts.iter().map(|(part, tokens)| (part_label(*part).to_owned(), part_color(*part), *tokens)).collect(),
        };
        let bar_total = window_size.unwrap_or(total).max(total).max(1) as f32;
        let bar = div()
            .flex()
            .h(px(10.))
            .w_full()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(theme.hover)
            .children(parts.iter().map(|(_, color, tokens)| div().h_full().w(relative(*tokens as f32 / bar_total)).bg(*color)));
        let legend = div().flex().flex_col().gap(px(4.)).children(parts.into_iter().map(|(label, color, tokens)| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .child(div().size(px(8.)).rounded(px(2.)).bg(color))
                .child(div().flex_1().text_color(theme.text_secondary).child(label))
                .child(div().font_family(MONO).text_size(px(11.5)).child(tokens_text(tokens)))
        }));
        // Claude Code tells its own count of the context on request.
        let can_ask = summary.key.harness == HarnessKind::Claude && !summary.key.id.is_empty();
        let exact_button = can_ask.then(|| {
            let busy = open.exact_busy;
            div()
                .id("exact-context")
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .text_size(px(12.))
                .when(busy, |this| this.opacity(0.6))
                .when(!busy, |this| this.hover(|style| style.bg(theme.hover)))
                .tooltip(|_, cx| tooltip(t("context.exact_tip"), None, cx))
                .child(if busy { t("context.exact_busy") } else { t("context.exact") })
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.exact_context(cx)))
        });
        let exact_note = exact.map(|_| div().text_size(px(11.)).text_color(theme.text_muted).child(t("context.by_claude")));
        let context = section_body()
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(px(6.))
                            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(tokens_text(total)))
                            .child(div().pb(px(3.)).text_color(theme.text_muted).text_size(px(12.)).child(match window_size {
                                Some(size) => format!("{} {}{}", t("of"), tokens_text(size), if measured { String::new() } else { format!(" · {}", t("context.estimated")) }),
                                None => if measured { String::new() } else { t("context.estimated").to_owned() },
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .children(exact_button)
                            .child(div()
                            .id("compact")
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(12.))
                            .hover(|style| style.bg(theme.hover))
                            .tooltip(|_, cx| tooltip(t("compact.hint"), None, cx))
                            .child(t("compact"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.compact(cx)))),
                    ),
            )
            .child(bar)
            .child(legend)
            .children(exact_note);
        // How full the context is, in the title, so it shows with the section folded too.
        let fill = match window_size {
            Some(size) if size > 0 => {
                let percent = total as f64 * 100.0 / size as f64;
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().text_size(px(11.)).font_family(MONO).text_color(theme.text_muted).child(format!("{:.0}%", percent)))
                    .child(
                        div()
                            .w(px(36.))
                            .h(px(6.))
                            .rounded(px(3.))
                            .overflow_hidden()
                            .bg(theme.hover)
                            .child(div().h_full().w(relative((percent / 100.0).clamp(0.0, 1.0) as f32)).bg(plan_view::fill_color(percent, &theme))),
                    )
                    .into_any_element()
            }
            _ => div().text_size(px(11.)).font_family(MONO).text_color(theme.text_muted).child(tokens_text(total)).into_any_element(),
        };
        let context = self.fold("context", t("context"), Some(fill), context, cx);

        let cost = self.render_cost(cx);
        let plan = self.render_plan(cx);
        let usage = open.usage;
        let spend = section_body().child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .child(stat("in", usage.input, &theme))
                .child(stat("out", usage.output, &theme))
                .child(stat("cache r", usage.cache_read, &theme))
                .child(stat("cache w", usage.cache_write, &theme))
                ,
        );
        let spend = self.fold("tokens", t("tokens"), None, spend, cx);

        let cwd = summary.cwd.clone().unwrap_or_default();
        let outputs = if open.changes.is_empty() {
            div().text_size(px(12.)).text_color(theme.text_muted).child(t("no_outputs")).into_any_element()
        } else {
            change_tree::render(&open.changes, &cwd, &self.opened_change_folders, &theme, cx).into_any_element()
        };
        let outputs = self.fold("outputs", t("outputs"), None, outputs, cx);

        // Agents and commands are lists of their own: an agent is a piece of the work, a
        // command one step, and hundreds of commands would bury the few agents.
        let (agents, commands): (Vec<&TaskRun>, Vec<&TaskRun>) = open.tasks.iter().partition(|task| task.kind == model::ToolKind::Agent);
        let shown_of = |state: &Option<(SessionKey, usize)>| match state {
            Some((key, shown)) if *key == summary.key => *shown,
            _ => TASKS_SHOWN,
        };
        let (agents_shown, commands_shown) = (shown_of(&self.agents_shown), shown_of(&self.tasks_shown));
        let count_of = |n: usize| (n > 0).then(|| div().text_size(px(11.)).text_color(theme.text_muted).child(n.to_string()).into_any_element());
        let (agents_count, commands_count) = (count_of(agents.len()), count_of(commands.len()));
        let more = |id: &'static str, hidden: usize, agents: bool, shown: usize, cx: &mut Context<Self>| {
            let key = summary.key.clone();
            (hidden > 0).then(|| {
                div()
                    .id(id)
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(theme.accent)
                    .hover(|style| style.underline())
                    .child(t("tree.show_more").replace("{n}", &hidden.min(TASKS_MORE).to_string()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let state = Some((key.clone(), shown + TASKS_MORE));
                        if agents {
                            this.agents_shown = state;
                        } else {
                            this.tasks_shown = state;
                        }
                        cx.notify();
                    }))
            })
        };
        let agents_more = more("agents-more", agents.len().saturating_sub(agents_shown), true, agents_shown, cx);
        let commands_more = more("tasks-more", commands.len().saturating_sub(commands_shown), false, commands_shown, cx);
        let agents = (!agents.is_empty()).then(|| {
            let list = div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(agents.iter().rev().take(agents_shown).map(|task| task_row(task, false, &theme)))
                .children(agents_more);
            self.fold("agents", t("agents"), agents_count, list, cx)
        });
        let live_tasks = if self.folded.contains("tasks") { None } else { self.render_live_tasks(cx) };
        let tasks = section_body().children(live_tasks).child(if commands.is_empty() {
            div().text_size(px(12.)).text_color(theme.text_muted).child(t("no_commands")).into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(commands.iter().rev().take(commands_shown).map(|task| task_row(task, true, &theme)))
                .children(commands_more)
                .into_any_element()
        });
        let tasks = self.fold("tasks", t("commands"), commands_count, tasks, cx);

        let notes = self.render_notes(cx);
        let tabs = self.render_right_tabs(cx);
        match self.right_tab {
            files_view::RightTab::Files => {
                let files = self.render_files(window, cx);
                return pane.flex().flex_col().child(tabs).child(files);
            }
            files_view::RightTab::Changes => {
                let changes = self.render_changes(window, cx);
                return pane.flex().flex_col().child(tabs).child(changes);
            }
            files_view::RightTab::Session => {}
        }
        let info = div()
            .id("info")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.info_scroll)
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(facts)
            .child(context)
            .children(plan)
            .child(cost)
            .child(spend)
            .child(outputs)
            .children(agents)
            .child(tasks)
            .children(notes);
        pane.flex()
            .flex_col()
            .child(tabs)
            .child(div().flex_1().min_h_0().w_full().child(scrollable("info-scroll", &self.info_scroll, info, &theme, window, cx).size_full()))
    }
}

trait HarnessColor {
    fn harness_color(&self) -> Hsla;
}

impl HarnessColor for SessionKey {
    fn harness_color(&self) -> Hsla {
        harness_color(&self.harness)
    }
}

// Pieces of drawing ----------------------------------------------------------------------

/// The mark of the application: a bow loosing three arrows, drawn by Gemini.
fn logo(size: f32) -> impl IntoElement {
    img("brand/bowyer.png").size(px(size)).rounded(px(size * 0.22))
}

fn badge(label: &str, color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(2.))
        .rounded(px(5.))
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.to_owned())
}

/// The space at the end of a tree row: the counts and times stay clear of the scrollbar.
const TREE_ROW_END: f32 = 18.;

fn chip(id: &'static str, label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(24.))
        .px(px(8.))
        .rounded(px(6.))
        .text_size(px(12.))
        .text_color(theme.text_secondary)
        .hover(move |style| style.bg(hover))
        .child(label.into())
}

fn switch(on: bool, enabled: bool, theme: &Theme) -> impl IntoElement {
    let track = if on { theme.accent } else { theme.text_disabled };
    div()
        .flex_none()
        .w(px(28.))
        .h(px(16.))
        .rounded_full()
        .bg(track)
        .when(!enabled, |this| this.opacity(0.45))
        .flex()
        .items_center()
        .when(on, |this| this.justify_end())
        .px(px(2.))
        .child(div().size(px(12.)).rounded_full().bg(hex(0xFFFFFF)))
}

/// The commands and agents the right pane lists at first, newest first, and how many more
/// each "show more" adds.
const TASKS_SHOWN: usize = 10;
const TASKS_MORE: usize = 50;

/// The lists of the sessions pane's footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterMenu {
    Sort = 0,
    Group = 1,
}

/// The sections of the right pane that fold.
const FOLDING: [&str; 8] = ["context", "plan", "cost", "tokens", "outputs", "agents", "tasks", "notes"];

impl Shell {
    /// A section of the right pane whose title folds it: the title with a chevron and
    /// `extra` beside it, and `body` under it while it is open.
    fn fold(&self, id: &'static str, title: &str, extra: Option<AnyElement>, body: impl IntoElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let folded = self.folded.contains(id);
        let head = div()
            .id(SharedString::from(format!("fold-{id}")))
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_muted)
            .hover(|style| style.text_color(theme.text_secondary))
            .child(icon_sized(if folded { "general/chevronRight" } else { "general/chevronDown" }, 12., &theme))
            .child(title.to_uppercase())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !this.folded.remove(id) {
                    this.folded.insert(id);
                }
                let ids: Vec<&str> = FOLDING.iter().copied().filter(|known| this.folded.contains(known)).collect();
                let _ = this.lib().set_setting("layout.folded", &ids.join(","));
                let _ = this.lib().set_setting("layout.tasks_collapsed", "0");
                cx.notify();
            }));
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(div().flex().items_center().justify_between().child(head).children(extra))
            .when(!folded, |this| this.child(body))
            .into_any_element()
    }
}

/// A command or an agent of the session: whether it runs, ended or failed, and what it is.
fn task_row(task: &TaskRun, command: bool, theme: &Theme) -> Div {
    let color = match task.status {
        TaskStatus::Running => theme.accent,
        TaskStatus::Done => theme.success,
        TaskStatus::Failed => theme.error,
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(22.))
        .text_size(px(12.))
        .child(div().size(px(6.)).flex_none().rounded_full().bg(color))
        .when(command, |this| this.child(div().flex_none().text_color(theme.text_muted).child("$")))
        .child(div().truncate().when(command, |this| this.font_family(MONO).text_size(px(11.5))).child(task.title.clone()))
}

/// The body of a section: its parts one under another.
fn section_body() -> Div {
    div().flex().flex_col().gap(px(10.))
}

fn fact(label: &str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .gap(px(10.))
        .text_size(px(12.5))
        .child(div().w(px(86.)).flex_none().text_color(theme.text_muted).child(label.to_owned()))
        .child(div().flex_1().min_w_0().truncate().child(value))
}

fn stat(label: &str, value: u64, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .gap(px(6.))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(6.))
        .bg(theme.hover)
        .text_size(px(12.))
        .child(div().text_color(theme.text_muted).child(label.to_owned()))
        .child(div().font_family(MONO).text_size(px(11.5)).child(tokens_text(value)))
}

fn code_box(text: &str, theme: &Theme) -> Div {
    div()
        .p(px(10.))
        .rounded(px(8.))
        .bg(theme.main_bg)
        .border_1()
        .border_color(theme.border)
        .font_family(MONO)
        .text_size(px(12.))
        .line_height(px(18.))
        .child(text.to_owned())
}

/// Old lines in red, new lines in green.
fn diff_view(old: &str, new: &str, theme: &Theme) -> impl IntoElement {
    let line = |text: &str, sign: &str, color: Hsla| {
        div()
            .flex()
            .px(px(8.))
            .bg(color.opacity(0.12))
            .child(div().w(px(14.)).flex_none().text_color(color).child(sign.to_owned()))
            .child(div().flex_1().min_w_0().child(text.to_owned()))
    };
    div()
        .rounded(px(8.))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .bg(theme.main_bg)
        .font_family(MONO)
        .text_size(px(12.))
        .line_height(px(18.))
        .py(px(4.))
        .children(old.lines().take(400).map(|l| line(l, "−", theme.error)))
        .children(new.lines().take(400).map(|l| line(l, "+", theme.success)))
}

/// Text coloured by the highlighter, with the code colours of the theme.
pub(crate) fn code_text(text: SharedString, spans: &[(std::ops::Range<usize>, highlight::Kind)], theme: &Theme) -> StyledText {
    let highlights = code_highlights(&text, spans, theme);
    StyledText::new(text).with_highlights(highlights)
}

fn code_highlights(text: &str, spans: &[(std::ops::Range<usize>, highlight::Kind)], theme: &Theme) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    spans
        .iter()
        .filter(|(range, _)| range.end <= text.len() && text.is_char_boundary(range.start) && text.is_char_boundary(range.end))
        .map(|(range, kind)| {
            let color = match kind {
                highlight::Kind::Keyword => theme.keyword,
                highlight::Kind::String => theme.string,
                highlight::Kind::Number => theme.number,
                highlight::Kind::Comment => theme.comment,
                highlight::Kind::Function => theme.function,
                highlight::Kind::Type => theme.semantic_type,
                highlight::Kind::Annotation => theme.annotation,
            };
            (range.clone(), HighlightStyle { color: Some(color), ..Default::default() })
        })
        .collect()
}

/// A whole block of code coloured line by line, as one text.
fn code_block_text(language: &str, text: &str, theme: &Theme, selected: Option<std::ops::Range<usize>>) -> StyledText {
    let mut spans = Vec::new();
    if let Some(lang) = highlight::lang_for(language) {
        let mut highlighter = highlight::Highlighter::new(lang);
        let mut offset = 0;
        for line in text.split('\n') {
            for (range, kind) in highlighter.line(line) {
                spans.push((range.start + offset..range.end + offset, kind));
            }
            offset += line.len() + 1;
        }
    }
    let highlights = selection::with_selection(text, code_highlights(text, &spans, theme), selected, theme.accent.opacity(0.32));
    StyledText::new(text.to_owned()).with_highlights(highlights)
}

/// The text of a block of an answer as it is drawn and selected; none for a rule.
fn block_text(block: &MdBlock) -> Option<&str> {
    match block {
        MdBlock::Paragraph(rich) | MdBlock::Heading(_, rich) | MdBlock::Quote(rich) | MdBlock::Item { text: rich, .. } => Some(&rich.text),
        MdBlock::Code { text, .. } => Some(text),
        MdBlock::Rule => None,
    }
}

fn rich_text(rich: &Rich, theme: &Theme, selected: Option<std::ops::Range<usize>>) -> StyledText {
    let style_of = |kind: &Inline| -> HighlightStyle {
        match kind {
            Inline::Bold => HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() },
            Inline::Italic => HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() },
            Inline::Code => HighlightStyle {
                color: Some(theme.string),
                background_color: Some(theme.hover),
                ..Default::default()
            },
            Inline::Link => HighlightStyle {
                color: Some(theme.link),
                underline: Some(UnderlineStyle { thickness: px(1.), color: Some(theme.link), wavy: false }),
                ..Default::default()
            },
            Inline::Strike => HighlightStyle {
                strikethrough: Some(StrikethroughStyle { thickness: px(1.), color: Some(theme.text_muted) }),
                ..Default::default()
            },
        }
    };
    // Nested styles (bold with code inside) come as pieces that do not overlap.
    let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = rich
        .segments()
        .into_iter()
        .map(|(range, kinds)| {
            let mut style = HighlightStyle::default();
            for kind in &kinds {
                style = style.highlight(style_of(kind));
            }
            (range, style)
        })
        .collect();
    let highlights = selection::with_selection(&rich.text, highlights, selected, theme.accent.opacity(0.32));
    StyledText::new(rich.text.clone()).with_highlights(highlights)
}

impl Shell {
    /// Copies a code block, and marks its button for a moment.
    fn copy_block(&mut self, id: usize, text: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        self.copied = Some((id, std::time::Instant::now()));
        self.repaint_after(std::time::Duration::from_millis(1600), cx);
        cx.notify();
    }

    /// Runs a block of code from the conversation in the terminal of the window, in the
    /// session's folder on its host; an external terminal when that cannot start.
    fn run_block(&mut self, shell: launch::Shell, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(env) = self.env_for(&open.summary.key.env) else { return };
        let cwd = open.summary.cwd.clone();
        let line = launch::block_line(&*env, cwd.as_deref(), shell, text);
        let title = model::shorten(text.lines().next().unwrap_or_default().trim(), 80);
        if self.show_in_terminal(line, title, window, cx) {
            return;
        }
        let result = launch::run_in_terminal(&*env, cwd.as_deref(), shell, text);
        if result.is_ok() {
            self.status = Some(t("code.started").to_owned());
        }
        self.report(result, cx);
        cx.notify();
    }

    /// The open session resumed in its harness's own interface, in the terminal of the window.
    pub(super) fn resume_in_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(env) = self.env_for(&open.summary.key.env) else { return };
        let summary = open.summary.clone();
        if self.turn_drop(&summary.key).is_some() {
            self.status = Some(t("drop.terminal").to_owned());
            cx.notify();
            return;
        }
        let title = format!("{} · {}", summary.key.harness.label(), summary.display_title());
        if self.show_in_terminal(launch::resume_line(&*env, &summary), title, window, cx) {
            if let Some(view) = self.terminals.last() {
                view.update(cx, |view, _| view.harness = true);
            }
        } else {
            let result = launch::resume_in_terminal(&*env, &summary);
            self.report(result, cx);
        }
    }

    fn markdown_view(&self, blocks: &[MdBlock], base: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let copied = self.copied.filter(|(_, at)| at.elapsed().as_millis() < 1500).map(|(id, _)| id);
        let buttons: Vec<Option<AnyElement>> = blocks
            .iter()
            .enumerate()
            .map(|(bi, block)| {
                let MdBlock::Code { language, text } = block else { return None };
                let id = base * 100 + bi;
                let shell = launch::Shell::of_tag(language);
                let button = |name: &str, label: &'static str| {
                    div()
                        .id(SharedString::from(format!("{name}-{id}")))
                        .flex()
                        .items_center()
                        .h(px(20.))
                        .px(px(7.))
                        .rounded(px(5.))
                        .text_size(px(11.))
                        .text_color(theme.text_secondary)
                        .hover(|style| style.bg(theme.hover).text_color(theme.text))
                        .child(label)
                };
                let copy_text = text.clone();
                let run_text = text.clone();
                Some(
                    div()
                        .flex()
                        .gap(px(2.))
                        .when_some(shell, |this, shell| {
                            this.child(button("run", t("code.run")).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.run_block(shell, &run_text, window, cx)
                            })))
                        })
                        .child(
                            button("copy", if copied == Some(id) { t("code.copied") } else { t("code.copy") }).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| this.copy_block(id, &copy_text, cx),
                            )),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        let texts: Vec<Option<AnyElement>> = blocks
            .iter()
            .enumerate()
            .map(|(bi, block)| {
                let len = block_text(block)?.len();
                let range = self.selection.as_ref().and_then(|selection| selection.range_in((base, bi), len));
                let (styled, links) = match block {
                    MdBlock::Paragraph(rich) | MdBlock::Heading(_, rich) | MdBlock::Quote(rich) | MdBlock::Item { text: rich, .. } => {
                        let links = rich.links.iter().filter(|(_, url)| crate::markdown::opens_in_browser(url)).cloned().collect();
                        (rich_text(rich, &theme, range), links)
                    }
                    MdBlock::Code { language, text } => (code_block_text(language, text, &theme, range), Vec::new()),
                    MdBlock::Rule => return None,
                };
                Some(self.selectable((base, bi), styled, links, cx))
            })
            .collect();
        markdown_blocks(blocks, &theme, buttons, texts)
    }
}

fn markdown_blocks(blocks: &[MdBlock], theme: &Theme, buttons: Vec<Option<AnyElement>>, texts: Vec<Option<AnyElement>>) -> impl IntoElement + use<> {
    let theme = *theme;
    let theme = &theme;
    let mut buttons = buttons.into_iter();
    let mut texts = texts.into_iter();
    let elements: Vec<AnyElement> = blocks.iter().map(|block| {
        let button = buttons.next().flatten();
        let custom = texts.next().flatten();
        let rich_of = |custom: Option<AnyElement>, rich: &Rich| custom.unwrap_or_else(|| rich_text(rich, theme, None).into_any_element());
        match block {
            MdBlock::Paragraph(rich) => div().child(rich_of(custom, rich)).into_any_element(),
            MdBlock::Heading(level, rich) => div()
                .mt(px(4.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(match level {
                    1 => 19.,
                    2 => 16.5,
                    3 => 14.5,
                    _ => 13.5,
                }))
                .child(rich_of(custom, rich))
                .into_any_element(),
            MdBlock::Item { depth, marker, text } => div()
                .flex()
                .gap(px(8.))
                .pl(px(4. + *depth as f32 * 18.))
                .child(div().flex_none().min_w(px(14.)).text_color(theme.text_muted).child(marker.clone()))
                .child(div().flex_1().min_w_0().child(rich_of(custom, text)))
                .into_any_element(),
            MdBlock::Quote(rich) => div()
                .pl(px(12.))
                .border_l_2()
                .border_color(theme.accent.opacity(0.6))
                .text_color(theme.text_secondary)
                .child(rich_of(custom, rich))
                .into_any_element(),
            MdBlock::Code { language, text } => div()
                .flex()
                .flex_col()
                .rounded(px(10.))
                .overflow_hidden()
                .border_1()
                .border_color(theme.border)
                .bg(theme.main_bg)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .pl(px(12.))
                        .pr(px(6.))
                        .py(px(3.))
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(if language.is_empty() { "code".to_owned() } else { language.clone() })
                        .children(button),
                )
                .child(
                    div()
                        .p(px(12.))
                        .font_family(MONO)
                        .text_size(px(12.5))
                        .line_height(px(19.))
                        .text_color(theme.code)
                        .child(custom.unwrap_or_else(|| code_block_text(language, text, theme, None).into_any_element())),
                )
                .into_any_element(),
            MdBlock::Rule => div().h(px(1.)).bg(theme.border).my(px(4.)).into_any_element(),
        }
    }).collect();
    div().flex().flex_col().gap(px(8.)).text_size(px(13.5)).line_height(px(21.)).children(elements)
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        self.drawn_text.borrow_mut().clear();
        let left = (!self.left_hidden).then(|| self.render_left(window, cx));
        let center = self.render_center(window, cx);
        let right = (!self.right_hidden).then(|| self.render_right(window, cx));
        let left_stripe = self.left_hidden.then(|| self.render_stripe(Side::Left, cx));
        let right_stripe = self.right_hidden.then(|| self.render_stripe(Side::Right, cx));
        if let Some((key, call)) = self.attention.take()
            && !window.is_window_active()
        {
            window.request_attention();
            self.notify_attention(key, call);
        }
        let viewer = self.render_viewer(window, cx);
        let context_menu = self.render_context_menu(cx);
        let selection_bubble = self.render_selection_bubble(cx);
        let left_handle = self.render_handle(Side::Left, cx);
        let right_handle = self.render_handle(Side::Right, cx);
        let titlebar = self.render_titlebar(window, cx);
        let resize_border = self.render_resize_border(window);
        let settings = self.settings.map(|page| self.render_settings(page, window, cx));
        let new_session = self.render_new_session(window, cx);
        let quit_ask = self.render_quit_ask(cx);
        div()
            .id("shell")
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::root_capture_key))
            .on_key_down(cx.listener(Self::root_key))
            .on_mouse_move(cx.listener(Self::resize_move))
            .on_mouse_move(cx.listener(Self::selection_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_resize(cx)))
            .when(self.resizing.is_some(), |this| this.cursor_col_resize())
            .when(self.resizing_terminal, |this| this.cursor_row_resize())
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.main_bg)
            .text_color(theme.text)
            .font_family("Inter")
            .text_size(px(13.))
            .child(titlebar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .px(px(EDGE))
                    .pb(px(EDGE))
                    .children(left)
                    .children(left_stripe)
                    .when(!self.left_hidden, |this| this.child(left_handle))
                    .when(self.left_hidden, |this| this.child(div().flex_none().w(px(EDGE))))
                    .child(center)
                    .when(!self.right_hidden, |this| this.child(right_handle))
                    .when(self.right_hidden, |this| this.child(div().flex_none().w(px(EDGE))))
                    .children(right)
                    .children(right_stripe),
            )
            .children(settings)
            .children(resize_border)
            .children(viewer)
            .children(context_menu)
            .children(selection_bubble)
            .children(new_session)
            .children(quit_ask)
    }
}

/// Why a session calls its user back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Waiting,
    YourTurn,
    /// The turn of the window asks a question or a permission.
    Asks,
    Done,
    Failed,
}

impl Call {
    fn text(self) -> &'static str {
        match self {
            Call::Waiting => t("notify.waiting"),
            Call::YourTurn => t("notify.your_turn"),
            Call::Asks => t("notify.asks"),
            Call::Done => t("notify.done"),
            Call::Failed => t("notify.failed"),
        }
    }
}

/// The Claude sessions of an SSH host a poll looks at.
struct WatchedHost {
    env: SharedEnv,
    /// The Claude projects folder on the host, from the sessions known there.
    root: Option<String>,
    /// Pinned session files, looked at whether written lately or not.
    pinned: Vec<String>,
    /// Every Claude session file known on the host, with its size; no size for the open
    /// session, which has a poll of its own.
    known: HashMap<String, (SessionKey, Option<u64>)>,
}

/// What the poll of a remote session saw.
struct WatchedRemote {
    size: u64,
    modified_ms: i64,
    turn: Option<TurnState>,
}

/// Polls between two looks at remote sessions.
const PINNED_POLL_TICKS: u64 = 5;
/// A remote session file written this many minutes ago or later is looked at.
const RECENT_MINUTES: u32 = 2;

/// The Claude projects folder of a session file: `…/.claude/projects`, where the
/// sessions are `<folder>/<id>.jsonl`.
fn claude_projects_root(path: &str) -> Option<String> {
    let end = path.find("/.claude/projects/")? + "/.claude/projects".len();
    Some(path[..end].to_owned())
}
/// The end of a pinned remote session read to see where its turn stands.
const PINNED_TAIL_BYTES: u64 = 64 * 1024;

/// Size and time of the last write of files on a Unix host, in one command, and of the
/// session files in the Claude projects folder `root` written in the last
/// [`RECENT_MINUTES`]; a missing file is left out.
fn remote_stats(env: &dyn Environment, root: Option<&str>, paths: &[&str]) -> std::io::Result<HashMap<String, (u64, i64)>> {
    let script = format!(
        r#"r=$1; shift; {{ for f; do printf '%s\n' "$f"; done; [ -n "$r" ] && find "$r" -mindepth 2 -maxdepth 2 -name '*.jsonl' -mmin -{RECENT_MINUTES} 2>/dev/null; }} | while IFS= read -r f; do stat -c '%s %Y %n' -- "$f" 2>/dev/null; done; true"#
    );
    let mut args = vec!["-c", script.as_str(), "sh", root.unwrap_or("")];
    args.extend_from_slice(paths);
    let out = env.run("sh", &args, None)?;
    Ok(parse_stats(&out))
}

/// When Codex, dsh and OpenCode last wrote their history on a host, for the harnesses
/// that wrote in the last [`RECENT_MINUTES`], in one command.
fn other_stamps(env: &dyn Environment) -> std::io::Result<Vec<(HarnessKind, i64)>> {
    let script = format!(
        r#"m() {{ k=$1; shift; t=$(find "$@" -mmin -{RECENT_MINUTES} -printf '%T@\n' 2>/dev/null | sort -n | tail -n 1); [ -n "$t" ] && printf '%s %s\n' "$k" "$t"; }}
m codex "$HOME/.codex/sessions" "$HOME/.codex/archived_sessions" -name '*.jsonl'
m deepseek "$HOME/.dsh/sessions" -name 'session.v*'
m opencode "$HOME/.local/share/opencode" -maxdepth 1 -name 'opencode.db*'
true"#
    );
    Ok(parse_stamps(&env.run("sh", &["-c", &script], None)?))
}

/// `harness seconds` lines of [`other_stamps`].
fn parse_stamps(out: &str) -> Vec<(HarnessKind, i64)> {
    out.lines()
        .filter_map(|line| {
            let (harness, seconds) = line.split_once(' ')?;
            let harness = match harness {
                "codex" => HarnessKind::Codex,
                "deepseek" => HarnessKind::DeepSeek,
                "opencode" => HarnessKind::OpenCode,
                _ => return None,
            };
            Some((harness, (seconds.trim().parse::<f64>().ok()? * 1000.0) as i64))
        })
        .collect()
}

/// `size seconds path` lines of `stat -c '%s %Y %n'`.
fn parse_stats(out: &str) -> HashMap<String, (u64, i64)> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let size = parts.next()?.parse().ok()?;
            let seconds: i64 = parts.next()?.parse().ok()?;
            Some((parts.next()?.to_owned(), (size, seconds * 1000)))
        })
        .collect()
}

#[cfg(test)]
mod tail_tests {
    #[test]
    fn only_the_end_of_a_growing_conversation_is_replaced() {
        assert_eq!(super::changed_tail(10, 12), (9..10, 3), "the last row and two new ones");
        assert_eq!(super::changed_tail(10, 10), (9..10, 1));
        assert_eq!(super::changed_tail(10, 8), (7..10, 1), "rows folded into the one before");
        assert_eq!(super::changed_tail(0, 5), (0..0, 5));
        assert_eq!(super::changed_tail(4, 0), (0..4, 0));
    }
}

#[cfg(test)]
mod pinned_tests {
    /// The order a model ranked a query in outlives the search done again for it.
    #[test]
    fn a_ranked_order_is_kept_when_the_query_is_searched_again() {
        let key = |id: &str| model::SessionKey { env: model::EnvId::local(), harness: model::HarnessKind::Claude, id: id.into() };
        let hit = |id: &str| library::SearchHit { key: key(id), title: id.into(), snippet: String::new() };
        let mut hits = vec![hit("a"), hit("b"), hit("c"), hit("new"), hit("d")];
        super::keep_ranking(&mut hits, &[key("c"), key("a"), key("gone")]);
        let ids: Vec<&str> = hits.iter().map(|hit| hit.key.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b", "new", "d"]);
    }

    #[test]
    fn failures_are_tried_again_ever_later_up_to_five_minutes() {
        let mut retry = super::Retry::after(None);
        assert_eq!(retry.delay.as_secs(), 10);
        assert!(!retry.due(), "not at once");
        let mut delays = Vec::new();
        for _ in 0..7 {
            retry = super::Retry::after(Some(&retry));
            delays.push(retry.delay.as_secs());
        }
        assert_eq!(delays, [20, 40, 80, 160, 300, 300, 300]);
    }

    #[test]
    fn stat_lines_give_size_and_time() {
        let stats = super::parse_stats("120 1795000000 /home/u/.claude/projects/a b.jsonl\nbad line\n");
        assert_eq!(stats.get("/home/u/.claude/projects/a b.jsonl"), Some(&(120, 1_795_000_000_000)));
        assert_eq!(stats.len(), 1);
    }

    #[test]
    fn stamps_name_the_harness_that_wrote() {
        let stamps = super::parse_stamps("codex 1795263575.5\nopencode 1795263580\nnoise\nother 1\n");
        assert_eq!(stamps, [(model::HarnessKind::Codex, 1_795_263_575_500), (model::HarnessKind::OpenCode, 1_795_263_580_000)]);
    }

    #[test]
    fn the_projects_folder_comes_from_a_session_file() {
        assert_eq!(super::claude_projects_root("/home/u/.claude/projects/-w/s.jsonl").as_deref(), Some("/home/u/.claude/projects"));
        assert_eq!(super::claude_projects_root("/srv/other/s.jsonl"), None);
    }
}
