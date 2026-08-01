use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::clipboard::{Clipboard, CopyOutcome, osc52_config_for_route};
use crate::command::{self, ExCommand};
use crate::config::Config;
use crate::git::{
    BranchHead, FileState, GitMutation, GitMutationResult, GitRepository,
    MAX_GIT_COMMIT_MESSAGE_BYTES, StatusEntryKind, validate_commit_message,
};
use crate::keymap::{self, Action, Key, Keymap, Resolution};
use crate::lsp::{DocumentSnapshot, DocumentVersion, LspPosition};
use crate::lsp_client::{
    DEFAULT_MAX_DOCUMENT_BYTES, DocumentIncarnation, LspClient, LspClientError, LspEvent,
    LspOperation, LspServerConfig, RequestId, TextDocumentSyncCapability, file_uri_identity,
};
use crate::lsp_session::{
    DiagnosticCache, MAX_SYNCHRONIZED_DOCUMENTS, SynchronizedDocument, SynchronizedDocumentRegistry,
};
use crate::lsp_ui::{
    CompletionItem, Diagnostic, DiagnosticSeverity, Location, apply_text_edits,
    cursor_position_in_snapshot, file_uri_to_path, parse_completion, parse_diagnostics,
    parse_document_symbols, parse_locations, parse_text_edits, parse_workspace_symbols,
    render_hover,
};
use crate::project::{ProjectIndex, ProjectTreeEntry, fuzzy_path_score};
use crate::recovery::{RecoveryRecord, RecoveryStore};
use crate::render::{CandidateOverlayLayout, Layout, project_sidebar_width};
use crate::search::{SearchMatch, SearchQuery, SearchWorker};
use crate::services::{GitSnapshot, RecoverySnapshot, ServiceCoordinator, ServiceEvent};
use crate::session::{
    BookmarkState, LayoutFlags, OpenFileState, Session, SessionStore, ViewportState,
};
use crate::syntax::line_comment_marker_for_path;
use crate::task_output::TaskOutputDecoder;
use crate::task_problem::{
    TaskProblem, TaskProblemColumnKind, TaskProblemSeverity, parse_task_problems,
};
use crate::tasks::{
    OutputStream, TaskConfig, TaskEventKind, TaskHandle, TaskRunner, WorkspaceTrust,
};
use crate::text::{
    char_for_scalar_column, char_for_visual_column, next_grapheme_end, previous_grapheme_start,
    visual_width,
};
use crate::visual::{VisualAnchor, VisualMetrics};
use crate::{Document, EditKind, Editor, LineCommentToggle, Workspace};

const MAX_JUMP_HISTORY: usize = 100;
const MAX_PENDING_LSP_REQUESTS: usize = 128;
const MAX_LSP_EVENTS_PER_POLL: usize = 128;
const MAX_LSP_EVENT_BYTES_PER_POLL: usize = 32 * 1024 * 1024;
const MAX_LSP_DISCOVERY_INSPECTIONS_PER_POLL: usize = 256;
const MAX_LSP_TEXT_DOCUMENTS_PER_SYNC_POLL: usize = 8;
const MAX_LSP_TEXT_BYTES_PER_SYNC_POLL: usize = DEFAULT_MAX_DOCUMENT_BYTES;
const MAX_LSP_QUARANTINED_DOCUMENTS: usize = MAX_SYNCHRONIZED_DOCUMENTS;
const MAX_AMBIGUOUS_DIAGNOSTIC_URIS: usize = 128;
const MAX_RECENT_FILES_VIEW: usize = 128;
const MAX_WORKSPACE_TREE_VISIBLE_NODES: usize = 4_096;
const MAX_WORKSPACE_TREE_EXPANDED_DIRECTORIES: usize = 4_096;
const MAX_WORKSPACE_TREE_QUERY_BYTES: usize = 512;
const MAX_WORKSPACE_TREE_ACTIVE_PATH_BYTES: usize = 4 * 1024;
const MAX_WORKSPACE_TREE_ACTIVE_PATH_COMPONENTS: usize = 65;
const MAX_PROBLEM_CANDIDATES: usize = 4_096;
const MAX_WORKSPACE_SYMBOL_QUERY_BYTES: usize = 512;
const MAX_WORKSPACE_OUTLINE_FILES: usize = 2_000;
const MAX_WORKSPACE_OUTLINE_FILE_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_OUTLINE_SYMBOLS: usize = 4_096;
const MAX_LOCAL_REFERENCE_FILES: usize = 2_000;
const MAX_LOCAL_REFERENCE_FILE_BYTES: usize = 1024 * 1024;
const MAX_LOCAL_REFERENCES: usize = 1_000;
const MAX_SOURCE_ANNOTATION_FILES: usize = 2_000;
const MAX_SOURCE_ANNOTATION_FILE_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_ANNOTATIONS: usize = 1_000;
const SOURCE_ANNOTATION_TAGS: &[&str] = &["TODO", "FIXME", "HACK", "NOTE"];
const MAX_BOOKMARKS: usize = 256;
const MAX_CLOSED_BUFFER_HISTORY: usize = 32;
/// Cap background-service paints (~15 fps) so mosh/Blink stay responsive.
const BACKGROUND_REDRAW_MIN_INTERVAL: Duration = Duration::from_millis(66);
/// One steady, non-oscillating cue after the first edit following Action exit.
const EDIT_TRANSITION_CUE_DURATION: Duration = Duration::from_millis(360);

#[derive(Clone, Debug)]
struct Status {
    message: String,
    error: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClosedBufferState {
    path: PathBuf,
    cursor: usize,
    anchor: Option<usize>,
    viewport: crate::editor::Viewport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JumpLocation {
    editor_id: u64,
    path: Option<PathBuf>,
    cursor: usize,
}

#[derive(Clone, Debug)]
struct WorkspaceOutlineScan {
    targets: Vec<(String, JumpLocation)>,
    skipped: usize,
    truncated_files: bool,
    truncated_symbols: bool,
}

#[derive(Clone, Debug)]
struct LocalReferenceScan {
    targets: Vec<(String, JumpLocation)>,
    skipped: usize,
    truncated_files: bool,
    truncated_matches: bool,
}

#[derive(Clone, Debug)]
struct SourceAnnotationScan {
    targets: Vec<(String, JumpLocation)>,
    skipped: usize,
    truncated_files: bool,
    truncated_matches: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecoveryCheckpointStamp {
    path: Option<PathBuf>,
    revision: u64,
    saved_revision: Option<u64>,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Clone, Debug)]
struct LspEditorSyncTarget {
    editor_id: u64,
    uri: String,
    language_id: String,
    document_bytes: usize,
    state_id: u64,
    saved_state_id: Option<u64>,
    save_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LspQuarantinedDocument {
    editor_id: u64,
    uri: String,
    state_id: u64,
}

#[derive(Clone, Copy, Debug)]
struct LspSyncBudget {
    remaining_documents: usize,
    remaining_bytes: usize,
}

impl LspSyncBudget {
    const fn new() -> Self {
        Self {
            remaining_documents: MAX_LSP_TEXT_DOCUMENTS_PER_SYNC_POLL,
            remaining_bytes: MAX_LSP_TEXT_BYTES_PER_SYNC_POLL,
        }
    }

    fn reserve_text(&mut self, bytes: usize) -> bool {
        if self.remaining_documents == 0 || bytes > self.remaining_bytes {
            return false;
        }
        self.remaining_documents -= 1;
        self.remaining_bytes -= bytes;
        true
    }
}

#[derive(Clone, Copy, Debug)]
struct LspEventPollBudget {
    handled_events: usize,
    retained_bytes: usize,
}

impl LspEventPollBudget {
    const fn new() -> Self {
        Self {
            handled_events: 0,
            retained_bytes: 0,
        }
    }

    const fn can_receive(&self) -> bool {
        self.handled_events < MAX_LSP_EVENTS_PER_POLL
    }

    fn reserve(&mut self, event: &LspEvent) -> bool {
        if !self.can_receive() {
            return false;
        }
        let event_bytes = event.estimated_retained_bytes();
        // An event can be larger than the per-poll budget. Admit it only as
        // the first event of a fresh poll so it cannot block ordered progress
        // forever, then defer every following event to the next poll.
        if self.handled_events != 0
            && event_bytes > MAX_LSP_EVENT_BYTES_PER_POLL.saturating_sub(self.retained_bytes)
        {
            return false;
        }
        self.handled_events += 1;
        self.retained_bytes = self.retained_bytes.saturating_add(event_bytes);
        true
    }

    #[cfg(test)]
    const fn handled_events(&self) -> usize {
        self.handled_events
    }

    #[cfg(test)]
    const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceTreeNode {
    path: PathBuf,
    is_directory: bool,
    depth: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceTreeListing {
    nodes: Vec<WorkspaceTreeNode>,
    truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LspDocumentRequestContext {
    editor_id: u64,
    uri: String,
    version: DocumentVersion,
    incarnation: DocumentIncarnation,
    state_id: u64,
}

struct ProblemNavigationCandidate {
    path: PathBuf,
    line: usize,
    column: usize,
    entry: PromptEntry,
}

#[derive(Clone, Debug)]
enum PendingLspRequest {
    Completion {
        context: LspDocumentRequestContext,
        cursor: usize,
        anchor: Option<usize>,
    },
    Hover {
        context: LspDocumentRequestContext,
    },
    Definition {
        context: LspDocumentRequestContext,
    },
    References {
        context: LspDocumentRequestContext,
    },
    DocumentSymbols {
        context: LspDocumentRequestContext,
    },
    Formatting {
        context: LspDocumentRequestContext,
    },
    WorkspaceSymbols {
        prompt_token: u64,
        query: String,
        server_name: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ConfirmKind {
    Quit,
    CloseBuffer,
    /// Git worktree has uncommitted changes; Y starts the agent goal anyway.
    AgentDirtyTree {
        goal: String,
        git_changes: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptFlow {
    Search,
    ReplaceFind,
    ReplaceWith,
    QuickOpen,
    OpenRecent,
    WorkspaceTree,
    Buffers,
    Palette,
    Command,
    SaveAs,
    OpenPath,
    NewFilePath,
    RenameFilePath,
    SaveCopyAsPath,
    GitChanges,
    GitDiffPicker,
    GitCommitPicker,
    GitCommitMessage,
    GoToLine,
    Recovery,
    GlobalSearch,
    Tasks,
    Completion,
    Locations,
    JumpList,
    Bookmarks,
    Problems,
    DocumentSymbols,
    LocalDefinitions,
    LocalReferences,
    SourceAnnotations,
    WorkspaceSymbolQuery,
    WorkspaceSymbolPending,
    WorkspaceSymbols,
    WorkspaceOutline,
    Stickies,
    AgentGoal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptCompletionBehavior {
    Commit,
    WorkspaceTree,
    Blocked,
}

#[derive(Clone, Debug)]
enum PromptCompletion {
    FinishSearch {
        original_cursor: usize,
    },
    BeginReplace {
        find: String,
        original_cursor: usize,
        original_anchor: Option<usize>,
    },
    ApplyReplace {
        replacement: String,
    },
    ActivateWorkspaceTree,
    Select(Option<PromptEntry>),
    ExecuteCommand(String),
    SaveAs(String),
    OpenPath(String),
    NewFile(String),
    RenameFile(String),
    SaveCopyAs(String),
    CommitGit(String),
    GoToLine(String),
    SubmitWorkspaceSymbol(String),
    WorkspaceSymbolPending,
    StartAgent(String),
}

impl PromptFlow {
    fn prefix(self) -> &'static str {
        match self {
            Self::Search => " find › ",
            Self::ReplaceFind => " replace find › ",
            Self::ReplaceWith => " replace with › ",
            Self::QuickOpen => " open › ",
            Self::OpenRecent => " recent › ",
            Self::WorkspaceTree => " workspace › ",
            Self::Buffers => " buffer › ",
            Self::Palette => " command › ",
            Self::Command => " : ",
            Self::SaveAs => " save as › ",
            Self::OpenPath => " open path › ",
            Self::NewFilePath => " new file › ",
            Self::RenameFilePath => " rename file › ",
            Self::SaveCopyAsPath => " save copy › ",
            Self::GitChanges => " git changes › ",
            Self::GitDiffPicker => " git diffs › ",
            Self::GitCommitPicker => " git commits › ",
            Self::GitCommitMessage => " commit staged › ",
            Self::GoToLine => " line › ",
            Self::Recovery => " recovery (Enter restore · V view · D discard) › ",
            Self::GlobalSearch => " project search › ",
            Self::Tasks => " run task › ",
            Self::Completion => " complete › ",
            Self::Locations => " location › ",
            Self::JumpList => " jump › ",
            Self::Bookmarks => " bookmarks › ",
            Self::Problems => " problems › ",
            Self::DocumentSymbols => " symbols › ",
            Self::LocalDefinitions => " definition › ",
            Self::LocalReferences => " references › ",
            Self::SourceAnnotations => " annotations › ",
            Self::WorkspaceSymbolQuery => " workspace symbol › ",
            Self::WorkspaceSymbolPending => " workspace symbol (searching) › ",
            Self::WorkspaceSymbols => " workspace symbols › ",
            Self::WorkspaceOutline => " outline › ",
            Self::Stickies => " stickies (Enter open · A archive) › ",
            Self::AgentGoal => " agent goal › ",
        }
    }

    fn input_limit(self) -> Option<(&'static str, usize)> {
        match self {
            Self::WorkspaceSymbolQuery | Self::WorkspaceSymbols => {
                Some(("Workspace symbol", MAX_WORKSPACE_SYMBOL_QUERY_BYTES))
            }
            Self::WorkspaceTree => Some(("Workspace tree", MAX_WORKSPACE_TREE_QUERY_BYTES)),
            Self::GitCommitMessage => Some(("Git commit message", MAX_GIT_COMMIT_MESSAGE_BYTES)),
            _ => None,
        }
    }

    fn uses_fixed_candidates(self) -> bool {
        matches!(
            self,
            Self::Completion
                | Self::Locations
                | Self::Problems
                | Self::DocumentSymbols
                | Self::LocalDefinitions
                | Self::LocalReferences
                | Self::SourceAnnotations
                | Self::GitChanges
                | Self::GitDiffPicker
                | Self::GitCommitPicker
                | Self::WorkspaceSymbols
                | Self::WorkspaceOutline
        )
    }

    fn completion_behavior(self) -> PromptCompletionBehavior {
        match self {
            Self::WorkspaceSymbolPending => PromptCompletionBehavior::Blocked,
            Self::WorkspaceTree => PromptCompletionBehavior::WorkspaceTree,
            _ => PromptCompletionBehavior::Commit,
        }
    }

    fn complete(self, prompt: &Prompt) -> PromptCompletion {
        match self {
            Self::Search => PromptCompletion::FinishSearch {
                original_cursor: prompt.original_cursor,
            },
            Self::ReplaceFind => PromptCompletion::BeginReplace {
                find: prompt.input.clone(),
                original_cursor: prompt.original_cursor,
                original_anchor: prompt.original_anchor,
            },
            Self::ReplaceWith => PromptCompletion::ApplyReplace {
                replacement: prompt.input.clone(),
            },
            Self::WorkspaceTree => PromptCompletion::ActivateWorkspaceTree,
            Self::QuickOpen
            | Self::OpenRecent
            | Self::Buffers
            | Self::Palette
            | Self::Recovery
            | Self::GlobalSearch
            | Self::Tasks
            | Self::GitChanges
            | Self::GitDiffPicker
            | Self::GitCommitPicker
            | Self::Completion
            | Self::Locations
            | Self::JumpList
            | Self::Bookmarks
            | Self::Problems
            | Self::DocumentSymbols
            | Self::LocalDefinitions
            | Self::LocalReferences
            | Self::SourceAnnotations
            | Self::WorkspaceSymbols
            | Self::WorkspaceOutline
            | Self::Stickies => {
                PromptCompletion::Select(prompt.entries.get(prompt.selected).cloned())
            }
            Self::AgentGoal => PromptCompletion::StartAgent(prompt.input.clone()),
            Self::Command => PromptCompletion::ExecuteCommand(prompt.input.clone()),
            Self::SaveAs => PromptCompletion::SaveAs(prompt.input.clone()),
            Self::OpenPath => PromptCompletion::OpenPath(prompt.input.clone()),
            Self::NewFilePath => PromptCompletion::NewFile(prompt.input.clone()),
            Self::RenameFilePath => PromptCompletion::RenameFile(prompt.input.clone()),
            Self::SaveCopyAsPath => PromptCompletion::SaveCopyAs(prompt.input.clone()),
            Self::GitCommitMessage => PromptCompletion::CommitGit(prompt.input.clone()),
            Self::GoToLine => PromptCompletion::GoToLine(prompt.input.clone()),
            Self::WorkspaceSymbolQuery => {
                PromptCompletion::SubmitWorkspaceSymbol(prompt.input.clone())
            }
            Self::WorkspaceSymbolPending => PromptCompletion::WorkspaceSymbolPending,
        }
    }
}

#[derive(Clone, Debug)]
enum PromptEntry {
    Path(PathBuf),
    RecentPath(PathBuf),
    WorkspaceTree(WorkspaceTreeNode),
    Buffer(usize),
    Action(Action),
    Recovery(String),
    Search(SearchMatch),
    Task(String),
    GitChange(PathBuf),
    GitDiff(PathBuf),
    GitCommit(String),
    DocumentSymbol(JumpLocation),
    LocalDefinition(JumpLocation),
    LocalReference(JumpLocation),
    SourceAnnotation(JumpLocation),
    WorkspaceOutline(JumpLocation),
    Completion(
        CompletionItem,
        LspDocumentRequestContext,
        usize,
        Option<usize>,
    ),
    Location(Location),
    ProblemLocation(Location, DiagnosticSeverity),
    Jump(JumpLocation),
    Bookmark(JumpLocation),
    TaskProblem(TaskProblem),
    Sticky {
        id: String,
        path: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub struct Prompt {
    kind: PromptFlow,
    pub input: String,
    cursor: usize,
    selected: usize,
    labels: Vec<String>,
    entries: Vec<PromptEntry>,
    all_labels: Vec<String>,
    all_entries: Vec<PromptEntry>,
    notice: Option<String>,
    original_cursor: usize,
    original_anchor: Option<usize>,
}

impl Prompt {
    fn new(kind: PromptFlow, input: String, cursor: usize, anchor: Option<usize>) -> Self {
        let input_cursor = input.chars().count();
        Self {
            kind,
            input,
            cursor: input_cursor,
            selected: 0,
            labels: Vec::new(),
            entries: Vec::new(),
            all_labels: Vec::new(),
            all_entries: Vec::new(),
            notice: None,
            original_cursor: cursor,
            original_anchor: anchor,
        }
    }

    pub fn prefix(&self) -> &'static str {
        self.kind.prefix()
    }

    pub fn before_cursor(&self) -> String {
        self.input.chars().take(self.cursor).collect()
    }

    fn insert(&mut self, text: &str) {
        let byte = char_to_byte(&self.input, self.cursor);
        self.input.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    /// Insert a complete user input only when the resulting UTF-8 payload
    /// stays within the caller's explicit bound. Refusing the whole paste is
    /// less surprising than silently clipping a query in the middle of what
    /// the user intended to send to a language server.
    fn insert_bounded(&mut self, text: &str, max_bytes: usize) -> bool {
        if self.input.len().saturating_add(text.len()) > max_bytes {
            return false;
        }
        self.insert(text);
        true
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = previous_grapheme_start(&self.input, self.cursor);
        let start = char_to_byte(&self.input, previous);
        let end = char_to_byte(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor = previous;
    }

    fn delete_forward(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let next = next_grapheme_end(&self.input, self.cursor);
        let start = char_to_byte(&self.input, self.cursor);
        let end = char_to_byte(&self.input, next);
        self.input.replace_range(start..end, "");
    }

    fn move_left(&mut self) {
        self.cursor = previous_grapheme_start(&self.input, self.cursor);
    }

    fn move_right(&mut self) {
        self.cursor = next_grapheme_end(&self.input, self.cursor);
    }
}

#[derive(Clone, Debug)]
enum UiMode {
    Edit,
    Prompt(Prompt),
    Help,
    Confirm(ConfirmKind),
    TaskTrust(String),
    GitTrust(GitMutation),
}

pub struct OverlayView<'a> {
    pub title: &'static str,
    pub items: &'a [String],
    pub selected: usize,
    pub notice: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSidebarLine {
    pub text: String,
    pub depth: usize,
    pub directory: bool,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSidebarView {
    pub lines: Vec<ProjectSidebarLine>,
    pub partial: bool,
    pub unavailable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ServiceStatus {
    Idle,
    Pending(u64),
    Ready,
    Failed(String),
}

impl ServiceStatus {
    fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

struct UiState {
    clipboard: Clipboard,
    keymap: Keymap,
    mode: UiMode,
    status: Option<Status>,
    search_query: Option<String>,
    search_match: Option<Range<usize>>,
    search_editor_id: Option<u64>,
    pending_replace: Option<String>,
    terminal_output: Vec<u8>,
    should_quit: bool,
    quit_after_save: bool,
    /// After a successful format, continue with a disk save (format_on_save).
    save_after_format: bool,
    save_all_after_save_as: bool,
    close_after_save_as: bool,
    full_redraw: bool,
    edit_transition: EditTransitionFeedback,
    screen_size: (u16, u16),
    mouse_selecting: bool,
    viewport_scroll_pending: bool,
    soft_wrap: bool,
    /// Throttle background-service paints so mosh is not flooded.
    last_background_redraw: Instant,
    pending_background_redraw: bool,
    /// Optional `WSCRPT_PERF` footer payload.
    perf_stats: Option<String>,
    /// Lazily sampled multiline syntax states for the active buffer.
    highlight_index: crate::syntax::HighlightStateIndex,
    terminal_requested: bool,
    jump_back: Vec<JumpLocation>,
    jump_forward: Vec<JumpLocation>,
    bookmarks: Vec<JumpLocation>,
    closed_buffers: Vec<ClosedBufferState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DocumentStateStamp {
    editor_id: u64,
    state_id: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct EditTransitionFeedback {
    /// Action has ended; wait for the first real document revision.
    armed: Option<DocumentStateStamp>,
    /// The one-shot visual acknowledgement is active until this deadline.
    cue_until: Option<Instant>,
}

impl UiState {
    fn merge_status(&mut self, message: impl Into<String>, error: bool) {
        merge_startup_status(&mut self.status, message, error);
    }
}

struct ProjectState {
    index: Option<ProjectIndex>,
    tree_expanded: HashSet<PathBuf>,
    tree_resume_path: Option<PathBuf>,
    tree_document_paths: HashMap<u64, Option<PathBuf>>,
    sidebar_visible: bool,
    index_built_at: Option<std::time::SystemTime>,
    search_worker: Option<SearchWorker>,
    status: ServiceStatus,
}

impl ProjectState {
    fn install_index(&mut self, index: ProjectIndex, workspace: &Workspace) {
        self.index = Some(index);
        self.index_built_at = Some(std::time::SystemTime::now());
        self.search_worker = self.index.clone().map(SearchWorker::new);
        self.tree_document_paths =
            snapshot_workspace_tree_document_paths(workspace, self.index.as_ref());
        self.status = ServiceStatus::Ready;
    }
}

#[derive(Clone, Debug)]
struct PendingGitMutation {
    generation: u64,
    mutation: GitMutation,
}

/// Git inspection state plus at most one trusted local mutation in flight.
struct GitState {
    repository: Option<GitRepository>,
    branch: Option<String>,
    changes: usize,
    status: ServiceStatus,
    pending: Option<PendingGitMutation>,
}

impl GitState {
    fn install(&mut self, snapshot: Option<GitSnapshot>) {
        match snapshot {
            Some(snapshot) => {
                self.branch = branch_label(&snapshot.status.branch.head);
                self.changes = snapshot.status.files.len();
                self.repository = Some(snapshot.repository);
            }
            None => {
                self.repository = None;
                self.branch = None;
                self.changes = 0;
            }
        }
        self.status = ServiceStatus::Ready;
    }

    fn fail(&mut self, error: String) {
        self.repository = None;
        self.branch = None;
        self.changes = 0;
        self.status = ServiceStatus::Failed(error);
    }

    fn unavailable_message(&self) -> String {
        match &self.status {
            ServiceStatus::Idle | ServiceStatus::Pending(_) => {
                "Git status is still loading".to_owned()
            }
            ServiceStatus::Failed(error) => format!("Git status is unavailable: {error}"),
            ServiceStatus::Ready => "Workspace is not inside a Git repository".to_owned(),
        }
    }
}

struct PersistenceState {
    recovery_store: Option<RecoveryStore>,
    recovery_records: Vec<RecoveryRecord>,
    recovery_ids: HashMap<u64, String>,
    recovery_checkpoint_state: HashMap<u64, RecoveryCheckpointStamp>,
    recovery_status: ServiceStatus,
    session_store: Option<SessionStore>,
    last_session: Option<Session>,
    recent_files: Vec<PathBuf>,
}

impl PersistenceState {
    fn install_recovery(&mut self, snapshot: &mut RecoverySnapshot) {
        self.recovery_store = snapshot.store.take();
        self.recovery_records = std::mem::take(&mut snapshot.listing.records);
        self.recovery_status = if let Some(error) = snapshot.notice.clone() {
            ServiceStatus::Failed(error)
        } else {
            ServiceStatus::Ready
        };
    }
}

struct TaskState {
    runner: Option<TaskRunner>,
    handle: Option<TaskHandle>,
    output: String,
    output_decoder: TaskOutputDecoder,
    cwd: Option<PathBuf>,
    last: Option<String>,
}

/// Host-local agent orchestration (W2). Fake agent by default; ACP process when configured.
struct AgentUiState {
    coordinator: crate::agent::AgentCoordinator,
    job: Option<crate::agent_runtime::AgentJob>,
    port: Option<crate::agent_runtime::AgentEventPort>,
    last_summary: Option<String>,
    /// Bottom dashboard strip (toggle with Esc w D), inspired by Grok Build.
    dashboard_visible: bool,
    /// ACP `session/request_permission` awaiting Y/N (Needs You).
    pending_permission: Option<crate::agent_runtime::PendingPermission>,
    /// True after auto (or manual) review handoff opened Git for this run.
    review_handoff_done: bool,
}

/// One display row in the bottom agent dashboard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDashboardLine {
    pub text: String,
    pub emphasis: AgentDashboardEmphasis,
}

/// Visual role for a dashboard line (renderer maps to colors).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentDashboardEmphasis {
    Title,
    RunActive,
    RunNeedsYou,
    RunReview,
    RunIdle,
    Receipt,
    Hint,
    Muted,
}

/// Bounded view model for the agent dashboard panel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentDashboardView {
    pub lines: Vec<AgentDashboardLine>,
}

impl TaskState {
    fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.state().is_finished())
    }
}

struct LspState {
    client: Option<LspClient>,
    server_name: Option<String>,
    workspace_symbols: Option<bool>,
    text_document_sync: Option<TextDocumentSyncCapability>,
    failed_server: Option<String>,
    documents: SynchronizedDocumentRegistry,
    document_incarnations: HashMap<u64, DocumentIncarnation>,
    document_ends: HashMap<u64, LspPosition>,
    discovery_cursor: usize,
    sync_cursor: usize,
    background_sync_due: bool,
    quarantined_documents: Vec<LspQuarantinedDocument>,
    next_document_version: u64,
    ambiguous_diagnostic_uris: HashSet<String>,
    all_versionless_diagnostics_ambiguous: bool,
    requests: HashMap<RequestId, PendingLspRequest>,
    deferred_event: Option<LspEvent>,
    diagnostics: DiagnosticCache,
    log: String,
    next_workspace_symbol_token: u64,
    active_workspace_symbol_token: Option<u64>,
}

impl LspState {
    fn cancel_ui_requests(&mut self) {
        let request_ids = self.requests.keys().copied().collect::<Vec<_>>();
        if let Some(client) = &mut self.client {
            for request_id in request_ids {
                let _ = client.cancel_request(request_id);
            }
        }
        self.requests.clear();
        self.active_workspace_symbol_token = None;
    }
}

pub struct App {
    workspace: Workspace,
    config: Config,
    ui: UiState,
    project: ProjectState,
    lsp: LspState,
    tasks: TaskState,
    agent: AgentUiState,
    sticky_pad: crate::stickies::StickyPad,
    persistence: PersistenceState,
    git: GitState,
    services: ServiceCoordinator,
    services_started: bool,
}

impl App {
    pub fn new(workspace: Workspace, config: Config) -> Self {
        // Indexing, Git discovery, and recovery scanning start only after the
        // terminal is initialized. `App::new` remains a first-frame-safe constructor.
        let mut status = None;
        // Inside tmux, direct OSC 52 stops at tmux; the DCS passthrough
        // envelope reaches the outer terminal (Blink) regardless of tmux's
        // `set-clipboard` forwarding.
        let osc52 = osc52_config_for_route(config.osc52_copy, env::var_os("TMUX").is_some());
        let (task_runner, task_error) = match TaskConfig::load_if_present(&workspace.root) {
            Ok(Some(config)) => (Some(TaskRunner::new(workspace.root.clone(), config)), None),
            Ok(None) => (None, None),
            Err(error) => (None, Some(error.to_string())),
        };
        if let Some(message) = task_error {
            merge_startup_status(&mut status, message, true);
        }
        let workspace_tree_document_paths =
            snapshot_workspace_tree_document_paths(&workspace, None);
        let recent_files = recent_files_from_workspace(&workspace);
        Self {
            workspace,
            config,
            ui: UiState {
                clipboard: Clipboard::new(osc52),
                keymap: Keymap::new(),
                mode: UiMode::Edit,
                status,
                search_query: None,
                search_match: None,
                search_editor_id: None,
                pending_replace: None,
                terminal_output: Vec::new(),
                should_quit: false,
                quit_after_save: false,
                save_after_format: false,
                save_all_after_save_as: false,
                close_after_save_as: false,
                full_redraw: true,
                edit_transition: EditTransitionFeedback::default(),
                screen_size: (80, 24),
                mouse_selecting: false,
                viewport_scroll_pending: false,
                soft_wrap: false,
                last_background_redraw: Instant::now()
                    .checked_sub(BACKGROUND_REDRAW_MIN_INTERVAL)
                    .unwrap_or_else(Instant::now),
                pending_background_redraw: false,
                perf_stats: None,
                highlight_index: crate::syntax::HighlightStateIndex::default(),
                terminal_requested: false,
                jump_back: Vec::new(),
                jump_forward: Vec::new(),
                bookmarks: Vec::new(),
                closed_buffers: Vec::new(),
            },
            project: ProjectState {
                index: None,
                tree_expanded: HashSet::new(),
                tree_resume_path: None,
                tree_document_paths: workspace_tree_document_paths,
                sidebar_visible: false,
                index_built_at: None,
                search_worker: None,
                status: ServiceStatus::Idle,
            },
            agent: AgentUiState {
                coordinator: crate::agent::AgentCoordinator::new(1),
                job: None,
                port: None,
                last_summary: None,
                dashboard_visible: false,
                pending_permission: None,
                review_handoff_done: false,
            },
            sticky_pad: crate::stickies::StickyPad::default(),
            lsp: LspState {
                client: None,
                server_name: None,
                workspace_symbols: None,
                text_document_sync: None,
                failed_server: None,
                documents: SynchronizedDocumentRegistry::new(),
                document_incarnations: HashMap::new(),
                document_ends: HashMap::new(),
                discovery_cursor: 0,
                sync_cursor: 0,
                background_sync_due: false,
                quarantined_documents: Vec::new(),
                next_document_version: 1,
                ambiguous_diagnostic_uris: HashSet::new(),
                all_versionless_diagnostics_ambiguous: false,
                requests: HashMap::new(),
                deferred_event: None,
                diagnostics: DiagnosticCache::new(),
                log: String::new(),
                next_workspace_symbol_token: 1,
                active_workspace_symbol_token: None,
            },
            tasks: TaskState {
                runner: task_runner,
                handle: None,
                output: String::new(),
                output_decoder: TaskOutputDecoder::new(),
                cwd: None,
                last: None,
            },
            persistence: PersistenceState {
                recovery_store: None,
                recovery_records: Vec::new(),
                recovery_ids: HashMap::new(),
                recovery_checkpoint_state: HashMap::new(),
                recovery_status: ServiceStatus::Idle,
                session_store: SessionStore::from_env().ok(),
                last_session: None,
                recent_files,
            },
            git: GitState {
                repository: None,
                branch: None,
                changes: 0,
                status: ServiceStatus::Idle,
                pending: None,
            },
            services: ServiceCoordinator::new(),
            services_started: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_ready_for_test(workspace: Workspace, config: Config) -> Self {
        let mut app = Self::new(workspace, config);
        match ProjectIndex::build(&app.workspace.root) {
            Ok(index) => {
                app.project.install_index(index, &app.workspace);
            }
            Err(error) => {
                app.project.status = ServiceStatus::Failed(error.to_string());
            }
        }
        app.git.repository = GitRepository::discover(&app.workspace.root).ok();
        (app.git.branch, app.git.changes) = app
            .git
            .repository
            .as_ref()
            .and_then(|repository| repository.status().ok())
            .map(|status| (branch_label(&status.branch.head), status.files.len()))
            .unwrap_or((None, 0));
        app.git.status = ServiceStatus::Ready;
        app.persistence.recovery_store = RecoveryStore::from_env().ok();
        app.persistence.recovery_status = ServiceStatus::Ready;
        app
    }

    /// Read-only access to the active workspace for renderer/runtime queries.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Narrow crate-internal mutation access used by the renderer to prepare
    /// viewports. Product workflows mutate buffers through `App` commands.
    #[allow(dead_code)]
    pub(crate) fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspace
    }

    /// Read-only runtime configuration snapshot.
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace.root
    }

    /// Start bounded startup workers after the terminal has entered its UI
    /// mode. The method only spawns jobs and never waits for filesystem or Git
    /// work, preserving the first-frame path.
    pub fn start_background_services(&mut self) {
        if self.services_started {
            return;
        }
        let (project, git, recovery) = self.services.start_all(self.workspace.root.clone());
        self.services_started = true;
        self.project.status = ServiceStatus::Pending(project);
        self.git.status = ServiceStatus::Pending(git);
        self.persistence.recovery_status = ServiceStatus::Pending(recovery);
        self.ui
            .merge_status("Indexing · Git loading · Recovery scanning", false);
    }

    fn git_unavailable_message(&self) -> String {
        self.git.unavailable_message()
    }

    pub fn set_screen_size(&mut self, size: (u16, u16)) {
        self.ui.screen_size = size;
    }

    pub fn soft_wrap_enabled(&self) -> bool {
        self.ui.soft_wrap
    }

    pub fn should_quit(&self) -> bool {
        self.ui.should_quit
    }

    /// Consume a request to temporarily hand the controlling terminal to the
    /// user's real shell. The binary owns terminal teardown/re-entry, so the UI
    /// only publishes intent here.
    pub fn take_terminal_request(&mut self) -> bool {
        std::mem::take(&mut self.ui.terminal_requested)
    }

    pub fn terminal_returned(&mut self, outcome: Result<std::process::ExitStatus, String>) {
        self.ui.full_redraw = true;
        match outcome {
            Ok(status) if status.success() => {
                self.status("Returned from workspace shell");
            }
            Ok(status) => {
                self.error(format!("Workspace shell exited with {status}"));
            }
            Err(error) => {
                self.error(format!("Could not launch workspace shell: {error}"));
            }
        }
    }

    /// Merge a startup note with any recovery/task warning discovered while
    /// constructing the application instead of silently hiding either one.
    pub fn startup_notice(&mut self, message: impl Into<String>, error: bool) {
        self.ui.merge_status(message, error);
    }

    /// Open first-run help once per state dir until the operator dismisses it.
    pub fn maybe_open_first_run_help(&mut self) {
        if !crate::onboarding::should_show_first_run_help() {
            return;
        }
        self.ui.mode = UiMode::Help;
        self.ui.merge_status(
            "First run — Esc closes help · Esc ? full keys · wscrpt --health for LSP setup",
            false,
        );
    }

    fn dismiss_help(&mut self) {
        if matches!(self.ui.mode, UiMode::Help) {
            self.ui.mode = UiMode::Edit;
            crate::onboarding::mark_first_run_help_seen();
        }
    }

    pub fn apply_session_layout(&mut self, layout: LayoutFlags) {
        self.ui.soft_wrap = layout.soft_wrap;
        self.project.sidebar_visible = layout.workspace_tree_visible;
        self.agent.dashboard_visible = layout.agent_dashboard_visible;
        self.sticky_pad.visible = layout.sticky_pad_visible;
        // Restoring visibility does not steal focus from the editor.
        self.sticky_pad.focused = false;
    }

    pub fn apply_session_recent_files(&mut self, recent_files: Vec<PathBuf>) {
        for path in recent_files.into_iter().rev() {
            self.record_recent_file(path);
        }
    }

    pub fn apply_session_bookmarks(&mut self, bookmarks: Vec<BookmarkState>) {
        self.ui.bookmarks.clear();
        for bookmark in bookmarks.into_iter().take(MAX_BOOKMARKS) {
            if !bookmark.path.is_file() {
                continue;
            }
            self.ui.bookmarks.push(JumpLocation {
                editor_id: u64::MAX,
                path: Some(bookmark.path),
                cursor: bookmark.cursor,
            });
        }
    }

    fn record_active_file_recent(&mut self) {
        if let Some(path) = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf)
        {
            self.record_recent_file(path);
        }
    }

    fn record_recent_file(&mut self, path: PathBuf) {
        if !path.is_absolute() {
            return;
        }
        self.persistence
            .recent_files
            .retain(|existing| !same_workspace(existing, &path));
        self.persistence.recent_files.insert(0, path);
        self.persistence
            .recent_files
            .truncate(MAX_RECENT_FILES_VIEW);
    }

    fn snapshot_closed_buffer(editor: &Editor) -> Option<ClosedBufferState> {
        let path = editor.document.path()?.to_path_buf();
        Some(ClosedBufferState {
            path,
            cursor: editor.cursor,
            anchor: editor.anchor,
            viewport: editor.viewport,
        })
    }

    fn push_closed_buffer(&mut self, closed: ClosedBufferState) {
        self.ui
            .closed_buffers
            .retain(|existing| !same_workspace(&existing.path, &closed.path));
        self.ui.closed_buffers.insert(0, closed);
        self.ui.closed_buffers.truncate(MAX_CLOSED_BUFFER_HISTORY);
    }

    fn close_active_buffer(&mut self, force: bool) -> Result<(), &'static str> {
        let closed = Self::snapshot_closed_buffer(self.workspace.active());
        self.workspace.close_active(force)?;
        if let Some(closed) = closed {
            self.push_closed_buffer(closed);
        }
        Ok(())
    }

    fn restore_closed_buffer_state(&mut self, closed: &ClosedBufferState) {
        let mut editor = self.workspace.active_mut();
        let len = editor.document.len_chars();
        editor.cursor = closed.cursor.min(len);
        editor.anchor = closed.anchor.filter(|anchor| *anchor <= len);
        editor.viewport = closed.viewport;
    }

    fn reopen_closed_buffer(&mut self) {
        if let Some(closed) = self.ui.closed_buffers.first().cloned() {
            self.ui.closed_buffers.remove(0);
            if !closed.path.exists() {
                self.error(format!(
                    "Closed buffer is missing: {}",
                    closed.path.display()
                ));
                return;
            }
            if !closed.path.is_file() {
                self.error(format!(
                    "Closed buffer is not a regular file: {}",
                    closed.path.display()
                ));
                return;
            }
            match self.workspace.open(&closed.path) {
                Ok(_) => {
                    self.restore_closed_buffer_state(&closed);
                    self.cache_active_workspace_tree_path();
                    self.record_active_file_recent();
                    let label = closed
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| closed.path.to_string_lossy().into_owned());
                    self.status(format!("Reopened closed buffer: {label}"));
                    return;
                }
                Err(error) => {
                    self.error(format!("Could not reopen closed buffer: {error}"));
                    return;
                }
            }
        }
        self.status("No closed file-backed buffers");
    }

    fn session_recent_files(&self, open_files: &[OpenFileState]) -> Vec<PathBuf> {
        let mut paths = self.persistence.recent_files.clone();
        for file in open_files.iter().rev() {
            paths.retain(|existing| !same_workspace(existing, &file.path));
            paths.insert(0, file.path.clone());
        }
        paths.truncate(MAX_RECENT_FILES_VIEW);
        paths
    }

    fn recent_file_candidates(&self, query: &str) -> (Vec<String>, Vec<PromptEntry>) {
        let active_path = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf);
        let mut candidates: Vec<_> = self
            .persistence
            .recent_files
            .iter()
            .enumerate()
            .filter_map(|(index, path)| {
                let active = if active_path
                    .as_ref()
                    .is_some_and(|active| same_workspace(active, path))
                {
                    "*"
                } else {
                    " "
                };
                let open = if self.workspace.file_editors().any(|editor| {
                    editor
                        .document
                        .path()
                        .is_some_and(|open| same_workspace(open, path))
                }) {
                    "o"
                } else {
                    " "
                };
                let disk = match fs::metadata(path) {
                    Ok(metadata) if metadata.is_file() => "file",
                    Ok(_) => "other",
                    Err(_) => "missing",
                };
                let label = format!(
                    "{:>2}  {active} {open}  {:<7}  {}",
                    index + 1,
                    disk,
                    path.display()
                );
                let score = if query.is_empty() {
                    Some(0)
                } else {
                    fuzzy_path_score(query, &label)
                        .or_else(|| fuzzy_path_score(query, &path.display().to_string()))
                }?;
                Some((score, index, label, PromptEntry::RecentPath(path.clone())))
            })
            .collect();
        if query.is_empty() {
            candidates.sort_by_key(|(_, index, _, _)| *index);
        } else {
            candidates
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        }
        candidates
            .into_iter()
            .map(|(_, _, label, entry)| (label, entry))
            .unzip()
    }

    fn begin_git_changes(&mut self) {
        self.begin_git_status_picker(
            PromptFlow::GitChanges,
            "Enter opens selected path · filter by path/status · Esc cancels",
            PromptEntry::GitChange,
        );
    }

    fn begin_git_diff_picker(&mut self) {
        self.begin_git_status_picker(
            PromptFlow::GitDiffPicker,
            "Enter opens selected diff · filter by path/status · Esc cancels",
            PromptEntry::GitDiff,
        );
    }

    fn begin_git_status_picker(
        &mut self,
        kind: PromptFlow,
        notice: &'static str,
        entry_for_path: impl Fn(PathBuf) -> PromptEntry,
    ) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let status = match repository.status() {
            Ok(status) => status,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        self.git.branch = branch_label(&status.branch.head);
        self.git.changes = status.files.len();
        if status.files.is_empty() {
            self.status("Working tree clean");
            return;
        }

        let editor = self.workspace.active();
        let mut prompt = Prompt::new(kind, String::new(), editor.cursor, editor.anchor);
        let mut rows = Vec::new();
        for (index, file) in status.files.into_iter().enumerate() {
            let mut label = format!(
                "{} {}  {}",
                git_state_glyph(file.index),
                git_state_glyph(file.worktree),
                file.path.display()
            );
            if let Some(original) = file.original_path.as_ref() {
                label.push_str(&format!("  ← {}", original.display()));
            }
            rows.push((
                format!("{:>2}  {label}", index + 1),
                entry_for_path(repository.root().join(file.path)),
            ));
        }
        (prompt.all_labels, prompt.all_entries) = rows.into_iter().unzip();
        prompt.labels = prompt.all_labels.clone();
        prompt.entries = prompt.all_entries.clone();
        prompt.notice = Some(notice.to_owned());
        self.ui.mode = UiMode::Prompt(prompt);
        self.ui.status = None;
    }

    fn begin_git_commit_picker(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let branch = match repository.current_branch() {
            Ok(branch) => branch,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        self.git.branch = branch_label(&branch.head);
        if branch.unborn {
            self.status("No commits yet");
            return;
        }
        let log = match repository.recent_log(100) {
            Ok(log) => log,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        if log.is_empty() {
            self.status("No commits yet");
            return;
        }

        let editor = self.workspace.active();
        let mut prompt = Prompt::new(
            PromptFlow::GitCommitPicker,
            String::new(),
            editor.cursor,
            editor.anchor,
        );
        let mut rows = Vec::new();
        for (index, line) in String::from_utf8_lossy(&log).lines().enumerate() {
            let Some(commit) = line.split_whitespace().next() else {
                continue;
            };
            if commit.is_empty() {
                continue;
            }
            rows.push((
                format!("{:>2}  {line}", index + 1),
                PromptEntry::GitCommit(commit.to_owned()),
            ));
        }
        if rows.is_empty() {
            self.status("No commits yet");
            return;
        }
        (prompt.all_labels, prompt.all_entries) = rows.into_iter().unzip();
        prompt.labels = prompt.all_labels.clone();
        prompt.entries = prompt.all_entries.clone();
        prompt.notice = Some(
            "Enter opens selected commit · filter by hash/date/message · Esc cancels".to_owned(),
        );
        self.ui.mode = UiMode::Prompt(prompt);
        self.ui.status = None;
    }

    fn jump_list_candidates(&self, query: &str) -> (Vec<String>, Vec<PromptEntry>) {
        let mut raw = Vec::new();
        for (distance, jump) in self.ui.jump_back.iter().rev().enumerate() {
            raw.push((
                distance,
                self.jump_location_label("older", distance + 1, jump),
                jump.clone(),
            ));
        }
        let current = self.current_jump_location();
        raw.push((
            MAX_JUMP_HISTORY,
            self.jump_location_label("current", 0, &current),
            current,
        ));
        for (distance, jump) in self.ui.jump_forward.iter().rev().enumerate() {
            raw.push((
                MAX_JUMP_HISTORY + distance + 1,
                self.jump_location_label("newer", distance + 1, jump),
                jump.clone(),
            ));
        }

        let mut candidates: Vec<_> = raw
            .into_iter()
            .filter_map(|(order, label, jump)| {
                let score = if query.is_empty() {
                    Some(0)
                } else {
                    fuzzy_path_score(query, &label)
                }?;
                Some((score, order, label, PromptEntry::Jump(jump)))
            })
            .collect();
        if query.is_empty() {
            candidates.sort_by_key(|(_, order, _, _)| *order);
        } else {
            candidates
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        }
        candidates
            .into_iter()
            .map(|(_, _, label, entry)| (label, entry))
            .unzip()
    }

    fn bookmark_candidates(&self, query: &str) -> (Vec<String>, Vec<PromptEntry>) {
        let mut candidates: Vec<_> = self
            .ui
            .bookmarks
            .iter()
            .enumerate()
            .filter_map(|(index, bookmark)| {
                let label = self.bookmark_location_label(index, bookmark);
                let score = if query.is_empty() {
                    Some(0)
                } else {
                    fuzzy_path_score(query, &label)
                }?;
                Some((score, index, label, PromptEntry::Bookmark(bookmark.clone())))
            })
            .collect();
        if query.is_empty() {
            candidates.sort_by_key(|(_, index, _, _)| *index);
        } else {
            candidates
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        }
        candidates
            .into_iter()
            .map(|(_, _, label, entry)| (label, entry))
            .unzip()
    }

    fn jump_location_label(&self, side: &str, distance: usize, jump: &JumpLocation) -> String {
        let (marker, slot) = match side {
            "older" => ("←", format!("older {distance}")),
            "newer" => ("→", format!("newer {distance}")),
            "current" => ("•", "current".to_owned()),
            "bookmark" => ("★", format!("bookmark {distance}")),
            _ => (" ", side.to_owned()),
        };
        let target = if let Some(editor) = self.workspace.editor_by_id(jump.editor_id) {
            let line = editor.document.char_to_line(jump.cursor) + 1;
            let column = jump
                .cursor
                .saturating_sub(editor.document.line_start_char(line - 1))
                + 1;
            let name = jump
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| editor.document.display_name().to_owned());
            format!("{name}:{line}:{column}")
        } else if let Some(path) = &jump.path {
            format!("{} @ char {}", path.display(), jump.cursor + 1)
        } else {
            format!("closed untitled buffer @ char {}", jump.cursor + 1)
        };
        format!("{marker} {slot:<9} {target}")
    }

    fn bookmark_location_label(&self, index: usize, bookmark: &JumpLocation) -> String {
        self.jump_location_label("bookmark", index + 1, bookmark)
    }

    pub fn disable_session_persistence(&mut self) {
        self.persistence.session_store = None;
        self.persistence.last_session = None;
    }

    /// Persist only navigational state. Unsaved text remains exclusively in
    /// the recovery journal, so a session file can never masquerade as a
    /// backup.
    pub fn checkpoint_session(&mut self) -> bool {
        let Some(store) = self.persistence.session_store.clone() else {
            return false;
        };
        let session = self.session_snapshot();
        if self.persistence.last_session.as_ref() == Some(&session) {
            return false;
        }
        match store.save(&session) {
            Ok(_) => {
                self.persistence.last_session = Some(session);
                false
            }
            Err(error) => {
                self.error(format!("Session checkpoint failed: {error}"));
                true
            }
        }
    }

    pub fn session_snapshot(&self) -> Session {
        let active_editor_id = self.workspace.active().id();
        let mut active_index = 0;
        let mut open_files = Vec::new();
        for editor in self.workspace.buffers() {
            let Some(path) = editor.document.path() else {
                continue;
            };
            if editor.id() == active_editor_id {
                active_index = open_files.len();
            }
            open_files.push(OpenFileState {
                path: path.to_path_buf(),
                cursor: editor.cursor,
                anchor: editor.anchor,
                viewport: ViewportState {
                    top_line: editor.viewport.top_line,
                    top_wrap_char: editor.viewport.top_wrap_char,
                    left_column: editor.viewport.left_column,
                },
            });
        }
        if open_files.is_empty() {
            active_index = 0;
        }
        Session {
            version: crate::session::SESSION_FORMAT_VERSION,
            root: self.workspace.root.clone(),
            recent_files: self.session_recent_files(&open_files),
            bookmarks: self.session_bookmarks(),
            open_files,
            active_index,
            layout: LayoutFlags {
                soft_wrap: self.ui.soft_wrap,
                workspace_tree_visible: self.project.sidebar_visible,
                agent_dashboard_visible: self.agent.dashboard_visible,
                sticky_pad_visible: self.sticky_pad.visible,
                ..LayoutFlags::default()
            },
        }
    }

    fn session_bookmarks(&self) -> Vec<BookmarkState> {
        self.ui
            .bookmarks
            .iter()
            .filter_map(|bookmark| {
                let path = bookmark.path.as_ref()?;
                Some(BookmarkState {
                    path: path.clone(),
                    cursor: bookmark.cursor,
                })
            })
            .take(MAX_BOOKMARKS)
            .collect()
    }

    pub fn take_terminal_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ui.terminal_output)
    }

    pub fn take_full_redraw(&mut self) -> bool {
        std::mem::take(&mut self.ui.full_redraw)
    }

    /// Prefer a short poll while background I/O may produce UI updates.
    pub fn wants_frequent_poll(&self) -> bool {
        self.ui.pending_background_redraw
            || self.ui.edit_transition.cue_until.is_some()
            || self.lsp.client.is_some()
            || self.tasks.is_running()
            || self.git.pending.is_some()
            || self.agent.job.is_some()
            || self.agent.port.is_some()
            || self.project.search_worker.is_some()
                && matches!(
                    self.ui.mode,
                    UiMode::Prompt(Prompt {
                        kind: PromptFlow::GlobalSearch,
                        ..
                    })
                )
    }

    pub fn set_perf_stats(&mut self, frame: Duration, paint: crate::render::PaintStats) {
        self.ui.perf_stats = Some(format!(
            "perf {}ms paint {}/{} rows",
            frame.as_millis().max(1),
            paint.rows_painted,
            paint.rows_total
        ));
    }

    pub fn perf_stats(&self) -> Option<&str> {
        self.ui.perf_stats.as_deref()
    }

    /// Multline highlight state at the start of `line` for the active editor.
    ///
    /// Uses a rope-backed sampled index so scrolling a large file does not
    /// re-scan hundreds of lines with full highlighter allocations per frame.
    pub fn syntax_state_before_line(&mut self, line: usize) -> crate::syntax::HighlightState {
        let editor = self.workspace.active();
        let editor_id = editor.id();
        let state_id = editor.document.state_id();
        let path = editor.document.path().map(Path::to_path_buf);
        let rope = editor.document.rope().clone();
        self.ui
            .highlight_index
            .state_before_line(editor_id, state_id, path.as_deref(), &rope, line)
    }

    fn take_background_redraw(&mut self, requested: bool) -> bool {
        if requested {
            self.ui.pending_background_redraw = true;
        }
        if !self.ui.pending_background_redraw {
            return false;
        }
        if self.ui.last_background_redraw.elapsed() < BACKGROUND_REDRAW_MIN_INTERVAL {
            return false;
        }
        self.ui.pending_background_redraw = false;
        self.ui.last_background_redraw = Instant::now();
        true
    }

    pub fn status_message(&self) -> Option<&str> {
        self.ui
            .status
            .as_ref()
            .map(|status| status.message.as_str())
    }

    pub fn status_is_error(&self) -> bool {
        self.ui.status.as_ref().is_some_and(|status| status.error)
    }

    pub(crate) fn edit_transition_cue_active(&self) -> bool {
        self.edit_transition_cue_active_at(Instant::now())
    }

    fn edit_transition_cue_active_at(&self, now: Instant) -> bool {
        matches!(self.ui.mode, UiMode::Edit)
            && !self.ui.keymap.is_active()
            && self
                .ui
                .edit_transition
                .cue_until
                .is_some_and(|deadline| now < deadline)
    }

    /// Expire the one-shot cue without driving continuous paints while it is visible.
    pub fn poll_ui_transients(&mut self) -> bool {
        self.poll_ui_transients_at(Instant::now())
    }

    fn poll_ui_transients_at(&mut self, now: Instant) -> bool {
        let expired = self
            .ui
            .edit_transition
            .cue_until
            .is_some_and(|deadline| now >= deadline);
        if expired {
            self.ui.edit_transition.cue_until = None;
        }
        expired
    }

    pub fn search_match(&self) -> Option<Range<usize>> {
        (self.ui.search_editor_id == Some(self.workspace.active().id()))
            .then(|| self.ui.search_match.clone())
            .flatten()
    }

    pub fn git_summary(&self) -> Option<String> {
        self.git.branch.as_ref().map(|branch| {
            if self.git.changes == 0 {
                branch.clone()
            } else {
                format!("{branch} +{}", self.git.changes)
            }
        })
    }

    pub fn lsp_summary(&self) -> Option<String> {
        let name = self.lsp.server_name.as_deref()?;
        let diagnostics = self.active_diagnostics();
        let errors = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
            .count();
        let warnings = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning)
            .count();
        let mut summary = if errors == 0 && warnings == 0 {
            name.to_owned()
        } else {
            format!("{name} E{errors} W{warnings}")
        };
        if self.lsp.documents.is_partial() || self.lsp.diagnostics.is_partial() {
            summary.push_str(" PARTIAL");
        }
        Some(summary)
    }

    pub fn diagnostic_highlights(&self) -> Vec<(Range<usize>, DiagnosticSeverity)> {
        let diagnostics = self.active_diagnostics();
        if diagnostics.is_empty() {
            return Vec::new();
        }
        let document = &self.workspace.active().document;
        let text_len = document.len_chars();
        // Only convert diagnostics that can affect the visible viewport. Full
        // project diagnostic sets must not re-map every range on each scroll
        // frame of a large buffer.
        let top = self.workspace.active().viewport.top_line;
        let visible = self.ui.screen_size.1 as usize;
        let bottom = top.saturating_add(visible.saturating_mul(2).max(64));
        let snapshot = DocumentSnapshot::from_rope(document.rope(), DocumentVersion::INITIAL);
        diagnostics
            .iter()
            .filter(|diagnostic| {
                let start_line = diagnostic.range.start.line.get();
                let end_line = diagnostic.range.end.line.get();
                end_line >= top && start_line <= bottom
            })
            .filter_map(|diagnostic| {
                let start = snapshot
                    .position_to_char(diagnostic.range.start)
                    .ok()?
                    .get();
                let end = snapshot.position_to_char(diagnostic.range.end).ok()?.get();
                (start <= end).then(|| {
                    let mut range = start..end;
                    if range.is_empty() {
                        if range.end < text_len {
                            range.end += 1;
                        } else if range.start > 0 {
                            range.start -= 1;
                        }
                    }
                    (range, diagnostic.severity)
                })
            })
            .collect()
    }

    fn active_diagnostics(&self) -> &[Diagnostic] {
        let Some(path) = self.workspace.active().document.path() else {
            return &[];
        };
        self.lsp
            .diagnostics
            .get(&file_uri_identity(path))
            .unwrap_or(&[])
    }

    fn current_line_diagnostic(&self) -> Option<&Diagnostic> {
        let line = self.workspace.active().position(self.config.tab_width).line;
        self.active_diagnostics().iter().find(|diagnostic| {
            diagnostic.range.start.line.get() <= line && diagnostic.range.end.line.get() >= line
        })
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        match &self.ui.mode {
            UiMode::Prompt(prompt) => Some(prompt),
            _ => None,
        }
    }

    pub fn overlay(&self) -> Option<OverlayView<'_>> {
        let prompt = self.prompt()?;
        let title = match prompt.kind {
            PromptFlow::QuickOpen => "QUICK OPEN",
            PromptFlow::OpenRecent => "OPEN RECENT",
            PromptFlow::WorkspaceTree => "WORKSPACE",
            PromptFlow::Buffers => "BUFFERS",
            PromptFlow::Palette => "COMMAND PALETTE",
            PromptFlow::Recovery => "RECOVERY JOURNALS",
            PromptFlow::GlobalSearch => "PROJECT SEARCH",
            PromptFlow::Tasks => "TASKS",
            PromptFlow::GitChanges => "GIT CHANGES",
            PromptFlow::GitDiffPicker => "GIT DIFFS",
            PromptFlow::GitCommitPicker => "GIT COMMITS",
            PromptFlow::Completion => "COMPLETION",
            PromptFlow::Locations => "LOCATIONS",
            PromptFlow::JumpList => "JUMP LIST",
            PromptFlow::Bookmarks => "BOOKMARKS",
            PromptFlow::Problems => "PROBLEMS",
            PromptFlow::DocumentSymbols => "DOCUMENT SYMBOLS",
            PromptFlow::LocalDefinitions => "LOCAL DEFINITIONS",
            PromptFlow::LocalReferences => "LOCAL REFERENCES",
            PromptFlow::SourceAnnotations => "SOURCE ANNOTATIONS",
            PromptFlow::WorkspaceSymbols => "WORKSPACE SYMBOLS",
            PromptFlow::WorkspaceOutline => "WORKSPACE OUTLINE",
            PromptFlow::Stickies => "STICKIES",
            _ => return None,
        };
        Some(OverlayView {
            title,
            items: &prompt.labels,
            selected: prompt.selected,
            notice: prompt.notice.as_deref(),
        })
    }

    pub fn is_help(&self) -> bool {
        matches!(self.ui.mode, UiMode::Help)
    }

    pub fn help_lines(&self) -> &'static [String] {
        keymap::action_layer_help_lines()
    }

    pub fn mode_label(&self) -> &'static str {
        match self.ui.mode {
            UiMode::Edit if self.sticky_pad.is_focused() => "STICKY",
            UiMode::Edit if self.ui.keymap.is_active() => "ACTION",
            UiMode::Edit if self.workspace.active().document.is_read_only() => "VIEW",
            UiMode::Edit if self.edit_transition_cue_active() => "EDIT*",
            UiMode::Edit => "EDIT",
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Search,
                ..
            }) => "FIND",
            UiMode::Prompt(Prompt {
                kind:
                    PromptFlow::WorkspaceSymbolQuery
                    | PromptFlow::WorkspaceSymbolPending
                    | PromptFlow::WorkspaceSymbols
                    | PromptFlow::LocalDefinitions
                    | PromptFlow::LocalReferences
                    | PromptFlow::SourceAnnotations
                    | PromptFlow::WorkspaceOutline,
                ..
            }) => "SYMBOLS",
            UiMode::Prompt(Prompt {
                kind: PromptFlow::WorkspaceTree,
                ..
            }) => "WORKSPACE",
            UiMode::Prompt(_) => "PROMPT",
            UiMode::Help => "HELP",
            UiMode::Confirm(_) => "CONFIRM",
            UiMode::TaskTrust(_) | UiMode::GitTrust(_) => "TRUST",
        }
    }

    pub fn footer_hint(&self) -> String {
        match self.ui.mode {
            UiMode::Edit if self.ui.keymap.is_active() => {
                keymap::action_hint(self.ui.keymap.state())
            }
            UiMode::Edit => {
                let base = self.current_line_diagnostic().map_or_else(
                    || " Esc actions   Esc h help   Esc c c complete   Esc c f format ".to_owned(),
                    |diagnostic| {
                        format!(
                            " {} {}   Esc c p all problems ",
                            diagnostic.severity.marker(),
                            diagnostic.message
                        )
                    },
                );
                format!("{base}{}", self.lsp_footer_suffix())
            }
            UiMode::Help => " Esc / Ctrl-G / Enter close help ".to_owned(),
            UiMode::Confirm(ConfirmKind::Quit) => {
                " Unsaved buffers — S save all   D discard & quit   Esc cancel ".to_owned()
            }
            UiMode::Confirm(ConfirmKind::CloseBuffer) => {
                " Unsaved buffer — S save   D discard & close   Esc cancel ".to_owned()
            }
            UiMode::Confirm(ConfirmKind::AgentDirtyTree { git_changes, .. }) => {
                format!(
                    " Dirty tree ({git_changes} path{}) — Y start agent anyway   Esc cancel · Esc v s Git ",
                    if git_changes == 1 { "" } else { "s" }
                )
            }
            UiMode::TaskTrust(ref name) => format!(
                " Task {name:?} may execute workspace code — V details   Y trust once & run   Esc cancel "
            ),
            UiMode::GitTrust(ref mutation) => format!(
                " Git {} may execute repository filters/hooks — V details   Y trust once & run   Esc cancel ",
                git_mutation_summary(mutation)
            ),
            UiMode::Prompt(_) => String::new(),
        }
    }

    pub fn prepare_viewport(&mut self, layout: Layout) {
        let manual_scroll = std::mem::take(&mut self.ui.viewport_scroll_pending);
        if self.ui.soft_wrap {
            let metrics = VisualMetrics::new(layout.content_width, self.config.tab_width, true);
            let result = if manual_scroll {
                let mut editor = self.workspace.active_mut();
                metrics
                    .normalize_anchor(
                        &editor.document,
                        VisualAnchor {
                            line: editor.viewport.top_line,
                            char_in_line: editor.viewport.top_wrap_char,
                        },
                    )
                    .map(|anchor| {
                        editor.viewport.top_line = anchor.line;
                        editor.viewport.top_wrap_char = anchor.char_in_line;
                    })
            } else {
                self.workspace.active_mut().ensure_wrapped_cursor_visible(
                    metrics,
                    layout.content_height,
                    self.config.scroll_margin,
                )
            };
            if let Err(error) = result {
                self.ui.soft_wrap = false;
                self.workspace.active_mut().viewport.top_wrap_char = 0;
                self.workspace.active_mut().ensure_cursor_visible(
                    layout.content_height,
                    layout.content_width,
                    self.config.tab_width,
                    self.config.scroll_margin,
                );
                self.error(format!("Soft wrap disabled: {error}"));
            }
        } else if manual_scroll {
            let max = self
                .workspace
                .active()
                .document
                .line_count()
                .saturating_sub(layout.content_height);
            self.workspace.active_mut().viewport.top_line =
                self.workspace.active().viewport.top_line.min(max);
        } else {
            self.workspace.active_mut().ensure_cursor_visible(
                layout.content_height,
                layout.content_width,
                self.config.tab_width,
                self.config.scroll_margin,
            );
        }
    }

    /// Persist dirty writable buffers without changing the visible status on
    /// success. This keeps idle recovery checkpoints silent over mosh.
    pub fn checkpoint_recovery(&mut self) -> bool {
        let Some(store) = self.persistence.recovery_store.clone() else {
            return false;
        };
        let live_ids: HashSet<u64> = self
            .workspace
            .buffers()
            .iter()
            .map(|editor| editor.id())
            .collect();
        let stale: Vec<_> = self
            .persistence
            .recovery_ids
            .iter()
            .filter(|(editor_id, _)| !live_ids.contains(editor_id))
            .map(|(editor_id, record_id)| (*editor_id, record_id.clone()))
            .collect();
        for (editor_id, record_id) in stale {
            let _ = store.remove(&record_id);
            self.persistence.recovery_ids.remove(&editor_id);
            self.persistence
                .recovery_checkpoint_state
                .remove(&editor_id);
        }

        let snapshots: Vec<_> = self
            .workspace
            .buffers()
            .iter()
            .filter(|editor| editor.document.is_modified() && !editor.document.is_read_only())
            .filter_map(|editor| {
                let stamp = RecoveryCheckpointStamp {
                    path: editor.document.path().map(Path::to_path_buf),
                    revision: editor.document.state_id(),
                    saved_revision: editor.document.saved_state_id(),
                    cursor: editor.cursor,
                    anchor: editor.anchor,
                };
                (!self.persistence.recovery_ids.contains_key(&editor.id())
                    || self.persistence.recovery_checkpoint_state.get(&editor.id()) != Some(&stamp))
                .then(|| (editor.id(), editor.document.text(), stamp))
            })
            .collect();

        for (editor_id, text, stamp) in snapshots {
            let mut record = RecoveryRecord::new(
                self.workspace.root.clone(),
                stamp.path.clone(),
                text,
                stamp.cursor,
                stamp.anchor,
                stamp.revision,
                stamp.saved_revision,
            );
            if let Some(existing) = self.persistence.recovery_ids.get(&editor_id) {
                record.id.clone_from(existing);
            }
            match store.write(&record) {
                Ok(_) => {
                    self.persistence.recovery_ids.insert(editor_id, record.id);
                    self.persistence
                        .recovery_checkpoint_state
                        .insert(editor_id, stamp);
                }
                Err(error) => {
                    self.error(format!("Recovery checkpoint failed: {error}"));
                    return true;
                }
            }
        }

        let clean: Vec<_> = self
            .workspace
            .buffers()
            .iter()
            .filter(|editor| !editor.document.is_modified())
            .filter_map(|editor| {
                self.persistence
                    .recovery_ids
                    .get(&editor.id())
                    .map(|record_id| (editor.id(), record_id.clone()))
            })
            .collect();
        for (editor_id, record_id) in clean {
            let _ = store.remove(&record_id);
            self.persistence.recovery_ids.remove(&editor_id);
            self.persistence
                .recovery_checkpoint_state
                .remove(&editor_id);
        }
        false
    }

    /// Drain background IDE service results. Stale project-search generations
    /// are filtered by the worker before they reach the active overlay.
    pub fn poll_services(&mut self) -> bool {
        let mut redraw = false;
        redraw |= self.poll_agent_events();
        while let Ok(event) = self.services.try_recv() {
            if self.services.is_current(&event) {
                redraw |= self.handle_service_event(event);
            }
        }
        let mut newest = None;
        let mut search_disconnected = false;
        if let Some(worker) = &self.project.search_worker {
            loop {
                match worker.try_recv_latest() {
                    Ok(result) => newest = Some(result),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        search_disconnected = true;
                        break;
                    }
                }
            }
        }
        if search_disconnected {
            self.error("Project search worker stopped");
            redraw = true;
        }
        if let Some(result) = newest
            && let UiMode::Prompt(prompt) = &mut self.ui.mode
            && prompt.kind == PromptFlow::GlobalSearch
        {
            prompt.labels = result
                .matches
                .iter()
                .map(|matched| {
                    format!(
                        "{}:{}:{}  {}",
                        matched.path.display(),
                        matched.line + 1,
                        matched.char_column + 1,
                        matched.preview.trim()
                    )
                })
                .collect();
            prompt.entries = result
                .matches
                .into_iter()
                .map(PromptEntry::Search)
                .collect();
            prompt.notice = result
                .truncated
                .then(|| "Partial results: project scan safety limit reached".to_owned());
            prompt.selected = prompt.selected.min(prompt.labels.len().saturating_sub(1));
            redraw = true;
        }

        let task_batch = self.tasks.handle.as_ref().map(|handle| {
            (
                handle.task_name().to_owned(),
                handle.drain_events(),
                handle.state(),
            )
        });
        if let Some((name, mut events, state)) = task_batch {
            events.sort_by_key(|event| event.sequence);
            let mut output_changed = false;
            for event in events {
                match event.kind {
                    TaskEventKind::Output { stream, bytes } => {
                        self.append_task_output_chunk(stream, &bytes);
                        output_changed = true;
                    }
                    TaskEventKind::OutputDropped { bytes } => {
                        self.tasks.output_decoder.discard_pending_on_gap();
                        self.tasks
                            .output
                            .push_str(&format!("\n[… {bytes} output bytes dropped …]\n"));
                        output_changed = true;
                    }
                    TaskEventKind::PipeReadFailed { stream, message } => {
                        let pending = self.tasks.output_decoder.finish(stream);
                        self.tasks.output.push_str(&pending);
                        self.tasks
                            .output
                            .push_str(&format!("\n[{stream:?} read failed: {message}]\n"));
                        output_changed = true;
                    }
                    TaskEventKind::Exited(exit) => {
                        self.finish_task_output_decoders();
                        self.tasks.output.push_str(&format!(
                            "\n[task exited: {}{}]\n",
                            exit.code()
                                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                            if exit.cancel_requested {
                                ", cancelled"
                            } else {
                                ""
                            }
                        ));
                        self.status(format!(
                            "Task {name:?} {}",
                            if exit.success() { "finished" } else { "failed" }
                        ));
                        output_changed = true;
                    }
                    TaskEventKind::MonitorFailed { message, .. } => {
                        self.finish_task_output_decoders();
                        self.error(format!("Task {name:?} monitor failed: {message}"));
                        output_changed = true;
                    }
                }
            }
            if output_changed {
                trim_task_output(&mut self.tasks.output);
                self.workspace.update_virtual(
                    "Task Output",
                    &self.tasks.output,
                    !state.is_finished(),
                );
                redraw = true;
            }
        }
        let lsp_redraw = self.poll_lsp();
        // Throttle continuous background paints; one-shot UI transitions from
        // search/task still return true above without this gate.
        redraw |= self.take_background_redraw(lsp_redraw);
        redraw |= self.observe_edit_transition_at(Instant::now());
        redraw
    }

    fn handle_service_event(&mut self, event: ServiceEvent) -> bool {
        match event {
            ServiceEvent::Project { result, .. } => match result {
                Ok(index) => {
                    let refreshed = self.project.index.is_some();
                    let text_files = index.len();
                    let tree_entries = index.tree_entries().len();
                    let text_partial = index.is_truncated();
                    let tree_partial = index.is_tree_truncated();
                    self.project.install_index(index, &self.workspace);
                    self.prune_workspace_tree_state();
                    let suffix = match (text_partial, tree_partial) {
                        (false, false) => String::new(),
                        (true, false) => " · quick-open/search partial".to_owned(),
                        (false, true) => " · explorer partial".to_owned(),
                        (true, true) => " · search and explorer partial".to_owned(),
                    };
                    self.status(if refreshed {
                        format!(
                            "Workspace snapshot refreshed: {text_files} text files · {tree_entries} tree entries{suffix}"
                        )
                    } else {
                        format!(
                            "Workspace indexed: {text_files} text files · {tree_entries} tree entries{suffix}"
                        )
                    });
                    if matches!(
                        self.ui.mode,
                        UiMode::Prompt(Prompt {
                            kind: PromptFlow::QuickOpen | PromptFlow::WorkspaceTree,
                            ..
                        })
                    ) {
                        self.refresh_prompt_candidates();
                    }
                }
                Err(error) => {
                    self.project.status = ServiceStatus::Failed(error.clone());
                    if self.project.index.is_some() {
                        self.error(format!(
                            "Workspace refresh failed; retained the previous snapshots: {error}"
                        ));
                    } else {
                        self.project.search_worker = None;
                        self.error(format!("Project index unavailable: {error}"));
                    }
                }
            },
            ServiceEvent::Git { result, .. } => match result {
                Ok(snapshot) => self.git.install(snapshot),
                Err(error) => {
                    self.git.fail(error.clone());
                    self.error(format!("Git status unavailable: {error}"));
                }
            },
            ServiceEvent::GitMutation { tag, result } => {
                let _ = self.services.finish_git_mutation(tag.generation);
                let requested = self.git.pending.take().and_then(|pending| {
                    (pending.generation == tag.generation).then_some(pending.mutation)
                });
                if let Some(repository) = self.git.repository.as_ref() {
                    let generation = self.services.start_git(repository.root().to_path_buf());
                    self.git.status = ServiceStatus::Pending(generation);
                }
                match result {
                    Ok(result) => self.status(format!(
                        "{}; refreshing Git status",
                        git_mutation_result_summary(&result)
                    )),
                    Err(error) => self.error(format!(
                        "Git {} failed: {error}; refreshing Git status",
                        requested
                            .as_ref()
                            .map(git_mutation_summary)
                            .unwrap_or_else(|| "operation".to_owned())
                    )),
                }
            }
            ServiceEvent::Recovery { mut snapshot, .. } => {
                self.persistence.install_recovery(&mut snapshot);
                if let Some(notice) = snapshot.notice {
                    self.error(notice);
                } else if let Some(warning) = snapshot.listing.warnings.first() {
                    self.error(format!(
                        "Recovery scan skipped {} journal{}: {warning}",
                        snapshot.listing.warnings.len(),
                        if snapshot.listing.warnings.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ));
                } else if !self.persistence.recovery_records.is_empty() {
                    self.status(format!(
                        "{} recovery journal{} available — Esc w r",
                        self.persistence.recovery_records.len(),
                        if self.persistence.recovery_records.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ));
                }
            }
        }
        true
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Resize(width, height) => {
                self.set_screen_size((width, height));
                self.ui.full_redraw = true;
            }
            Event::Paste(text) => self.handle_paste(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Mouse(mouse) if self.config.mouse => self.handle_mouse(mouse),
            _ => {}
        }
        self.observe_edit_transition_at(Instant::now());
    }

    fn handle_paste(&mut self, text: &str) {
        let prompt_was_empty = matches!(
            &self.ui.mode,
            UiMode::Prompt(prompt) if prompt.input.is_empty()
        );
        let workspace_tree_was_empty = matches!(
            &self.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind == PromptFlow::WorkspaceTree && prompt.input.is_empty()
        );
        let workspace_tree_resume_candidate = workspace_tree_was_empty
            .then(|| self.selected_workspace_tree_node().map(|node| node.path))
            .flatten();
        let mut refused_limit = None;
        let mut prompt_input_changed = false;
        match &mut self.ui.mode {
            UiMode::Edit if !self.ui.keymap.is_active() => {
                let result = self.workspace.active_mut().insert(text, EditKind::Paste);
                if let Err(error) = result {
                    self.error(error.to_string());
                }
            }
            UiMode::Prompt(prompt) if prompt.kind != PromptFlow::WorkspaceSymbolPending => {
                let (inserted, limit) = if let Some((label, max_bytes)) = prompt.kind.input_limit()
                {
                    (
                        prompt.insert_bounded(text, max_bytes),
                        Some((label, max_bytes)),
                    )
                } else {
                    prompt.insert(text);
                    (true, None)
                };
                if inserted {
                    prompt_input_changed = !text.is_empty();
                } else {
                    refused_limit = limit;
                }
            }
            _ => {}
        }
        if let Some((label, limit)) = refused_limit {
            self.error(format!("{label} input is limited to {limit} UTF-8 bytes"));
            return;
        }
        if prompt_input_changed {
            if workspace_tree_was_empty {
                self.project.tree_resume_path = workspace_tree_resume_candidate;
            }
            self.prompt_changed();
            if prompt_was_empty
                && let UiMode::Prompt(prompt) = &mut self.ui.mode
                && !prompt.input.is_empty()
            {
                prompt.selected = 0;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if is_control_char(key, 'g') {
            let action_was_active = self.ui.keymap.is_active();
            self.cancel_transient();
            if action_was_active {
                self.arm_edit_transition();
            }
            return;
        }
        if is_control_char(key, 'l') {
            self.ui.full_redraw = true;
            self.status("Full redraw requested");
            return;
        }
        match self.ui.mode.clone() {
            UiMode::Prompt(_) => self.handle_prompt_key(key),
            UiMode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.dismiss_help();
                }
            }
            UiMode::Confirm(kind) => self.handle_confirm_key(kind, key),
            UiMode::TaskTrust(name) => self.handle_task_trust_key(name, key),
            UiMode::GitTrust(mutation) => self.handle_git_trust_key(mutation, key),
            UiMode::Edit => {
                if self.sticky_pad.is_focused() {
                    self.handle_sticky_pad_key(key);
                    return;
                }
                // Needs You: Y allow / N deny for ACP tool permission (dashboard).
                if self.agent.pending_permission.is_some()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.answer_agent_permission(true);
                            return;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            self.answer_agent_permission(false);
                            return;
                        }
                        _ => {}
                    }
                }
                let before = self.active_editor_intent();
                self.handle_edit_key(key);
                if self.active_editor_intent() != before {
                    self.cancel_pending_lsp_ui_requests();
                }
            }
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        // Legacy terminals encode Alt+key as Escape followed by key. Crossterm
        // may therefore report a quickly typed `Esc s` as one Alt+s event.
        // Treat that representation as the same no-timeout Action sequence so
        // mosh batching cannot make the primary command layer unreliable.
        if !self.ui.keymap.is_active()
            && legacy_escape_modifiers(key.modifiers)
            && let Some(action_key) = legacy_escape_action_key(key.code)
        {
            let _ = self.ui.keymap.feed(Key::Escape);
            if let Some(resolution) = self.ui.keymap.feed(action_key) {
                self.apply_action_resolution(resolution);
            }
            return;
        }

        if let Some(normalized) = normalize_action_key(key, self.ui.keymap.is_active()) {
            if let Some(resolution) = self.ui.keymap.feed(normalized) {
                self.apply_action_resolution(resolution);
                return;
            }
        } else if self.ui.keymap.is_active() {
            self.arm_edit_transition();
            self.ui.keymap.cancel();
            self.error("That key is not available in the Action layer");
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let character = match key.code {
                KeyCode::Char(character) => character.to_ascii_lowercase(),
                _ => return,
            };
            match character {
                's' => self.execute_action(Action::Save),
                'q' => self.execute_action(Action::Quit),
                'f' => self.execute_action(Action::Find),
                'o' | 'p' => self.execute_action(Action::QuickOpen),
                'z' if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.execute_action(Action::Redo)
                }
                'z' => self.execute_action(Action::Undo),
                'y' => self.execute_action(Action::Redo),
                'c' => self.execute_action(Action::Yank),
                'x' => self.execute_action(Action::Cut),
                'v' => self.execute_action(Action::Paste),
                'a' => self.execute_action(Action::SelectAll),
                't' => {
                    let message = {
                        let mut editor = self.workspace.active_mut();
                        if editor.anchor.is_some() {
                            editor.clear_selection();
                            "Selection anchor cleared"
                        } else {
                            editor.anchor = Some(editor.cursor);
                            "Selection anchor set; use Shift+arrows"
                        }
                    };
                    self.status(message);
                }
                _ => {}
            }
            return;
        }

        let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
        let word_motion = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        let page_height = self.page_height();
        let visual_delta = match key.code {
            KeyCode::Up => Some(-1),
            KeyCode::Down => Some(1),
            KeyCode::PageUp => Some(-page_height),
            KeyCode::PageDown => Some(page_height),
            _ => None,
        };
        if let Some(delta) = visual_delta {
            match self.move_screen_vertical(delta, selecting) {
                Ok(()) => self.ui.status = None,
                Err(error) => self.error(error),
            }
            return;
        }
        let mut editor = self.workspace.active_mut();
        let result = match key.code {
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
            {
                editor.insert(&character.to_string(), EditKind::Insert)
            }
            KeyCode::Enter => editor.insert_newline_with_indent(self.config.tab_width),
            KeyCode::Tab => editor.insert_tab(self.config.tab_width, self.config.insert_spaces),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete_forward(),
            KeyCode::Left if word_motion => {
                editor.move_word_left(selecting);
                Ok(())
            }
            KeyCode::Right if word_motion => {
                editor.move_word_right(selecting);
                Ok(())
            }
            KeyCode::Left => {
                editor.move_left(selecting);
                Ok(())
            }
            KeyCode::Right => {
                editor.move_right(selecting);
                Ok(())
            }
            KeyCode::Home => {
                editor.move_home(selecting);
                Ok(())
            }
            KeyCode::End => {
                editor.move_end(selecting);
                Ok(())
            }
            _ => Ok(()),
        };
        drop(editor);
        if let Err(error) = result {
            self.error(error.to_string());
        } else {
            self.ui.status = None;
        }
    }

    fn apply_action_resolution(&mut self, resolution: Resolution) {
        match resolution {
            Resolution::Pending(_) => self.ui.status = None,
            Resolution::Cancel => {
                self.arm_edit_transition();
                self.ui.status = None;
            }
            Resolution::Command(action) => {
                // Arm before execution so an Action command that edits (undo,
                // duplicate, delete, paste…) is itself eligible for the cue.
                self.arm_edit_transition();
                self.execute_action(action);
            }
            Resolution::Unknown(unknown) => {
                self.arm_edit_transition();
                self.error(unknown.message());
            }
        }
    }

    fn active_document_state_stamp(&self) -> DocumentStateStamp {
        let editor = self.workspace.active();
        DocumentStateStamp {
            editor_id: editor.id(),
            state_id: editor.document.state_id(),
        }
    }

    fn arm_edit_transition(&mut self) {
        self.ui.edit_transition.armed = Some(self.active_document_state_stamp());
    }

    /// Revision-driven rather than key-driven: paste, undo, LSP formatting and
    /// every other real mutation share the same first-edit acknowledgement.
    fn observe_edit_transition_at(&mut self, now: Instant) -> bool {
        let Some(armed) = self.ui.edit_transition.armed else {
            return false;
        };
        let current = self.active_document_state_stamp();
        if current.editor_id != armed.editor_id {
            // Action may open a picker and switch buffers before editing. A
            // switch is not an edit; follow the new active document instead.
            self.ui.edit_transition.armed = Some(current);
            return false;
        }
        if self.workspace.active().document.is_read_only() {
            // Live virtual views can replace their snapshot in the background;
            // that is not an edit and must not consume the user's next cue.
            self.ui.edit_transition.armed = Some(current);
            return false;
        }
        if current.state_id == armed.state_id {
            return false;
        }
        self.ui.edit_transition.armed = None;
        self.ui.edit_transition.cue_until = Some(now + EDIT_TRANSITION_CUE_DURATION);
        true
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc) {
            self.cancel_prompt();
            return;
        }
        if self.prompt().is_some_and(|prompt| {
            prompt.kind.completion_behavior() == PromptCompletionBehavior::Blocked
        }) {
            return;
        }
        let workspace_tree_hierarchy = matches!(
            &self.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind.completion_behavior() == PromptCompletionBehavior::WorkspaceTree
                    && prompt.input.is_empty()
        );
        if workspace_tree_hierarchy {
            match key.code {
                KeyCode::Right => {
                    self.expand_selected_workspace_tree_directory();
                    return;
                }
                KeyCode::Left => {
                    self.collapse_selected_workspace_tree_directory();
                    return;
                }
                KeyCode::Enter => {
                    self.activate_selected_workspace_tree_node();
                    return;
                }
                _ => {}
            }
        }
        if matches!(key.code, KeyCode::Enter) {
            self.commit_prompt();
            return;
        }
        if matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Recovery,
                ..
            })
        ) {
            match key.code {
                KeyCode::Char('V') => {
                    self.view_selected_recovery();
                    return;
                }
                KeyCode::Char('D') => {
                    self.discard_selected_recovery();
                    return;
                }
                _ => {}
            }
        }
        if matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Stickies,
                ..
            })
        ) {
            match key.code {
                KeyCode::Char('A') => {
                    self.archive_selected_sticky();
                    return;
                }
                KeyCode::Char('X') => {
                    self.delete_selected_sticky();
                    return;
                }
                _ => {}
            }
        }

        let workspace_tree_was_empty = matches!(
            &self.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind == PromptFlow::WorkspaceTree && prompt.input.is_empty()
        );
        let workspace_tree_resume_candidate = workspace_tree_was_empty
            .then(|| self.selected_workspace_tree_node().map(|node| node.path))
            .flatten();
        let Some(prompt) = (match &mut self.ui.mode {
            UiMode::Prompt(prompt) => Some(prompt),
            _ => None,
        }) else {
            return;
        };
        let prompt_kind = prompt.kind;
        let input_bytes_before = prompt.input.len();
        let mut refused_limit = None;
        match key.code {
            KeyCode::Up => prompt.selected = prompt.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                if !prompt.labels.is_empty() {
                    prompt.selected = (prompt.selected + 1).min(prompt.labels.len() - 1);
                }
            }
            KeyCode::BackTab => {
                prompt.selected = prompt.selected.saturating_sub(1);
            }
            KeyCode::Left => prompt.move_left(),
            KeyCode::Right => prompt.move_right(),
            KeyCode::Home => prompt.cursor = 0,
            KeyCode::End => prompt.cursor = prompt.input.chars().count(),
            KeyCode::Backspace => prompt.backspace(),
            KeyCode::Delete => prompt.delete_forward(),
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                let character = character.to_string();
                if let Some((label, max_bytes)) = prompt.kind.input_limit() {
                    if !prompt.insert_bounded(&character, max_bytes) {
                        refused_limit = Some((label, max_bytes));
                    }
                } else {
                    prompt.insert(&character);
                }
            }
            _ => return,
        }
        let input_changed = prompt.input.len() != input_bytes_before;
        let query_started = input_bytes_before == 0 && !prompt.input.is_empty();
        let workspace_tree_is_now_empty =
            prompt_kind == PromptFlow::WorkspaceTree && prompt.input.is_empty();
        if let Some((label, limit)) = refused_limit {
            self.error(format!("{label} input is limited to {limit} UTF-8 bytes"));
            return;
        }
        if !input_changed {
            return;
        }
        if prompt_kind == PromptFlow::WorkspaceTree && workspace_tree_was_empty {
            self.project.tree_resume_path = workspace_tree_resume_candidate;
        }
        self.prompt_changed();
        if query_started && let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.selected = 0;
        }
        if prompt_kind == PromptFlow::WorkspaceTree && workspace_tree_is_now_empty {
            let resume = self.project.tree_resume_path.clone();
            if let Some(path) = resume {
                self.select_workspace_tree_path(&path);
            }
        }
    }

    fn handle_confirm_key(&mut self, kind: ConfirmKind, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.ui.mode = UiMode::Edit,
            KeyCode::Char('d' | 'D') => match kind {
                ConfirmKind::Quit => {
                    self.discard_all_recovery();
                    self.ui.should_quit = true;
                }
                ConfirmKind::CloseBuffer => {
                    self.remove_active_recovery();
                    let _ = self.close_active_buffer(true);
                    self.ui.mode = UiMode::Edit;
                }
                ConfirmKind::AgentDirtyTree { .. } => {}
            },
            KeyCode::Char('s' | 'S') => match kind {
                ConfirmKind::Quit => {
                    self.ui.quit_after_save = true;
                    self.save_all();
                }
                ConfirmKind::CloseBuffer => {
                    self.ui.close_after_save_as = self.workspace.active().document.path().is_none();
                    self.save_current();
                    if !self.workspace.active().document.is_modified() {
                        let _ = self.close_active_buffer(false);
                        self.ui.mode = UiMode::Edit;
                    }
                }
                ConfirmKind::AgentDirtyTree { .. } => {}
            },
            KeyCode::Char('y' | 'Y') => {
                if let ConfirmKind::AgentDirtyTree { goal, git_changes } = kind {
                    self.ui.mode = UiMode::Edit;
                    self.status(format!(
                        "starting agent on dirty tree ({git_changes} Git path{})",
                        if git_changes == 1 { "" } else { "s" }
                    ));
                    self.start_agent_run_unchecked(goal);
                }
            }
            _ => {}
        }
    }

    fn handle_task_trust_key(&mut self, name: String, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.ui.mode = UiMode::Edit,
            KeyCode::Char('v' | 'V') => {
                self.ui.mode = UiMode::Edit;
                self.open_task_details(&name);
            }
            KeyCode::Char('y' | 'Y') => self.start_task(&name),
            _ => {}
        }
    }

    fn handle_git_trust_key(&mut self, mutation: GitMutation, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.ui.mode = UiMode::Edit,
            KeyCode::Char('v' | 'V') => {
                self.ui.mode = UiMode::Edit;
                self.open_git_mutation_details(&mutation);
            }
            KeyCode::Char('y' | 'Y') => self.start_git_mutation(mutation),
            _ => {}
        }
    }

    fn execute_action(&mut self, action: Action) {
        // A new explicit user action supersedes any unanswered LSP UI intent.
        // Removing the app correlation prevents a late response from opening
        // a picker/view or navigating after the user has moved on; the
        // protocol client still retains the request until its terminal reply.
        self.cancel_pending_lsp_ui_requests();
        self.ui.keymap.cancel();
        match action {
            Action::Save => self.save_current(),
            Action::SaveAll => self.save_all(),
            Action::Quit => self.request_quit(),
            Action::QuickOpen if self.project.status.is_pending() => {
                self.status("Workspace is still indexing; Quick Open will be available shortly")
            }
            Action::QuickOpen => self.begin_prompt(PromptFlow::QuickOpen),
            Action::OpenPath => self.begin_prompt(PromptFlow::OpenPath),
            Action::WorkspaceTree if self.project.status.is_pending() => {
                self.status("Workspace is still indexing; explorer is pending")
            }
            Action::WorkspaceTree => self.open_workspace_tree(),
            Action::WorkspaceRefresh => self.refresh_workspace_snapshots(),
            Action::BufferSwitcher => self.begin_prompt(PromptFlow::Buffers),
            Action::PreviousBuffer => self.workspace.previous_buffer(),
            Action::NextBuffer => self.workspace.next_buffer(),
            Action::CloseBuffer => {
                if self.workspace.active().document.is_modified() {
                    self.ui.mode = UiMode::Confirm(ConfirmKind::CloseBuffer);
                } else {
                    let _ = self.close_active_buffer(false);
                }
            }
            Action::CloseOtherBuffers => self.close_other_buffers(),
            Action::ReopenClosedBuffer => self.reopen_closed_buffer(),
            Action::Find => self.begin_prompt(PromptFlow::Search),
            Action::Replace => self.begin_replace(),
            Action::NextMatch => self.next_match(false),
            Action::PreviousMatch => self.next_match(true),
            Action::Undo => self.workspace.active_mut().undo(),
            Action::Redo => self.workspace.active_mut().redo(),
            Action::DuplicateLine => self.duplicate_lines(),
            Action::DeleteLine => self.delete_lines(),
            Action::MoveLinesUp => self.move_lines(true),
            Action::MoveLinesDown => self.move_lines(false),
            Action::IndentLines => self.indent_lines(true),
            Action::OutdentLines => self.indent_lines(false),
            Action::Yank => self.yank(false),
            Action::Cut => self.yank(true),
            Action::Paste => {
                let text = self.ui.clipboard.register().to_owned();
                if text.is_empty() {
                    self.status("Internal register is empty");
                } else {
                    let result = self.workspace.active_mut().insert(&text, EditKind::Paste);
                    if let Err(error) = result {
                        self.error(error.to_string());
                    }
                }
            }
            Action::SelectLines => self.select_lines(),
            Action::SelectAll => self.workspace.active_mut().select_all(),
            Action::ToggleSoftWrap => {
                self.ui.soft_wrap = !self.ui.soft_wrap;
                {
                    let mut editor = self.workspace.active_mut();
                    editor.reset_vertical_goal();
                    if self.ui.soft_wrap {
                        editor.viewport.top_wrap_char = 0;
                    }
                }
                self.ui.full_redraw = true;
                self.status(if self.ui.soft_wrap {
                    "Soft wrap on"
                } else {
                    "Soft wrap off"
                });
            }
            Action::ToggleLineNumbers => self.set_line_numbers(!self.config.line_numbers),
            Action::PreviousWord => self.workspace.active_mut().move_word_left(false),
            Action::NextWord => self.workspace.active_mut().move_word_right(false),
            Action::PreviousViewport => {
                let amount = -self.page_height();
                if let Err(error) = self.move_screen_vertical(amount, false) {
                    self.error(error);
                }
            }
            Action::NextViewport => {
                let amount = self.page_height();
                if let Err(error) = self.move_screen_vertical(amount, false) {
                    self.error(error);
                }
            }
            Action::JumpBack => self.navigate_history(false),
            Action::JumpForward => self.navigate_history(true),
            Action::JumpList => self.begin_prompt(PromptFlow::JumpList),
            Action::ToggleBookmark => self.toggle_bookmark(),
            Action::Bookmarks => self.open_bookmarks(),
            Action::PreviousBookmark => self.navigate_bookmark(false),
            Action::NextBookmark => self.navigate_bookmark(true),
            Action::GoToLine => self.begin_prompt(PromptFlow::GoToLine),
            Action::FileTop => {
                let origin = self.current_jump_location();
                self.workspace.active_mut().set_cursor(0, false);
                self.record_jump_origin(origin);
            }
            Action::FileBottom => {
                let origin = self.current_jump_location();
                let end = self.workspace.active().document.len_chars();
                self.workspace.active_mut().set_cursor(end, false);
                self.record_jump_origin(origin);
            }
            Action::MatchingBracket => self.go_to_matching_bracket(),
            Action::Help => {
                self.ui.mode = UiMode::Help;
            }
            Action::KeymapReference => self.open_keymap_reference(),
            Action::CommandPalette => self.begin_prompt(PromptFlow::Palette),
            Action::CommandLine => self.begin_prompt(PromptFlow::Command),
            Action::NewFile => self.begin_prompt(PromptFlow::NewFilePath),
            Action::RenameFile => self.begin_rename_file(),
            Action::SaveCopyAs => self.begin_save_copy_as(),
            Action::WorkspaceSidebar if self.project.status.is_pending() => {
                self.status("Workspace is still indexing; sidebar is pending")
            }
            Action::WorkspaceSidebar => self.toggle_workspace_sidebar(),
            Action::WorkspaceInfo => self.open_workspace_info(),
            Action::BufferInfo => self.open_buffer_info(),
            Action::DirtyBuffers => self.open_dirty_buffers(),
            Action::PreviousDirtyBuffer => self.navigate_dirty_buffer(false),
            Action::NextDirtyBuffer => self.navigate_dirty_buffer(true),
            Action::RecentFiles => self.open_recent_files(),
            Action::OpenRecentFile => self.begin_prompt(PromptFlow::OpenRecent),
            Action::Recovery if self.persistence.recovery_status.is_pending() => {
                self.status("Recovery journals are still being scanned")
            }
            Action::Recovery => self.begin_prompt(PromptFlow::Recovery),
            Action::Stickies => self.open_stickies(),
            Action::NewSticky => self.create_new_sticky(),
            Action::AgentRun => self.begin_agent_run_prompt(),
            Action::AgentApprove => self.answer_agent_permission(true),
            Action::AgentReviewHandoff => self.handoff_agent_review(None),
            Action::AgentCancel => self.cancel_agent_run(),
            Action::AgentDashboard => self.toggle_agent_dashboard(),
            Action::GlobalSearch if self.project.status.is_pending() => {
                self.status("Workspace is still indexing; project search will be available shortly")
            }
            Action::GlobalSearch => self.begin_prompt(PromptFlow::GlobalSearch),
            Action::Completion => self.request_lsp_completion(),
            Action::Definition => self.request_lsp_definition(),
            Action::Hover => self.request_lsp_hover(),
            Action::Problems => self.open_problems(),
            Action::DocumentSymbols => self.request_lsp_document_symbols(),
            Action::WorkspaceSymbols => self.begin_workspace_symbol_query(),
            Action::WorkspaceOutline if self.project.status.is_pending() => {
                self.status("Workspace is still indexing; outline is pending")
            }
            Action::WorkspaceOutline => self.open_workspace_outline(),
            Action::SourceAnnotations if self.project.status.is_pending() => {
                self.status("Workspace is still indexing; annotations are pending")
            }
            Action::SourceAnnotations => self.open_source_annotations(),
            Action::References => self.request_lsp_references(),
            Action::NextSymbolOccurrence => self.navigate_local_symbol_occurrence(true),
            Action::PreviousSymbolOccurrence => self.navigate_local_symbol_occurrence(false),
            Action::ToggleLineComment => self.toggle_line_comment(),
            Action::CopyLocation => self.copy_active_location(),
            Action::CopyProblem => self.copy_current_problem(),
            Action::LspLog => self.open_lsp_log(),
            Action::LspRestart => self.restart_lsp(),
            Action::Format => self.request_lsp_formatting(),
            Action::Terminal => {
                self.ui.terminal_requested = true;
                self.status("Opening workspace shell; exit returns to wscrpt");
            }
            Action::TaskOutput => self.open_task_output(),
            Action::RunDefaultTask => self.request_default_task(),
            Action::NextProblem => self.navigate_problem(true, false),
            Action::PreviousProblem => self.navigate_problem(false, false),
            Action::NextError => self.navigate_problem(true, true),
            Action::PreviousError => self.navigate_problem(false, true),
            Action::TaskCatalog => self.open_task_catalog(),
            Action::TaskPicker => self.begin_prompt(PromptFlow::Tasks),
            Action::RerunLastTask => {
                if let Some(name) = self.tasks.last.clone() {
                    self.request_task(name);
                } else {
                    self.status("No previous task");
                }
            }
            Action::StopTask => self.stop_task(),
            Action::VersionControlStatus => self.open_git_status(),
            Action::GitChanges => self.begin_git_changes(),
            Action::CurrentFileStatus => self.open_current_file_status(),
            Action::CurrentDiff => self.open_current_diff(),
            Action::GitDiffPicker => self.begin_git_diff_picker(),
            Action::GitLog => self.open_git_log(),
            Action::GitCommitPicker => self.begin_git_commit_picker(),
            Action::GitFileHistory => self.open_git_file_history(),
            Action::GitHead => self.open_git_head(),
            Action::GitBlameLine => self.open_git_blame_line(),
            Action::Branches => self.open_branch_view(),
            Action::GitStageCurrent => self.request_git_path_mutation(true),
            Action::GitUnstageCurrent => self.request_git_path_mutation(false),
            Action::GitCommitStaged => self.request_git_commit(),
        }
    }

    fn begin_prompt(&mut self, kind: PromptFlow) {
        self.ui.keymap.cancel();
        self.ui.status = None;
        let editor = self.workspace.active();
        let input = match kind {
            PromptFlow::Search => self.ui.search_query.clone().unwrap_or_default(),
            PromptFlow::SaveAs => editor
                .document
                .path()
                .and_then(|path| path.strip_prefix(&self.workspace.root).ok())
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            PromptFlow::RenameFilePath => editor
                .document
                .path()
                .and_then(|path| path.strip_prefix(&self.workspace.root).ok())
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            PromptFlow::SaveCopyAsPath => editor
                .document
                .path()
                .and_then(|path| path.strip_prefix(&self.workspace.root).ok())
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        self.ui.mode = UiMode::Prompt(Prompt::new(kind, input, editor.cursor, editor.anchor));
        self.refresh_prompt_candidates();
        if kind == PromptFlow::Search {
            self.update_incremental_search();
        } else if kind == PromptFlow::GlobalSearch {
            self.submit_project_search();
        }
    }

    fn open_workspace_tree(&mut self) {
        // Snapshot semantics stay explicit: never silently rebuild here.
        // When the workspace root mtime is newer than the last rebuild, surface
        // a non-blocking notice so Esc w R remains the intentional refresh path.
        if self.workspace_snapshot_looks_stale() {
            self.status(
                "Workspace root changed on disk since last snapshot — Esc w R refreshes explorer/search",
            );
        }
        let active_path = self.workspace_tree_active_path();
        let active_ancestors = active_path.as_ref().map(|path| {
            path.ancestors()
                .skip(1)
                .filter(|ancestor| !ancestor.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        });
        if let Some(ancestors) = active_ancestors {
            let missing = ancestors
                .iter()
                .filter(|path| !self.project.tree_expanded.contains(*path))
                .count();
            if self.project.tree_expanded.len().saturating_add(missing)
                > MAX_WORKSPACE_TREE_EXPANDED_DIRECTORIES
            {
                self.project.tree_expanded.clear();
            }
            self.project.tree_expanded.extend(ancestors);
        }
        self.begin_prompt(PromptFlow::WorkspaceTree);
        self.project.tree_resume_path = active_path.clone();
        if let Some(path) = active_path {
            self.select_workspace_tree_path(&path);
        }
    }

    fn toggle_workspace_sidebar(&mut self) {
        self.project.sidebar_visible = !self.project.sidebar_visible;
        if self.project.sidebar_visible {
            self.cache_active_workspace_tree_path();
            self.ui.full_redraw = true;
            self.status("Workspace sidebar on; Esc w t opens the navigable tree");
        } else {
            self.ui.full_redraw = true;
            self.status("Workspace sidebar off");
        }
    }

    pub fn workspace_sidebar_visible(&self) -> bool {
        self.project.sidebar_visible
    }

    pub fn project_sidebar_view(&self, visible_rows: usize) -> ProjectSidebarView {
        let Some(index) = &self.project.index else {
            return ProjectSidebarView {
                lines: vec![ProjectSidebarLine {
                    text: "No project index".to_owned(),
                    depth: 0,
                    directory: false,
                    active: false,
                }],
                partial: false,
                unavailable: true,
            };
        };
        let active_path = self.workspace_tree_active_path();
        let mut expanded = self.project.tree_expanded.clone();
        if let Some(active_path) = &active_path {
            for ancestor in active_path
                .ancestors()
                .skip(1)
                .filter(|ancestor| !ancestor.as_os_str().is_empty())
            {
                expanded.insert(ancestor.to_path_buf());
            }
        }
        let listing = build_workspace_tree_listing(
            index.tree_entries(),
            &expanded,
            active_path.as_deref(),
            visible_rows,
        );
        let lines = listing
            .nodes
            .into_iter()
            .map(|node| {
                let name = node
                    .path
                    .file_name()
                    .map(safe_tree_component)
                    .unwrap_or_else(|| safe_tree_path(&node.path));
                let marker = if node.is_directory {
                    if expanded.contains(&node.path) {
                        "▾"
                    } else {
                        "▸"
                    }
                } else if active_path.as_ref() == Some(&node.path) {
                    "●"
                } else {
                    "·"
                };
                let suffix = if node.is_directory { "/" } else { "" };
                ProjectSidebarLine {
                    text: format!("{marker} {name}{suffix}"),
                    depth: node.depth,
                    directory: node.is_directory,
                    active: active_path.as_ref() == Some(&node.path),
                }
            })
            .collect();
        ProjectSidebarView {
            lines,
            partial: listing.truncated || index.is_tree_truncated(),
            unavailable: false,
        }
    }

    fn begin_replace(&mut self) {
        self.ui.keymap.cancel();
        self.ui.status = None;
        self.ui.pending_replace = None;
        let editor = self.workspace.active();
        let input = editor
            .selected_text()
            .or_else(|| self.ui.search_query.clone())
            .unwrap_or_default();
        self.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::ReplaceFind,
            input,
            editor.cursor,
            editor.anchor,
        ));
    }

    fn prompt_changed(&mut self) {
        let is_search = matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Search,
                ..
            })
        );
        let fixed_candidates = self
            .prompt()
            .is_some_and(|prompt| prompt.kind.uses_fixed_candidates());
        if fixed_candidates {
            self.filter_fixed_prompt();
        } else {
            self.refresh_prompt_candidates();
        }
        if is_search {
            self.update_incremental_search();
        }
        if matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::GlobalSearch,
                ..
            })
        ) {
            self.submit_project_search();
        }
    }

    fn filter_fixed_prompt(&mut self) {
        let UiMode::Prompt(prompt) = &mut self.ui.mode else {
            return;
        };
        let query = prompt.input.clone();
        let mut candidates: Vec<_> = prompt
            .all_labels
            .iter()
            .cloned()
            .zip(prompt.all_entries.iter().cloned())
            .filter_map(|(label, entry)| {
                if query.is_empty() {
                    Some((0, label, entry))
                } else {
                    fuzzy_path_score(&query, &label).map(|score| (score, label, entry))
                }
            })
            .collect();
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        (prompt.labels, prompt.entries) = candidates
            .into_iter()
            .map(|(_, label, entry)| (label, entry))
            .unzip();
        prompt.selected = prompt.selected.min(prompt.labels.len().saturating_sub(1));
    }

    fn refresh_prompt_candidates(&mut self) {
        let Some((kind, query)) = self
            .prompt()
            .map(|prompt| (prompt.kind, prompt.input.clone()))
        else {
            return;
        };
        let mut tree_notice = None;
        let service_notice = match kind {
            PromptFlow::QuickOpen if self.project.status.is_pending() => {
                Some("Indexing workspace…".to_owned())
            }
            PromptFlow::Recovery if self.persistence.recovery_status.is_pending() => {
                Some("Scanning recovery journals…".to_owned())
            }
            _ => None,
        };
        let (labels, entries) = match kind {
            PromptFlow::QuickOpen => self
                .project
                .index
                .as_ref()
                .map(|index| {
                    index
                        .quick_open(&query)
                        .into_iter()
                        .map(|matched| {
                            (
                                safe_tree_path(&matched.path),
                                PromptEntry::Path(index.absolute_path(matched.path)),
                            )
                        })
                        .unzip()
                })
                .unwrap_or_default(),
            PromptFlow::OpenRecent => self.recent_file_candidates(&query),
            PromptFlow::JumpList => self.jump_list_candidates(&query),
            PromptFlow::Bookmarks => self.bookmark_candidates(&query),
            PromptFlow::WorkspaceTree => {
                let (labels, entries, notice) = self.workspace_tree_candidates(&query);
                tree_notice = Some(notice);
                (labels, entries)
            }
            PromptFlow::Buffers => {
                let mut buffers: Vec<_> = self
                    .workspace
                    .buffers()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, editor)| {
                        let label = format!(
                            "{} {}{}",
                            index + 1,
                            editor.document.display_name(),
                            if editor.document.is_modified() {
                                " *"
                            } else {
                                ""
                            }
                        );
                        if query.is_empty() {
                            Some((0, label, PromptEntry::Buffer(index)))
                        } else {
                            fuzzy_path_score(&query, &label)
                                .map(|score| (score, label, PromptEntry::Buffer(index)))
                        }
                    })
                    .collect();
                buffers
                    .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
                buffers
                    .into_iter()
                    .map(|(_, label, entry)| (label, entry))
                    .unzip()
            }
            PromptFlow::Palette => keymap::search_commands(&query)
                .into_iter()
                .map(|command| {
                    (
                        format!("{}  —  Esc {}", command.title, command.sequence),
                        PromptEntry::Action(command.action),
                    )
                })
                .unzip(),
            PromptFlow::Recovery => {
                let mut records: Vec<_> = self
                    .persistence
                    .recovery_records
                    .iter()
                    .filter_map(|record| {
                        let name = record
                            .original_path
                            .as_deref()
                            .and_then(Path::file_name)
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "Untitled".to_owned());
                        let label = format!(
                            "{}  —  {} chars  —  journal {}",
                            name,
                            record.text.chars().count(),
                            record.id
                        );
                        if query.is_empty() || fuzzy_path_score(&query, &label).is_some() {
                            Some((label, PromptEntry::Recovery(record.id.clone())))
                        } else {
                            None
                        }
                    })
                    .collect();
                records.sort_by(|left, right| left.0.cmp(&right.0));
                records.into_iter().unzip()
            }
            PromptFlow::GlobalSearch => (Vec::new(), Vec::new()),
            PromptFlow::Stickies => self.sticky_candidates(&query),
            PromptFlow::AgentGoal => (Vec::new(), Vec::new()),
            PromptFlow::Tasks => self
                .tasks
                .runner
                .as_ref()
                .map(|runner| {
                    runner
                        .config()
                        .tasks()
                        .iter()
                        .filter_map(|(name, task)| {
                            let label = format!("{}  —  {}", name, task.argv().join(" "));
                            if query.is_empty() || fuzzy_path_score(&query, &label).is_some() {
                                Some((label, PromptEntry::Task(name.clone())))
                            } else {
                                None
                            }
                        })
                        .unzip()
                })
                .unwrap_or_default(),
            _ => (Vec::new(), Vec::new()),
        };
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.labels = labels;
            prompt.entries = entries;
            prompt.notice = tree_notice.or(service_notice);
            prompt.selected = prompt.selected.min(prompt.labels.len().saturating_sub(1));
        }
    }

    fn workspace_tree_active_path(&self) -> Option<PathBuf> {
        self.project.index.as_ref()?;
        self.project
            .tree_document_paths
            .get(&self.workspace.active().id())
            .and_then(Clone::clone)
    }

    fn cache_workspace_tree_editor_path(&mut self, editor_id: u64) {
        let Some(editor) = self.workspace.editor_by_id(editor_id) else {
            return;
        };
        let identity = editor.document.path().and_then(|path| {
            resolve_workspace_tree_document_path(
                &self.workspace.root,
                self.project.index.as_ref(),
                path,
            )
        });
        self.project
            .tree_document_paths
            .retain(|editor_id, _| self.workspace.editor_index(*editor_id).is_some());
        self.project.tree_document_paths.insert(editor_id, identity);
    }

    fn cache_active_workspace_tree_path(&mut self) {
        self.cache_workspace_tree_editor_path(self.workspace.active().id());
    }

    fn workspace_tree_candidates(&self, query: &str) -> (Vec<String>, Vec<PromptEntry>, String) {
        let Some(index) = &self.project.index else {
            return (
                Vec::new(),
                Vec::new(),
                if self.project.status.is_pending() {
                    "Indexing workspace…".to_owned()
                } else {
                    "Error: Workspace index is unavailable".to_owned()
                },
            );
        };
        let active_path = self.workspace_tree_active_path();
        let active_overlay = active_path
            .as_ref()
            .is_some_and(|path| !index.contains_tree_file(path));
        if !query.is_empty() {
            let active_match = active_path.clone().and_then(|active_path| {
                (!index.contains_tree_file(&active_path))
                    .then(|| {
                        fuzzy_path_score(query, &safe_tree_path(&active_path)).map(|score| {
                            crate::project::QuickOpenMatch {
                                path: active_path,
                                score,
                            }
                        })
                    })
                    .flatten()
            });
            let indexed_limit =
                crate::project::MAX_RESULTS.saturating_sub(usize::from(active_match.is_some()));
            let search = index.search_tree_files_with_metadata(query, indexed_limit);
            let search_has_more = search.has_more;
            let mut matches = search.matches;
            let active_included = active_match.is_some();
            if let Some(active_match) = active_match {
                matches.push(active_match);
                matches.sort_by(|left, right| {
                    right
                        .score
                        .cmp(&left.score)
                        .then_with(|| left.path.cmp(&right.path))
                });
            }
            debug_assert!(matches.len() <= crate::project::MAX_RESULTS);
            let (labels, entries): (Vec<String>, Vec<PromptEntry>) = matches
                .into_iter()
                .map(|matched| {
                    (
                        format!("⌕ {}", safe_tree_path(&matched.path)),
                        PromptEntry::WorkspaceTree(WorkspaceTreeNode {
                            depth: matched.path.components().count().saturating_sub(1),
                            path: matched.path,
                            is_directory: false,
                        }),
                    )
                })
                .unzip();
            let partial = index.is_tree_truncated() || search_has_more;
            let mut notice = if labels.is_empty() && index.is_tree_truncated() {
                "No file match in the bounded snapshot · workspace tree is partial · clear query for hierarchy".to_owned()
            } else if labels.is_empty() {
                "No file match · clear query for hierarchy".to_owned()
            } else if partial {
                "Partial filtered file view · clear query for hierarchy".to_owned()
            } else {
                "Filtered file view · clear query for hierarchy".to_owned()
            };
            if active_included {
                notice.push_str(" · active buffer included outside snapshot");
            }
            return (labels, entries, notice);
        }

        let listing = build_workspace_tree_listing(
            index.tree_entries(),
            &self.project.tree_expanded,
            active_path.as_deref(),
            MAX_WORKSPACE_TREE_VISIBLE_NODES,
        );
        let listing_truncated = listing.truncated;
        let (labels, entries) = listing
            .nodes
            .into_iter()
            .map(|node| {
                let name = node
                    .path
                    .file_name()
                    .map(safe_tree_component)
                    .unwrap_or_else(|| safe_tree_path(&node.path));
                let indent = "  ".repeat(node.depth);
                let marker = if node.is_directory {
                    if self.project.tree_expanded.contains(&node.path) {
                        "▾"
                    } else {
                        "▸"
                    }
                } else if active_path.as_ref() == Some(&node.path) {
                    "●"
                } else {
                    "·"
                };
                let suffix = if node.is_directory { "/" } else { "" };
                (
                    format!("{indent}{marker} {name}{suffix}"),
                    PromptEntry::WorkspaceTree(node),
                )
            })
            .unzip();
        let mut notice = if listing_truncated || index.is_tree_truncated() {
            format!(
                "Partial tree ({MAX_WORKSPACE_TREE_VISIBLE_NODES} visible-node cap or index limit) · Right/Enter expand · Left collapse"
            )
        } else {
            "Right/Enter expand · Left collapse/up · type to filter files".to_owned()
        };
        if active_overlay {
            notice.push_str(" · active buffer revealed outside snapshot; siblings remain omitted");
        }
        (labels, entries, notice)
    }

    fn update_incremental_search(&mut self) {
        let Some(prompt) = self.prompt() else {
            return;
        };
        let query = prompt.input.clone();
        let from = prompt.original_cursor;
        self.ui.search_query = (!query.is_empty()).then_some(query.clone());
        self.ui.search_editor_id = (!query.is_empty()).then_some(self.workspace.active().id());
        self.ui.search_match = self.workspace.active().find_from(&query, from, true);
        if let Some(range) = self.ui.search_match.clone() {
            self.workspace.active_mut().set_cursor(range.start, false);
            self.ui.status = None;
        } else if !query.is_empty() {
            self.ui.status = Some(Status {
                message: "No match".to_owned(),
                error: true,
            });
        }
    }

    fn submit_project_search(&mut self) {
        let Some(query) = self.prompt().map(|prompt| prompt.input.clone()) else {
            return;
        };
        let Some(worker) = &self.project.search_worker else {
            if self.project.status.is_pending() {
                self.status("Workspace is still indexing; project search is pending");
            } else {
                self.error("Project search index is unavailable");
            }
            return;
        };
        if query.is_empty() {
            worker.cancel();
            if let UiMode::Prompt(prompt) = &mut self.ui.mode {
                prompt.labels.clear();
                prompt.entries.clear();
                prompt.notice = None;
                prompt.selected = 0;
            }
            return;
        }
        match worker.request(SearchQuery::new(query).with_limits(100, 1_000)) {
            Ok(_) => {
                if let UiMode::Prompt(prompt) = &mut self.ui.mode {
                    prompt.labels = vec!["Searching…".to_owned()];
                    prompt.entries.clear();
                    prompt.notice = None;
                    prompt.selected = 0;
                }
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn open_search_match(&mut self, matched: SearchMatch) {
        let origin = self.current_jump_location();
        let path = self.workspace.root.join(&matched.path);
        match self.workspace.open(path) {
            Ok(_) => {
                self.cache_active_workspace_tree_path();
                self.record_active_file_recent();
                {
                    let mut editor = self.workspace.active_mut();
                    editor.goto_line(matched.line + 1);
                    let line_start = editor.document.line_start_char(matched.line);
                    editor.set_cursor(line_start + matched.char_column, false);
                }
                self.record_jump_origin(origin);
                self.status(format!(
                    "{}:{}:{}",
                    matched.path.display(),
                    matched.line + 1,
                    matched.char_column + 1
                ));
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn selected_workspace_tree_node(&self) -> Option<WorkspaceTreeNode> {
        let prompt = self.prompt()?;
        match prompt.entries.get(prompt.selected)? {
            PromptEntry::WorkspaceTree(node) => Some(node.clone()),
            _ => None,
        }
    }

    fn refresh_workspace_tree_preserving(&mut self, selected_path: &Path) {
        self.refresh_prompt_candidates();
        self.select_workspace_tree_path(selected_path);
    }

    fn select_workspace_tree_path(&mut self, selected_path: &Path) -> bool {
        if let UiMode::Prompt(prompt) = &mut self.ui.mode
            && prompt.kind == PromptFlow::WorkspaceTree
            && let Some(index) = prompt.entries.iter().position(|entry| {
                matches!(entry, PromptEntry::WorkspaceTree(node) if node.path == selected_path)
            })
        {
            prompt.selected = index;
            return true;
        }
        false
    }

    fn expand_selected_workspace_tree_directory(&mut self) {
        let Some(node) = self.selected_workspace_tree_node() else {
            return;
        };
        if !node.is_directory || self.project.tree_expanded.contains(&node.path) {
            return;
        }
        if self.project.tree_expanded.len() >= MAX_WORKSPACE_TREE_EXPANDED_DIRECTORIES {
            self.error(format!(
                "Workspace tree already retains {MAX_WORKSPACE_TREE_EXPANDED_DIRECTORIES} expanded directories; collapse a branch first"
            ));
            return;
        }
        self.project.tree_expanded.insert(node.path.clone());
        self.refresh_workspace_tree_preserving(&node.path);
    }

    fn collapse_selected_workspace_tree_directory(&mut self) {
        let Some(node) = self.selected_workspace_tree_node() else {
            return;
        };
        if node.is_directory && self.project.tree_expanded.contains(&node.path) {
            self.project
                .tree_expanded
                .retain(|expanded| expanded != &node.path && !expanded.starts_with(&node.path));
            self.refresh_workspace_tree_preserving(&node.path);
            return;
        }
        let Some(parent) = node
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
        else {
            return;
        };
        self.select_workspace_tree_path(&parent);
    }

    fn activate_selected_workspace_tree_node(&mut self) {
        let Some(node) = self.selected_workspace_tree_node() else {
            self.error("No workspace tree selection");
            return;
        };
        if node.is_directory {
            if self.project.tree_expanded.contains(&node.path) {
                self.project
                    .tree_expanded
                    .retain(|expanded| expanded != &node.path && !expanded.starts_with(&node.path));
                self.refresh_workspace_tree_preserving(&node.path);
            } else {
                self.expand_selected_workspace_tree_directory();
            }
            return;
        }
        let origin = self.current_jump_location();
        if let Some(editor_index) = self.workspace_tree_editor_index(&node.path) {
            self.workspace.activate(editor_index);
            self.ui.mode = UiMode::Edit;
            self.record_jump_origin(origin);
            self.ui.status = None;
            return;
        }
        let Some(index) = self.project.index.as_ref() else {
            self.error("Workspace index is unavailable");
            return;
        };
        let bytes = match index.read_tree_file(&node.path, crate::document::MAX_DOCUMENT_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.error(format!(
                    "Could not open workspace file {}: {error}",
                    safe_tree_path(&node.path)
                ));
                return;
            }
        };
        let path = index.absolute_path(&node.path);
        let document = match Document::from_disk_snapshot(path, &bytes) {
            Ok(document) => document,
            Err(error) => {
                self.error(format!(
                    "Could not open workspace file {}: {error}",
                    safe_tree_path(&node.path)
                ));
                return;
            }
        };
        self.workspace.admit_prevalidated_file_document(document);
        self.record_active_file_recent();
        self.ui.mode = UiMode::Edit;
        self.cache_active_workspace_tree_path();
        self.record_jump_origin(origin);
        self.ui.status = None;
    }

    fn workspace_tree_editor_index(&self, relative: &Path) -> Option<usize> {
        self.workspace.file_editors().find_map(|editor| {
            let cached_matches = self
                .project
                .tree_document_paths
                .get(&editor.id())
                .and_then(Option::as_deref)
                .is_some_and(|cached| cached == relative);
            cached_matches
                .then(|| self.workspace.editor_index(editor.id()))
                .flatten()
        })
    }

    fn commit_prompt(&mut self) {
        let UiMode::Prompt(prompt) = self.ui.mode.clone() else {
            return;
        };
        match prompt.kind.complete(&prompt) {
            PromptCompletion::FinishSearch { original_cursor } => {
                let mut origin = self.current_jump_location();
                origin.cursor = original_cursor;
                self.ui.mode = UiMode::Edit;
                if self.ui.search_match.is_none() {
                    self.status("No match");
                } else {
                    self.record_jump_origin(origin);
                }
            }
            PromptCompletion::BeginReplace {
                find,
                original_cursor,
                original_anchor,
            } => {
                if find.is_empty() {
                    self.error("Replace needs non-empty find text (literal or re:/re:i: regex)");
                    return;
                }
                if let Err(error) = crate::pattern::Pattern::parse(&find, true) {
                    self.error(error.to_string());
                    return;
                }
                self.ui.pending_replace = Some(find);
                self.ui.mode = UiMode::Prompt(Prompt::new(
                    PromptFlow::ReplaceWith,
                    String::new(),
                    original_cursor,
                    original_anchor,
                ));
            }
            PromptCompletion::ApplyReplace { replacement } => {
                let Some(find) = self.ui.pending_replace.take() else {
                    self.ui.mode = UiMode::Edit;
                    self.error("Replace state expired; start Replace again");
                    return;
                };
                self.ui.mode = UiMode::Edit;
                let pattern = match crate::pattern::Pattern::parse(&find, true) {
                    Ok(pattern) => pattern,
                    Err(error) => {
                        self.error(error.to_string());
                        return;
                    }
                };
                let result = self
                    .workspace
                    .active_mut()
                    .replace_all_pattern(&pattern, &replacement);
                match result {
                    Ok(0) => {
                        self.status(format!("No {} matches for {find:?}", pattern.mode_label()))
                    }
                    Ok(count) => self.status(format!(
                        "Replaced {count} {} match{}; Undo restores all",
                        pattern.mode_label(),
                        if count == 1 { "" } else { "es" }
                    )),
                    Err(error) => self.error(error),
                }
            }
            PromptCompletion::ActivateWorkspaceTree => self.activate_selected_workspace_tree_node(),
            PromptCompletion::Select(entry) => {
                self.ui.mode = UiMode::Edit;
                match entry {
                    Some(PromptEntry::Path(path)) => {
                        let origin = self.current_jump_location();
                        match self.workspace.open(path) {
                            Ok(_) => {
                                self.cache_active_workspace_tree_path();
                                self.record_active_file_recent();
                                self.record_jump_origin(origin);
                                self.ui.status = None;
                            }
                            Err(error) => self.error(error.to_string()),
                        }
                    }
                    Some(PromptEntry::RecentPath(path)) => match fs::metadata(&path) {
                        Ok(metadata) if metadata.is_file() => {
                            let origin = self.current_jump_location();
                            match self.workspace.open(path) {
                                Ok(_) => {
                                    self.cache_active_workspace_tree_path();
                                    self.record_active_file_recent();
                                    self.record_jump_origin(origin);
                                    self.ui.status = None;
                                }
                                Err(error) => self.error(error.to_string()),
                            }
                        }
                        Ok(_) => self.error(format!(
                            "Recent path is not a regular file: {}",
                            path.display()
                        )),
                        Err(error) => self.error(format!(
                            "Recent path is unavailable: {}: {error}",
                            path.display()
                        )),
                    },
                    Some(PromptEntry::Buffer(index)) => {
                        self.workspace.activate(index);
                        self.ui.status = None;
                    }
                    Some(PromptEntry::Action(action)) => self.execute_action(action),
                    Some(PromptEntry::Recovery(id)) => self.restore_recovery(&id),
                    Some(PromptEntry::Search(matched)) => self.open_search_match(matched),
                    Some(PromptEntry::Task(name)) => self.request_task(name),
                    Some(PromptEntry::GitChange(path)) => {
                        let origin = self.current_jump_location();
                        match self.workspace.open(path) {
                            Ok(_) => {
                                self.cache_active_workspace_tree_path();
                                self.record_active_file_recent();
                                self.record_jump_origin(origin);
                                self.ui.status = None;
                            }
                            Err(error) => self.error(error.to_string()),
                        }
                    }
                    Some(PromptEntry::GitDiff(path)) => self.open_git_diff_for_path(path),
                    Some(PromptEntry::GitCommit(commit)) => self.open_git_commit_info(&commit),
                    Some(PromptEntry::Completion(item, context, cursor, anchor)) => {
                        self.apply_completion(item, context, cursor, anchor)
                    }
                    Some(PromptEntry::Location(location))
                    | Some(PromptEntry::ProblemLocation(location, _)) => {
                        self.open_lsp_location(location)
                    }
                    Some(PromptEntry::Jump(target)) => self.open_jump_location(target),
                    Some(PromptEntry::Bookmark(target)) => self.open_bookmark_location(target),
                    Some(PromptEntry::DocumentSymbol(target)) => {
                        self.open_document_symbol_location(target)
                    }
                    Some(PromptEntry::LocalDefinition(target)) => {
                        self.open_local_definition_location(target)
                    }
                    Some(PromptEntry::LocalReference(target)) => {
                        self.open_local_reference_location(target)
                    }
                    Some(PromptEntry::SourceAnnotation(target)) => {
                        self.open_source_annotation_location(target)
                    }
                    Some(PromptEntry::WorkspaceOutline(target)) => {
                        self.open_workspace_outline_location(target)
                    }
                    Some(PromptEntry::TaskProblem(problem)) => self.open_task_problem(problem),
                    Some(PromptEntry::Sticky { id, .. }) => match self.sticky_library() {
                        Ok(library) => match self.sticky_pad.load_id(&library, &id) {
                            Ok(()) => {
                                self.sticky_pad.visible = true;
                                self.sticky_pad.focused = true;
                                self.ui.full_redraw = true;
                                self.status("Sticky loaded into pad");
                            }
                            Err(error) => self.error(error.to_string()),
                        },
                        Err(error) => self.error(error),
                    },
                    Some(PromptEntry::WorkspaceTree(_)) => {
                        unreachable!("workspace-tree prompts commit in their dedicated branch")
                    }
                    None => self.status("No selection"),
                }
            }
            PromptCompletion::ExecuteCommand(input) => {
                self.ui.mode = UiMode::Edit;
                match command::parse(&input) {
                    Ok(command) => self.execute_ex_command(command),
                    Err(error) => self.error(error),
                }
            }
            PromptCompletion::SaveAs(input) => {
                if input.trim().is_empty() {
                    self.error("Save As needs a path");
                    return;
                }
                let path = self.resolve_user_path(input.trim());
                let result = self.workspace.active_mut().document.save_as(&path);
                match result {
                    Ok(()) => {
                        self.remove_active_recovery();
                        self.ui.mode = UiMode::Edit;
                        self.cache_active_workspace_tree_path();
                        self.record_active_file_recent();
                        let refresh_suffix = self.snapshot_refresh_failure_suffix();
                        self.status(format!("Saved {}{refresh_suffix}", path.display()));
                        if std::mem::take(&mut self.ui.close_after_save_as) {
                            let _ = self.close_active_buffer(false);
                        } else if std::mem::take(&mut self.ui.save_all_after_save_as)
                            || self.ui.quit_after_save
                        {
                            self.save_all();
                        }
                    }
                    Err(error) => self.error(error.to_string()),
                }
            }
            PromptCompletion::OpenPath(input) => {
                let input = input.trim();
                if input.is_empty() {
                    self.error("Open Path needs a project-relative path");
                    return;
                }
                match self.open_workspace_path(input) {
                    Ok((path, existed)) => {
                        self.ui.mode = UiMode::Edit;
                        self.cache_active_workspace_tree_path();
                        self.record_active_file_recent();
                        self.status(format!(
                            "{} {}",
                            if existed { "Opened" } else { "Opened new path" },
                            path.display()
                        ));
                    }
                    Err(error) => self.error(error),
                }
            }
            PromptCompletion::NewFile(input) => {
                let input = input.trim();
                if input.is_empty() {
                    self.error("New File needs a project-relative path");
                    return;
                }
                match self.create_workspace_file(input) {
                    Ok(path) => {
                        self.ui.mode = UiMode::Edit;
                        self.cache_active_workspace_tree_path();
                        self.record_active_file_recent();
                        let refresh_suffix = self.snapshot_refresh_failure_suffix();
                        self.status(format!("Created {}{refresh_suffix}", path.display()));
                    }
                    Err(error) => self.error(error),
                }
            }
            PromptCompletion::RenameFile(input) => {
                let input = input.trim();
                if input.is_empty() {
                    self.error("Rename File needs a project-relative target path");
                    return;
                }
                match self.rename_active_file(input) {
                    Ok((old_path, new_path)) => {
                        self.ui.mode = UiMode::Edit;
                        self.cache_active_workspace_tree_path();
                        self.record_active_file_recent();
                        let refresh_suffix = self.snapshot_refresh_failure_suffix();
                        self.status(format!(
                            "Renamed {} to {}{refresh_suffix}",
                            old_path.display(),
                            new_path.display()
                        ));
                    }
                    Err(error) => self.error(error),
                }
            }
            PromptCompletion::SaveCopyAs(input) => {
                let input = input.trim();
                if input.is_empty() {
                    self.error("Save Copy As needs a project-relative path");
                    return;
                }
                match self.save_copy_as(input) {
                    Ok(path) => {
                        self.ui.mode = UiMode::Edit;
                        let refresh_suffix = self.snapshot_refresh_failure_suffix();
                        self.status(format!("Saved copy as {}{refresh_suffix}", path.display()));
                    }
                    Err(error) => self.error(error),
                }
            }
            PromptCompletion::CommitGit(message) => match validate_commit_message(&message) {
                Ok(message) => {
                    self.ui.mode = UiMode::GitTrust(GitMutation::CommitStaged {
                        message: message.to_owned(),
                    });
                    self.ui.status = None;
                }
                Err(error) => self.error(error.to_string()),
            },
            PromptCompletion::GoToLine(input) => match input.trim().parse::<usize>() {
                Ok(line) if line > 0 => {
                    let origin = self.current_jump_location();
                    self.workspace.active_mut().goto_line(line);
                    self.record_jump_origin(origin);
                    self.ui.mode = UiMode::Edit;
                }
                _ => self.error("Line must be a number greater than zero"),
            },
            PromptCompletion::SubmitWorkspaceSymbol(query) => {
                self.submit_workspace_symbol_query(query);
            }
            PromptCompletion::WorkspaceSymbolPending => {
                self.status("Workspace symbol search is already pending");
            }
            PromptCompletion::StartAgent(goal) => {
                self.ui.mode = UiMode::Edit;
                self.start_agent_run(goal);
            }
        }
    }

    fn cancel_prompt(&mut self) {
        self.cancel_pending_lsp_ui_requests();
        if let UiMode::Prompt(prompt) = &self.ui.mode
            && prompt.kind == PromptFlow::Search
        {
            self.workspace
                .active_mut()
                .set_cursor(prompt.original_cursor, false);
            self.workspace.active_mut().anchor = prompt.original_anchor;
            self.ui.search_query = None;
            self.ui.search_match = None;
            self.ui.search_editor_id = None;
        }
        if matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::GlobalSearch,
                ..
            })
        ) && let Some(worker) = &self.project.search_worker
        {
            worker.request_sender().cancel();
        }
        self.ui.mode = UiMode::Edit;
        self.ui.status = None;
        self.ui.pending_replace = None;
        self.ui.quit_after_save = false;
        self.ui.save_all_after_save_as = false;
        self.ui.close_after_save_as = false;
    }

    fn cancel_transient(&mut self) {
        if self.ui.keymap.cancel() {
            self.ui.status = None;
            return;
        }
        match self.ui.mode {
            UiMode::Prompt(_) => self.cancel_prompt(),
            UiMode::Help => {
                self.dismiss_help();
                self.ui.status = None;
            }
            UiMode::Confirm(_) | UiMode::TaskTrust(_) | UiMode::GitTrust(_) => {
                self.ui.mode = UiMode::Edit;
                self.ui.quit_after_save = false;
                self.ui.save_all_after_save_as = false;
                self.ui.close_after_save_as = false;
                self.ui.status = None;
            }
            UiMode::Edit => {
                if self.sticky_pad.is_focused() {
                    if let Ok(library) = self.sticky_library() {
                        let _ = self.sticky_pad.unfocus_save(&library);
                    }
                    self.ui.full_redraw = true;
                    self.ui.status = None;
                    return;
                }
                self.cancel_pending_lsp_ui_requests();
                self.workspace.active_mut().clear_selection();
                self.ui.search_query = None;
                self.ui.search_match = None;
                self.ui.search_editor_id = None;
                self.ui.status = None;
            }
        }
    }

    fn cancel_pending_lsp_ui_requests(&mut self) {
        self.lsp.cancel_ui_requests();
        self.ui.save_after_format = false;
    }

    fn dismiss_workspace_symbol_prompt(&mut self) {
        self.lsp.active_workspace_symbol_token = None;
        if matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::WorkspaceSymbolQuery | PromptFlow::WorkspaceSymbolPending,
                ..
            })
        ) {
            self.ui.mode = UiMode::Edit;
        }
    }

    fn execute_ex_command(&mut self, command: ExCommand) {
        match command {
            ExCommand::Save(path) => {
                if let Some(path) = path {
                    let path = self.resolve_user_path(&path.to_string_lossy());
                    let result = self.workspace.active_mut().document.save_as(&path);
                    match result {
                        Ok(()) => {
                            self.remove_active_recovery();
                            self.cache_active_workspace_tree_path();
                            self.record_active_file_recent();
                            let refresh_suffix = self.snapshot_refresh_failure_suffix();
                            self.status(format!("Saved {}{refresh_suffix}", path.display()));
                        }
                        Err(error) => self.error(error.to_string()),
                    }
                } else {
                    self.save_current();
                }
            }
            ExCommand::SaveForce => {
                let result = self
                    .workspace
                    .active_mut()
                    .document
                    .save_over_external_change();
                match result {
                    Ok(()) => {
                        self.remove_active_recovery();
                        self.cache_active_workspace_tree_path();
                        self.record_active_file_recent();
                        let name = self.workspace.active().document.display_name().to_owned();
                        self.status(format!("Saved {name} after explicit overwrite"));
                    }
                    Err(error) => self.error(error.to_string()),
                }
            }
            ExCommand::Quit { force } => {
                if force {
                    if self.tasks.is_running() {
                        self.error(
                            "A task is still running; stop it with Esc t s before force-quitting",
                        );
                    } else {
                        self.ui.should_quit = true;
                    }
                } else {
                    self.request_quit();
                }
            }
            ExCommand::SaveQuit(path) => {
                if let Some(path) = path {
                    let path = self.resolve_user_path(&path.to_string_lossy());
                    let result = self.workspace.active_mut().document.save_as(&path);
                    if let Err(error) = result {
                        self.error(error.to_string());
                        return;
                    }
                    self.remove_active_recovery();
                    self.cache_active_workspace_tree_path();
                    self.record_active_file_recent();
                    let refresh_suffix = self.snapshot_refresh_failure_suffix();
                    self.status(format!("Saved {}{refresh_suffix}", path.display()));
                } else {
                    self.ui.quit_after_save = true;
                    self.save_current();
                }
                if !self.workspace.active().document.is_modified() {
                    self.request_quit();
                }
            }
            ExCommand::Edit(path) => {
                if self.workspace.active().document.is_modified() {
                    self.error(
                        "Current buffer is dirty; save or use quick open to keep both buffers",
                    );
                } else {
                    let path = self.resolve_user_path(&path.to_string_lossy());
                    match self.workspace.open(path) {
                        Ok(_) => {
                            self.cache_active_workspace_tree_path();
                            self.record_active_file_recent();
                            self.ui.status = None;
                        }
                        Err(error) => self.error(error.to_string()),
                    }
                }
            }
            ExCommand::OpenPath(path) => {
                if let Some(path) = path {
                    match path.to_str() {
                        Some(path) => match self.open_workspace_path(path) {
                            Ok((opened, existed)) => {
                                self.cache_active_workspace_tree_path();
                                self.record_active_file_recent();
                                self.status(format!(
                                    "{} {}",
                                    if existed { "Opened" } else { "Opened new path" },
                                    opened.display()
                                ));
                            }
                            Err(error) => self.error(error),
                        },
                        None => self.error("Open Path must be valid UTF-8"),
                    }
                } else {
                    self.begin_prompt(PromptFlow::OpenPath);
                }
            }
            ExCommand::CloseOtherBuffers => self.close_other_buffers(),
            ExCommand::ReopenClosedBuffer => self.reopen_closed_buffer(),
            ExCommand::New => {
                self.workspace.new_buffer();
                self.cache_active_workspace_tree_path();
                self.status("New untitled buffer");
            }
            ExCommand::NewFile(path) => {
                if let Some(path) = path {
                    match path.to_str() {
                        Some(path) => match self.create_workspace_file(path) {
                            Ok(created) => {
                                self.cache_active_workspace_tree_path();
                                self.record_active_file_recent();
                                let refresh_suffix = self.snapshot_refresh_failure_suffix();
                                self.status(format!(
                                    "Created {}{refresh_suffix}",
                                    created.display()
                                ));
                            }
                            Err(error) => self.error(error),
                        },
                        None => self.error("New File path must be valid UTF-8"),
                    }
                } else {
                    self.begin_prompt(PromptFlow::NewFilePath);
                }
            }
            ExCommand::RenameFile(path) => {
                if let Some(path) = path {
                    match path.to_str() {
                        Some(path) => match self.rename_active_file(path) {
                            Ok((old_path, new_path)) => {
                                self.cache_active_workspace_tree_path();
                                self.record_active_file_recent();
                                let refresh_suffix = self.snapshot_refresh_failure_suffix();
                                self.status(format!(
                                    "Renamed {} to {}{refresh_suffix}",
                                    old_path.display(),
                                    new_path.display()
                                ));
                            }
                            Err(error) => self.error(error),
                        },
                        None => self.error("Rename File path must be valid UTF-8"),
                    }
                } else {
                    self.begin_rename_file();
                }
            }
            ExCommand::SaveCopyAs(path) => {
                if let Some(path) = path {
                    match path.to_str() {
                        Some(path) => match self.save_copy_as(path) {
                            Ok(created) => {
                                let refresh_suffix = self.snapshot_refresh_failure_suffix();
                                self.status(format!(
                                    "Saved copy as {}{refresh_suffix}",
                                    created.display()
                                ));
                            }
                            Err(error) => self.error(error),
                        },
                        None => self.error("Save Copy As path must be valid UTF-8"),
                    }
                } else {
                    self.begin_save_copy_as();
                }
            }
            ExCommand::GoTo(line) => self.workspace.active_mut().goto_line(line),
            ExCommand::JumpList => self.begin_prompt(PromptFlow::JumpList),
            ExCommand::Bookmarks => self.open_bookmarks(),
            ExCommand::SetLineEnding(ending) => {
                self.workspace.active_mut().document.set_line_ending(ending);
                self.status(format!("Line endings: {}", ending.label()));
            }
            ExCommand::SetLineNumbers(enabled) => self.set_line_numbers(enabled),
            ExCommand::Reload { force } => self.reload_current(force),
            ExCommand::RefreshWorkspace => self.refresh_workspace_snapshots(),
            ExCommand::WorkspaceSidebar => self.toggle_workspace_sidebar(),
            ExCommand::WorkspaceInfo => self.open_workspace_info(),
            ExCommand::BufferInfo => self.open_buffer_info(),
            ExCommand::DirtyBuffers => self.open_dirty_buffers(),
            ExCommand::RecentFiles => self.open_recent_files(),
            ExCommand::OpenRecentFile => self.begin_prompt(PromptFlow::OpenRecent),
            ExCommand::Terminal => self.execute_action(Action::Terminal),
            ExCommand::Tasks => self.begin_prompt(PromptFlow::Tasks),
            ExCommand::TaskCatalog => self.open_task_catalog(),
            ExCommand::TaskDefault => self.request_default_task(),
            ExCommand::Task(name) => self.request_task(name),
            ExCommand::TaskInfo(name) => self.open_task_details(&name),
            ExCommand::WorkspaceOutline => self.open_workspace_outline(),
            ExCommand::SourceAnnotations => self.open_source_annotations(),
            ExCommand::GitStatus => self.open_git_status(),
            ExCommand::GitChanges => self.begin_git_changes(),
            ExCommand::GitFileStatus => self.open_current_file_status(),
            ExCommand::GitDiff => self.open_current_diff(),
            ExCommand::GitDiffPicker => self.begin_git_diff_picker(),
            ExCommand::GitLog => self.open_git_log(),
            ExCommand::GitCommitPicker => self.begin_git_commit_picker(),
            ExCommand::GitFileHistory => self.open_git_file_history(),
            ExCommand::GitHead => self.open_git_head(),
            ExCommand::GitCommitInfo(commit) => self.open_git_commit_info(&commit),
            ExCommand::GitBlameLine => self.open_git_blame_line(),
            ExCommand::GitBranches => self.open_branch_view(),
            ExCommand::GitStageCurrent => self.request_git_path_mutation(true),
            ExCommand::GitUnstageCurrent => self.request_git_path_mutation(false),
            ExCommand::GitCommitStaged => self.request_git_commit(),
            ExCommand::LspLog => self.open_lsp_log(),
            ExCommand::LspRestart => self.restart_lsp(),
            ExCommand::KeymapReference => self.open_keymap_reference(),
            ExCommand::Help => self.ui.mode = UiMode::Help,
            ExCommand::Stickies => self.open_stickies(),
            ExCommand::NewSticky => self.create_new_sticky(),
            ExCommand::AgentRun => self.begin_agent_run_prompt(),
            ExCommand::AgentCancel => self.cancel_agent_run(),
            ExCommand::AgentDashboard => self.toggle_agent_dashboard(),
            ExCommand::AgentReview => self.handoff_agent_review(None),
        }
    }

    fn save_current(&mut self) {
        if self.workspace.active().document.is_read_only() {
            self.status("This IDE view is read-only; close it with Esc k");
            return;
        }
        if self.workspace.active().document.path().is_none() {
            self.begin_prompt(PromptFlow::SaveAs);
            return;
        }
        if self.config.format_on_save && self.try_format_before_save() {
            return;
        }
        self.write_active_document_to_disk();
    }

    /// Request LSP formatting and defer the disk write until it completes.
    /// Returns true when a format request was queued.
    fn try_format_before_save(&mut self) -> bool {
        if self.workspace.active().document.path().is_none() {
            return false;
        }
        if self
            .workspace
            .active()
            .document
            .path()
            .and_then(|path| self.config.language_server_for(path))
            .is_none()
        {
            return false;
        }
        self.ensure_lsp_service();
        let ready = self
            .lsp
            .client
            .as_ref()
            .is_some_and(crate::lsp_client::LspClient::is_ready);
        if !ready {
            return false;
        }
        // Snapshot context the same way explicit Format does.
        let Some((editor_id, _, uri, version, incarnation, state_id, _)) =
            self.lsp_request_context()
        else {
            return false;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("ready client")
            .request_formatting(
                uri,
                version,
                self.config.tab_width,
                self.config.insert_spaces,
            ) {
            Ok(request_id) => {
                self.lsp
                    .requests
                    .insert(request_id, PendingLspRequest::Formatting { context });
                self.ui.save_after_format = true;
                self.status("Formatting before save…");
                true
            }
            Err(_) => false,
        }
    }

    fn write_active_document_to_disk(&mut self) {
        let result = self.workspace.active_mut().document.save();
        match result {
            Ok(()) => {
                self.remove_active_recovery();
                self.cache_active_workspace_tree_path();
                self.record_active_file_recent();
                let name = self.workspace.active().document.display_name().to_owned();
                self.status(format!("Saved {name}"));
                if self.ui.quit_after_save && self.workspace.modified_count() == 0 {
                    self.ui.should_quit = true;
                }
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn reload_current(&mut self, force: bool) {
        if self.workspace.active().document.is_read_only() {
            self.error("This IDE view cannot be reloaded from disk");
            return;
        }
        if self.workspace.active().document.is_modified() && !force {
            self.error("Buffer is dirty; use :reload! to explicitly discard its changes");
            return;
        }
        let editor_id = self.workspace.active().id();
        let result = self.workspace.active_mut().document.reload_from_disk();
        match result {
            Ok(()) => {
                let length = self.workspace.active().document.len_chars();
                {
                    let mut editor = self.workspace.active_mut();
                    editor.cursor = editor.cursor.min(length);
                    editor.anchor = editor.anchor.filter(|anchor| *anchor <= length);
                    editor.viewport.top_line = editor
                        .viewport
                        .top_line
                        .min(editor.document.line_count().saturating_sub(1));
                    editor.viewport.top_wrap_char = 0;
                    editor.reset_vertical_goal();
                }
                self.cache_workspace_tree_editor_path(editor_id);
                self.persistence
                    .recovery_checkpoint_state
                    .remove(&editor_id);
                if let Some(record_id) = self.persistence.recovery_ids.remove(&editor_id)
                    && let Some(store) = &self.persistence.recovery_store
                {
                    let _ = store.remove(&record_id);
                }
                self.status("Reloaded buffer from disk");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn save_all(&mut self) {
        let untitled =
            self.workspace.buffers().iter().position(|editor| {
                editor.document.is_modified() && editor.document.path().is_none()
            });
        if let Some(index) = untitled {
            self.workspace.activate(index);
            self.ui.save_all_after_save_as = true;
            self.begin_prompt(PromptFlow::SaveAs);
            return;
        }
        self.ui.save_all_after_save_as = false;
        let mut saved = 0;
        let mut first_error = None;
        let editor_ids = self
            .workspace
            .buffers()
            .iter()
            .filter(|editor| editor.document.is_modified())
            .map(crate::Editor::id)
            .collect::<Vec<_>>();
        for editor_id in editor_ids {
            let result = self
                .workspace
                .editor_by_id_mut(editor_id)
                .expect("the named save-all buffer still exists")
                .document
                .save();
            match result {
                Ok(()) => {
                    saved += 1;
                    self.cache_workspace_tree_editor_path(editor_id);
                }
                Err(error) if first_error.is_none() => first_error = Some(error.to_string()),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            self.error(error);
        } else {
            let _ = self.checkpoint_recovery();
            self.status(format!(
                "Saved {saved} buffer{}",
                if saved == 1 { "" } else { "s" }
            ));
            if self.ui.quit_after_save {
                self.ui.should_quit = true;
            }
        }
    }

    fn request_quit(&mut self) {
        if self.tasks.is_running() {
            self.error("A task is still running; stop it with Esc t s before quitting");
            return;
        }
        if self.workspace.modified_count() == 0 {
            self.ui.should_quit = true;
        } else {
            self.ui.mode = UiMode::Confirm(ConfirmKind::Quit);
        }
    }

    fn next_match(&mut self, backwards: bool) {
        let Some(query) = self
            .ui
            .search_query
            .clone()
            .filter(|query| !query.is_empty())
        else {
            self.begin_prompt(PromptFlow::Search);
            return;
        };
        let active_id = self.workspace.active().id();
        if self.ui.search_editor_id != Some(active_id) {
            self.ui.search_match = None;
            self.ui.search_editor_id = Some(active_id);
        }
        let editor = self.workspace.active();
        let range = if backwards {
            let before = self
                .ui
                .search_match
                .as_ref()
                .map_or(editor.cursor, |range| range.start);
            editor.find_previous(&query, before, true)
        } else {
            let from = self
                .ui
                .search_match
                .as_ref()
                .map_or(editor.cursor, |range| range.end);
            editor.find_from(&query, from, true)
        };
        if let Some(range) = range {
            let origin = self.current_jump_location();
            self.workspace.active_mut().set_cursor(range.start, false);
            self.ui.search_match = Some(range);
            self.record_jump_origin(origin);
            self.ui.status = None;
        } else {
            self.error("No match");
        }
    }

    fn yank(&mut self, cut: bool) {
        let range = self.workspace.active().selection().unwrap_or_else(|| {
            let editor = self.workspace.active();
            let line = editor.document.char_to_line(editor.cursor);
            let start = editor.document.line_start_char(line);
            let end = if line + 1 < editor.document.line_count() {
                editor.document.line_start_char(line + 1)
            } else {
                editor.document.line_end_char(line)
            };
            start..end
        });
        let text = self.workspace.active().document.slice(range.clone());
        let outcome = self.ui.clipboard.copy(&text);
        match outcome {
            CopyOutcome::Prepared(sequence) => {
                self.ui
                    .terminal_output
                    .extend_from_slice(sequence.as_bytes());
                self.status("Copied locally; attempted OSC 52 clipboard copy");
            }
            CopyOutcome::Refused(reason) => {
                self.status(format!("Copied to internal register ({reason})"));
            }
        }
        if cut {
            let result = {
                let mut editor = self.workspace.active_mut();
                editor.anchor = Some(range.start);
                editor.cursor = range.end;
                editor.insert("", EditKind::Replace)
            };
            if let Err(error) = result {
                self.error(error.to_string());
            }
        }
    }

    fn copy_active_location(&mut self) {
        let location = self.active_location_text();
        let outcome = self.ui.clipboard.copy(&location);
        match outcome {
            CopyOutcome::Prepared(sequence) => {
                self.ui
                    .terminal_output
                    .extend_from_slice(sequence.as_bytes());
                self.status("Copied location locally; attempted OSC 52 clipboard copy");
            }
            CopyOutcome::Refused(reason) => {
                self.status(format!("Copied location to internal register ({reason})"));
            }
        }
    }

    fn copy_current_problem(&mut self) {
        let Some(problem) = self.current_problem_text() else {
            self.status("No problem at current line");
            return;
        };
        let outcome = self.ui.clipboard.copy(&problem);
        match outcome {
            CopyOutcome::Prepared(sequence) => {
                self.ui
                    .terminal_output
                    .extend_from_slice(sequence.as_bytes());
                self.status("Copied problem locally; attempted OSC 52 clipboard copy");
            }
            CopyOutcome::Refused(reason) => {
                self.status(format!("Copied problem to internal register ({reason})"));
            }
        }
    }

    fn active_location_text(&self) -> String {
        let editor = self.workspace.active();
        let label = editor
            .document
            .path()
            .map(|path| active_path_label(&self.workspace.root, path))
            .unwrap_or_else(|| editor.document.display_name().to_owned());

        if let Some(range) = editor.selection() {
            let (start_line, start_column) = editor_location_position(editor, range.start);
            let (end_line, end_column) = editor_location_position(editor, range.end);
            format!("{label}:{start_line}:{start_column}-{end_line}:{end_column}")
        } else {
            let (line, column) = editor_location_position(editor, editor.cursor);
            format!("{label}:{line}:{column}")
        }
    }

    fn current_problem_text(&self) -> Option<String> {
        let editor = self.workspace.active();
        let active_path = editor.document.path()?;
        let label = active_path_label(&self.workspace.root, active_path);
        let position = editor.position(self.config.tab_width);

        if let Some(diagnostic) = self.current_line_diagnostic() {
            let source = diagnostic
                .source
                .as_ref()
                .map(|source| format!(" [{source}]"))
                .unwrap_or_default();
            return Some(format!(
                "{label}:{}:{}: LSP {}{}: {}",
                diagnostic.range.start.line.get().saturating_add(1),
                diagnostic.range.start.character.get().saturating_add(1),
                diagnostic_severity_label(diagnostic.severity),
                source,
                diagnostic.message
            ));
        }

        let (Some(task_cwd), Some(task_name)) = (&self.tasks.cwd, &self.tasks.last) else {
            return None;
        };
        if self.tasks.output.is_empty() {
            return None;
        }
        let active_path = crate::workspace::normalized_file_path(active_path);
        let report =
            parse_task_problems(&self.tasks.output, task_cwd, &self.workspace.root).ok()?;
        report
            .problems
            .into_iter()
            .find(|problem| {
                crate::workspace::normalized_file_path(&problem.path) == active_path
                    && problem.line == position.line
            })
            .map(|problem| {
                format!(
                    "{label}:{}:{}: task {} [{task_name}]: {}",
                    problem.line + 1,
                    problem.column + 1,
                    problem.severity.label(),
                    problem.message
                )
            })
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let full_layout = Layout::calculate(
            self.ui.screen_size.0,
            self.ui.screen_size.1,
            self.workspace.active().document.line_count(),
            self.config.line_numbers,
        );
        if !matches!(self.ui.mode, UiMode::Edit) {
            self.ui.mouse_selecting = false;
            self.handle_transient_mouse(mouse, full_layout);
            return;
        }
        // The renderer shifts the editor right of the project sidebar, shortens
        // the content band for the agent dashboard, and lays out at the reduced
        // size; hit-testing must mirror that geometry.
        let sidebar_width = project_sidebar_width(self, full_layout);
        let agent_panel_height = crate::render::agent_dashboard_height(self, full_layout);
        let mut layout = if sidebar_width > 0 {
            Layout::calculate(
                full_layout.width.saturating_sub(sidebar_width) as u16,
                self.ui.screen_size.1,
                self.workspace.active().document.line_count(),
                self.config.line_numbers,
            )
        } else {
            full_layout
        };
        layout.content_height = layout.content_height.saturating_sub(agent_panel_height);
        let before = self.active_editor_intent();
        self.ui.keymap.cancel();
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.ui.soft_wrap {
                    let metrics =
                        VisualMetrics::new(layout.content_width, self.config.tab_width, true);
                    let result = self.workspace.active_mut().scroll_wrapped_rows(metrics, -3);
                    match result {
                        Ok(()) => self.ui.viewport_scroll_pending = true,
                        Err(error) => self.error(format!("Could not scroll wrapped text: {error}")),
                    }
                } else {
                    let top = self.workspace.active().viewport.top_line.saturating_sub(3);
                    self.workspace.active_mut().viewport.top_line = top;
                    self.ui.viewport_scroll_pending = true;
                }
            }
            MouseEventKind::ScrollDown => {
                if self.ui.soft_wrap {
                    let metrics =
                        VisualMetrics::new(layout.content_width, self.config.tab_width, true);
                    let result = self.workspace.active_mut().scroll_wrapped_rows(metrics, 3);
                    match result {
                        Ok(()) => self.ui.viewport_scroll_pending = true,
                        Err(error) => self.error(format!("Could not scroll wrapped text: {error}")),
                    }
                } else {
                    let max = self
                        .workspace
                        .active()
                        .document
                        .line_count()
                        .saturating_sub(layout.content_height);
                    let top = (self.workspace.active().viewport.top_line + 3).min(max);
                    self.workspace.active_mut().viewport.top_line = top;
                    self.ui.viewport_scroll_pending = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(column) = mouse.column.checked_sub(sidebar_width as u16) {
                    self.ui.mouse_selecting = true;
                    self.place_mouse_cursor(column, mouse.row, layout, false);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.ui.mouse_selecting => {
                if let Some(column) = mouse.column.checked_sub(sidebar_width as u16) {
                    self.place_mouse_cursor(column, mouse.row, layout, true);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.ui.mouse_selecting = false,
            _ => {}
        }
        if self.active_editor_intent() != before {
            self.cancel_pending_lsp_ui_requests();
        }
    }

    fn active_editor_intent(&self) -> (u64, u64, usize, Option<usize>) {
        let editor = self.workspace.active();
        (
            editor.id(),
            editor.document.state_id(),
            editor.cursor,
            editor.anchor,
        )
    }

    fn handle_transient_mouse(&mut self, mouse: MouseEvent, layout: Layout) {
        let Some((item_count, selected, has_notice)) = self.overlay().map(|overlay| {
            (
                overlay.items.len(),
                overlay.selected,
                overlay.notice.is_some(),
            )
        }) else {
            return;
        };
        let Some(overlay_layout) =
            CandidateOverlayLayout::calculate(layout, item_count, selected, has_notice)
        else {
            return;
        };

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if let UiMode::Prompt(prompt) = &mut self.ui.mode {
                    prompt.selected = prompt.selected.saturating_sub(1);
                }
            }
            MouseEventKind::ScrollDown => {
                if let UiMode::Prompt(prompt) = &mut self.ui.mode
                    && !prompt.labels.is_empty()
                {
                    prompt.selected = (prompt.selected + 1).min(prompt.labels.len() - 1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let selected = overlay_layout.item_at(mouse.column as usize, mouse.row as usize);
                let should_commit = if let (Some(selected), UiMode::Prompt(prompt)) =
                    (selected, &mut self.ui.mode)
                {
                    if prompt.entries.get(selected).is_some() {
                        prompt.selected = selected;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if should_commit {
                    self.commit_prompt();
                }
            }
            _ => {}
        }
    }

    fn place_mouse_cursor(&mut self, column: u16, row: u16, layout: Layout, selecting: bool) {
        let row = row as usize;
        let column = column as usize;
        if row < layout.content_y || row >= layout.content_y + layout.content_height {
            return;
        }
        if self.ui.soft_wrap {
            let metrics = VisualMetrics::new(layout.content_width, self.config.tab_width, true);
            let screen_row = row - layout.content_y;
            let result = {
                let editor = self.workspace.active();
                let rows = metrics.visible_rows(
                    &editor.document,
                    VisualAnchor {
                        line: editor.viewport.top_line,
                        char_in_line: editor.viewport.top_wrap_char,
                    },
                    layout.content_height,
                );
                rows.and_then(|rows| {
                    let Some(visual_row) = rows.get(screen_row) else {
                        return Ok(None);
                    };
                    let x = column.saturating_sub(layout.gutter_width);
                    metrics
                        .cursor_for_point(&editor.document, visual_row, x)
                        .map(Some)
                })
            };
            match result {
                Ok(Some(cursor)) => self.workspace.active_mut().set_cursor(cursor, selecting),
                Ok(None) => {}
                Err(error) => self.error(format!("Could not place wrapped cursor: {error}")),
            }
            return;
        }
        let editor = self.workspace.active();
        let line = editor.viewport.top_line + row - layout.content_y;
        if line >= editor.document.line_count() {
            return;
        }
        let start = editor.document.line_start_char(line);
        let end = editor.document.line_end_char(line);
        let text = editor.document.slice(start..end);
        let visual = editor.viewport.left_column + column.saturating_sub(layout.gutter_width);
        let cursor = start + char_for_visual_column(&text, visual, self.config.tab_width);
        self.workspace.active_mut().set_cursor(cursor, selecting);
    }

    fn resolve_user_path(&self, input: &str) -> PathBuf {
        if let Some(rest) = input.strip_prefix("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(rest);
        }
        let path = Path::new(input);
        self.workspace.resolve(path)
    }

    fn create_workspace_file(&mut self, input: &str) -> Result<PathBuf, String> {
        let path = self.resolve_workspace_target_path("New File", input)?;
        if path.exists() {
            return Err(format!(
                "New File target already exists: {}",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            if parent == path {
                return Err("New File needs a file path, not a filesystem root".to_owned());
            }
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create parent directory: {error}"))?;
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
        self.workspace
            .open(&path)
            .map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn open_workspace_path(&mut self, input: &str) -> Result<(PathBuf, bool), String> {
        let path = self.resolve_workspace_target_path("Open Path", input)?;
        let existed = path.exists();
        if existed {
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("could not inspect Open Path target: {error}"))?;
            if !metadata.is_file() {
                return Err("Open Path target is not a regular file".to_owned());
            }
        } else {
            let Some(parent) = path.parent() else {
                return Err("Open Path needs a file path".to_owned());
            };
            let metadata = fs::metadata(parent).map_err(|error| {
                format!(
                    "Open Path parent directory is unavailable; use New File to create directories: {error}"
                )
            })?;
            if !metadata.is_dir() {
                return Err("Open Path parent is not a directory".to_owned());
            }
        }
        self.workspace
            .open(&path)
            .map_err(|error| error.to_string())?;
        Ok((path, existed))
    }

    fn begin_rename_file(&mut self) {
        if let Err(error) = self.validate_active_file_rename_source() {
            self.error(error);
            return;
        }
        self.begin_prompt(PromptFlow::RenameFilePath);
    }

    fn rename_active_file(&mut self, input: &str) -> Result<(PathBuf, PathBuf), String> {
        self.validate_active_file_rename_source()?;
        let old_path = self
            .workspace
            .active()
            .document
            .path()
            .ok_or_else(|| "Rename File needs a file-backed buffer".to_owned())?
            .to_path_buf();
        let new_path = self.resolve_workspace_target_path("Rename File", input)?;
        if crate::workspace::normalized_file_path(&old_path)
            == crate::workspace::normalized_file_path(&new_path)
        {
            return Err("Rename File target is the current file".to_owned());
        }
        if new_path.exists() {
            return Err(format!(
                "Rename File target already exists: {}",
                new_path.display()
            ));
        }
        if let Some(parent) = new_path.parent() {
            if parent == new_path {
                return Err("Rename File needs a file path, not a filesystem root".to_owned());
            }
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create parent directory: {error}"))?;
        }
        fs::hard_link(&old_path, &new_path).map_err(|error| {
            format!(
                "could not create renamed file {}: {error}",
                new_path.display()
            )
        })?;
        if let Err(error) = fs::remove_file(&old_path) {
            let _ = fs::remove_file(&new_path);
            return Err(format!(
                "could not remove original file {} after rename: {error}",
                old_path.display()
            ));
        }
        self.workspace
            .active_mut()
            .document
            .retarget_after_rename(&new_path)
            .map_err(|error| error.to_string())?;
        Ok((old_path, new_path))
    }

    fn begin_save_copy_as(&mut self) {
        if self.workspace.active().document.is_read_only() {
            self.error("Save Copy As is unavailable in read-only IDE views");
            return;
        }
        self.begin_prompt(PromptFlow::SaveCopyAsPath);
    }

    fn save_copy_as(&mut self, input: &str) -> Result<PathBuf, String> {
        let path = self.resolve_workspace_target_path("Save Copy As", input)?;
        if path.exists() {
            return Err(format!(
                "Save Copy As target already exists: {}",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            if parent == path {
                return Err("Save Copy As needs a file path, not a filesystem root".to_owned());
            }
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create parent directory: {error}"))?;
        }
        self.workspace
            .active()
            .document
            .save_copy_as(&path)
            .map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn validate_active_file_rename_source(&self) -> Result<(), String> {
        let editor = self.workspace.active();
        if editor.document.is_read_only() {
            return Err("Rename File is unavailable in read-only IDE views".to_owned());
        }
        let Some(path) = editor.document.path() else {
            return Err("Rename File needs a file-backed buffer".to_owned());
        };
        if editor.document.is_modified() {
            return Err("Save or discard changes before Rename File".to_owned());
        }
        let metadata = fs::metadata(path)
            .map_err(|error| format!("could not inspect current file before rename: {error}"))?;
        if !metadata.is_file() {
            return Err("Rename File source is not a regular file".to_owned());
        }
        Ok(())
    }

    fn resolve_workspace_target_path(
        &self,
        operation: &str,
        input: &str,
    ) -> Result<PathBuf, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(format!("{operation} needs a project-relative path"));
        }
        let requested = self.resolve_user_path(trimmed);
        let root = crate::workspace::normalized_file_path(&self.workspace.root);
        let resolved = crate::workspace::normalized_file_path(&requested);
        if resolved == root {
            return Err(format!(
                "{operation} needs a file path, not the workspace root"
            ));
        }
        if !resolved.starts_with(&root) {
            return Err(format!("{operation} path must stay inside the workspace"));
        }
        Ok(resolved)
    }

    fn snapshot_refresh_failure_suffix(&mut self) -> String {
        self.refresh_workspace_snapshots();
        " · workspace reindexing".to_owned()
    }

    fn workspace_snapshot_looks_stale(&self) -> bool {
        let Some(index) = self.project.index.as_ref() else {
            return false;
        };
        let Some(last) = self.project.index_built_at else {
            return false;
        };
        let Ok(metadata) = fs::metadata(index.root()) else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        modified > last
    }

    fn refresh_workspace_snapshots(&mut self) {
        if !self.services_started {
            self.start_background_services();
            return;
        }
        let generation = self.services.start_project(self.workspace.root.clone());
        self.project.status = ServiceStatus::Pending(generation);
        if matches!(
            self.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::QuickOpen | PromptFlow::WorkspaceTree | PromptFlow::GlobalSearch,
                ..
            })
        ) {
            self.ui.mode = UiMode::Edit;
        }
        self.status("Indexing refreshed workspace snapshot");
    }

    fn prune_workspace_tree_state(&mut self) {
        let Some(index) = &self.project.index else {
            self.project.tree_expanded.clear();
            self.project.tree_resume_path = None;
            self.project.tree_document_paths.clear();
            return;
        };
        let directories = index
            .tree_entries()
            .iter()
            .filter(|entry| entry.is_directory)
            .map(|entry| entry.path.clone())
            .collect::<HashSet<_>>();
        self.project
            .tree_expanded
            .retain(|expanded| directories.contains(expanded));
        self.project.tree_document_paths.retain(|editor_id, path| {
            self.workspace.editor_index(*editor_id).is_some()
                && path
                    .as_deref()
                    .is_none_or(workspace_tree_active_path_is_bounded)
        });
        if self.project.tree_resume_path.as_ref().is_some_and(|path| {
            !index.contains_tree_path(path)
                && !self
                    .project
                    .tree_document_paths
                    .values()
                    .any(|document_path| document_path.as_ref() == Some(path))
        }) {
            self.project.tree_resume_path = None;
        }
    }

    fn selected_recovery_id(&self) -> Option<String> {
        let prompt = self.prompt()?;
        match prompt.entries.get(prompt.selected)? {
            PromptEntry::Recovery(id) => Some(id.clone()),
            _ => None,
        }
    }

    fn restore_recovery(&mut self, id: &str) {
        let Some(record) = self
            .persistence
            .recovery_records
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            self.error("Recovery journal is no longer available");
            return;
        };
        let existing = record.original_path.as_ref().and_then(|recovery_path| {
            self.workspace.buffers().iter().position(|editor| {
                editor
                    .document
                    .path()
                    .is_some_and(|open_path| same_workspace(open_path, recovery_path))
            })
        });
        let recover_as_untitled =
            existing.is_some_and(|index| self.workspace.buffers()[index].document.is_modified());
        let recovered_path = (!recover_as_untitled)
            .then(|| record.original_path.clone())
            .flatten();
        let document = Document::recovered(recovered_path, &record.text);
        let index = if let Some(index) = existing.filter(|_| !recover_as_untitled) {
            let mut editor = crate::Editor::new(document);
            editor.cursor = record.cursor.min(editor.document.len_chars());
            editor.anchor = record
                .anchor
                .filter(|anchor| *anchor <= editor.document.len_chars());
            let replaced = self
                .workspace
                .replace_editor(index, editor)
                .expect("the recovered workspace buffer still exists");
            drop(replaced);
            self.workspace.activate(index);
            index
        } else {
            self.workspace
                .open_recovered(document, record.cursor, record.anchor)
        };
        self.cache_workspace_tree_editor_path(self.workspace.buffers()[index].id());
        let editor = &self.workspace.buffers()[index];
        let editor_id = editor.id();
        self.persistence.recovery_checkpoint_state.insert(
            editor_id,
            RecoveryCheckpointStamp {
                path: editor.document.path().map(Path::to_path_buf),
                revision: editor.document.state_id(),
                saved_revision: editor.document.saved_state_id(),
                cursor: editor.cursor,
                anchor: editor.anchor,
            },
        );
        self.persistence
            .recovery_ids
            .insert(editor_id, record.id.clone());
        self.persistence
            .recovery_records
            .retain(|item| item.id != record.id);
        self.ui.mode = UiMode::Edit;
        self.status(if recover_as_untitled {
            "Original path already has dirty edits; recovery loaded as untitled for safe comparison"
        } else {
            "Recovered buffer loaded; use Save As, or :w! after reviewing the on-disk file"
        });
    }

    fn view_selected_recovery(&mut self) {
        let Some(id) = self.selected_recovery_id() else {
            self.status("No recovery journal selected");
            return;
        };
        let Some(record) = self
            .persistence
            .recovery_records
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            self.error("Recovery journal is no longer available");
            return;
        };
        self.workspace
            .open_virtual(format!("Recovery Preview: {}", record.id), &record.text);
        self.ui.mode = UiMode::Edit;
        self.status("Recovery preview is read-only; Esc w r returns to journals");
    }

    fn discard_selected_recovery(&mut self) {
        let Some(id) = self.selected_recovery_id() else {
            self.status("No recovery journal selected");
            return;
        };
        if let Some(store) = &self.persistence.recovery_store
            && let Err(error) = store.remove(&id)
        {
            self.error(error.to_string());
            return;
        }
        self.persistence
            .recovery_records
            .retain(|record| record.id != id);
        self.refresh_prompt_candidates();
        self.status("Recovery journal discarded");
    }

    fn remove_active_recovery(&mut self) {
        let editor_id = self.workspace.active().id();
        self.remove_recovery_for_editor(editor_id);
    }

    fn remove_recovery_for_editor(&mut self, editor_id: u64) {
        self.persistence
            .recovery_checkpoint_state
            .remove(&editor_id);
        let Some(record_id) = self.persistence.recovery_ids.remove(&editor_id) else {
            return;
        };
        if let Some(store) = &self.persistence.recovery_store {
            let _ = store.remove(&record_id);
        }
    }

    fn discard_all_recovery(&mut self) {
        if let Some(store) = &self.persistence.recovery_store {
            for record_id in self.persistence.recovery_ids.values() {
                let _ = store.remove(record_id);
            }
        }
        self.persistence.recovery_ids.clear();
        self.persistence.recovery_checkpoint_state.clear();
    }

    fn poll_lsp(&mut self) -> bool {
        let mut redraw = self.ensure_lsp_service();
        let mut sync_budget = LspSyncBudget::new();
        // Publish any editor change before accepting queued responses. The
        // protocol thread also rejects responses whose document version has
        // advanced, but an event may already be queued when the user edits.
        // Synchronizing first closes that app-loop ordering window.
        redraw |= self.sync_lsp_document_with_budget(&mut sync_budget);
        let mut disconnected = false;
        // Handle one event before releasing another queue slot. Collecting a
        // batch here would let a fast producer refill the byte-budgeted queue
        // while the UI retained the whole drained batch outside that budget.
        let mut event_budget = LspEventPollBudget::new();
        while event_budget.can_receive() {
            let event = if let Some(event) = self.lsp.deferred_event.take() {
                event
            } else {
                match self.lsp.client.as_ref().map(LspClient::try_recv_event) {
                    Some(Ok(event)) => event,
                    Some(Err(TryRecvError::Empty)) | None => break,
                    Some(Err(TryRecvError::Disconnected)) => {
                        disconnected = true;
                        break;
                    }
                }
            };
            if !event_budget.reserve(&event) {
                self.lsp.deferred_event = Some(event);
                break;
            }
            redraw |= self.handle_lsp_event(event);
        }
        // Ready events can make the first document eligible for didOpen, and
        // accepted service edits need a fresh didChange before the next UI
        // action can issue another request.
        redraw |= self.sync_lsp_document_with_budget(&mut sync_budget);

        let stopped = disconnected || self.lsp.client.as_ref().is_some_and(LspClient::is_stopped);
        if stopped {
            let failed = self.lsp.server_name.take();
            self.lsp.client = None;
            self.lsp.workspace_symbols = None;
            self.lsp.text_document_sync = None;
            self.lsp.documents.clear();
            self.lsp.document_incarnations.clear();
            self.lsp.document_ends.clear();
            self.lsp.discovery_cursor = 0;
            self.lsp.sync_cursor = 0;
            self.lsp.background_sync_due = false;
            self.lsp.quarantined_documents.clear();
            self.lsp.next_document_version = 1;
            self.lsp.ambiguous_diagnostic_uris.clear();
            self.lsp.all_versionless_diagnostics_ambiguous = false;
            self.lsp.diagnostics.clear();
            self.lsp.requests.clear();
            self.lsp.deferred_event = None;
            self.dismiss_workspace_symbol_prompt();
            self.lsp.failed_server = failed.clone();
            if let Some(name) = failed {
                self.error(format!(
                    "Language server {name:?} stopped; use Esc c R or :lsp-restart to retry"
                ));
                redraw = true;
            }
        }
        redraw
    }

    fn ensure_lsp_service(&mut self) -> bool {
        let desired = self
            .workspace
            .active()
            .document
            .path()
            .and_then(|path| self.config.language_server_for(path))
            .cloned();
        let Some(server) = desired else {
            return false;
        };
        if self.lsp.server_name.as_deref() == Some(server.name.as_str()) {
            return false;
        }
        if self.lsp.failed_server.as_deref() == Some(server.name.as_str()) {
            return false;
        }

        if let Some(mut client) = self.lsp.client.take() {
            let _ = client.shutdown();
        }
        self.lsp.documents.clear();
        self.lsp.document_incarnations.clear();
        self.lsp.document_ends.clear();
        self.lsp.discovery_cursor = 0;
        self.lsp.sync_cursor = 0;
        self.lsp.background_sync_due = false;
        self.lsp.quarantined_documents.clear();
        self.lsp.next_document_version = 1;
        self.lsp.ambiguous_diagnostic_uris.clear();
        self.lsp.all_versionless_diagnostics_ambiguous = false;
        self.lsp.diagnostics.clear();
        self.lsp.requests.clear();
        self.lsp.deferred_event = None;
        self.dismiss_workspace_symbol_prompt();
        self.lsp.server_name = None;
        self.lsp.workspace_symbols = None;
        self.lsp.text_document_sync = None;
        if self.lsp.failed_server.as_deref() != Some(server.name.as_str()) {
            self.lsp.failed_server = None;
        }

        let mut config = LspServerConfig::new(server.argv.clone(), self.workspace.root.clone());
        // Admission validates didOpen before mutating the app registry. Keep
        // one transport-side slot of headroom so a full 64-document registry
        // can validate a replacement, then queue didClose before didOpen.
        config.limits.max_open_documents = MAX_SYNCHRONIZED_DOCUMENTS + 1;
        match LspClient::spawn(config) {
            Ok(client) => {
                let name = server.name;
                let pid = client.pid();
                self.lsp.client = Some(client);
                self.lsp.server_name = Some(name.clone());
                self.status(format!("Starting language server {name:?} (pid {pid})"));
            }
            Err(error) => {
                self.lsp.failed_server = Some(server.name.clone());
                self.error(format!(
                    "Could not start language server {:?}: {error}",
                    server.name
                ));
            }
        }
        true
    }

    fn sync_lsp_document(&mut self) -> bool {
        let mut budget = LspSyncBudget::new();
        self.sync_lsp_document_with_budget_mode(&mut budget, true)
    }

    fn sync_lsp_document_with_budget(&mut self, budget: &mut LspSyncBudget) -> bool {
        self.sync_lsp_document_with_budget_mode(budget, false)
    }

    fn sync_lsp_document_with_budget_mode(
        &mut self,
        budget: &mut LspSyncBudget,
        prioritize_active: bool,
    ) -> bool {
        let Some(client) = &self.lsp.client else {
            return false;
        };
        if !client.is_ready() {
            return false;
        }
        let Some(text_document_sync) = self.lsp.text_document_sync else {
            return false;
        };
        if !text_document_sync.open_close
            || (!text_document_sync.full && !text_document_sync.incremental)
        {
            return false;
        }
        let Some(server_name) = self.lsp.server_name.clone() else {
            return false;
        };

        let mut redraw = false;
        let mut first_error = None;
        let mut fatal_error = None;
        let mut backpressured = false;

        // Editors can disappear, change paths through save-as, or stop mapping
        // to this service while it remains live for other open buffers.
        let tracked = self
            .lsp
            .documents
            .iter()
            .map(|document| (document.editor_id, document.uri.clone()))
            .collect::<Vec<_>>();
        for (editor_id, uri) in tracked {
            let oversized = self
                .workspace
                .editor_by_id(editor_id)
                .is_some_and(|editor| editor.document.len_bytes() > DEFAULT_MAX_DOCUMENT_BYTES);
            if self.lsp_editor_matches_service(editor_id, &uri, &server_name) && !oversized {
                continue;
            }
            if oversized {
                if let Some(state_id) = self
                    .workspace
                    .editor_by_id(editor_id)
                    .map(|editor| editor.document.state_id())
                {
                    self.quarantine_lsp_document(editor_id, uri.clone(), state_id);
                } else {
                    self.lsp.documents.mark_partial();
                }
            }
            let Some(document) = self.lsp.documents.remove_by_editor_id(editor_id) else {
                continue;
            };
            self.lsp.document_incarnations.remove(&editor_id);
            self.lsp.document_ends.remove(&editor_id);
            self.lsp.diagnostics.purge(&document.uri);
            self.mark_lsp_diagnostics_ambiguous(document.uri.clone());
            if let Some(client) = &mut self.lsp.client
                && let Err(error) = client.did_close(document.uri)
            {
                fatal_error = Some(error.to_string());
                break;
            }
            redraw = true;
        }
        if let Some(error) = fatal_error.take() {
            return self.stop_lsp_after_sync_failure(error);
        }

        let active_editor_id = self.workspace.active().id();
        if let Some(active) = self.lsp_sync_target(active_editor_id, &server_name) {
            let active_uri = active.uri.clone();
            let became_active = self.lsp.documents.active_editor_id() != Some(active.editor_id);
            if self
                .lsp
                .documents
                .get_by_editor_id(active.editor_id)
                .is_some()
            {
                if became_active {
                    self.lsp.documents.mark_active(active.editor_id);
                    redraw = true;
                }
            } else {
                redraw |= self.admit_lsp_document(
                    active,
                    true,
                    &mut first_error,
                    &mut fatal_error,
                    budget,
                    &mut backpressured,
                );
            }
            if became_active {
                self.lsp.diagnostics.mark_used(&active_uri);
            }
        }
        if let Some(error) = fatal_error.take() {
            return self.stop_lsp_after_sync_failure(error);
        }

        let active_requires_text_sync = self
            .lsp
            .documents
            .get_by_editor_id(active_editor_id)
            .and_then(|document| {
                self.workspace
                    .editor_by_id(active_editor_id)
                    .map(|editor| editor.document.state_id() != document.state_id)
            })
            .unwrap_or(false);
        if !active_requires_text_sync && (prioritize_active || !self.lsp.background_sync_due) {
            let available = MAX_SYNCHRONIZED_DOCUMENTS.saturating_sub(self.lsp.documents.len());
            let targets =
                self.lsp_unsynchronized_targets(&server_name, active_editor_id, available + 1);
            for target in targets {
                if backpressured {
                    break;
                }
                if self.lsp.documents.len() >= MAX_SYNCHRONIZED_DOCUMENTS {
                    self.lsp.documents.mark_partial();
                    break;
                }
                redraw |= self.admit_lsp_document(
                    target,
                    false,
                    &mut first_error,
                    &mut fatal_error,
                    budget,
                    &mut backpressured,
                );
                if let Some(error) = fatal_error.take() {
                    return self.stop_lsp_after_sync_failure(error);
                }
            }
        }
        if self
            .lsp
            .documents
            .get_by_editor_id(active_editor_id)
            .is_some()
            && self.lsp.documents.active_editor_id() != Some(active_editor_id)
        {
            self.lsp.documents.mark_active(active_editor_id);
            if let Some(uri) = self
                .lsp
                .documents
                .get_by_editor_id(active_editor_id)
                .map(|document| document.uri.clone())
            {
                self.lsp.diagnostics.mark_used(&uri);
            }
        }

        // Background buffers remain live LSP documents. Synchronize each one,
        // not only whichever editor currently has focus.
        let mut registered = self
            .lsp
            .documents
            .iter()
            .map(|document| {
                (
                    document.editor_id,
                    document.uri.clone(),
                    document.version,
                    document.state_id,
                    document.saved_state_id,
                    document.save_generation,
                )
            })
            .collect::<Vec<_>>();
        // Purging stale UI state is cheap and must not depend on which text
        // publications fit this poll's transport budget.
        for (editor_id, uri, _, state_id, _, _) in &registered {
            if self
                .workspace
                .editor_by_id(*editor_id)
                .is_some_and(|editor| editor.document.state_id() != *state_id)
            {
                self.lsp.diagnostics.purge(uri);
            }
        }
        registered.sort_by_key(|document| document.0);
        let mut active_document = registered
            .iter()
            .position(|document| document.0 == active_editor_id)
            .map(|index| registered.remove(index));
        let background_len = registered.len();
        let background_start = if background_len == 0 {
            self.lsp.sync_cursor = 0;
            0
        } else {
            self.lsp.sync_cursor % background_len
        };
        let background_priority =
            !prioritize_active && self.lsp.background_sync_due && background_len != 0;
        let mut ordered =
            Vec::with_capacity(background_len + usize::from(active_document.is_some()));
        if !background_priority && let Some(document) = active_document.take() {
            ordered.push((None, document));
        }
        for offset in 0..background_len {
            let index = (background_start + offset) % background_len;
            ordered.push((Some(index), registered[index].clone()));
        }
        if let Some(document) = active_document {
            ordered.push((None, document));
        }
        for (
            background_index,
            (
                editor_id,
                uri,
                version,
                state_id,
                synchronized_saved_state_id,
                synchronized_save_generation,
            ),
        ) in ordered
        {
            if backpressured {
                break;
            }
            let Some((
                current_state_id,
                current_saved_state_id,
                current_save_generation,
                document_bytes,
            )) = self.workspace.editor_by_id(editor_id).map(|editor| {
                (
                    editor.document.state_id(),
                    editor.document.saved_state_id(),
                    editor.document.save_generation(),
                    editor.document.len_bytes(),
                )
            })
            else {
                if let Some(index) = background_index {
                    self.lsp.sync_cursor = (index + 1) % background_len;
                    if background_priority && index == background_start {
                        self.lsp.background_sync_due = false;
                    }
                }
                continue;
            };
            let text_changed = current_state_id != state_id;
            let changed_text = if text_changed {
                if !budget.reserve_text(document_bytes) {
                    if let Some(index) = background_index {
                        self.lsp.sync_cursor = index;
                        self.lsp.background_sync_due = true;
                    }
                    break;
                }
                let editor = self
                    .workspace
                    .editor_by_id(editor_id)
                    .expect("registered editor was checked above");
                Some((editor.document.text(), lsp_document_end(&editor.document)))
            } else {
                None
            };

            let mut synchronized_version = version;
            let mut synchronized_state_id = state_id;
            let mut next_saved_state_id = synchronized_saved_state_id;
            let mut next_save_generation = synchronized_save_generation;
            let mut document_error = None;
            let mut removed = false;
            let mut deferred_for_budget = false;
            if let Some((text, next_document_end)) = changed_text {
                if let Some(next_version) = self.next_lsp_document_version() {
                    let change_result = if text_document_sync.full {
                        Some(
                            self.lsp
                                .client
                                .as_mut()
                                .expect("ready client was checked above")
                                .did_change(uri.clone(), next_version, text),
                        )
                    } else if text_document_sync.incremental {
                        match self.lsp.document_ends.get(&editor_id).copied() {
                            Some(previous_end) => Some(
                                self.lsp
                                    .client
                                    .as_mut()
                                    .expect("ready client was checked above")
                                    .did_change_full_document_replacement(
                                        uri.clone(),
                                        next_version,
                                        previous_end,
                                        text,
                                    ),
                            ),
                            None => {
                                let problem = format!(
                                    "missing prior LSP document end for synchronized editor {editor_id}"
                                );
                                fatal_error = Some(problem.clone());
                                document_error = Some(problem);
                                None
                            }
                        }
                    } else {
                        unreachable!("unsupported text synchronization was rejected at Ready")
                    };
                    if let Some(change_result) = change_result {
                        match change_result {
                            Ok(()) => {
                                self.commit_lsp_document_version();
                                self.mark_lsp_diagnostics_ambiguous(uri.clone());
                                self.lsp.document_ends.insert(editor_id, next_document_end);
                                synchronized_version = next_version;
                                synchronized_state_id = current_state_id;
                                redraw = true;
                            }
                            Err(error) => {
                                let permanent_text_rejection = matches!(
                                    error,
                                    LspClientError::DocumentTooLarge { .. }
                                        | LspClientError::MessageTooLarge { .. }
                                        | LspClientError::DocumentUriTooLarge { .. }
                                        | LspClientError::PositionOutOfRange(_)
                                );
                                if permanent_text_rejection {
                                    self.quarantine_lsp_document(
                                        editor_id,
                                        uri.clone(),
                                        current_state_id,
                                    );
                                    self.mark_lsp_diagnostics_ambiguous(uri.clone());
                                    match self
                                        .lsp
                                        .client
                                        .as_mut()
                                        .expect("ready client was checked above")
                                        .did_close(uri.clone())
                                    {
                                        Ok(()) => {
                                            self.lsp.documents.remove_by_editor_id(editor_id);
                                            self.lsp.document_incarnations.remove(&editor_id);
                                            self.lsp.document_ends.remove(&editor_id);
                                            removed = true;
                                        }
                                        Err(close_error) => {
                                            fatal_error = Some(close_error.to_string());
                                        }
                                    }
                                } else if matches!(error, LspClientError::QueueFull) {
                                    backpressured = true;
                                }
                                document_error = Some(error.to_string());
                            }
                        }
                    }
                } else {
                    fatal_error = Some(
                        "language-server document version space exhausted; restart the service"
                            .to_owned(),
                    );
                    document_error =
                        Some("language-server document version space exhausted".to_owned());
                }
            }

            if !removed
                && document_error.is_none()
                && synchronized_state_id == current_state_id
                && synchronized_save_generation != current_save_generation
            {
                if text_document_sync.save {
                    let save_text = if text_document_sync.save_include_text {
                        if budget.reserve_text(document_bytes) {
                            Some(
                                self.workspace
                                    .editor_by_id(editor_id)
                                    .expect("registered editor was checked above")
                                    .document
                                    .text(),
                            )
                        } else {
                            deferred_for_budget = true;
                            None
                        }
                    } else {
                        None
                    };
                    if !deferred_for_budget {
                        match self
                            .lsp
                            .client
                            .as_mut()
                            .expect("ready client was checked above")
                            .did_save(uri.clone(), synchronized_version, save_text)
                        {
                            Ok(()) => {
                                next_saved_state_id = current_saved_state_id;
                                next_save_generation = current_save_generation;
                                redraw = true;
                            }
                            Err(error) => {
                                let permanent_text_rejection = matches!(
                                    error,
                                    LspClientError::DocumentTooLarge { .. }
                                        | LspClientError::MessageTooLarge { .. }
                                );
                                if permanent_text_rejection {
                                    self.quarantine_lsp_document(
                                        editor_id,
                                        uri.clone(),
                                        current_state_id,
                                    );
                                    self.mark_lsp_diagnostics_ambiguous(uri.clone());
                                    match self
                                        .lsp
                                        .client
                                        .as_mut()
                                        .expect("ready client was checked above")
                                        .did_close(uri.clone())
                                    {
                                        Ok(()) => {
                                            self.lsp.documents.remove_by_editor_id(editor_id);
                                            self.lsp.document_incarnations.remove(&editor_id);
                                            self.lsp.document_ends.remove(&editor_id);
                                            removed = true;
                                        }
                                        Err(close_error) => {
                                            fatal_error = Some(close_error.to_string());
                                        }
                                    }
                                } else if matches!(error, LspClientError::QueueFull) {
                                    backpressured = true;
                                }
                                document_error = Some(error.to_string());
                            }
                        }
                    }
                } else {
                    next_saved_state_id = current_saved_state_id;
                    next_save_generation = current_save_generation;
                }
            } else if !removed
                && synchronized_state_id == current_state_id
                && synchronized_saved_state_id != current_saved_state_id
            {
                next_saved_state_id = current_saved_state_id;
            }

            if !removed {
                self.lsp.documents.update(
                    editor_id,
                    synchronized_version,
                    synchronized_state_id,
                    next_saved_state_id,
                    next_save_generation,
                );
            }
            if let Some(error) = document_error {
                first_error.get_or_insert(error);
            }
            if let Some(index) = background_index {
                if backpressured || deferred_for_budget {
                    self.lsp.background_sync_due = true;
                } else if background_priority && index == background_start {
                    self.lsp.background_sync_due = false;
                }
                self.lsp.sync_cursor = if (backpressured || deferred_for_budget) && !removed {
                    index
                } else {
                    (index + 1) % background_len
                };
            }
            if fatal_error.is_some() {
                break;
            }
            if deferred_for_budget {
                break;
            }
        }

        if let Some(error) = fatal_error.take() {
            return self.stop_lsp_after_sync_failure(error);
        }

        if let Some(error) = first_error {
            self.error(format!("Language-server sync failed: {error}"));
            true
        } else {
            redraw
        }
    }

    fn lsp_sync_target(&self, editor_id: u64, server_name: &str) -> Option<LspEditorSyncTarget> {
        let editor = self.workspace.editor_by_id(editor_id)?;
        let path = editor.document.path()?;
        let server = self.config.language_server_for(path)?;
        (server.name == server_name).then(|| LspEditorSyncTarget {
            editor_id: editor.id(),
            uri: file_uri_identity(path),
            language_id: server.language_id.clone(),
            document_bytes: editor.document.len_bytes(),
            state_id: editor.document.state_id(),
            saved_state_id: editor.document.saved_state_id(),
            save_generation: editor.document.save_generation(),
        })
    }

    fn lsp_unsynchronized_targets(
        &mut self,
        server_name: &str,
        active_editor_id: u64,
        limit: usize,
    ) -> Vec<LspEditorSyncTarget> {
        let buffer_count = self.workspace.len();
        if buffer_count == 0 || limit == 0 {
            return Vec::new();
        }

        let mut index = self.lsp.discovery_cursor % buffer_count;
        let inspections = buffer_count.min(MAX_LSP_DISCOVERY_INSPECTIONS_PER_POLL);
        let mut targets = Vec::with_capacity(limit.min(MAX_SYNCHRONIZED_DOCUMENTS + 1));
        for _ in 0..inspections {
            let editor = &self.workspace.buffers()[index];
            index = (index + 1) % buffer_count;
            if editor.id() == active_editor_id
                || self.lsp.documents.get_by_editor_id(editor.id()).is_some()
            {
                continue;
            }
            let Some(path) = editor.document.path() else {
                continue;
            };
            let Some(server) = self.config.language_server_for(path) else {
                continue;
            };
            if server.name != server_name {
                continue;
            }
            targets.push(LspEditorSyncTarget {
                editor_id: editor.id(),
                uri: file_uri_identity(path),
                language_id: server.language_id.clone(),
                document_bytes: editor.document.len_bytes(),
                state_id: editor.document.state_id(),
                saved_state_id: editor.document.saved_state_id(),
                save_generation: editor.document.save_generation(),
            });
            if targets.len() >= limit {
                break;
            }
        }
        self.lsp.discovery_cursor = index;
        targets
    }

    fn lsp_editor_matches_service(&self, editor_id: u64, uri: &str, server_name: &str) -> bool {
        self.workspace
            .editor_by_id(editor_id)
            .and_then(|editor| {
                let path = editor.document.path()?;
                let server = self.config.language_server_for(path)?;
                Some(server.name == server_name && file_uri_identity(path) == uri)
            })
            .unwrap_or(false)
    }

    fn lsp_document_is_quarantined(&self, target: &LspEditorSyncTarget) -> bool {
        self.lsp.quarantined_documents.iter().any(|document| {
            document.editor_id == target.editor_id
                && document.uri == target.uri
                && document.state_id == target.state_id
        })
    }

    fn quarantine_lsp_document(&mut self, editor_id: u64, uri: String, state_id: u64) {
        self.lsp
            .quarantined_documents
            .retain(|document| document.editor_id != editor_id);
        if self.lsp.quarantined_documents.len() >= MAX_LSP_QUARANTINED_DOCUMENTS {
            self.lsp.quarantined_documents.remove(0);
        }
        self.lsp.quarantined_documents.push(LspQuarantinedDocument {
            editor_id,
            uri,
            state_id,
        });
        self.lsp.documents.mark_partial();
    }

    fn clear_lsp_quarantine_for_editor(&mut self, editor_id: u64) {
        self.lsp
            .quarantined_documents
            .retain(|document| document.editor_id != editor_id);
    }

    fn next_lsp_document_version(&self) -> Option<DocumentVersion> {
        (self.lsp.next_document_version <= i32::MAX as u64)
            .then(|| DocumentVersion::new(self.lsp.next_document_version))
    }

    fn commit_lsp_document_version(&mut self) {
        self.lsp.next_document_version = self.lsp.next_document_version.saturating_add(1);
    }

    fn mark_lsp_diagnostics_ambiguous(&mut self, uri: String) {
        if self.lsp.all_versionless_diagnostics_ambiguous
            || self.lsp.ambiguous_diagnostic_uris.contains(&uri)
        {
            return;
        }
        if self.lsp.ambiguous_diagnostic_uris.len() >= MAX_AMBIGUOUS_DIAGNOSTIC_URIS {
            self.lsp.ambiguous_diagnostic_uris.clear();
            self.lsp.all_versionless_diagnostics_ambiguous = true;
        } else {
            self.lsp.ambiguous_diagnostic_uris.insert(uri);
        }
    }

    fn lsp_versionless_diagnostics_are_ambiguous(&self, uri: &str) -> bool {
        self.lsp.all_versionless_diagnostics_ambiguous
            || self.lsp.ambiguous_diagnostic_uris.contains(uri)
    }

    fn admit_lsp_document(
        &mut self,
        target: LspEditorSyncTarget,
        allow_eviction: bool,
        first_error: &mut Option<String>,
        fatal_error: &mut Option<String>,
        budget: &mut LspSyncBudget,
        backpressured: &mut bool,
    ) -> bool {
        if !allow_eviction && self.lsp.documents.len() >= MAX_SYNCHRONIZED_DOCUMENTS {
            self.lsp.documents.mark_partial();
            return false;
        }
        if self.lsp_document_is_quarantined(&target) {
            return false;
        }
        if target.document_bytes > DEFAULT_MAX_DOCUMENT_BYTES {
            self.quarantine_lsp_document(target.editor_id, target.uri, target.state_id);
            first_error.get_or_insert_with(|| {
                format!(
                    "document payload is {} bytes; limit is {}",
                    target.document_bytes, DEFAULT_MAX_DOCUMENT_BYTES
                )
            });
            return true;
        }
        if !budget.reserve_text(target.document_bytes) {
            return false;
        }
        let Some((text, document_end)) = self
            .workspace
            .editor_by_id(target.editor_id)
            .map(|editor| (editor.document.text(), lsp_document_end(&editor.document)))
        else {
            return false;
        };
        let Some(version) = self.next_lsp_document_version() else {
            first_error.get_or_insert_with(|| {
                "language-server document version space exhausted; restart the service".to_owned()
            });
            return true;
        };
        if let Err(error) = self
            .lsp
            .client
            .as_ref()
            .expect("ready client was checked above")
            .validate_did_open(&target.uri, &target.language_id, version, &text)
        {
            if matches!(
                error,
                LspClientError::DocumentTooLarge { .. }
                    | LspClientError::MessageTooLarge { .. }
                    | LspClientError::DocumentUriTooLarge { .. }
            ) {
                self.quarantine_lsp_document(target.editor_id, target.uri.clone(), target.state_id);
            }
            first_error.get_or_insert_with(|| error.to_string());
            return true;
        }
        let document = SynchronizedDocument::new(
            target.editor_id,
            target.uri.clone(),
            version,
            target.state_id,
            target.saved_state_id,
            target.save_generation,
        );
        let report = match self.lsp.documents.insert(document) {
            Ok(report) => report,
            Err(error) => {
                self.lsp.documents.mark_partial();
                first_error.get_or_insert_with(|| error.to_string());
                return true;
            }
        };
        if let Some(evicted) = report.evicted {
            self.lsp.document_incarnations.remove(&evicted.editor_id);
            self.lsp.document_ends.remove(&evicted.editor_id);
            self.lsp.diagnostics.purge(&evicted.uri);
            self.mark_lsp_diagnostics_ambiguous(evicted.uri.clone());
            if let Some(client) = &mut self.lsp.client
                && let Err(error) = client.did_close(evicted.uri)
            {
                *fatal_error = Some(error.to_string());
                return true;
            }
        }
        match self
            .lsp
            .client
            .as_mut()
            .expect("ready client was checked above")
            .did_open(
                target.uri.clone(),
                target.language_id.clone(),
                version,
                text,
            ) {
            Ok(incarnation) => {
                self.commit_lsp_document_version();
                self.lsp
                    .document_incarnations
                    .insert(target.editor_id, incarnation);
                self.lsp
                    .document_ends
                    .insert(target.editor_id, document_end);
                self.clear_lsp_quarantine_for_editor(target.editor_id);
                true
            }
            Err(error) => {
                self.lsp.documents.remove_by_editor_id(target.editor_id);
                self.lsp.document_incarnations.remove(&target.editor_id);
                self.lsp.document_ends.remove(&target.editor_id);
                if matches!(
                    error,
                    LspClientError::DocumentTooLarge { .. }
                        | LspClientError::MessageTooLarge { .. }
                        | LspClientError::DocumentUriTooLarge { .. }
                ) {
                    self.quarantine_lsp_document(
                        target.editor_id,
                        target.uri.clone(),
                        target.state_id,
                    );
                } else {
                    self.lsp.documents.mark_partial();
                }
                if matches!(error, LspClientError::QueueFull) {
                    *backpressured = true;
                }
                first_error.get_or_insert_with(|| error.to_string());
                true
            }
        }
    }

    fn stop_lsp_after_sync_failure(&mut self, problem: String) -> bool {
        let failed = self.lsp.server_name.take();
        if let Some(mut client) = self.lsp.client.take() {
            let _ = client.shutdown();
        }
        self.lsp.workspace_symbols = None;
        self.lsp.text_document_sync = None;
        self.lsp.documents.clear();
        self.lsp.document_incarnations.clear();
        self.lsp.document_ends.clear();
        self.lsp.discovery_cursor = 0;
        self.lsp.sync_cursor = 0;
        self.lsp.background_sync_due = false;
        self.lsp.quarantined_documents.clear();
        self.lsp.next_document_version = 1;
        self.lsp.ambiguous_diagnostic_uris.clear();
        self.lsp.all_versionless_diagnostics_ambiguous = false;
        self.lsp.diagnostics.clear();
        self.lsp.requests.clear();
        self.lsp.deferred_event = None;
        self.dismiss_workspace_symbol_prompt();
        self.lsp.failed_server = failed.clone();
        if let Some(name) = failed {
            self.error(format!(
                "Language server {name:?} stopped after document-close synchronization failed: {problem}; restart it to retry"
            ));
        } else {
            self.error(format!(
                "Language-server document-close synchronization failed: {problem}"
            ));
        }
        true
    }

    fn stop_lsp_after_initialization_event_loss(&mut self, count: usize) -> bool {
        let failed = self.lsp.server_name.take();
        if let Some(mut client) = self.lsp.client.take() {
            let _ = client.shutdown();
        }
        self.lsp.workspace_symbols = None;
        self.lsp.text_document_sync = None;
        self.lsp.documents.clear();
        self.lsp.document_incarnations.clear();
        self.lsp.document_ends.clear();
        self.lsp.discovery_cursor = 0;
        self.lsp.sync_cursor = 0;
        self.lsp.background_sync_due = false;
        self.lsp.quarantined_documents.clear();
        self.lsp.next_document_version = 1;
        self.lsp.ambiguous_diagnostic_uris.clear();
        self.lsp.all_versionless_diagnostics_ambiguous = false;
        self.lsp.diagnostics.clear();
        self.lsp.diagnostics.mark_partial();
        self.lsp.requests.clear();
        self.lsp.deferred_event = None;
        self.dismiss_workspace_symbol_prompt();
        self.lsp.failed_server = failed;
        self.error(format!(
            "Language server dropped {count} UI events before synchronization capabilities arrived; use Esc c R or :lsp-restart to retry"
        ));
        true
    }

    fn lsp_document_context_is_current(&self, context: &LspDocumentRequestContext) -> bool {
        let editor = self.workspace.active();
        if editor.id() != context.editor_id || editor.document.state_id() != context.state_id {
            return false;
        }
        self.lsp
            .documents
            .get_by_editor_id(context.editor_id)
            .is_some_and(|document| {
                document.editor_id == context.editor_id
                    && document.uri == context.uri
                    && document.version == context.version
                    && self
                        .lsp
                        .document_incarnations
                        .get(&context.editor_id)
                        .is_some_and(|incarnation| *incarnation == context.incarnation)
                    && document.state_id == context.state_id
            })
    }

    fn accept_lsp_document_response(
        &mut self,
        operation: &str,
        context: &LspDocumentRequestContext,
        uri: &str,
        version: DocumentVersion,
    ) -> bool {
        if uri == context.uri
            && version == context.version
            && self.lsp_document_context_is_current(context)
        {
            true
        } else {
            self.status(format!(
                "Ignored stale {operation} result after the buffer changed"
            ));
            false
        }
    }

    fn require_lsp_edit_prompt_context(
        &mut self,
        operation: &str,
        context: &LspDocumentRequestContext,
    ) -> bool {
        if self.lsp_document_context_is_current(context) {
            true
        } else {
            self.error(format!(
                "{operation} result expired after the buffer changed; request it again"
            ));
            false
        }
    }

    fn handle_lsp_event(&mut self, event: LspEvent) -> bool {
        match event {
            LspEvent::Ready {
                workspace_symbols,
                text_document_sync,
                ..
            } => {
                self.lsp.workspace_symbols = Some(workspace_symbols);
                self.lsp.text_document_sync = Some(text_document_sync);
                if !text_document_sync.open_close
                    || (!text_document_sync.full && !text_document_sync.incremental)
                {
                    self.error(
                        "Language server does not support the open/close + full or incremental synchronization required by wscrpt",
                    );
                    return true;
                }
                if let Some(name) = &self.lsp.server_name {
                    self.status(format!("Language server {name:?} ready"));
                }
                true
            }
            LspEvent::Diagnostics {
                uri,
                version,
                observed_version,
                observed_incarnation,
                diagnostics,
            } => {
                let Some(document) = self.lsp.documents.get_by_uri(&uri) else {
                    return false;
                };
                let Some(editor) = self.workspace.editor_by_id(document.editor_id) else {
                    return false;
                };
                if editor.document.state_id() != document.state_id
                    || editor
                        .document
                        .path()
                        .is_none_or(|path| file_uri_identity(path) != uri)
                {
                    return false;
                }
                let Some(expected_incarnation) = self
                    .lsp
                    .document_incarnations
                    .get(&document.editor_id)
                    .copied()
                else {
                    return false;
                };
                if observed_version != Some(document.version)
                    || observed_incarnation != Some(expected_incarnation)
                    || version.is_some_and(|version| version != document.version)
                {
                    return false;
                }
                if version.is_none() && self.lsp_versionless_diagnostics_are_ambiguous(&uri) {
                    self.lsp.diagnostics.purge(&uri);
                    self.lsp.diagnostics.mark_partial();
                    return true;
                }
                let report = parse_diagnostics(&uri, &diagnostics);
                let upstream_partial = report.truncated
                    || report.skipped_invalid != 0
                    || report.fields_truncated != 0
                    || report.raw_omitted != 0;
                self.lsp.diagnostics.replace(uri, report.diagnostics);
                if upstream_partial {
                    self.lsp.diagnostics.mark_partial();
                }
                true
            }
            LspEvent::DiagnosticsRejected { uri, reason } => {
                if let Some(uri) = uri {
                    self.lsp.diagnostics.purge(&uri);
                } else {
                    self.lsp.diagnostics.clear();
                }
                self.lsp.diagnostics.mark_partial();
                self.append_lsp_log(&format!("rejected diagnostics publication: {reason}\n"));
                self.error(format!(
                    "Language server sent malformed diagnostics: {reason}"
                ));
                true
            }
            LspEvent::Completion {
                request_id,
                uri,
                version,
                result,
            } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::Completion {
                    context,
                    cursor,
                    anchor,
                }) = pending
                else {
                    return false;
                };
                if self.workspace.active().cursor != cursor
                    || self.workspace.active().anchor != anchor
                {
                    self.status(
                        "Ignored stale completion result after the cursor or selection moved",
                    );
                    return true;
                }
                if !self.accept_lsp_document_response("completion", &context, &uri, version) {
                    return true;
                }
                let items = parse_completion(&result);
                if items.is_empty() {
                    self.status("No completion suggestions");
                } else {
                    let candidates = items.into_iter().map(|item| {
                        let label = item.detail.as_ref().map_or_else(
                            || item.label.clone(),
                            |detail| format!("{}  —  {detail}", item.label),
                        );
                        (
                            label,
                            PromptEntry::Completion(item, context.clone(), cursor, anchor),
                        )
                    });
                    self.show_fixed_prompt(PromptFlow::Completion, candidates);
                }
                true
            }
            LspEvent::Hover {
                request_id,
                uri,
                version,
                result,
            } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::Hover { context }) = pending else {
                    return false;
                };
                if !self.accept_lsp_document_response("hover", &context, &uri, version) {
                    return true;
                }
                if let Some(text) = render_hover(&result) {
                    self.workspace.open_virtual("LSP Hover", &text);
                    self.status("Hover opened as a read-only IDE view");
                } else {
                    self.status("No hover information");
                }
                true
            }
            LspEvent::Definition {
                request_id,
                uri,
                version,
                result,
            } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::Definition { context }) = pending else {
                    return false;
                };
                if !self.accept_lsp_document_response("definition", &context, &uri, version) {
                    return true;
                }
                let locations = parse_locations(&result);
                if locations.len() == 1 {
                    self.open_lsp_location(locations.into_iter().next().unwrap());
                } else if locations.is_empty() {
                    self.status("No definition found");
                } else {
                    self.show_location_prompt(PromptFlow::Locations, locations);
                }
                true
            }
            LspEvent::References {
                request_id,
                uri,
                version,
                result,
            } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::References { context }) = pending else {
                    return false;
                };
                if !self.accept_lsp_document_response("references", &context, &uri, version) {
                    return true;
                }
                let locations = parse_locations(&result);
                if locations.is_empty() {
                    self.status("No references found");
                } else {
                    self.show_location_prompt(PromptFlow::Locations, locations);
                }
                true
            }
            LspEvent::DocumentSymbols {
                request_id,
                uri,
                version,
                result,
            } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::DocumentSymbols { context }) = pending else {
                    return false;
                };
                if !self.accept_lsp_document_response("document symbols", &context, &uri, version) {
                    return true;
                }
                let symbols = parse_document_symbols(&result, &uri);
                if symbols.is_empty() {
                    self.status("No document symbols found");
                } else {
                    self.show_fixed_prompt(
                        PromptFlow::DocumentSymbols,
                        symbols
                            .into_iter()
                            .map(|(label, location)| (label, PromptEntry::Location(location))),
                    );
                }
                true
            }
            LspEvent::WorkspaceSymbols { request_id, result } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::WorkspaceSymbols {
                    prompt_token,
                    query,
                    server_name,
                }) = pending
                else {
                    return false;
                };
                let prompt_is_current = self.lsp.active_workspace_symbol_token
                    == Some(prompt_token)
                    && self.lsp.server_name.as_deref() == Some(server_name.as_str())
                    && matches!(
                        &self.ui.mode,
                        UiMode::Prompt(prompt)
                            if prompt.kind == PromptFlow::WorkspaceSymbolPending
                                && prompt.input == query
                    );
                if !prompt_is_current {
                    if self.lsp.active_workspace_symbol_token == Some(prompt_token) {
                        self.lsp.active_workspace_symbol_token = None;
                        if matches!(
                            self.ui.mode,
                            UiMode::Prompt(Prompt {
                                kind: PromptFlow::WorkspaceSymbolPending,
                                ..
                            })
                        ) {
                            self.ui.mode = UiMode::Edit;
                            self.status("Ignored an expired workspace symbol response");
                        }
                    }
                    return false;
                }
                self.lsp.active_workspace_symbol_token = None;
                let report = parse_workspace_symbols(&result);
                if report.symbols.is_empty() {
                    self.ui.mode = UiMode::Edit;
                    if report.skipped_invalid == 0 {
                        self.status(format!("No workspace symbols found for {query:?}"));
                    } else {
                        self.status(format!(
                            "No navigable workspace symbols; skipped {} malformed or range-less result{}",
                            report.skipped_invalid,
                            if report.skipped_invalid == 1 { "" } else { "s" }
                        ));
                    }
                    return true;
                }

                let display_root = crate::workspace::normalized_file_path(&self.workspace.root);
                let candidates = report.symbols.into_iter().map(|symbol| {
                    let path = file_uri_to_path(&symbol.location.uri)
                        .unwrap_or_else(|_| PathBuf::from(&symbol.location.uri));
                    // Result construction must stay CPU-only. Filesystem
                    // identity and existence are revalidated only if the user
                    // selects this candidate, avoiding thousands of blocking
                    // metadata calls for a large server response.
                    let display_path = path.strip_prefix(&display_root).unwrap_or(&path);
                    let container = symbol
                        .container_name
                        .as_deref()
                        .map(|container| format!(" · {container}"))
                        .unwrap_or_default();
                    (
                        format!(
                            "{} — {}{} — {}:{}:{}",
                            symbol.name,
                            symbol.kind.label(),
                            container,
                            display_path.display(),
                            symbol.location.range.start.line.get().saturating_add(1),
                            symbol
                                .location
                                .range
                                .start
                                .character
                                .get()
                                .saturating_add(1)
                        ),
                        PromptEntry::Location(symbol.location),
                    )
                });
                self.show_fixed_prompt(PromptFlow::WorkspaceSymbols, candidates);
                if let UiMode::Prompt(prompt) = &mut self.ui.mode {
                    prompt.notice = if report.truncated && report.skipped_invalid == 0 {
                        Some(
                            "Partial workspace symbols: response or fields reached a safety limit"
                                .to_owned(),
                        )
                    } else if !report.truncated && report.skipped_invalid != 0 {
                        Some(format!(
                            "Skipped {skipped} malformed or range-less workspace symbol result{}",
                            if report.skipped_invalid == 1 { "" } else { "s" },
                            skipped = report.skipped_invalid
                        ))
                    } else if report.truncated {
                        Some(format!(
                            "Partial workspace symbols: safety limit reached; skipped {skipped} malformed or range-less result{}",
                            if report.skipped_invalid == 1 { "" } else { "s" },
                            skipped = report.skipped_invalid
                        ))
                    } else {
                        None
                    };
                }
                true
            }
            LspEvent::Formatting {
                request_id,
                uri,
                version,
                result,
            } => {
                let pending = self.lsp.requests.remove(&request_id);
                let Some(PendingLspRequest::Formatting { context }) = pending else {
                    return false;
                };
                if !self.accept_lsp_document_response("formatting", &context, &uri, version) {
                    self.ui.save_after_format = false;
                    return true;
                }
                match parse_text_edits(&result) {
                    Some(edits) if edits.is_empty() => {
                        if self.ui.save_after_format {
                            self.ui.save_after_format = false;
                            self.write_active_document_to_disk();
                        } else {
                            self.status("Document is already formatted");
                        }
                    }
                    Some(edits) => {
                        let text = self.workspace.active().document.text();
                        match apply_text_edits(&text, &edits) {
                            Ok(updated) => {
                                let result = self
                                    .workspace
                                    .active_mut()
                                    .replace_all_from_service(&updated);
                                match result {
                                    Ok(()) => {
                                        if self.ui.save_after_format {
                                            self.ui.save_after_format = false;
                                            self.write_active_document_to_disk();
                                        } else {
                                            self.status("Formatting applied; review and save");
                                        }
                                    }
                                    Err(error) => {
                                        self.ui.save_after_format = false;
                                        self.error(error.to_string());
                                    }
                                }
                            }
                            Err(error) => {
                                self.ui.save_after_format = false;
                                self.error(format!("Invalid formatting edit: {error}"));
                            }
                        }
                    }
                    None => {
                        self.ui.save_after_format = false;
                        self.error("Language server returned malformed formatting edits");
                    }
                }
                true
            }
            LspEvent::RequestFailed {
                request_id,
                operation,
                error,
            } => {
                let Some(pending) = self.lsp.requests.remove(&request_id) else {
                    return false;
                };
                if operation == LspOperation::Formatting {
                    self.ui.save_after_format = false;
                }
                if operation == LspOperation::WorkspaceSymbols {
                    let PendingLspRequest::WorkspaceSymbols { prompt_token, .. } = pending else {
                        return false;
                    };
                    if self.lsp.active_workspace_symbol_token != Some(prompt_token) {
                        return false;
                    }
                    self.lsp.active_workspace_symbol_token = None;
                    if matches!(
                        self.ui.mode,
                        UiMode::Prompt(Prompt {
                            kind: PromptFlow::WorkspaceSymbolPending,
                            ..
                        })
                    ) {
                        self.ui.mode = UiMode::Edit;
                    }
                }
                self.error(format!(
                    "Language-server {} failed: {} ({})",
                    lsp_operation_name(operation),
                    error.message,
                    error.code
                ));
                true
            }
            LspEvent::StaleResponse {
                request_id,
                operation,
                ..
            } => {
                if self.lsp.requests.remove(&request_id).is_none() {
                    return false;
                }
                self.status(format!(
                    "Ignored stale {} result after the buffer changed",
                    lsp_operation_name(operation)
                ));
                true
            }
            LspEvent::StaleDiagnostics { .. } => false,
            LspEvent::ServerNotification { method, params } => {
                if matches!(method.as_str(), "window/showMessage" | "window/logMessage")
                    && let Some(message) = params
                        .as_ref()
                        .and_then(|params| params.get("message"))
                        .and_then(crate::lsp_client::JsonValue::as_str)
                {
                    self.status(format!("LSP: {message}"));
                    return true;
                }
                false
            }
            LspEvent::ServerRequestRejected { method } => {
                self.append_lsp_log(&format!("server request rejected: {method}\n"));
                false
            }
            LspEvent::Stderr(bytes) => {
                self.append_lsp_log(&String::from_utf8_lossy(&bytes));
                false
            }
            LspEvent::StderrTruncated { limit } => {
                self.append_lsp_log(&format!("\n[stderr truncated at {limit} bytes]\n"));
                false
            }
            LspEvent::ProtocolError(message) | LspEvent::TransportError(message) => {
                self.append_lsp_log(&format!("{message}\n"));
                self.error(format!("Language-server error: {message}"));
                true
            }
            LspEvent::EventsDropped { count } => {
                if self.lsp.text_document_sync.is_none() {
                    return self.stop_lsp_after_initialization_event_loss(count);
                }
                self.lsp.requests.clear();
                self.dismiss_workspace_symbol_prompt();
                self.lsp.diagnostics.clear();
                self.lsp.diagnostics.mark_partial();
                self.error(format!("Language server dropped {count} UI events"));
                true
            }
            LspEvent::ShutdownComplete | LspEvent::ServerClosed => false,
        }
    }

    fn begin_workspace_symbol_query(&mut self) {
        self.cancel_pending_lsp_ui_requests();
        let desired_server = self
            .workspace
            .active()
            .document
            .path()
            .and_then(|path| self.config.language_server_for(path))
            .map(|server| server.name.clone());
        if desired_server.is_none() {
            self.error_no_language_server_configured();
            return;
        }
        self.ensure_lsp_service();
        if self.lsp.client.is_none() {
            return;
        }
        let Some(next_token) = self.lsp.next_workspace_symbol_token.checked_add(1) else {
            self.error("Workspace symbol prompt token space is exhausted; restart wscrpt");
            return;
        };
        let token = self.lsp.next_workspace_symbol_token;
        self.lsp.next_workspace_symbol_token = next_token;
        self.lsp.active_workspace_symbol_token = Some(token);
        self.ui.status = None;
        let editor = self.workspace.active();
        self.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolQuery,
            String::new(),
            editor.cursor,
            editor.anchor,
        ));
    }

    fn submit_workspace_symbol_query(&mut self, query: String) {
        let query = query.trim();
        if query.is_empty() {
            self.error("Workspace symbol search needs a non-empty query");
            return;
        }
        if query.chars().any(char::is_control) {
            self.error("Workspace symbol query cannot contain control characters");
            return;
        }
        if query.len() > MAX_WORKSPACE_SYMBOL_QUERY_BYTES {
            self.error(format!(
                "Workspace symbol query exceeds {MAX_WORKSPACE_SYMBOL_QUERY_BYTES} UTF-8 bytes"
            ));
            return;
        }
        if self.lsp.requests.len() >= MAX_PENDING_LSP_REQUESTS {
            self.error(format!(
                "Language server has {MAX_PENDING_LSP_REQUESTS} unanswered requests; wait or restart it"
            ));
            return;
        }

        let desired_server = self
            .workspace
            .active()
            .document
            .path()
            .and_then(|path| self.config.language_server_for(path))
            .map(|server| server.name.clone());
        let Some(desired_server) = desired_server else {
            self.error_no_language_server_configured();
            return;
        };
        self.ensure_lsp_service();
        if self.lsp.server_name.as_deref() != Some(desired_server.as_str()) {
            self.error("The active file's language server is not available");
            return;
        }
        let Some(client) = self.lsp.client.as_ref() else {
            return;
        };
        if !client.is_ready() {
            self.error("Language server is still starting; retry the workspace symbol search");
            return;
        }
        match self.lsp.workspace_symbols {
            None => {
                self.error(
                    "Language-server capabilities are still arriving; retry the workspace symbol search",
                );
                return;
            }
            Some(false) => {
                self.error("The active language server does not advertise workspace symbol search");
                return;
            }
            Some(true) => {}
        }
        let Some(prompt_token) = self.lsp.active_workspace_symbol_token else {
            self.error("Workspace symbol prompt expired; open it again");
            return;
        };
        let query = query.to_owned();
        match self
            .lsp
            .client
            .as_mut()
            .expect("workspace symbol readiness requires a client")
            .request_workspace_symbols(query.clone())
        {
            Ok(request_id) => {
                self.lsp.requests.insert(
                    request_id,
                    PendingLspRequest::WorkspaceSymbols {
                        prompt_token,
                        query: query.clone(),
                        server_name: desired_server,
                    },
                );
                if let UiMode::Prompt(prompt) = &mut self.ui.mode {
                    prompt.kind = PromptFlow::WorkspaceSymbolPending;
                    prompt.input = query;
                    prompt.cursor = prompt.input.chars().count();
                    prompt.labels.clear();
                    prompt.entries.clear();
                    prompt.all_labels.clear();
                    prompt.all_entries.clear();
                    prompt.notice = None;
                }
                self.ui.status = None;
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn lsp_request_context(
        &mut self,
    ) -> Option<(
        u64,
        usize,
        String,
        DocumentVersion,
        DocumentIncarnation,
        u64,
        DocumentSnapshot,
    )> {
        if self.lsp.requests.len() >= MAX_PENDING_LSP_REQUESTS {
            self.error(format!(
                "Language server has {MAX_PENDING_LSP_REQUESTS} unanswered requests; wait or restart it"
            ));
            return None;
        }
        self.ensure_lsp_service();
        self.sync_lsp_document();
        let editor = self.workspace.active();
        let Some(document) = self.lsp.documents.get_by_editor_id(editor.id()) else {
            self.error_no_ready_language_server();
            return None;
        };
        if editor.document.state_id() != document.state_id {
            self.error("Language server is still synchronizing the active buffer");
            return None;
        }
        let Some(incarnation) = self.lsp.document_incarnations.get(&editor.id()).copied() else {
            self.error("Language server is still opening the active buffer");
            return None;
        };
        Some((
            editor.id(),
            editor.cursor,
            document.uri.clone(),
            document.version,
            incarnation,
            editor.document.state_id(),
            DocumentSnapshot::from_rope(editor.document.rope(), document.version),
        ))
    }

    fn request_lsp_completion(&mut self) {
        let Some((editor_id, cursor, uri, version, incarnation, state_id, snapshot)) =
            self.lsp_request_context()
        else {
            return;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        let anchor = self.workspace.active().anchor;
        let position = match cursor_position_in_snapshot(&snapshot, cursor) {
            Ok(position) => position,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("LSP context requires client")
            .request_completion(uri, version, position)
        {
            Ok(request_id) => {
                self.lsp.requests.insert(
                    request_id,
                    PendingLspRequest::Completion {
                        context,
                        cursor,
                        anchor,
                    },
                );
                self.status("Requesting completion…");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn request_lsp_hover(&mut self) {
        let Some((editor_id, cursor, uri, version, incarnation, state_id, snapshot)) =
            self.lsp_request_context()
        else {
            return;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        let position = match cursor_position_in_snapshot(&snapshot, cursor) {
            Ok(position) => position,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("LSP context requires client")
            .request_hover(uri, version, position)
        {
            Ok(request_id) => {
                self.lsp
                    .requests
                    .insert(request_id, PendingLspRequest::Hover { context });
                self.status("Requesting hover…");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn request_lsp_definition(&mut self) {
        let Some((editor_id, cursor, uri, version, incarnation, state_id, snapshot)) =
            self.lsp_request_context()
        else {
            self.open_local_definition_fallback();
            return;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        let position = match cursor_position_in_snapshot(&snapshot, cursor) {
            Ok(position) => position,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("LSP context requires client")
            .request_definition(uri, version, position)
        {
            Ok(request_id) => {
                self.lsp
                    .requests
                    .insert(request_id, PendingLspRequest::Definition { context });
                self.status("Requesting definition…");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn request_lsp_references(&mut self) {
        let Some((editor_id, cursor, uri, version, incarnation, state_id, snapshot)) =
            self.lsp_request_context()
        else {
            self.open_local_references_fallback();
            return;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        let position = match cursor_position_in_snapshot(&snapshot, cursor) {
            Ok(position) => position,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("LSP context requires client")
            .request_references(uri, version, position, true)
        {
            Ok(request_id) => {
                self.lsp
                    .requests
                    .insert(request_id, PendingLspRequest::References { context });
                self.status("Requesting references…");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn request_lsp_document_symbols(&mut self) {
        self.cancel_pending_lsp_ui_requests();
        let Some((editor_id, uri, version, incarnation, state_id)) =
            self.ready_document_symbol_lsp_context()
        else {
            self.open_local_document_symbols();
            return;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("LSP context requires client")
            .request_document_symbols(uri, version)
        {
            Ok(request_id) => {
                self.lsp
                    .requests
                    .insert(request_id, PendingLspRequest::DocumentSymbols { context });
                self.status("Requesting document symbols…");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn ready_document_symbol_lsp_context(
        &mut self,
    ) -> Option<(u64, String, DocumentVersion, DocumentIncarnation, u64)> {
        if self.lsp.requests.len() >= MAX_PENDING_LSP_REQUESTS {
            return None;
        }
        self.ensure_lsp_service();
        self.sync_lsp_document();
        let editor = self.workspace.active();
        let document = self.lsp.documents.get_by_editor_id(editor.id())?;
        if editor.document.state_id() != document.state_id {
            return None;
        }
        let incarnation = self.lsp.document_incarnations.get(&editor.id()).copied()?;
        Some((
            editor.id(),
            document.uri.clone(),
            document.version,
            incarnation,
            editor.document.state_id(),
        ))
    }

    fn open_local_document_symbols(&mut self) {
        let editor = self.workspace.active();
        let symbols =
            crate::syntax::outline_symbols(editor.document.path(), &editor.document.text());
        if symbols.is_empty() {
            self.status("No document symbols found");
            return;
        }
        let editor_id = editor.id();
        let path = editor.document.path().map(Path::to_path_buf);
        let labels_and_entries = symbols
            .into_iter()
            .map(|symbol| {
                let line_start = editor.document.line_start_char(symbol.line);
                let line_end = editor.document.line_end_char(symbol.line);
                let cursor = line_start.saturating_add(symbol.char_column).min(line_end);
                (
                    format!("{:>4}: {}", symbol.line + 1, symbol.label),
                    PromptEntry::DocumentSymbol(JumpLocation {
                        editor_id,
                        path: path.clone(),
                        cursor,
                    }),
                )
            })
            .collect::<Vec<_>>();
        self.show_fixed_prompt(PromptFlow::DocumentSymbols, labels_and_entries);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(
                "Local outline fallback · Enter jumps · filter by symbol · Esc cancels".to_owned(),
            );
        }
        self.ui.status = None;
    }

    fn open_workspace_outline(&mut self) {
        let scan = match self.collect_workspace_outline(None) {
            Ok(scan) => scan,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if scan.targets.is_empty() {
            self.status("No workspace outline symbols found");
            return;
        }
        let mut notice =
            "Local workspace outline · Enter jumps · filter by path/symbol · Esc cancels"
                .to_owned();
        append_workspace_outline_scan_notice(&mut notice, &scan);
        let labels_and_entries = scan
            .targets
            .into_iter()
            .map(|(label, target)| (label, PromptEntry::WorkspaceOutline(target)))
            .collect::<Vec<_>>();
        self.show_fixed_prompt(PromptFlow::WorkspaceOutline, labels_and_entries);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(notice);
        }
        self.ui.status = None;
    }

    fn open_local_definition_fallback(&mut self) {
        let Some(identifier) = self.active_identifier_under_cursor() else {
            self.status("No identifier under cursor for local definition");
            return;
        };
        let scan = match self.collect_workspace_outline(Some(identifier.as_str())) {
            Ok(scan) => scan,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if scan.targets.is_empty() {
            self.status(format!("No local definition found for {identifier}"));
            return;
        }
        if scan.targets.len() == 1 {
            let (_, target) = scan.targets.into_iter().next().unwrap();
            self.open_local_definition_location(target);
            return;
        }
        let mut notice =
            format!("Local definition fallback for `{identifier}` · Enter jumps · Esc cancels");
        append_workspace_outline_scan_notice(&mut notice, &scan);
        let labels_and_entries = scan
            .targets
            .into_iter()
            .map(|(label, target)| (label, PromptEntry::LocalDefinition(target)))
            .collect::<Vec<_>>();
        self.show_fixed_prompt(PromptFlow::LocalDefinitions, labels_and_entries);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(notice);
        }
        self.ui.status = None;
    }

    fn open_local_references_fallback(&mut self) {
        let Some(identifier) = self.active_identifier_under_cursor() else {
            self.status("No identifier under cursor for local references");
            return;
        };
        let scan = match self.collect_local_references(&identifier) {
            Ok(result) => result,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if scan.targets.is_empty() {
            self.status(format!("No local references found for {identifier}"));
            return;
        }
        let mut notice = format!("Local references for `{identifier}` · Enter jumps · Esc cancels");
        if scan.truncated_files || scan.truncated_matches || scan.skipped > 0 {
            let mut details = Vec::new();
            if scan.truncated_files {
                details.push(format!("first {MAX_LOCAL_REFERENCE_FILES} files"));
            }
            if scan.truncated_matches {
                details.push(format!("first {MAX_LOCAL_REFERENCES} references"));
            }
            if scan.skipped > 0 {
                details.push(format!("{} skipped", scan.skipped));
            }
            notice.push_str(" · ");
            notice.push_str(&details.join(", "));
        }
        let labels_and_entries = scan
            .targets
            .into_iter()
            .map(|(label, target)| (label, PromptEntry::LocalReference(target)))
            .collect::<Vec<_>>();
        self.show_fixed_prompt(PromptFlow::LocalReferences, labels_and_entries);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(notice);
        }
        self.ui.status = None;
    }

    fn open_source_annotations(&mut self) {
        let scan = match self.collect_source_annotations() {
            Ok(scan) => scan,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if scan.targets.is_empty() {
            self.status("No source annotations found");
            return;
        }
        let mut notice =
            "Source annotations · Enter jumps · filter by path/tag/text · Esc cancels".to_owned();
        append_source_annotation_scan_notice(&mut notice, &scan);
        let labels_and_entries = scan
            .targets
            .into_iter()
            .map(|(label, target)| (label, PromptEntry::SourceAnnotation(target)))
            .collect::<Vec<_>>();
        self.show_fixed_prompt(PromptFlow::SourceAnnotations, labels_and_entries);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(notice);
        }
        self.ui.status = None;
    }

    fn active_identifier_under_cursor(&self) -> Option<String> {
        let editor = self.workspace.active();
        let line = editor.document.char_to_line(editor.cursor);
        let line_start = editor.document.line_start_char(line);
        let column = editor.cursor.saturating_sub(line_start);
        crate::syntax::identifier_at_line_char(&editor.document.line(line), column)
    }

    fn navigate_local_symbol_occurrence(&mut self, forward: bool) {
        let Some(identifier) = self.active_identifier_under_cursor() else {
            self.status("No identifier under cursor for local occurrence navigation");
            return;
        };
        let editor = self.workspace.active();
        let occurrences = local_identifier_occurrences(&editor.document, &identifier);
        if occurrences.is_empty() {
            self.status(format!("No local occurrences found for {identifier}"));
            return;
        }
        let current = editor.cursor;
        let target = if forward {
            occurrences
                .iter()
                .copied()
                .find(|occurrence| *occurrence > current)
                .unwrap_or(occurrences[0])
        } else {
            occurrences
                .iter()
                .rev()
                .copied()
                .find(|occurrence| *occurrence < current)
                .unwrap_or(*occurrences.last().unwrap())
        };
        if target == current {
            self.status(format!("Only one local occurrence for {identifier}"));
            return;
        }
        let origin = self.current_jump_location();
        self.workspace.active_mut().set_cursor(target, false);
        self.record_jump_origin(origin);
        let line = self.workspace.active().document.char_to_line(target) + 1;
        let column = target - self.workspace.active().document.line_start_char(line - 1) + 1;
        self.status(format!(
            "{} local occurrence of {identifier}: {line}:{column}",
            if forward { "Next" } else { "Previous" }
        ));
    }

    fn go_to_matching_bracket(&mut self) {
        let Some(target) = self.workspace.active().matching_bracket() else {
            self.status("No matching bracket at cursor");
            return;
        };
        if target == self.workspace.active().cursor {
            self.status("Already on matching bracket");
            return;
        }
        let origin = self.current_jump_location();
        self.workspace.active_mut().set_cursor(target, false);
        self.record_jump_origin(origin);
        let editor = self.workspace.active();
        let line = editor.document.char_to_line(target) + 1;
        let column = target - editor.document.line_start_char(line - 1) + 1;
        self.status(format!("Jumped to matching bracket: {line}:{column}"));
    }

    fn toggle_line_comment(&mut self) {
        let path = self.workspace.active().document.path();
        let Some(marker) = line_comment_marker_for_path(path) else {
            self.status("No line-comment marker for this buffer");
            return;
        };
        let result = {
            let mut editor = self.workspace.active_mut();
            editor.toggle_line_comment(marker)
        };
        match result {
            Ok(Some(outcome)) => {
                let verb = match outcome.mode {
                    LineCommentToggle::Commented => "Commented",
                    LineCommentToggle::Uncommented => "Uncommented",
                };
                self.status(format!(
                    "{verb} {} line{}",
                    outcome.lines_changed,
                    if outcome.lines_changed == 1 { "" } else { "s" }
                ));
            }
            Ok(None) => self.status("No nonblank lines to comment"),
            Err(error) => self.error(error.to_string()),
        }
    }

    fn duplicate_lines(&mut self) {
        let result = {
            let mut editor = self.workspace.active_mut();
            editor.duplicate_lines()
        };
        match result {
            Ok(0) => self.status("No line to duplicate"),
            Ok(lines) => self.status(format!(
                "Duplicated {lines} line{}",
                if lines == 1 { "" } else { "s" }
            )),
            Err(error) => self.error(error.to_string()),
        }
    }

    fn delete_lines(&mut self) {
        let result = {
            let mut editor = self.workspace.active_mut();
            editor.delete_lines()
        };
        match result {
            Ok(0) => self.status("No line to delete"),
            Ok(lines) => self.status(format!(
                "Deleted {lines} line{}",
                if lines == 1 { "" } else { "s" }
            )),
            Err(error) => self.error(error.to_string()),
        }
    }

    fn move_lines(&mut self, up: bool) {
        let result = {
            let mut editor = self.workspace.active_mut();
            if up {
                editor.move_lines_up()
            } else {
                editor.move_lines_down()
            }
        };
        match result {
            Ok(0) => self.status(if up {
                "Already at top"
            } else {
                "Already at bottom"
            }),
            Ok(lines) => self.status(format!(
                "Moved {lines} line{} {}",
                if lines == 1 { "" } else { "s" },
                if up { "up" } else { "down" }
            )),
            Err(error) => self.error(error.to_string()),
        }
    }

    fn indent_lines(&mut self, indent: bool) {
        let tab_width = self.config.tab_width;
        let insert_spaces = self.config.insert_spaces;
        let result = {
            let mut editor = self.workspace.active_mut();
            if indent {
                editor.indent_lines(tab_width, insert_spaces)
            } else {
                editor.outdent_lines(tab_width)
            }
        };
        match result {
            Ok(0) => self.status(if indent {
                "No lines to indent"
            } else {
                "No indentation to remove"
            }),
            Ok(lines) => self.status(format!(
                "{} {lines} line{}",
                if indent { "Indented" } else { "Outdented" },
                if lines == 1 { "" } else { "s" }
            )),
            Err(error) => self.error(error.to_string()),
        }
    }

    fn select_lines(&mut self) {
        let lines = self.workspace.active_mut().select_lines();
        self.status(format!(
            "Selected {lines} line{}",
            if lines == 1 { "" } else { "s" }
        ));
    }

    fn close_other_buffers(&mut self) {
        let active_id = self.workspace.active().id();
        let closed_states: Vec<_> = self
            .workspace
            .buffers()
            .iter()
            .filter(|editor| editor.id() != active_id)
            .filter_map(Self::snapshot_closed_buffer)
            .collect();
        match self.workspace.close_other_buffers() {
            Ok(closed) if closed.is_empty() => self.status("No other buffers to close"),
            Ok(closed) => {
                let count = closed.len();
                for editor_id in closed {
                    self.remove_recovery_for_editor(editor_id);
                }
                for closed in closed_states {
                    self.push_closed_buffer(closed);
                }
                self.cache_active_workspace_tree_path();
                self.status(format!(
                    "Closed {count} other buffer{}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            Err(error) => self.error(error),
        }
    }

    fn collect_local_references(&self, identifier: &str) -> Result<LocalReferenceScan, String> {
        let index = self
            .project
            .index
            .as_ref()
            .ok_or_else(|| "Local references need a project root".to_owned())?;
        index
            .validate_root_identity()
            .map_err(|error| format!("Workspace root is stale: {error}"))?;

        let mut scan = LocalReferenceScan {
            targets: Vec::new(),
            skipped: 0,
            truncated_files: index.files().len() > MAX_LOCAL_REFERENCE_FILES,
            truncated_matches: false,
        };
        for relative in index.files().iter().take(MAX_LOCAL_REFERENCE_FILES) {
            if scan.targets.len() >= MAX_LOCAL_REFERENCES {
                scan.truncated_matches = true;
                break;
            }
            if crate::syntax::language_for_path(Some(relative.as_path())).is_none() {
                continue;
            }
            let Ok(bytes) = index.read_indexed_file(relative, MAX_LOCAL_REFERENCE_FILE_BYTES)
            else {
                scan.skipped = scan.skipped.saturating_add(1);
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                scan.skipped = scan.skipped.saturating_add(1);
                continue;
            };
            let document = Document::from_text(&text);
            let absolute = index.absolute_path(relative);
            let display_path = safe_tree_path(relative);
            for (line_index, line) in text.lines().enumerate() {
                for char_column in identifier_columns_in_line(line, identifier) {
                    if scan.targets.len() >= MAX_LOCAL_REFERENCES {
                        scan.truncated_matches = true;
                        break;
                    }
                    let line_start = document.line_start_char(line_index);
                    let line_end = document.line_end_char(line_index);
                    let cursor = line_start.saturating_add(char_column).min(line_end);
                    scan.targets.push((
                        format!(
                            "{}:{:<4}:{}  {}",
                            display_path,
                            line_index + 1,
                            char_column + 1,
                            line.trim()
                        ),
                        JumpLocation {
                            editor_id: 0,
                            path: Some(absolute.clone()),
                            cursor,
                        },
                    ));
                }
                if scan.truncated_matches {
                    break;
                }
            }
        }

        index.validate_root_identity().map_err(|error| {
            format!("Workspace root changed during local references scan: {error}")
        })?;
        Ok(scan)
    }

    fn collect_source_annotations(&self) -> Result<SourceAnnotationScan, String> {
        let index = self
            .project
            .index
            .as_ref()
            .ok_or_else(|| "Source annotations need a project root".to_owned())?;
        index
            .validate_root_identity()
            .map_err(|error| format!("Workspace root is stale: {error}"))?;

        let mut scan = SourceAnnotationScan {
            targets: Vec::new(),
            skipped: 0,
            truncated_files: index.files().len() > MAX_SOURCE_ANNOTATION_FILES,
            truncated_matches: false,
        };
        for relative in index.files().iter().take(MAX_SOURCE_ANNOTATION_FILES) {
            if scan.targets.len() >= MAX_SOURCE_ANNOTATIONS {
                scan.truncated_matches = true;
                break;
            }
            if crate::syntax::language_for_path(Some(relative.as_path())).is_none() {
                continue;
            }
            let Ok(bytes) = index.read_indexed_file(relative, MAX_SOURCE_ANNOTATION_FILE_BYTES)
            else {
                scan.skipped = scan.skipped.saturating_add(1);
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                scan.skipped = scan.skipped.saturating_add(1);
                continue;
            };
            let document = Document::from_text(&text);
            let absolute = index.absolute_path(relative);
            let display_path = safe_tree_path(relative);
            for (line_index, line) in text.lines().enumerate() {
                for (tag, char_column) in annotation_columns_in_line(line) {
                    if scan.targets.len() >= MAX_SOURCE_ANNOTATIONS {
                        scan.truncated_matches = true;
                        break;
                    }
                    let line_start = document.line_start_char(line_index);
                    let line_end = document.line_end_char(line_index);
                    let cursor = line_start.saturating_add(char_column).min(line_end);
                    scan.targets.push((
                        format!(
                            "{}:{:<4}:{}  {tag:<5} {}",
                            display_path,
                            line_index + 1,
                            char_column + 1,
                            line.trim()
                        ),
                        JumpLocation {
                            editor_id: 0,
                            path: Some(absolute.clone()),
                            cursor,
                        },
                    ));
                }
                if scan.truncated_matches {
                    break;
                }
            }
        }

        index.validate_root_identity().map_err(|error| {
            format!("Workspace root changed during source annotations scan: {error}")
        })?;
        Ok(scan)
    }

    fn collect_workspace_outline(
        &self,
        identifier_filter: Option<&str>,
    ) -> Result<WorkspaceOutlineScan, String> {
        let index = self
            .project
            .index
            .as_ref()
            .ok_or_else(|| "Workspace outline needs a project root".to_owned())?;
        index
            .validate_root_identity()
            .map_err(|error| format!("Workspace root is stale: {error}"))?;

        let mut scan = WorkspaceOutlineScan {
            targets: Vec::new(),
            skipped: 0,
            truncated_files: index.files().len() > MAX_WORKSPACE_OUTLINE_FILES,
            truncated_symbols: false,
        };

        for relative in index.files().iter().take(MAX_WORKSPACE_OUTLINE_FILES) {
            if scan.targets.len() >= MAX_WORKSPACE_OUTLINE_SYMBOLS {
                scan.truncated_symbols = true;
                break;
            }
            if crate::syntax::language_for_path(Some(relative.as_path())).is_none() {
                continue;
            }
            let Ok(bytes) = index.read_indexed_file(relative, MAX_WORKSPACE_OUTLINE_FILE_BYTES)
            else {
                scan.skipped = scan.skipped.saturating_add(1);
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                scan.skipped = scan.skipped.saturating_add(1);
                continue;
            };
            let symbols = crate::syntax::outline_symbols(Some(relative.as_path()), &text);
            if symbols.is_empty() {
                continue;
            }
            let document = Document::from_text(&text);
            let absolute = index.absolute_path(relative);
            let display_path = safe_tree_path(relative);
            for symbol in symbols {
                if scan.targets.len() >= MAX_WORKSPACE_OUTLINE_SYMBOLS {
                    scan.truncated_symbols = true;
                    break;
                }
                if identifier_filter
                    .is_some_and(|identifier| !text_mentions_identifier(&symbol.label, identifier))
                {
                    continue;
                }
                let line_start = document.line_start_char(symbol.line);
                let line_end = document.line_end_char(symbol.line);
                let cursor = line_start.saturating_add(symbol.char_column).min(line_end);
                scan.targets.push((
                    format!("{}:{:<4} {}", display_path, symbol.line + 1, symbol.label),
                    JumpLocation {
                        editor_id: 0,
                        path: Some(absolute.clone()),
                        cursor,
                    },
                ));
            }
        }

        index
            .validate_root_identity()
            .map_err(|error| format!("Workspace root changed during outline scan: {error}"))?;
        Ok(scan)
    }

    fn request_lsp_formatting(&mut self) {
        let Some((editor_id, _, uri, version, incarnation, state_id, _)) =
            self.lsp_request_context()
        else {
            return;
        };
        let context = LspDocumentRequestContext {
            editor_id,
            uri: uri.clone(),
            version,
            incarnation,
            state_id,
        };
        match self
            .lsp
            .client
            .as_mut()
            .expect("LSP context requires client")
            .request_formatting(
                uri,
                version,
                self.config.tab_width,
                self.config.insert_spaces,
            ) {
            Ok(request_id) => {
                self.lsp
                    .requests
                    .insert(request_id, PendingLspRequest::Formatting { context });
                self.status("Requesting document formatting…");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn show_fixed_prompt(
        &mut self,
        kind: PromptFlow,
        candidates: impl IntoIterator<Item = (String, PromptEntry)>,
    ) {
        let editor = self.workspace.active();
        let mut prompt = Prompt::new(kind, String::new(), editor.cursor, editor.anchor);
        (prompt.all_labels, prompt.all_entries) = candidates.into_iter().unzip();
        prompt.labels.clone_from(&prompt.all_labels);
        prompt.entries.clone_from(&prompt.all_entries);
        self.ui.mode = UiMode::Prompt(prompt);
    }

    fn show_location_prompt(&mut self, kind: PromptFlow, locations: Vec<Location>) {
        let candidates = locations.into_iter().map(|location| {
            let path = file_uri_to_path(&location.uri)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| location.uri.clone());
            (
                format!(
                    "{}:{}:{}",
                    path,
                    location.range.start.line.get().saturating_add(1),
                    location.range.start.character.get().saturating_add(1)
                ),
                PromptEntry::Location(location),
            )
        });
        self.show_fixed_prompt(kind, candidates);
    }

    fn open_problems(&mut self) {
        let (candidates, notice) = self.problem_candidates();
        if candidates.is_empty() {
            if let Some(notice) = notice {
                self.error(notice);
            } else {
                self.status("No language-server or task problems");
            }
            return;
        }
        self.show_fixed_prompt(PromptFlow::Problems, candidates);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = notice;
        }
    }

    fn problem_candidates(&self) -> (Vec<(String, PromptEntry)>, Option<String>) {
        let mut candidates = Vec::new();
        let mut notice = None;
        let display_root = std::fs::canonicalize(&self.workspace.root)
            .unwrap_or_else(|_| self.workspace.root.clone());

        if let (Some(task_cwd), Some(task_name)) = (&self.tasks.cwd, &self.tasks.last)
            && !self.tasks.output.is_empty()
        {
            match parse_task_problems(&self.tasks.output, task_cwd, &self.workspace.root) {
                Ok(report) => {
                    if report.truncated {
                        merge_problem_notice(
                            &mut notice,
                            format!(
                                "Task problem scan is partial: a safety limit was reached ({} problems from {} candidates; {} lines / {} bytes scanned)",
                                report.problems.len(),
                                report.candidates_checked,
                                report.scanned_lines,
                                report.scanned_bytes
                            ),
                        );
                    }
                    for problem in report.problems {
                        let display_path = problem
                            .path
                            .strip_prefix(&display_root)
                            .unwrap_or(&problem.path);
                        candidates.push((
                            format!(
                                "T {} {}:{}:{} [{task_name}]  {}",
                                task_problem_marker(problem.severity),
                                display_path.display(),
                                problem.line + 1,
                                problem.column + 1,
                                problem.message
                            ),
                            PromptEntry::TaskProblem(problem),
                        ));
                    }
                }
                Err(error) => {
                    merge_problem_notice(
                        &mut notice,
                        format!("Task problems unavailable: {error}"),
                    );
                }
            }
        }

        let mut lsp_candidates = Vec::new();
        let lsp_capacity = MAX_PROBLEM_CANDIDATES.saturating_sub(candidates.len());
        let mut lsp_truncated = false;
        if self.lsp.documents.is_partial() || self.lsp.diagnostics.is_partial() {
            merge_problem_notice(
                &mut notice,
                "Language-server Problems are partial because a synchronization or diagnostic safety limit was reached",
            );
        }
        'documents: for (_, diagnostics) in self.lsp.diagnostics.iter() {
            for diagnostic in diagnostics {
                if lsp_candidates.len() >= lsp_capacity {
                    lsp_truncated = true;
                    break 'documents;
                }
                let path = file_uri_to_path(&diagnostic.uri)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| diagnostic.uri.clone());
                let source = diagnostic
                    .source
                    .as_ref()
                    .map(|source| format!(" [{source}]"))
                    .unwrap_or_default();
                let location = Location {
                    uri: diagnostic.uri.clone(),
                    range: diagnostic.range,
                };
                lsp_candidates.push((
                    format!(
                        "L {} {}:{}:{}{}  {}",
                        diagnostic.severity.marker(),
                        path,
                        diagnostic.range.start.line.get().saturating_add(1),
                        diagnostic.range.start.character.get().saturating_add(1),
                        source,
                        diagnostic.message
                    ),
                    PromptEntry::ProblemLocation(location, diagnostic.severity),
                ));
            }
        }
        lsp_candidates.sort_by(|left, right| left.0.cmp(&right.0));
        candidates.extend(lsp_candidates);
        if lsp_truncated {
            merge_problem_notice(
                &mut notice,
                format!(
                    "Problems list is partial at its {MAX_PROBLEM_CANDIDATES}-entry safety limit"
                ),
            );
        }

        (candidates, notice)
    }

    fn navigate_problem(&mut self, forward: bool, errors_only: bool) {
        let (candidates, notice) = self.problem_candidates();
        let mut navigable = candidates
            .into_iter()
            .filter(|(_, entry)| !errors_only || problem_entry_is_error(entry))
            .filter_map(|(_, entry)| {
                problem_entry_position(&entry).map(|(path, line, column)| {
                    ProblemNavigationCandidate {
                        path,
                        line,
                        column,
                        entry,
                    }
                })
            })
            .collect::<Vec<_>>();
        if navigable.is_empty() {
            if let Some(notice) = notice {
                self.error(notice);
            } else if errors_only {
                self.status("No navigable language-server or task errors");
            } else {
                self.status("No navigable language-server or task problems");
            }
            return;
        }
        navigable.sort_by(|left, right| {
            (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
        });
        let active_key = self
            .workspace
            .active()
            .document
            .path()
            .map(crate::workspace::normalized_file_path)
            .map(|path| {
                let position = self.workspace.active().position(self.config.tab_width);
                (path, position.line, position.char_column)
            });
        let selected = if let Some((active_path, active_line, active_column)) = active_key {
            if forward {
                navigable
                    .iter()
                    .position(|candidate| {
                        (&candidate.path, candidate.line, candidate.column)
                            > (&active_path, active_line, active_column)
                    })
                    .unwrap_or(0)
            } else {
                navigable
                    .iter()
                    .rposition(|candidate| {
                        (&candidate.path, candidate.line, candidate.column)
                            < (&active_path, active_line, active_column)
                    })
                    .unwrap_or_else(|| navigable.len().saturating_sub(1))
            }
        } else if forward {
            0
        } else {
            navigable.len().saturating_sub(1)
        };
        match navigable.swap_remove(selected).entry {
            PromptEntry::Location(location) | PromptEntry::ProblemLocation(location, _) => {
                self.open_lsp_location(location)
            }
            PromptEntry::TaskProblem(problem) => self.open_task_problem(problem),
            _ => unreachable!("problem navigation only stores problem entries"),
        }
    }

    fn open_lsp_location(&mut self, location: Location) {
        let origin = self.current_jump_location();
        let path = match file_uri_to_path(&location.uri) {
            Ok(path) => path,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if !path.is_file() {
            self.error(format!("LSP target does not exist: {}", path.display()));
            return;
        }

        // Validate the position against the text that would be navigated
        // before changing the active buffer. Dirty open buffers are
        // authoritative; unopened files use the same bounded Document loader
        // as a real open. We validate again after activation to close the
        // small disk-change window between the two operations.
        let candidate_text = if let Some(editor) = self.workspace.editor_by_path(&path) {
            editor.document.text()
        } else {
            match Document::open(&path) {
                Ok(document) => document.text(),
                Err(error) => {
                    self.error(error.to_string());
                    return;
                }
            }
        };
        let candidate_snapshot =
            DocumentSnapshot::from_text(&candidate_text, DocumentVersion::INITIAL);
        if let Err(error) = candidate_snapshot.position_to_char(location.range.start) {
            self.error(format!(
                "Stale LSP location for {}: {error}",
                path.display()
            ));
            return;
        }

        match self.workspace.open(&path) {
            Ok(_) => {
                self.cache_active_workspace_tree_path();
                self.record_active_file_recent();
                let text = self.workspace.active().document.text();
                let snapshot = DocumentSnapshot::from_text(&text, DocumentVersion::INITIAL);
                match snapshot.position_to_char(location.range.start) {
                    Ok(offset) => {
                        self.workspace.active_mut().set_cursor(offset.get(), false);
                        self.record_jump_origin(origin);
                        self.status(format!(
                            "{}:{}:{}",
                            path.display(),
                            location.range.start.line.get().saturating_add(1),
                            location.range.start.character.get().saturating_add(1)
                        ));
                    }
                    Err(error) => {
                        if let Some(index) = self.workspace.editor_index(origin.editor_id) {
                            self.workspace.activate(index);
                        }
                        self.error(format!(
                            "LSP target changed before navigation; stayed at the origin: {error}"
                        ));
                    }
                }
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn open_task_problem(&mut self, problem: TaskProblem) {
        let root = match std::fs::canonicalize(&self.workspace.root) {
            Ok(root) => root,
            Err(error) => {
                self.error(format!("Could not resolve workspace root: {error}"));
                return;
            }
        };
        let current_path = match std::fs::canonicalize(&problem.path) {
            Ok(path)
                if path == problem.path
                    && path.starts_with(&root)
                    && path.metadata().is_ok_and(|metadata| metadata.is_file()) =>
            {
                path
            }
            Ok(_) => {
                self.error("Task problem target moved, escaped the workspace, or is not a file");
                return;
            }
            Err(error) => {
                self.error(format!("Task problem target is stale: {error}"));
                return;
            }
        };

        let origin = self.current_jump_location();
        if let Err(error) = self.workspace.open(&current_path) {
            self.error(error.to_string());
            return;
        }
        self.cache_active_workspace_tree_path();
        self.record_active_file_recent();

        let (line, actual_column, location_clamped) = {
            let mut editor = self.workspace.active_mut();
            let line = problem
                .line
                .min(editor.document.line_count().saturating_sub(1));
            let line_start = editor.document.line_start_char(line);
            let line_end = editor.document.line_end_char(line);
            let line_text = editor.document.slice(line_start..line_end);
            let (column_chars, actual_column, column_clamped) = match problem.column_kind {
                TaskProblemColumnKind::UnicodeScalar => {
                    let scalar_count = line_text.chars().count();
                    let bounded = problem.column.min(scalar_count);
                    let column_chars = char_for_scalar_column(&line_text, bounded);
                    (
                        column_chars,
                        column_chars,
                        problem.column > scalar_count || column_chars != bounded,
                    )
                }
                TaskProblemColumnKind::Unknown => {
                    let line_width = visual_width(&line_text, self.config.tab_width);
                    let column_chars =
                        char_for_visual_column(&line_text, problem.column, self.config.tab_width);
                    (
                        column_chars,
                        problem.column.min(line_width),
                        problem.column > line_width,
                    )
                }
            };
            editor.set_cursor(line_start.saturating_add(column_chars), false);
            (line, actual_column, line != problem.line || column_clamped)
        };
        self.record_jump_origin(origin);
        self.status(if location_clamped {
            format!(
                "Task problem opened at {}:{}:{} (location clamped to current buffer)",
                current_path.display(),
                line + 1,
                actual_column + 1
            )
        } else {
            format!(
                "Task problem opened at {}:{}:{}",
                current_path.display(),
                line + 1,
                problem.column + 1
            )
        });
    }

    fn apply_completion(
        &mut self,
        item: CompletionItem,
        context: LspDocumentRequestContext,
        cursor: usize,
        anchor: Option<usize>,
    ) {
        if self.workspace.active().cursor != cursor
            || self.workspace.active().anchor != anchor
            || !self.require_lsp_edit_prompt_context("Completion", &context)
        {
            if self
                .status_message()
                .is_none_or(|message| !message.contains("Completion result expired"))
            {
                self.error("Completion result expired after the cursor or selection moved");
            }
            return;
        }
        if item.is_snippet {
            self.error("Snippet completion requires placeholder support; no text was inserted");
            return;
        }
        if let Some(edit) = item.text_edit {
            let text = self.workspace.active().document.text();
            match apply_text_edits(&text, &[edit]) {
                Ok(updated) => {
                    let result = self
                        .workspace
                        .active_mut()
                        .replace_all_from_service(&updated);
                    match result {
                        Ok(()) => self.status(format!("Completed {}", item.label)),
                        Err(error) => self.error(error.to_string()),
                    }
                }
                Err(error) => self.error(format!("Invalid completion edit: {error}")),
            }
        } else {
            self.workspace.active_mut().set_cursor(cursor, false);
            let result = self
                .workspace
                .active_mut()
                .insert(&item.insert_text, EditKind::Replace);
            match result {
                Ok(()) => self.status(format!("Completed {}", item.label)),
                Err(error) => self.error(error.to_string()),
            }
        }
    }

    fn append_lsp_log(&mut self, text: &str) {
        self.lsp.log.push_str(text);
        trim_bounded_text(
            &mut self.lsp.log,
            256 * 1024,
            "[… earlier LSP log trimmed …]\n",
        );
        self.workspace
            .update_virtual("LSP Log", &self.lsp.log, true);
    }

    fn open_lsp_log(&mut self) {
        if self.lsp.log.is_empty() {
            self.status("Language-server log is empty");
        } else {
            self.workspace.open_virtual("LSP Log", &self.lsp.log);
            self.status("Language-server log opened as a read-only IDE view");
        }
    }

    fn restart_lsp(&mut self) {
        if let Some(mut client) = self.lsp.client.take() {
            let _ = client.shutdown();
        }
        self.lsp.server_name = None;
        self.lsp.failed_server = None;
        self.lsp.workspace_symbols = None;
        self.lsp.text_document_sync = None;
        self.lsp.documents.clear();
        self.lsp.document_incarnations.clear();
        self.lsp.document_ends.clear();
        self.lsp.discovery_cursor = 0;
        self.lsp.sync_cursor = 0;
        self.lsp.background_sync_due = false;
        self.lsp.quarantined_documents.clear();
        self.lsp.next_document_version = 1;
        self.lsp.ambiguous_diagnostic_uris.clear();
        self.lsp.all_versionless_diagnostics_ambiguous = false;
        self.lsp.diagnostics.clear();
        self.lsp.requests.clear();
        self.lsp.deferred_event = None;
        self.dismiss_workspace_symbol_prompt();
        if !self.ensure_lsp_service() {
            self.error_no_language_server_configured();
        }
    }

    fn lsp_footer_suffix(&self) -> String {
        if let Some(name) = self.lsp.server_name.as_deref() {
            let phase = if self
                .lsp
                .client
                .as_ref()
                .is_some_and(|client| client.is_ready())
            {
                "ready"
            } else {
                "starting"
            };
            return format!(" · LSP {name} ({phase}) ");
        }
        if let Some(path) = self.workspace.active().document.path() {
            if self.config.language_server_for(path).is_some() {
                return " · LSP retry Esc c R ".to_owned();
            }
            if crate::lsp_discover::discovered_for_path(path).is_some() {
                return " · LSP on PATH — authorize in config ".to_owned();
            }
        }
        String::new()
    }

    fn error_no_language_server_configured(&mut self) {
        if let Some(path) = self.workspace.active().document.path()
            && let Some(discovered) = crate::lsp_discover::discovered_for_path(path)
        {
            self.error(format!(
                "No language server authorized for this file; {name} is on PATH — run wscrpt --print-default-config and uncomment it in ~/.config/wscrpt/config.toml",
                name = discovered.name
            ));
            return;
        }
        self.error(
            "No language server is configured for the active file — run wscrpt --health and authorize one in ~/.config/wscrpt/config.toml",
        );
    }

    fn error_no_ready_language_server(&mut self) {
        if let Some(path) = self.workspace.active().document.path() {
            if self.config.language_server_for(path).is_some() {
                self.error(
                    "Language server is configured but not ready — wait for startup or Esc c R to restart",
                );
                return;
            }
            if let Some(discovered) = crate::lsp_discover::discovered_for_path(path) {
                self.error(format!(
                    "No ready language server; {name} is on PATH but not authorized — wscrpt --print-default-config",
                    name = discovered.name
                ));
                return;
            }
            let detail = path
                .extension()
                .map(|extension| format!(" for .{} files", extension.to_string_lossy()))
                .unwrap_or_default();
            self.error(format!(
                "No ready language server{detail}; configure one in ~/.config/wscrpt/config.toml"
            ));
            return;
        }
        self.error("No ready language server for this buffer");
    }

    fn request_task(&mut self, name: String) {
        let Some(runner) = &self.tasks.runner else {
            self.error(format!(
                "No {} file with runnable tasks",
                crate::tasks::TASK_FILE_RELATIVE_PATH
            ));
            return;
        };
        if runner.config().get(&name).is_none() {
            self.error(format!("Unknown task {name:?}"));
            return;
        }
        if self.tasks.is_running() {
            self.error("A task is already running; stop it before starting another");
            return;
        }
        self.ui.mode = UiMode::TaskTrust(name);
    }

    fn request_default_task(&mut self) {
        let Some(runner) = &self.tasks.runner else {
            self.error(format!(
                "No {} file with runnable tasks",
                crate::tasks::TASK_FILE_RELATIVE_PATH
            ));
            return;
        };
        if self.tasks.is_running() {
            self.error("A task is already running; stop it before starting another");
            return;
        }
        let tasks = runner.config().tasks();
        let preferred = ["check", "test", "build", "lint", "fmt"]
            .into_iter()
            .find(|name| tasks.contains_key(*name))
            .map(str::to_owned);
        let selected = preferred.or_else(|| {
            if tasks.len() == 1 {
                tasks.keys().next().cloned()
            } else {
                None
            }
        });
        if let Some(name) = selected {
            self.ui.mode = UiMode::TaskTrust(name);
        } else if tasks.is_empty() {
            self.status("No configured tasks");
        } else {
            self.begin_prompt(PromptFlow::Tasks);
            self.status("No conventional default task; choose a task");
        }
    }

    fn append_task_output_chunk(&mut self, stream: OutputStream, bytes: &[u8]) {
        let decoded = self.tasks.output_decoder.push(stream, bytes);
        self.tasks.output.push_str(&decoded);
    }

    fn finish_task_output_decoders(&mut self) {
        for stream in [OutputStream::Stdout, OutputStream::Stderr] {
            let pending = self.tasks.output_decoder.finish(stream);
            self.tasks.output.push_str(&pending);
        }
    }

    fn start_task(&mut self, name: &str) {
        let Some(runner) = &self.tasks.runner else {
            self.error("Task runner is unavailable");
            self.ui.mode = UiMode::Edit;
            return;
        };
        match runner.start(name, WorkspaceTrust::Trusted) {
            Ok(handle) => {
                let task_cwd = handle.cwd().to_path_buf();
                self.tasks.output_decoder.reset();
                self.tasks.output = format!(
                    "$ {}\n\n",
                    runner
                        .config()
                        .get(name)
                        .map(|task| task.argv().join(" "))
                        .unwrap_or_else(|| name.to_owned())
                );
                self.tasks.cwd = Some(task_cwd);
                self.tasks.handle = Some(handle);
                self.tasks.last = Some(name.to_owned());
                self.ui.mode = UiMode::Edit;
                self.status(format!("Task {name:?} started — Esc t o shows output"));
            }
            Err(error) => {
                self.ui.mode = UiMode::Edit;
                self.error(error.to_string());
            }
        }
    }

    fn stop_task(&mut self) {
        let Some(handle) = &self.tasks.handle else {
            self.status("No task is running");
            return;
        };
        match handle.cancel() {
            Ok(result) => self.status(format!("Task cancellation: {result:?}")),
            Err(error) => self.error(format!("Could not cancel task: {error}")),
        }
    }

    fn open_task_output(&mut self) {
        if self.tasks.output.is_empty() {
            self.status("No task output yet — Esc t r opens the task picker");
            return;
        }
        self.workspace
            .open_virtual("Task Output", &self.tasks.output);
        self.status("Task output opened; Esc t s stops a running task");
    }

    fn open_task_catalog(&mut self) {
        let Some(runner) = &self.tasks.runner else {
            self.error(format!(
                "No {} file with runnable tasks",
                crate::tasks::TASK_FILE_RELATIVE_PATH
            ));
            return;
        };
        let config = runner.config();
        let task_count = config.tasks().len();
        let mut text = format!(
            "Task Catalog\n\nWorkspace: {}\nTask file: {}\nConfigured tasks: {}\n\n",
            self.workspace.root.display(),
            self.workspace
                .root
                .join(crate::tasks::TASK_FILE_RELATIVE_PATH)
                .display(),
            task_count
        );
        text.push_str("Execution model\n");
        text.push_str(
            "- Selecting a task from Esc t r or :task NAME still requires one-time trust.\n",
        );
        text.push_str("- argv is executed directly without an inserted shell.\n");
        text.push_str("- Use Esc t i / :task-catalog to inspect this list; use :task-info NAME for one full task.\n\n");

        for (name, task) in config.tasks() {
            let cwd = task
                .cwd()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(workspace root)".to_owned());
            let command = task
                .argv()
                .first()
                .map(String::as_str)
                .unwrap_or("(invalid)");
            text.push_str(&format!("{name}\n"));
            text.push_str(&format!("  command: {command:?}\n"));
            text.push_str(&format!("  argv count: {}\n", task.argv().len()));
            text.push_str(&format!("  cwd: {cwd}\n"));
            text.push_str(&format!("  env overrides: {}\n", task.env().len()));
            text.push_str(&format!("  details: :task-info {name}\n\n"));
        }

        self.workspace.open_virtual("Task Catalog", &text);
        self.status(format!(
            "{task_count} configured task(s) opened read-only; no task was started"
        ));
    }

    fn open_task_details(&mut self, name: &str) {
        let Some(runner) = &self.tasks.runner else {
            self.error(format!(
                "No {} file with runnable tasks",
                crate::tasks::TASK_FILE_RELATIVE_PATH
            ));
            return;
        };
        let Some(task) = runner.config().get(name) else {
            self.error(format!("Unknown task {name:?}"));
            return;
        };

        let cwd_label = task
            .cwd()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(workspace root)".to_owned());
        let resolved_cwd = task.cwd().map_or_else(
            || self.workspace.root.clone(),
            |path| {
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.workspace.root.join(path)
                }
            },
        );

        let mut text = format!("Task Details: {name}\n\n");
        text.push_str("Execution model\n");
        text.push_str("- Workspace-provided code; running still requires one-time trust.\n");
        text.push_str("- argv is passed directly to the OS; no shell is inserted or reparsed.\n");
        text.push_str(
            "- stdin is null; stdout/stderr are captured into the bounded Task Output view.\n\n",
        );

        text.push_str("Command argv\n");
        for (index, argument) in task.argv().iter().enumerate() {
            text.push_str(&format!("  [{index}] {argument:?}\n"));
        }

        text.push_str("\nWorking directory\n");
        text.push_str(&format!("  configured: {cwd_label}\n"));
        text.push_str(&format!("  resolved: {}\n", resolved_cwd.display()));

        text.push_str("\nEnvironment overrides\n");
        if task.env().is_empty() {
            text.push_str("  (none)\n");
        } else {
            for (key, value) in task.env() {
                text.push_str(&format!("  {key}={value:?}\n"));
            }
        }

        text.push_str("\nNext step\n");
        text.push_str("  Reopen the task picker or run :task NAME, then press Y at the trust prompt to execute once.\n");

        self.workspace
            .open_virtual(format!("Task Details: {name}"), &text);
        self.status(format!(
            "Task {name:?} details opened read-only; no task was started"
        ));
    }

    fn open_keymap_reference(&mut self) {
        let mut commands_by_namespace: BTreeMap<keymap::Namespace, Vec<&keymap::Command>> =
            BTreeMap::new();
        for command in keymap::COMMANDS {
            commands_by_namespace
                .entry(command.namespace)
                .or_default()
                .push(command);
        }

        let mut text = String::new();
        text.push_str("Keymap Reference\n\n");
        text.push_str("This read-only view is generated from the authoritative command registry. It is the current shortcut contract, not a hand-maintained help copy.\n\n");
        text.push_str("Action layer\n");
        text.push_str("- Press `Esc` or `Ctrl-K`, then the listed sequence. Prefixes wait without a timeout.\n");
        text.push_str(
            "- `Esc h` opens compact help; `Esc Space` opens the searchable command palette.\n",
        );
        text.push_str(
            "- `Ctrl-G` cancels prompts/actions/selections; `Ctrl-L` requests a full redraw.\n\n",
        );

        for namespace in [
            keymap::Namespace::Core,
            keymap::Namespace::Workspace,
            keymap::Namespace::Code,
            keymap::Namespace::Tasks,
            keymap::Namespace::VersionControl,
        ] {
            let Some(commands) = commands_by_namespace.get(&namespace) else {
                continue;
            };
            text.push_str(namespace.title());
            text.push('\n');
            text.push_str("  Sequence        Command                         ID\n");
            text.push_str(
                "  ─────────────── ─────────────────────────────── ─────────────────────────\n",
            );
            for command in commands {
                text.push_str(&format!(
                    "  Esc {:<11} {:<31} {}\n",
                    command.sequence, command.title, command.id
                ));
                if !command.keywords.is_empty() {
                    text.push_str(&format!("  keywords: {}\n", command.keywords.join(", ")));
                }
            }
            text.push('\n');
        }

        text.push_str("Command line aliases\n");
        text.push_str(
            "- `:keys`, `:keymap`, `:shortcuts`, and `:bindings` reopen this reference.\n",
        );
        text.push_str("- `:help`, `:h`, and `:?` open compact help.\n");
        text.push_str("- Use `Esc Space` when you want fuzzy search and immediate execution.\n");

        self.workspace.open_virtual("Keymap Reference", &text);
        self.status(format!(
            "{} command shortcut(s) opened read-only",
            keymap::COMMANDS.len()
        ));
    }

    fn open_workspace_info(&mut self) {
        let active = self.workspace.active();
        let position = active.position(self.config.tab_width);
        let active_path = active
            .document
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(untitled or virtual)".to_owned());
        let active_kind = if active.document.is_read_only() {
            "read-only IDE view"
        } else if active.document.path().is_some() {
            "file"
        } else {
            "untitled"
        };
        let modified_buffers = self.workspace.modified_count();
        let total_buffers = self.workspace.buffers().len();
        let (text_files, tree_entries, text_partial, tree_partial) = self
            .project
            .index
            .as_ref()
            .map(|index| {
                (
                    index.len(),
                    index.tree_entries().len(),
                    index.is_truncated(),
                    index.is_tree_truncated(),
                )
            })
            .unwrap_or((0, 0, true, true));
        let task_catalog = self
            .tasks
            .runner
            .as_ref()
            .map(|runner| runner.config().tasks().len())
            .unwrap_or(0);
        let task_state = self
            .tasks
            .handle
            .as_ref()
            .map(|handle| format!("{:?}", handle.state()))
            .unwrap_or_else(|| "none".to_owned());
        let last_task = self.tasks.last.as_deref().unwrap_or("none");
        let git = self
            .git
            .branch
            .as_ref()
            .map(|branch| {
                if self.git.changes == 0 {
                    branch.clone()
                } else {
                    format!("{branch} +{}", self.git.changes)
                }
            })
            .unwrap_or_else(|| {
                if self.git.repository.is_some() {
                    "unknown".to_owned()
                } else {
                    "not a Git repository".to_owned()
                }
            });
        let lsp = self
            .lsp
            .server_name
            .as_deref()
            .map_or_else(|| "none".to_owned(), str::to_owned);
        let lsp_symbols = match self.lsp.workspace_symbols {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        };
        let lsp_sync = self
            .lsp
            .text_document_sync
            .map(|sync| match sync {
                TextDocumentSyncCapability {
                    full: true,
                    incremental: true,
                    ..
                } => "full+incremental",
                TextDocumentSyncCapability {
                    full: true,
                    incremental: false,
                    ..
                } => "full",
                TextDocumentSyncCapability {
                    full: false,
                    incremental: true,
                    ..
                } => "incremental",
                _ => "unsupported",
            })
            .unwrap_or("unknown");
        let lsp_partial = self.lsp.documents.is_partial() || self.lsp.diagnostics.is_partial();
        let active_diagnostics = self.active_diagnostics().len();
        let mut text = String::new();
        text.push_str("Workspace Info\n\n");
        text.push_str(&format!("Root: {}\n", self.workspace.root.display()));
        text.push_str(&format!(
            "Terminal: {cols}x{rows}  soft-wrap={wrap}  mouse={mouse}\n",
            cols = self.ui.screen_size.0,
            rows = self.ui.screen_size.1,
            wrap = if self.ui.soft_wrap { "on" } else { "off" },
            mouse = if self.config.mouse { "on" } else { "off" }
        ));
        text.push_str(&format!(
            "Route snapshot: TERM={} transport={} tmux={} osc52={}\n",
            env_value("TERM", "unknown"),
            route_transport(),
            if env::var_os("TMUX").is_some() {
                "yes"
            } else {
                "no"
            },
            self.ui.clipboard.osc52_route_label()
        ));
        text.push_str("\nActive buffer\n");
        text.push_str(&format!("Name: {}\n", active.document.display_name()));
        text.push_str(&format!("Kind: {active_kind}\n"));
        text.push_str(&format!("Path: {active_path}\n"));
        text.push_str(&format!(
            "Cursor: line {} column {}  chars={} lines={}  line-ending={}\n",
            position.line + 1,
            position.char_column + 1,
            active.document.len_chars(),
            active.document.line_count(),
            active.document.line_ending().label()
        ));
        text.push_str(&format!(
            "Dirty: {}\n",
            if active.document.is_modified() {
                "yes"
            } else {
                "no"
            }
        ));
        text.push_str("\nWorkspace state\n");
        text.push_str(&format!(
            "Buffers: {total_buffers} open, {modified_buffers} dirty\n"
        ));
        text.push_str(&format!(
            "Snapshots: {text_files} text files{} · {tree_entries} tree entries{}\n",
            if text_partial { " (partial)" } else { "" },
            if tree_partial { " (partial)" } else { "" }
        ));
        text.push_str(&format!(
            "Recovery journals: {}\n",
            self.persistence.recovery_records.len()
        ));
        text.push_str(&format!("Git: {git}\n"));
        text.push_str("\nTasks\n");
        text.push_str(&format!("Configured tasks: {task_catalog}\n"));
        text.push_str(&format!("Last task: {last_task}\n"));
        text.push_str(&format!("Task state: {task_state}\n"));
        text.push_str(&format!(
            "Captured task output bytes: {}\n",
            self.tasks.output.len()
        ));
        text.push_str("\nLanguage server\n");
        text.push_str(&format!("Server: {lsp}\n"));
        text.push_str(&format!(
            "Synchronized documents: {} / {}{}\n",
            self.lsp.documents.len(),
            MAX_SYNCHRONIZED_DOCUMENTS,
            if self.lsp.documents.is_partial() {
                " (partial)"
            } else {
                ""
            }
        ));
        text.push_str(&format!("Sync mode: {lsp_sync}\n"));
        text.push_str(&format!("Workspace symbols: {lsp_symbols}\n"));
        text.push_str(&format!(
            "Diagnostics: {} retained in {} URI bucket{}{}; active buffer has {active_diagnostics}\n",
            self.lsp.diagnostics.diagnostic_count(),
            self.lsp.diagnostics.len(),
            if self.lsp.diagnostics.len() == 1 { "" } else { "s" },
            if lsp_partial { " (partial)" } else { "" }
        ));
        text.push_str("\nHardware gate\n");
        text.push_str("This view is an in-editor context snapshot. Use `wscrpt --health` and `wscrpt --input-diagnostics` inside the exact iPad/Blink/mosh/SSH/tmux route for release evidence.\n");
        self.workspace.open_virtual("Workspace Info", &text);
        self.status("Workspace info opened as a read-only IDE view");
    }

    fn open_buffer_info(&mut self) {
        let active = self.workspace.active();
        let document = &active.document;
        let position = active.position(self.config.tab_width);
        let path = document.path().map(Path::to_path_buf);
        let display_path = path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(untitled or virtual)".to_owned());
        let kind = if document.is_read_only() {
            "read-only IDE view"
        } else if path.is_some() {
            "file"
        } else {
            "untitled"
        };
        let selection = active.selection();
        let selection_label = selection.as_ref().map_or_else(
            || "none".to_owned(),
            |range| {
                format!(
                    "{} chars ({}..{})",
                    range.end - range.start,
                    range.start,
                    range.end
                )
            },
        );
        let syntax = crate::syntax::language_for_path(path.as_deref())
            .map(|language| format!("{language:?}"))
            .unwrap_or_else(|| "none".to_owned());
        let disk = path.as_ref().map_or_else(
            || "not file-backed".to_owned(),
            |path| match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => format!(
                    "{} bytes, readonly={}",
                    metadata.len(),
                    metadata.permissions().readonly()
                ),
                Ok(metadata) => format!("not a regular file ({} bytes)", metadata.len()),
                Err(error) => format!("metadata unavailable: {error}"),
            },
        );
        let configured_lsp = path
            .as_ref()
            .and_then(|path| self.config.language_server_for(path))
            .map(|server| format!("{} ({})", server.name, server.language_id))
            .unwrap_or_else(|| "none".to_owned());
        let synchronized_lsp = self
            .lsp
            .documents
            .get_by_editor_id(active.id())
            .map(|document| {
                format!(
                    "yes: version={} state={}",
                    document.version.get(),
                    document.state_id
                )
            })
            .unwrap_or_else(|| "no".to_owned());
        let diagnostics = self.active_diagnostics();
        let diagnostic_errors = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
            .count();
        let diagnostic_warnings = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning)
            .count();
        let diagnostic_info = diagnostics
            .iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic.severity,
                    DiagnosticSeverity::Information | DiagnosticSeverity::Hint
                )
            })
            .count();
        let git = match (&self.git.repository, &path) {
            (Some(repository), Some(path)) => {
                let repo_path = repository
                    .relative_path(path)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|error| format!("not inside repository: {error}"));
                match repository.status_path(path) {
                    Ok(Some(file)) => format!(
                        "{}  index={} ({}) worktree={} ({})",
                        repo_path,
                        git_state_glyph(file.index),
                        git_state_label(file.index),
                        git_state_glyph(file.worktree),
                        git_state_label(file.worktree)
                    ),
                    Ok(None) => format!("{repo_path}  clean or untracked-not-reported"),
                    Err(error) => format!("{repo_path}  status unavailable: {error}"),
                }
            }
            (Some(_), None) => "not file-backed".to_owned(),
            (None, _) => "not a Git repository".to_owned(),
        };

        let mut text = String::new();
        text.push_str("Buffer Info\n\n");
        text.push_str("Identity\n");
        text.push_str(&format!("Name: {}\n", document.display_name()));
        text.push_str(&format!("Kind: {kind}\n"));
        text.push_str(&format!("Path: {display_path}\n"));
        text.push_str(&format!("Editor id: {}\n", active.id()));
        text.push_str("\nText state\n");
        text.push_str(&format!(
            "Cursor: line {} column {} visual-column {}\n",
            position.line + 1,
            position.char_column + 1,
            position.visual_column + 1
        ));
        text.push_str(&format!("Selection: {selection_label}\n"));
        text.push_str(&format!(
            "Size: {} chars, {} bytes, {} lines\n",
            document.len_chars(),
            document.len_bytes(),
            document.line_count()
        ));
        text.push_str(&format!(
            "Dirty: {}  read-only: {}  line-ending: {}  utf8-bom: {}\n",
            if document.is_modified() { "yes" } else { "no" },
            if document.is_read_only() { "yes" } else { "no" },
            document.line_ending().label(),
            if document.has_utf8_bom() { "yes" } else { "no" }
        ));
        text.push_str(&format!(
            "State id: {}  saved state: {}  save generation: {}\n",
            document.state_id(),
            document
                .saved_state_id()
                .map_or_else(|| "none".to_owned(), |state| state.to_string()),
            document.save_generation()
        ));
        text.push_str("\nFile and language\n");
        text.push_str(&format!("Disk: {disk}\n"));
        text.push_str(&format!("Syntax: {syntax}\n"));
        text.push_str(&format!("Configured LSP: {configured_lsp}\n"));
        text.push_str(&format!(
            "Synchronized with active LSP: {synchronized_lsp}\n"
        ));
        text.push_str(&format!(
            "Diagnostics: {} retained (errors={diagnostic_errors}, warnings={diagnostic_warnings}, info/hints={diagnostic_info}){}\n",
            diagnostics.len(),
            if self.lsp.diagnostics.is_partial() {
                " (partial)"
            } else {
                ""
            }
        ));
        text.push_str("\nVersion control\n");
        text.push_str(&format!("Git: {git}\n"));
        text.push_str("\nNext actions\n");
        text.push_str("- `Esc b` switches buffers; `Esc [` and `Esc ]` move between them.\n");
        text.push_str(
            "- `Esc v f` opens full current-file Git status when this is a file in Git.\n",
        );
        text.push_str("- `Esc c p` opens Problems when diagnostics or task locations exist.\n");
        text.push_str("- `Esc w i` opens the wider workspace context snapshot.\n");

        self.workspace.open_virtual("Buffer Info", &text);
        self.status("Buffer info opened as a read-only IDE view");
    }

    fn open_dirty_buffers(&mut self) {
        let active_id = self.workspace.active().id();
        let dirty: Vec<_> = self
            .workspace
            .buffers()
            .iter()
            .enumerate()
            .filter(|(_, editor)| editor.document.is_modified())
            .collect();
        let mut text = String::new();
        text.push_str("Dirty Buffers\n\n");
        text.push_str(&format!(
            "Open buffers: {}  dirty: {}\n",
            self.workspace.buffers().len(),
            dirty.len()
        ));
        text.push_str("This is a read-only review snapshot. It does not save, discard, stage, run tasks, or change the active source buffer.\n\n");
        if dirty.is_empty() {
            text.push_str("No dirty buffers.\n\n");
        } else {
            text.push_str(" #  A  Kind       Chars   Lines   Path / Name\n");
            text.push_str(" ─  ─  ─────────  ──────  ──────  ───────────\n");
            for (index, editor) in dirty {
                let active = if editor.id() == active_id { "*" } else { " " };
                let kind = if editor.document.is_read_only() {
                    "read-only"
                } else if editor.document.path().is_some() {
                    "file"
                } else {
                    "untitled"
                };
                let name = editor
                    .document
                    .path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| editor.document.display_name().to_owned());
                text.push_str(&format!(
                    "{:>2}  {active}  {:<9}  {:>6}  {:>6}  {name}\n",
                    index + 1,
                    kind,
                    editor.document.len_chars(),
                    editor.document.line_count()
                ));
            }
            text.push('\n');
        }
        text.push_str("Next actions\n");
        text.push_str(
            "- `Esc S` saves all dirty file-backed buffers; untitled buffers prompt for paths.\n",
        );
        text.push_str("- `Esc b` opens the buffer switcher to visit a listed buffer.\n");
        text.push_str("- `Esc w b` opens detailed context for the active buffer.\n");
        text.push_str("- `Esc q` keeps the normal dirty-buffer confirmation before quitting.\n");

        self.workspace.open_virtual("Dirty Buffers", &text);
        self.status("Dirty buffers opened as a read-only IDE view");
    }

    fn navigate_dirty_buffer(&mut self, forward: bool) {
        let dirty_indices: Vec<usize> = self
            .workspace
            .buffers()
            .iter()
            .enumerate()
            .filter_map(|(index, editor)| editor.document.is_modified().then_some(index))
            .collect();
        if dirty_indices.is_empty() {
            self.status("No dirty buffers");
            return;
        }

        let active_index = self.workspace.active_index();
        if dirty_indices.len() == 1 && dirty_indices[0] == active_index {
            self.status("Only current buffer is dirty");
            return;
        }

        let target = if forward {
            dirty_indices
                .iter()
                .copied()
                .find(|index| *index > active_index)
                .unwrap_or(dirty_indices[0])
        } else {
            dirty_indices
                .iter()
                .rev()
                .copied()
                .find(|index| *index < active_index)
                .unwrap_or_else(|| *dirty_indices.last().expect("dirty buffer exists"))
        };

        let _ = self.workspace.activate(target);
        let label = self.workspace.active().document.display_name().to_owned();
        if forward {
            self.status(format!("Next dirty buffer: {label}"));
        } else {
            self.status(format!("Previous dirty buffer: {label}"));
        }
    }

    fn open_recent_files(&mut self) {
        let mut text = String::new();
        text.push_str("Recent Files\n\n");
        text.push_str(&format!(
            "Workspace: {}\nRecent paths retained: {}\n",
            self.workspace.root.display(),
            self.persistence.recent_files.len()
        ));
        text.push_str("This is a read-only session/navigation snapshot. It does not open files, read file contents, or restore missing paths.\n\n");
        if self.persistence.recent_files.is_empty() {
            text.push_str("No recent file-backed buffers yet.\n\n");
        } else {
            text.push_str(" #  A  O  Disk     Path\n");
            text.push_str(" ─  ─  ─  ───────  ────\n");
            let active_path = self
                .workspace
                .active()
                .document
                .path()
                .map(Path::to_path_buf);
            for (index, path) in self.persistence.recent_files.iter().enumerate() {
                let active = if active_path
                    .as_ref()
                    .is_some_and(|active| same_workspace(active, path))
                {
                    "*"
                } else {
                    " "
                };
                let open = if self.workspace.file_editors().any(|editor| {
                    editor
                        .document
                        .path()
                        .is_some_and(|open| same_workspace(open, path))
                }) {
                    "o"
                } else {
                    " "
                };
                let disk = match fs::metadata(path) {
                    Ok(metadata) if metadata.is_file() => "file",
                    Ok(_) => "other",
                    Err(_) => "missing",
                };
                text.push_str(&format!(
                    "{:>2}  {active}  {open}  {:<7}  {}\n",
                    index + 1,
                    disk,
                    path.display()
                ));
            }
            text.push('\n');
        }
        text.push_str("Next actions\n");
        text.push_str("- `Esc o` opens Quick Open for indexed workspace files.\n");
        text.push_str("- `:e PATH` opens a listed path when the active buffer is clean.\n");
        text.push_str("- `Esc b` switches among already-open buffers.\n");
        text.push_str("- Pathless launch restores persisted open file-backed buffers only; unsaved text remains in recovery journals.\n");

        self.workspace.open_virtual("Recent Files", &text);
        self.status("Recent files opened as a read-only IDE view");
    }

    fn request_git_path_mutation(&mut self, stage: bool) {
        if self.git.pending.is_some() {
            self.error("A Git operation is already running");
            return;
        }
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let editor = self.workspace.active();
        if editor.document.is_read_only() {
            self.error("Open an editable file-backed buffer before changing the Git index");
            return;
        }
        let Some(path) = editor.document.path().map(Path::to_path_buf) else {
            self.error("Save this buffer before changing the Git index");
            return;
        };
        if editor.document.is_modified() {
            self.error("Save the current buffer first; Git would otherwise use the disk version");
            return;
        }
        let relative = match repository.relative_path(&path) {
            Ok(relative) => relative,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let file = match repository.status_path(&path) {
            Ok(file) => file,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let Some(file) = file else {
            self.status(if stage {
                "Current file has no disk change to stage"
            } else {
                "Current file has no staged change"
            });
            return;
        };
        if file.kind == StatusEntryKind::Unmerged {
            self.error("Unmerged paths must be resolved in the workspace shell");
            return;
        }
        if file.submodule.is_submodule {
            self.error("Submodule index changes must be handled in the workspace shell");
            return;
        }
        let mutation = if stage {
            if !file.has_worktree_change() {
                self.status("Current file has no additional disk change to stage");
                return;
            }
            GitMutation::StageCurrent(relative)
        } else {
            if !file.is_staged() {
                self.status("Current file has no staged change");
                return;
            }
            GitMutation::UnstageCurrent(relative)
        };
        self.ui.status = None;
        self.ui.mode = UiMode::GitTrust(mutation);
    }

    fn request_git_commit(&mut self) {
        if self.git.pending.is_some() {
            self.error("A Git operation is already running");
            return;
        }
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let status = match repository.status() {
            Ok(status) => status,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        if status
            .files
            .iter()
            .any(|file| file.kind == StatusEntryKind::Unmerged)
        {
            self.error("Resolve unmerged paths before committing");
            return;
        }
        if !status.files.iter().any(|file| file.is_staged()) {
            self.status("Nothing is staged to commit");
            return;
        }
        self.begin_prompt(PromptFlow::GitCommitMessage);
    }

    fn start_git_mutation(&mut self, mutation: GitMutation) {
        if self.git.pending.is_some() {
            self.error("A Git operation is already running");
            return;
        }
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let summary = git_mutation_summary(&mutation);
        match self
            .services
            .start_git_mutation(repository, mutation.clone())
        {
            Ok(generation) => {
                self.git.pending = Some(PendingGitMutation {
                    generation,
                    mutation,
                });
                self.ui.mode = UiMode::Edit;
                self.status(format!("Git {summary} running in background"));
            }
            Err(error) => self.error(error),
        }
    }

    fn open_git_mutation_details(&mut self, mutation: &GitMutation) {
        let Some(repository) = self.git.repository.as_ref() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let mut text = String::from("Git Operation Trust\n\n");
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        match mutation {
            GitMutation::StageCurrent(path) => {
                text.push_str("Operation: stage current file\n");
                text.push_str(&format!("Repository path: {}\n", path.display()));
            }
            GitMutation::UnstageCurrent(path) => {
                text.push_str("Operation: unstage current file\n");
                text.push_str(&format!("Repository path: {}\n", path.display()));
            }
            GitMutation::CommitStaged { message } => {
                text.push_str("Operation: commit staged changes\n");
                text.push_str(&format!("Exact message: {message:?}\n"));
            }
        }
        text.push_str("\nSafety boundary\n");
        text.push_str("───────────────\n");
        text.push_str(
            "- Uses one fixed Git subcommand with direct arguments; no shell is involved.\n",
        );
        text.push_str(
            "- Staging may run repository clean filters; committing may run repository hooks.\n",
        );
        text.push_str("- Signed commits, branches, discard/reset/clean, and all network operations remain outside this workflow.\n");
        text.push_str("- This view did not run Git. Close it and request the operation again to trust and run it.\n");
        self.workspace.open_virtual("Git Operation Trust", &text);
        self.status("Git operation details opened; no Git mutation ran");
    }

    fn open_git_status(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        match repository.status() {
            Ok(status) => {
                self.git.branch = branch_label(&status.branch.head);
                self.git.changes = status.files.len();
                let branch = self.git.branch.as_deref().unwrap_or("detached");
                let mut text = format!("Git Status — {branch}\n\n");
                if status.files.is_empty() {
                    text.push_str("Working tree clean.\n");
                } else {
                    text.push_str(" I W  PATH\n");
                    text.push_str(" ─ ─  ────\n");
                    for file in status.files {
                        text.push_str(&format!(
                            " {} {}  {}",
                            git_state_glyph(file.index),
                            git_state_glyph(file.worktree),
                            file.path.display()
                        ));
                        if let Some(original) = file.original_path {
                            text.push_str(&format!("  ← {}", original.display()));
                        }
                        text.push('\n');
                    }
                }
                self.workspace.open_virtual("Git Status", &text);
                self.status("Git status opened as a read-only IDE view");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn open_current_diff(&mut self) {
        let Some(path) = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf)
        else {
            self.error("Open a file-backed buffer before requesting its diff");
            return;
        };
        self.open_git_diff_for_path(path);
    }

    fn open_git_diff_for_path(&mut self, path: PathBuf) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let working = match repository.diff_path(&path, false) {
            Ok(diff) => diff,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let staged = match repository.diff_path(&path, true) {
            Ok(diff) => diff,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let mut text = String::new();
        if !staged.is_empty() {
            text.push_str("# Staged changes\n\n");
            text.push_str(&String::from_utf8_lossy(&staged));
            text.push('\n');
        }
        if !working.is_empty() {
            text.push_str("# Working-tree changes\n\n");
            text.push_str(&String::from_utf8_lossy(&working));
        }
        if text.is_empty() {
            text.push_str("No tracked diff for this file.\n");
        }
        let name = format!(
            "Diff: {}",
            path.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| path.as_os_str().to_string_lossy())
        );
        self.workspace.open_virtual(name, &text);
        self.status("Diff opened as a read-only IDE view");
    }

    fn open_git_log(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let branch = match repository.current_branch() {
            Ok(branch) => branch,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let log = if branch.unborn {
            Vec::new()
        } else {
            match repository.recent_log(100) {
                Ok(log) => log,
                Err(error) => {
                    self.error(error.to_string());
                    return;
                }
            }
        };
        self.git.branch = branch_label(&branch.head);
        let name = self.git.branch.as_deref().unwrap_or("detached HEAD");
        let mut text = format!("Git Log — {name}\n\n");
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        if let Some(upstream) = branch.upstream {
            text.push_str(&format!("Upstream: {upstream}\n"));
        }
        text.push_str(&format!(
            "Ahead: {}   Behind: {}\n",
            branch.ahead, branch.behind
        ));
        if branch.unborn {
            text.push_str("State: unborn branch (no commits yet)\n");
        }
        text.push_str("\nRecent commits (newest first, max 100)\n");
        text.push_str("──────────────────────────────────────\n");
        if log.is_empty() {
            text.push_str("No commits yet.\n");
        } else {
            text.push_str(&String::from_utf8_lossy(&log));
            text.push('\n');
        }
        text.push_str("\nNext actions\n");
        text.push_str("- `Esc v s` opens current status.\n");
        text.push_str("- `Esc v c` commits staged changes with an explicit message.\n");
        text.push_str("- Branch switching, pull, and push are not implemented here.\n");

        self.workspace.open_virtual("Git Log", &text);
        self.status("Git log opened as a read-only IDE view");
    }

    fn open_git_file_history(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let Some(path) = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf)
        else {
            self.error("Open a file-backed buffer before requesting Git file history");
            return;
        };
        let repo_relative = match repository.relative_path(&path) {
            Ok(relative) => relative,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let branch = match repository.current_branch() {
            Ok(branch) => branch,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        self.git.branch = branch_label(&branch.head);
        let name = self.git.branch.as_deref().unwrap_or("detached HEAD");
        let history = if branch.unborn {
            Vec::new()
        } else {
            match repository.file_history(&path, 100) {
                Ok(history) => history,
                Err(error) => {
                    self.error(error.to_string());
                    return;
                }
            }
        };

        let mut text = format!("Git File History — {}\n\n", repo_relative.display());
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        text.push_str(&format!("Branch: {name}\n"));
        if branch.unborn {
            text.push_str("State: unborn branch (no commits yet)\n");
        }
        text.push_str("\nRecent commits touching this file (newest first, max 100)\n");
        text.push_str("────────────────────────────────────────────────────────\n");
        if history.is_empty() {
            text.push_str("No commits found for this file.\n");
        } else {
            text.push_str(&String::from_utf8_lossy(&history));
            text.push('\n');
        }
        text.push_str("\nNext actions\n");
        text.push_str("- `Esc v a` opens blame for the active line.\n");
        text.push_str("- `Esc v h` opens the full current HEAD commit.\n");
        text.push_str("- This view is read-only and never mutates Git state.\n");

        self.workspace.open_virtual("Git File History", &text);
        self.status("Git file history opened as a read-only IDE view");
    }

    fn open_git_head(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let branch = match repository.current_branch() {
            Ok(branch) => branch,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        self.git.branch = branch_label(&branch.head);
        let name = self.git.branch.as_deref().unwrap_or("detached HEAD");
        let mut text = format!("Git HEAD — {name}\n\n");
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        if branch.unborn {
            text.push_str("State: unborn branch (no HEAD commit yet)\n");
            text.push_str("\nNo HEAD commit yet.\n");
        } else {
            let details = match repository.head_details() {
                Ok(details) => details,
                Err(error) => {
                    self.error(error.to_string());
                    return;
                }
            };
            text.push_str("\nCurrent HEAD commit\n");
            text.push_str("───────────────────\n");
            text.push_str(&String::from_utf8_lossy(&details));
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str("\nNext actions\n");
        text.push_str("- `Esc v l` opens recent commit history.\n");
        text.push_str("- `Esc v s` opens current status.\n");
        text.push_str("- This view is read-only and never mutates Git state.\n");

        self.workspace.open_virtual("Git HEAD", &text);
        self.status("Git HEAD opened as a read-only IDE view");
    }

    fn open_git_commit_info(&mut self, commit: &str) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let details = match repository.commit_details(commit) {
            Ok(details) => details,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let short = if commit.len() > 12 {
            &commit[..12]
        } else {
            commit
        };
        let mut text = format!("Git Commit — {short}\n\n");
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        text.push_str(&format!("Requested commit: {commit}\n"));
        text.push_str("\nCommit details\n");
        text.push_str("──────────────\n");
        text.push_str(&String::from_utf8_lossy(&details));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\nNext actions\n");
        text.push_str("- `:commit-info HASH` opens another explicit commit.\n");
        text.push_str("- `Esc v l` opens recent commit history.\n");
        text.push_str("- This view is read-only and never mutates Git state.\n");

        self.workspace.open_virtual("Git Commit", &text);
        self.status("Git commit opened as a read-only IDE view");
    }

    fn open_git_blame_line(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let Some(path) = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf)
        else {
            self.error("Open a file-backed buffer before requesting Git blame");
            return;
        };
        let position = self.workspace.active().position(self.config.tab_width);
        let line = position.line + 1;
        let blame = match repository.blame_line(&path, line) {
            Ok(blame) => blame,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let repo_relative = match repository.relative_path(&path) {
            Ok(relative) => relative,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let mut text = format!("Git Blame — {}:{line}\n\n", repo_relative.display());
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        text.push_str(&format!("Commit: {}\n", blame.commit));
        if let Some(summary) = blame.summary {
            text.push_str(&format!("Summary: {summary}\n"));
        }
        if let Some(author) = blame.author {
            text.push_str(&format!("Author: {author}\n"));
        }
        if let Some(mail) = blame.author_mail {
            text.push_str(&format!("Author mail: {mail}\n"));
        }
        if let Some(time) = blame.author_time {
            text.push_str(&format!("Author time (epoch): {time}\n"));
        }
        text.push_str(&format!("Original line: {}\n", blame.original_line));
        text.push_str(&format!("Current line: {}\n", blame.final_line));
        if let Some(filename) = blame.filename {
            text.push_str(&format!("Blamed file: {}\n", filename.display()));
        }
        text.push_str("\nLine content\n");
        text.push_str("────────────\n");
        text.push_str(&blame.content);
        text.push('\n');
        text.push_str("\nNext actions\n");
        text.push_str("- `Esc v h` opens the full current HEAD commit.\n");
        text.push_str("- `Esc v l` opens recent commit history.\n");
        text.push_str("- This view is read-only and never mutates Git state.\n");

        self.workspace.open_virtual("Git Blame", &text);
        self.status("Git blame opened as a read-only IDE view");
    }

    fn open_current_file_status(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        let Some(path) = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf)
        else {
            self.error("Open a file-backed buffer before requesting its Git status");
            return;
        };

        let file_status = match repository.status_path(&path) {
            Ok(status) => status,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let staged = match repository.diff_path(&path, true) {
            Ok(diff) => diff,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let working = match repository.diff_path(&path, false) {
            Ok(diff) => diff,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let repo_relative = match repository.relative_path(&path) {
            Ok(relative) => relative,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        let mut text = format!("Git File Status\n\nPath: {}\n", path.display());
        text.push_str(&format!("Repository: {}\n", repository.root().display()));
        text.push_str(&format!("Repo path: {}\n\n", repo_relative.display()));
        match file_status {
            Some(file) => {
                text.push_str(&format!(
                    "Index: {} ({})\n",
                    git_state_glyph(file.index),
                    git_state_label(file.index)
                ));
                text.push_str(&format!(
                    "Worktree: {} ({})\n",
                    git_state_glyph(file.worktree),
                    git_state_label(file.worktree)
                ));
                text.push_str(&format!(
                    "Entry kind: {}\n",
                    git_entry_kind_label(file.kind)
                ));
                if let Some(original) = file.original_path {
                    text.push_str(&format!("Original path: {}\n", original.display()));
                }
                if file.submodule.is_submodule {
                    text.push_str(&format!(
                        "Submodule: commit_changed={} tracked_changes={} untracked_changes={}\n",
                        file.submodule.commit_changed,
                        file.submodule.tracked_changes,
                        file.submodule.untracked_changes
                    ));
                }
            }
            None => {
                text.push_str("Index: · (clean or tracked-clean)\n");
                text.push_str("Worktree: · (clean or tracked-clean)\n");
                text.push_str("Entry kind: clean/not reported by porcelain status\n");
            }
        }
        text.push_str("\nDiff payloads\n");
        text.push_str(&format!("Staged diff bytes: {}\n", staged.len()));
        text.push_str(&format!("Working diff bytes: {}\n", working.len()));
        text.push_str("\nNext steps\n");
        text.push_str("- `Esc v d` opens the staged/working patch for this file.\n");
        text.push_str("- `Esc v S` / `:stage-current` stages this saved file after trust.\n");
        text.push_str("- `Esc v U` / `:unstage-current` unstages this saved file after trust.\n");

        self.workspace.open_virtual("Git File Status", &text);
        self.status("Current file Git status opened as a read-only IDE view");
    }

    fn open_branch_view(&mut self) {
        let Some(repository) = self.git.repository.clone() else {
            let message = self.git_unavailable_message();
            self.error(message);
            return;
        };
        match repository.current_branch() {
            Ok(branch) => {
                let name = branch_label(&branch.head).unwrap_or_else(|| "detached HEAD".to_owned());
                let mut text = format!("Git Branch\n\nCurrent: {name}\n");
                if let Some(ref upstream) = branch.upstream {
                    text.push_str(&format!("Upstream: {upstream}\n"));
                }
                text.push_str(&format!(
                    "Ahead: {}   Behind: {}\n",
                    branch.ahead, branch.behind
                ));
                if branch.unborn {
                    text.push_str("State: unborn branch (no commit yet)\n");
                }
                let current_name = branch.name().map(str::to_owned);
                match repository.list_local_branches(50) {
                    Ok(branches) if !branches.is_empty() => {
                        text.push_str("\nLocal branches\n");
                        text.push_str("──────────────\n");
                        for branch_name in branches {
                            let marker = if current_name.as_deref() == Some(branch_name.as_str()) {
                                "*"
                            } else {
                                " "
                            };
                            text.push_str(&format!("{marker} {branch_name}\n"));
                        }
                    }
                    Ok(_) => text.push_str("\nNo local branches listed.\n"),
                    Err(error) => text.push_str(&format!("\nCould not list branches: {error}\n")),
                }
                text.push_str("\nTrusted local stage-current, unstage-current, and commit-staged are available in the editor.\n");
                text.push_str("Use `Esc t t` or `:terminal` for branch changes, pull, push, discard/reset/clean, and other Git operations.\n");
                self.workspace.open_virtual("Git Branch", &text);
                self.status("Branch information opened");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn move_screen_vertical(&mut self, delta: isize, selecting: bool) -> Result<(), String> {
        if self.ui.soft_wrap {
            let layout = Layout::calculate(
                self.ui.screen_size.0,
                self.ui.screen_size.1,
                self.workspace.active().document.line_count(),
                self.config.line_numbers,
            );
            let metrics = VisualMetrics::new(layout.content_width, self.config.tab_width, true);
            self.workspace
                .active_mut()
                .move_wrapped_vertical(delta, selecting, metrics)
                .map_err(|error| format!("Could not move through wrapped text: {error}"))
        } else {
            self.workspace
                .active_mut()
                .move_vertical(delta, selecting, self.config.tab_width);
            Ok(())
        }
    }

    fn current_jump_location(&self) -> JumpLocation {
        let editor = self.workspace.active();
        JumpLocation {
            editor_id: editor.id(),
            path: editor.document.path().map(Path::to_path_buf),
            cursor: editor.cursor,
        }
    }

    fn record_jump_origin(&mut self, origin: JumpLocation) {
        if origin == self.current_jump_location() {
            return;
        }
        if origin.path.is_none() && self.workspace.editor_index(origin.editor_id).is_none() {
            return;
        }
        push_bounded_jump(&mut self.ui.jump_back, origin);
        self.ui.jump_forward.clear();
    }

    fn navigate_history(&mut self, forward: bool) {
        let target = if forward {
            self.ui.jump_forward.pop()
        } else {
            self.ui.jump_back.pop()
        };
        let Some(target) = target else {
            self.status(if forward {
                "No newer jump"
            } else {
                "No older jump"
            });
            return;
        };
        let current = self.current_jump_location();
        match self.restore_jump_location(&target) {
            Ok(()) => {
                if forward {
                    push_bounded_jump(&mut self.ui.jump_back, current);
                } else {
                    push_bounded_jump(&mut self.ui.jump_forward, current);
                }
                self.status(if forward {
                    "Jumped forward"
                } else {
                    "Jumped back"
                });
            }
            Err(error) => {
                if forward {
                    self.ui.jump_forward.push(target);
                } else {
                    self.ui.jump_back.push(target);
                }
                self.error(error);
            }
        }
    }

    fn toggle_bookmark(&mut self) {
        if self.workspace.active().document.is_read_only() {
            self.status("Bookmarks are for source buffers");
            return;
        }
        let current = self.current_jump_location();
        if let Some(index) = self
            .ui
            .bookmarks
            .iter()
            .position(|bookmark| same_bookmark_location(bookmark, &current))
        {
            self.ui.bookmarks.remove(index);
            self.status("Bookmark removed");
            return;
        }
        if self.ui.bookmarks.len() == MAX_BOOKMARKS {
            self.ui.bookmarks.remove(0);
        }
        self.ui.bookmarks.push(current);
        self.status("Bookmark added");
    }

    fn open_bookmarks(&mut self) {
        if self.ui.bookmarks.is_empty() {
            self.status("No bookmarks");
            return;
        }
        self.begin_prompt(PromptFlow::Bookmarks);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(
                "Enter jumps to selected bookmark · filter by path/line · Esc cancels".to_owned(),
            );
        }
    }

    fn sticky_library(&self) -> Result<crate::stickies::StickyLibrary, String> {
        crate::stickies::StickyLibrary::for_workspace(self.workspace_root())
            .map_err(|error| error.to_string())
    }

    pub fn sticky_pad_visible(&self) -> bool {
        self.sticky_pad.visible
    }

    pub fn sticky_pad_focused(&self) -> bool {
        self.sticky_pad.is_focused()
    }

    pub fn sticky_pad_view(
        &self,
        body_rows: usize,
        width: usize,
    ) -> Vec<crate::stickies::StickyPadLine> {
        self.sticky_pad.view_lines(body_rows, width)
    }

    /// Toggle the floating top-right sticky notepad (not a buffer).
    fn open_stickies(&mut self) {
        let library = match self.sticky_library() {
            Ok(library) => library,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match self.sticky_pad.toggle(&library) {
            Ok(message) => {
                self.ui.full_redraw = true;
                self.status(message);
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn sticky_candidates(&self, query: &str) -> (Vec<String>, Vec<PromptEntry>) {
        let Ok(library) = self.sticky_library() else {
            return (Vec::new(), Vec::new());
        };
        let listing = library.list();
        listing
            .notes
            .into_iter()
            .filter_map(|note| {
                let label = crate::stickies::sticky_label(&note);
                if !query.is_empty() && fuzzy_path_score(query, &label).is_none() {
                    return None;
                }
                let path = library.path_for(note.store, &note.id).ok()?;
                Some((label, PromptEntry::Sticky { id: note.id, path }))
            })
            .unzip()
    }

    /// Create a personal sticky and open it in the floating pad.
    fn create_new_sticky(&mut self) {
        let library = match self.sticky_library() {
            Ok(library) => library,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        let active_path = self
            .workspace
            .active()
            .document
            .path()
            .map(Path::to_path_buf);
        let title = "Sticky".to_owned();
        let anchor =
            crate::stickies::anchor_for_open_file(self.workspace_root(), active_path.as_deref());
        match self.sticky_pad.show_new(&library, title, anchor) {
            Ok(()) => {
                self.ui.full_redraw = true;
                self.status("New sticky pad — type freely · Esc returns to editor");
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn handle_sticky_pad_key(&mut self, key: KeyEvent) {
        // Action layer still reachable? Prefer Esc unfocus; Ctrl-K can still enter action.
        if let Some(normalized) = normalize_action_key(key, self.ui.keymap.is_active()) {
            // Esc while focused unfocuses the pad (does not enter Action).
            if matches!(normalized, crate::keymap::Key::Escape) && !self.ui.keymap.is_active() {
                if let Ok(library) = self.sticky_library() {
                    let _ = self.sticky_pad.unfocus_save(&library);
                }
                self.ui.full_redraw = true;
                self.status("Sticky pad unfocused — Esc w k to hide or re-focus");
                return;
            }
            if let Some(resolution) = self.ui.keymap.feed(normalized) {
                self.apply_action_resolution(resolution);
                return;
            }
        }

        let Ok(library) = self.sticky_library() else {
            self.error("Sticky storage unavailable");
            return;
        };

        match key.code {
            KeyCode::Esc => {
                let _ = self.sticky_pad.unfocus_save(&library);
                self.ui.full_redraw = true;
                self.status("Sticky pad unfocused");
            }
            KeyCode::Enter => {
                self.sticky_pad.insert_char('\n');
                self.ui.full_redraw = true;
            }
            KeyCode::Backspace => {
                self.sticky_pad.backspace();
                self.ui.full_redraw = true;
            }
            KeyCode::Delete => {
                self.sticky_pad.delete_forward();
                self.ui.full_redraw = true;
            }
            KeyCode::Left => {
                self.sticky_pad.move_left();
                self.ui.full_redraw = true;
            }
            KeyCode::Right => {
                self.sticky_pad.move_right();
                self.ui.full_redraw = true;
            }
            KeyCode::Up => {
                self.sticky_pad.move_up();
                self.ui.full_redraw = true;
            }
            KeyCode::Down => {
                self.sticky_pad.move_down();
                self.ui.full_redraw = true;
            }
            KeyCode::Char('[') => {
                if let Err(error) = self.sticky_pad.cycle(&library, -1) {
                    self.error(error.to_string());
                }
                self.ui.full_redraw = true;
            }
            KeyCode::Char(']') => {
                if let Err(error) = self.sticky_pad.cycle(&library, 1) {
                    self.error(error.to_string());
                }
                self.ui.full_redraw = true;
            }
            KeyCode::Char(ch)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.sticky_pad.insert_char(ch);
                self.ui.full_redraw = true;
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.sticky_pad.save_if_dirty(&library) {
                    Ok(()) => self.status("Sticky saved"),
                    Err(error) => self.error(error.to_string()),
                }
                self.ui.full_redraw = true;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.sticky_pad.archive_current(&library) {
                    Ok(()) => self.status("Sticky archived"),
                    Err(error) => self.error(error.to_string()),
                }
                self.ui.full_redraw = true;
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.sticky_pad.delete_current(&library) {
                    Ok(()) => self.status("Sticky deleted"),
                    Err(error) => self.error(error.to_string()),
                }
                self.ui.full_redraw = true;
            }
            _ => {}
        }
    }

    fn archive_selected_sticky(&mut self) {
        let selected = match &self.ui.mode {
            UiMode::Prompt(prompt) if prompt.kind == PromptFlow::Stickies => {
                prompt.entries.get(prompt.selected).cloned()
            }
            _ => None,
        };
        let Some(PromptEntry::Sticky { id, .. }) = selected else {
            self.status("No sticky selected");
            return;
        };
        let library = match self.sticky_library() {
            Ok(library) => library,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match library.archive(&id) {
            Ok(note) => {
                self.status(format!("Archived sticky {}", note.id));
                self.refresh_prompt_candidates();
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    fn delete_selected_sticky(&mut self) {
        let selected = match &self.ui.mode {
            UiMode::Prompt(prompt) if prompt.kind == PromptFlow::Stickies => {
                prompt.entries.get(prompt.selected).cloned()
            }
            _ => None,
        };
        let Some(PromptEntry::Sticky { id, path }) = selected else {
            self.status("No sticky selected");
            return;
        };
        let library = match self.sticky_library() {
            Ok(library) => library,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        match library.delete(&id) {
            Ok(_) => {
                // If the deleted sticky is the active buffer, close it (force).
                if self.workspace.active().document.path() == Some(path.as_path()) {
                    let _ = self.close_active_buffer(true);
                }
                let listing = library.list();
                if listing.notes.is_empty() && !listing.partial {
                    self.ui.mode = UiMode::Edit;
                    self.status(format!("Deleted sticky {id} — no stickies left"));
                } else {
                    self.status(format!("Deleted sticky {id}"));
                    self.refresh_prompt_candidates();
                }
            }
            Err(error) => self.error(error.to_string()),
        }
    }

    pub fn agent_dashboard_visible(&self) -> bool {
        self.agent.dashboard_visible
    }

    fn toggle_agent_dashboard(&mut self) {
        self.agent.dashboard_visible = !self.agent.dashboard_visible;
        self.ui.full_redraw = true;
        if self.agent.dashboard_visible {
            self.status("Agents dashboard on — Esc w a dispatch · Esc w x cancel · Esc w D hide");
        } else {
            self.status("Agents dashboard off");
        }
    }

    /// True when the bottom strip should claim more vertical room (live run / receipt).
    pub fn agent_dashboard_wants_depth(&self) -> bool {
        self.agent.job.is_some()
            || self.agent.coordinator.is_active()
            || self.agent.pending_permission.is_some()
            || !self.agent.coordinator.receipt().is_empty()
    }

    /// Single Agents surface: roster + full receipt detail (replaces the old activity popup).
    pub fn agent_dashboard_view(&self, visible_rows: usize) -> AgentDashboardView {
        let mut lines = Vec::new();
        if visible_rows == 0 {
            return AgentDashboardView { lines };
        }

        let state = self.agent.coordinator.run_state();
        let active = self.agent.coordinator.is_active();
        let job_live = self.agent.job.is_some();
        let (icon, run_emphasis) = match state {
            crate::agent_contract::AgentRunState::Working if job_live || active => {
                ("⋅", AgentDashboardEmphasis::RunActive)
            }
            crate::agent_contract::AgentRunState::NeedsYou => {
                ("●", AgentDashboardEmphasis::RunNeedsYou)
            }
            crate::agent_contract::AgentRunState::Review => {
                ("●", AgentDashboardEmphasis::RunReview)
            }
            crate::agent_contract::AgentRunState::Brief if active => {
                ("⋅", AgentDashboardEmphasis::RunActive)
            }
            _ => ("○", AgentDashboardEmphasis::RunIdle),
        };

        let needs_you = self.agent.pending_permission.is_some()
            || matches!(state, crate::agent_contract::AgentRunState::NeedsYou);
        let awaiting = needs_you || matches!(state, crate::agent_contract::AgentRunState::Review);
        let header = if needs_you {
            format!(
                " AGENTS · 1 awaiting · {}",
                crate::agent_runtime::run_state_label(
                    crate::agent_contract::AgentRunState::NeedsYou
                )
            )
        } else if active || job_live {
            format!(
                " AGENTS · 1 session · {} · {}",
                if awaiting { "1 awaiting" } else { "live" },
                crate::agent_runtime::run_state_label(state)
            )
        } else if !self.agent.coordinator.receipt().is_empty() {
            format!(
                " AGENTS · last run · {}",
                crate::agent_runtime::run_state_label(state)
            )
        } else {
            " AGENTS · idle · Esc w a dispatch ".to_owned()
        };
        lines.push(AgentDashboardLine {
            text: header,
            emphasis: AgentDashboardEmphasis::Title,
        });

        if lines.len() >= visible_rows {
            return AgentDashboardView { lines };
        }

        if let Some(pending) = &self.agent.pending_permission {
            lines.push(AgentDashboardLine {
                text: format!(" ● NEED YOU · {}", pending.summary),
                emphasis: AgentDashboardEmphasis::RunNeedsYou,
            });
            for option in &pending.options {
                if lines.len() + 1 >= visible_rows {
                    break;
                }
                let tag = if option.is_allow() {
                    "Y"
                } else if option.is_reject() {
                    "N"
                } else {
                    "·"
                };
                lines.push(AgentDashboardLine {
                    text: format!("   [{tag}] {} ({})", option.name, option.kind),
                    emphasis: AgentDashboardEmphasis::Receipt,
                });
            }
            if lines.len() < visible_rows {
                lines.push(AgentDashboardLine {
                    text: " Y allow · N deny · Esc w A allow · Esc w x cancel ".to_owned(),
                    emphasis: AgentDashboardEmphasis::Hint,
                });
            }
            lines.truncate(visible_rows);
            return AgentDashboardView { lines };
        }

        // Body reuses the same detail formatter the activity popup used (session,
        // gen, goal, authority, receipt with kind/path) so one surface owns it all.
        let body_budget = visible_rows.saturating_sub(lines.len()).saturating_sub(1);
        let receipt_event_limit = body_budget.clamp(4, 48);
        let detail = crate::agent_runtime::format_receipt_lines(
            &self.agent.coordinator,
            receipt_event_limit,
        );
        let mut detail_lines: Vec<AgentDashboardLine> = Vec::new();
        for (index, raw) in detail.into_iter().enumerate() {
            if raw.is_empty() {
                continue;
            }
            let emphasis = if index == 0 {
                run_emphasis
            } else if raw.starts_with("goal:") || raw.starts_with("authority:") {
                AgentDashboardEmphasis::RunIdle
            } else if raw.starts_with("receipt")
                || raw.trim_start().starts_with("path ")
                || raw.contains("(empty)")
            {
                AgentDashboardEmphasis::Muted
            } else {
                AgentDashboardEmphasis::Receipt
            };
            let text = if index == 0 {
                format!(" {icon} {raw}")
            } else {
                format!("   {raw}")
            };
            detail_lines.push(AgentDashboardLine { text, emphasis });
        }
        if detail_lines.is_empty() {
            detail_lines.push(AgentDashboardLine {
                text: "   (no receipt yet)".to_owned(),
                emphasis: AgentDashboardEmphasis::Muted,
            });
        }
        // Prefer session line + newest receipt tail when the panel is short.
        if detail_lines.len() > body_budget && body_budget > 0 {
            if body_budget == 1 {
                detail_lines.truncate(1);
            } else {
                let head = detail_lines[0].clone();
                let tail_take = body_budget.saturating_sub(1);
                let start = detail_lines.len().saturating_sub(tail_take);
                let mut packed = vec![head];
                packed.extend(detail_lines.into_iter().skip(start.max(1)));
                packed.truncate(body_budget);
                detail_lines = packed;
            }
        }
        for line in detail_lines {
            if lines.len() + 1 >= visible_rows {
                break;
            }
            lines.push(line);
        }

        if lines.len() < visible_rows {
            let hint = if matches!(state, crate::agent_contract::AgentRunState::Review) {
                " G review Git · v s status · v D diffs · D hide "
            } else {
                " a run · G review · x cancel · D hide "
            };
            lines.push(AgentDashboardLine {
                text: hint.to_owned(),
                emphasis: AgentDashboardEmphasis::Hint,
            });
        }

        lines.truncate(visible_rows);
        AgentDashboardView { lines }
    }

    fn begin_agent_run_prompt(&mut self) {
        if self.agent.job.is_some() && self.agent.coordinator.is_active() {
            self.status("Agent already running — Esc w D for dashboard, Esc w x to cancel");
            return;
        }
        let readiness = crate::agent_auth::probe_agent(&self.config.agent);
        if !self.config.agent.use_fake {
            if !readiness.ready_for_real_agent() {
                self.error(format!(
                    "{} — fix host auth, then retry (docs/AGENT_AUTH.md · wscrpt --health)",
                    readiness.summary()
                ));
                return;
            }
            self.status(format!(
                "{} · Enter launches ACP process ({})",
                readiness.summary(),
                if self.config.agent.argv.is_empty() {
                    "argv empty".to_owned()
                } else {
                    self.config.agent.argv.join(" ")
                }
            ));
        }
        self.begin_prompt(PromptFlow::AgentGoal);
        if let UiMode::Prompt(prompt) = &mut self.ui.mode {
            prompt.notice = Some(if self.config.agent.use_fake {
                "Enter starts a plan-first fake agent run · Esc cancels · progress on bottom Agents dashboard (Esc w D)"
                    .to_owned()
            } else {
                format!(
                    "profile={} · Enter starts ACP `{}` · Esc cancels · dashboard Esc w D",
                    self.config.agent.profile,
                    self.config.agent.argv.join(" ")
                )
            });
        }
    }

    fn start_agent_run(&mut self, goal: String) {
        if let Err(error) = crate::agent_runtime::validate_goal_input(&goal) {
            self.error(error);
            return;
        }
        if self.agent.job.is_some() && self.agent.coordinator.is_active() {
            self.error("Agent already running; cancel with Esc w x first");
            return;
        }

        // Dirty-tree gate: unsaved buffers hard-block; Git dirt soft-confirms.
        let dirty_buffers = self.count_dirty_agent_buffers();
        if dirty_buffers > 0 {
            self.error(format!(
                "save {dirty_buffers} dirty buffer{} first (Esc S) — agent will not race unsaved edits",
                if dirty_buffers == 1 { "" } else { "s" }
            ));
            return;
        }
        if let Some(git_changes) = self.git_dirty_change_count() {
            if git_changes > 0 {
                self.ui.mode = UiMode::Confirm(ConfirmKind::AgentDirtyTree { goal, git_changes });
                self.status(format!(
                    "Git worktree dirty ({git_changes} path{}) — Y start anyway · Esc cancel · Esc v s status",
                    if git_changes == 1 { "" } else { "s" }
                ));
                return;
            }
        } else if matches!(self.git.status, ServiceStatus::Pending(_)) {
            self.status("Git status still loading — dirty-tree not checked for this run");
        }

        self.start_agent_run_unchecked(goal);
    }

    /// Count modified, writable buffers (agent must not race unsaved text).
    fn count_dirty_agent_buffers(&self) -> usize {
        self.workspace
            .buffers()
            .iter()
            .filter(|editor| editor.document.is_modified() && !editor.document.is_read_only())
            .count()
    }

    /// Known Git path count when a repository snapshot is ready; `None` if unknown.
    fn git_dirty_change_count(&self) -> Option<usize> {
        match &self.git.status {
            ServiceStatus::Ready if self.git.repository.is_some() => Some(self.git.changes),
            ServiceStatus::Ready => Some(0),
            ServiceStatus::Idle | ServiceStatus::Pending(_) | ServiceStatus::Failed(_) => None,
        }
    }

    /// Start a run after dirty-tree checks have passed (or user confirmed).
    fn start_agent_run_unchecked(&mut self, goal: String) {
        if let Err(error) = crate::agent_runtime::validate_goal_input(&goal) {
            self.error(error);
            return;
        }
        if self.agent.job.is_some() && self.agent.coordinator.is_active() {
            self.error("Agent already running; cancel with Esc w x first");
            return;
        }

        let use_process = !self.config.agent.use_fake;
        if use_process {
            let readiness = crate::agent_auth::probe_agent(&self.config.agent);
            if !readiness.ready_for_real_agent() {
                self.error(readiness.summary());
                return;
            }
        }

        let session = crate::agent_runtime::new_session_id();
        let workspace_id = self.agent.coordinator.workspace_id();
        let packet = match crate::agent_runtime::work_packet_for_goal(
            workspace_id,
            self.workspace_root(),
            goal.trim(),
        ) {
            Ok(packet) => packet,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        let generation = match self
            .agent
            .coordinator
            .start_run(session.clone(), packet, true)
        {
            Ok(generation) => generation,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };

        let (job, port) = if use_process {
            match crate::agent_runtime::spawn_process_agent(
                workspace_id,
                session,
                generation,
                self.workspace_root(),
                &self.config.agent.argv,
                goal.trim(),
            ) {
                Ok(pair) => pair,
                Err(error) => {
                    let _ = self.agent.coordinator.cancel_run();
                    self.error(error);
                    return;
                }
            }
        } else {
            crate::agent_runtime::spawn_fake_agent(
                workspace_id,
                session,
                generation,
                crate::agent::FakeAgent::happy_path_edit(),
            )
        };

        self.agent.job = Some(job);
        self.agent.port = Some(port);
        self.agent.pending_permission = None;
        self.agent.review_handoff_done = false;
        self.agent.last_summary = Some(if use_process {
            "agent started (ACP process)".to_owned()
        } else {
            "agent started (fake plan-first loop)".to_owned()
        });
        if !self.agent.dashboard_visible {
            self.agent.dashboard_visible = true;
            self.ui.full_redraw = true;
        }
        self.status(if use_process {
            "ACP agent run started — Agents dashboard open · Esc w x cancel · Esc w D hide"
        } else {
            "Agent run started — Agents dashboard open · Esc w x cancel · Esc w D hide"
        });
    }

    fn cancel_agent_run(&mut self) {
        if let Some(job) = &self.agent.job {
            job.cancel();
        }
        let cancelled = self.agent.coordinator.cancel_run();
        self.agent.job = None;
        self.agent.port = None;
        self.agent.pending_permission = None;
        self.agent.review_handoff_done = false;
        if cancelled.is_some() {
            self.agent.last_summary = Some("agent cancelled".to_owned());
            self.status("Agent cancelled");
        } else {
            self.status("No active agent run");
        }
    }

    /// Paths touched in the active/last agent receipt (workspace-relative).
    fn agent_touched_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for event in self.agent.coordinator.receipt() {
            if let Some(path) = &event.path
                && !paths.iter().any(|existing| existing == path)
            {
                paths.push(path.clone());
            }
        }
        paths
    }

    /// Open existing Git surfaces for agent review — only when useful.
    ///
    /// Quiet path (perf + less noise):
    /// - one touched path → open that file's **diff only**
    /// - multiple paths or Git dirt → open **Git status** (not both status+diff)
    /// - clean tree and no receipt paths → status line only (no virtual buffers)
    ///
    /// Reuses `open_git_status` / `open_git_diff_for_path` only.
    fn handoff_agent_review(&mut self, summary: Option<&str>) {
        let summary = summary
            .map(str::to_owned)
            .or_else(|| self.agent.last_summary.clone())
            .unwrap_or_else(|| "agent review".to_owned());
        let paths = self.agent_touched_paths();
        let path_hint = if paths.is_empty() {
            String::new()
        } else {
            let listed: Vec<String> = paths
                .iter()
                .take(4)
                .map(|path| path.display().to_string())
                .collect();
            let more = if paths.len() > 4 {
                format!(" +{}", paths.len() - 4)
            } else {
                String::new()
            };
            format!(" · paths: {}{more}", listed.join(", "))
        };
        let single_path = (paths.len() == 1).then(|| self.workspace_root().join(&paths[0]));
        let multi_paths = paths.len() > 1;
        let git_changes = self.git_dirty_change_count().unwrap_or(0);
        let has_repo = self.git.repository.is_some();

        if !self.agent.dashboard_visible {
            self.agent.dashboard_visible = true;
            self.ui.full_redraw = true;
        }

        let mut opened = "status line only";
        if has_repo {
            if let Some(absolute) = single_path {
                self.open_git_diff_for_path(absolute);
                opened = "diff";
            } else if multi_paths || git_changes > 0 {
                self.open_git_status();
                opened = "Git status";
            }
            // else: clean tree, no receipt paths — don't open virtual buffers
        }

        self.agent.review_handoff_done = true;
        if !has_repo {
            self.status(format!(
                "AGENT REVIEW: {summary} — no Git repo{path_hint} · Esc w D dashboard"
            ));
            return;
        }
        let extra = if multi_paths {
            " · Esc v D pick diffs"
        } else if opened == "status line only" {
            " · Esc v s status · Esc v D diffs if needed"
        } else if opened == "diff" {
            " · Esc v s full status"
        } else {
            ""
        };
        self.status(format!(
            "AGENT REVIEW: {summary} — {opened}{path_hint}{extra} · Esc w G again · Esc w D"
        ));
    }

    /// Answer a pending ACP permission prompt (Needs You).
    fn answer_agent_permission(&mut self, allow: bool) {
        let Some(pending) = self.agent.pending_permission.clone() else {
            self.status("No agent permission waiting");
            return;
        };
        let choice = if allow {
            pending
                .options
                .iter()
                .find(|option| option.is_allow())
                .or_else(|| pending.options.first())
        } else {
            pending
                .options
                .iter()
                .find(|option| option.is_reject())
                .or_else(|| pending.options.last())
        };
        let Some(option) = choice else {
            self.error("permission prompt has no options");
            return;
        };
        let option_id = option.option_id.clone();
        let name = option.name.clone();
        let Some(job) = &self.agent.job else {
            self.agent.pending_permission = None;
            self.error("agent job gone; permission dropped");
            return;
        };
        match job.reply_permission(crate::agent_runtime::PermissionDecision::Select {
            option_id: option_id.clone(),
        }) {
            Ok(()) => {
                self.agent.pending_permission = None;
                self.agent.last_summary = Some(format!("permission: {name}"));
                self.status(format!(
                    "AGENT: {} — {name}",
                    if allow { "allowed" } else { "denied" }
                ));
                self.ui.full_redraw = true;
            }
            Err(error) => {
                self.agent.pending_permission = None;
                self.error(error);
            }
        }
    }

    fn poll_agent_events(&mut self) -> bool {
        let Some(port) = &self.agent.port else {
            return false;
        };
        let mut batch = Vec::new();
        loop {
            match port.try_recv() {
                Ok(event) => batch.push(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    batch.push(crate::agent_runtime::AgentJobEvent::Finished {
                        cancelled: false,
                        error: Some("Agent worker disconnected".to_owned()),
                    });
                    break;
                }
            }
        }
        if batch.is_empty() {
            return false;
        }

        // Coalesce progress into one status line + throttled paint (remote-friendly).
        // Urgent paths (Needs You, Review, errors, finish) still paint immediately.
        let mut progress_status: Option<String> = None;
        let mut urgent = false;
        let mut finished = false;
        let mut finish_message = None;
        for event in batch {
            match event {
                crate::agent_runtime::AgentJobEvent::Event(event) => {
                    let summary = event.summary.clone();
                    let kind = event.kind;
                    match self.agent.coordinator.admit(event) {
                        Ok(outcome) => {
                            self.agent.last_summary = Some(summary.clone());
                            if kind == crate::agent_contract::AgentEventKind::ReviewReady
                                || outcome.run_state == crate::agent_contract::AgentRunState::Review
                            {
                                urgent = true;
                                progress_status = None;
                                if !self.agent.review_handoff_done {
                                    self.handoff_agent_review(Some(&summary));
                                } else {
                                    self.status(format!(
                                        "AGENT REVIEW: {summary} — Esc w G handoff · Esc v s · Esc v D · Esc w D"
                                    ));
                                }
                            } else {
                                let label =
                                    crate::agent_runtime::run_state_label(outcome.run_state);
                                progress_status = Some(format!("{label}: {summary}"));
                            }
                        }
                        Err(error) => {
                            urgent = true;
                            progress_status = None;
                            self.error(format!("agent event refused: {error}"));
                        }
                    }
                }
                crate::agent_runtime::AgentJobEvent::Notice(message) => {
                    progress_status = Some(message);
                }
                crate::agent_runtime::AgentJobEvent::PermissionNeeded(pending) => {
                    urgent = true;
                    progress_status = None;
                    self.agent.pending_permission = Some(pending.clone());
                    if !self.agent.dashboard_visible {
                        self.agent.dashboard_visible = true;
                        self.ui.full_redraw = true;
                    }
                    self.status(format!(
                        "AGENT NEED YOU: {} — Y allow · N deny · Esc w A allow · Esc w x cancel",
                        pending.summary
                    ));
                }
                crate::agent_runtime::AgentJobEvent::Finished { cancelled, error } => {
                    finished = true;
                    urgent = true;
                    progress_status = None;
                    finish_message = Some(if cancelled {
                        "Agent finished (cancelled)".to_owned()
                    } else if let Some(error) = error {
                        format!("Agent finished with error: {error}")
                    } else {
                        format!(
                            "Agent finished — {} — Esc w G review · Esc w D dashboard",
                            crate::agent_runtime::run_state_label(
                                self.agent.coordinator.run_state()
                            )
                        )
                    });
                }
            }
        }
        if finished {
            let was_review = matches!(
                self.agent.coordinator.run_state(),
                crate::agent_contract::AgentRunState::Review
            );
            self.agent.job = None;
            self.agent.port = None;
            self.agent.pending_permission = None;
            if was_review && !self.agent.review_handoff_done {
                let summary = self
                    .agent
                    .last_summary
                    .clone()
                    .unwrap_or_else(|| "agent finished".to_owned());
                self.handoff_agent_review(Some(&summary));
            } else if let Some(message) = finish_message {
                self.status(message);
            }
        } else if !urgent {
            if let Some(message) = progress_status {
                self.status(message);
            }
        }
        if urgent {
            true
        } else {
            // Progress-only batch: share the global background paint budget (~15 fps).
            self.take_background_redraw(true)
        }
    }

    fn navigate_bookmark(&mut self, forward: bool) {
        if self.ui.bookmarks.is_empty() {
            self.status("No bookmarks");
            return;
        }
        let origin = self.current_jump_location();
        let current_index = self
            .ui
            .bookmarks
            .iter()
            .position(|bookmark| same_bookmark_location(bookmark, &origin));
        let target_index = if forward {
            current_index.map_or(0, |index| (index + 1) % self.ui.bookmarks.len())
        } else {
            current_index.map_or_else(
                || self.ui.bookmarks.len().saturating_sub(1),
                |index| index.checked_sub(1).unwrap_or(self.ui.bookmarks.len() - 1),
            )
        };
        let target = self.ui.bookmarks[target_index].clone();
        if same_bookmark_location(&target, &origin) {
            self.status("Only current bookmark");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                if forward {
                    self.status("Jumped to next bookmark");
                } else {
                    self.status("Jumped to previous bookmark");
                }
            }
            Err(error) => self.error(error),
        }
    }

    fn open_jump_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if target == origin {
            self.status("Already at selected jump");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.ui.jump_back.retain(|jump| jump != &target);
                self.ui.jump_forward.retain(|jump| jump != &target);
                self.record_jump_origin(origin);
                self.status("Jumped to selected history location");
            }
            Err(error) => self.error(error),
        }
    }

    fn open_bookmark_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if same_bookmark_location(&target, &origin) {
            self.status("Already at selected bookmark");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                self.status("Jumped to bookmark");
            }
            Err(error) => self.error(error),
        }
    }

    fn open_document_symbol_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if target == origin {
            self.status("Already at selected document symbol");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                self.status("Jumped to document symbol");
            }
            Err(error) => self.error(error),
        }
    }

    fn open_local_definition_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if target == origin {
            self.status("Already at selected local definition");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                self.status("Jumped to local definition");
            }
            Err(error) => self.error(error),
        }
    }

    fn open_local_reference_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if target == origin {
            self.status("Already at selected local reference");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                self.status("Jumped to local reference");
            }
            Err(error) => self.error(error),
        }
    }

    fn open_source_annotation_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if target == origin {
            self.status("Already at selected source annotation");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                self.status("Jumped to source annotation");
            }
            Err(error) => self.error(error),
        }
    }

    fn open_workspace_outline_location(&mut self, target: JumpLocation) {
        let origin = self.current_jump_location();
        if target == origin {
            self.status("Already at selected workspace outline symbol");
            return;
        }
        match self.restore_jump_location(&target) {
            Ok(()) => {
                self.record_jump_origin(origin);
                self.status("Jumped to workspace outline symbol");
            }
            Err(error) => self.error(error),
        }
    }

    fn restore_jump_location(&mut self, target: &JumpLocation) -> Result<(), String> {
        let index = if let Some(index) = self
            .workspace
            .buffers()
            .iter()
            .position(|editor| editor.id() == target.editor_id)
        {
            index
        } else if let Some(path) = &target.path {
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => {
                    return Err(format!(
                        "Could not restore jump: target is not a regular file: {}",
                        path.display()
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "Could not restore jump: target is unavailable: {}: {error}",
                        path.display()
                    ));
                }
            }
            let index = self
                .workspace
                .open(path)
                .map_err(|error| format!("Could not restore jump: {error}"))?;
            self.cache_active_workspace_tree_path();
            self.record_active_file_recent();
            index
        } else {
            return Err("Could not restore jump: its untitled buffer was closed".to_owned());
        };
        self.workspace.activate(index);
        self.workspace.active_mut().set_cursor(target.cursor, false);
        Ok(())
    }

    fn page_height(&self) -> isize {
        let layout = Layout::calculate(
            self.ui.screen_size.0,
            self.ui.screen_size.1,
            self.workspace.active().document.line_count(),
            self.config.line_numbers,
        );
        layout.content_height.saturating_sub(1).max(1) as isize
    }

    fn set_line_numbers(&mut self, enabled: bool) {
        self.config.line_numbers = enabled;
        self.workspace.active_mut().reset_vertical_goal();
        self.ui.full_redraw = true;
        self.status(if enabled {
            "Line numbers on"
        } else {
            "Line numbers off"
        });
    }

    fn status(&mut self, message: impl Into<String>) {
        self.ui.status = Some(Status {
            message: message.into(),
            error: false,
        });
    }

    fn error(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let UiMode::Prompt(prompt) = &mut self.ui.mode
            && prompt.kind == PromptFlow::WorkspaceTree
        {
            prompt.notice = Some(format!("Error: {message}"));
        }
        self.ui.status = Some(Status {
            message,
            error: true,
        });
    }
}

fn normalize_action_key(key: KeyEvent, action_active: bool) -> Option<Key> {
    if matches!(key.code, KeyCode::Esc) {
        return Some(Key::Escape);
    }
    if is_control_char(key, 'k') {
        return Some(Key::ControlK);
    }
    if !action_active {
        return match key.code {
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                Some(Key::Character(character))
            }
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            Some(Key::Character(character))
        }
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        _ => None,
    }
}

fn legacy_escape_action_key(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::Char(character) => Some(Key::Character(character)),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        _ => None,
    }
}

fn legacy_escape_modifiers(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::ALT)
        && !modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META | KeyModifiers::HYPER,
        )
}

fn is_control_char(key: KeyEvent, expected: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(character) if character.to_ascii_lowercase() == expected)
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn build_workspace_tree_listing(
    entries: &[ProjectTreeEntry],
    expanded: &HashSet<PathBuf>,
    active_path: Option<&Path>,
    limit: usize,
) -> WorkspaceTreeListing {
    if limit == 0 {
        return WorkspaceTreeListing {
            nodes: Vec::new(),
            truncated: !entries.is_empty(),
        };
    }

    // The active chain is reserved before generic siblings consume the cap.
    // This keeps a deeply nested active file reachable even when it sorts
    // after thousands of root entries.
    let mut reserved = Vec::<WorkspaceTreeNode>::new();
    if let Some(active_path) = active_path {
        let components = active_path.iter().collect::<Vec<_>>();
        let mut directory = PathBuf::new();
        let mut chain_visible = true;
        for (depth, component) in components
            .iter()
            .take(components.len().saturating_sub(1))
            .enumerate()
        {
            directory.push(component);
            reserved.push(WorkspaceTreeNode {
                path: directory.clone(),
                is_directory: true,
                depth,
            });
            if !expanded.contains(&directory) {
                chain_visible = false;
                break;
            }
        }
        if chain_visible && !components.is_empty() {
            reserved.push(WorkspaceTreeNode {
                path: active_path.to_path_buf(),
                is_directory: false,
                depth: components.len().saturating_sub(1),
            });
        }
    }
    let reserved_overflow = reserved.len() > limit;
    reserved.truncate(limit);
    let reserved_paths = reserved
        .iter()
        .map(|node| node.path.clone())
        .collect::<HashSet<_>>();
    let generic_limit = limit.saturating_sub(reserved.len());
    let mut nodes = Vec::with_capacity(limit.min(entries.len().saturating_add(reserved.len())));
    let mut truncated = reserved_overflow;

    for entry in entries {
        if reserved_paths.contains(&entry.path)
            || !workspace_tree_entry_is_visible(&entry.path, expanded)
        {
            continue;
        }
        if nodes.len() >= generic_limit {
            truncated = true;
            break;
        }
        nodes.push(WorkspaceTreeNode {
            path: entry.path.clone(),
            is_directory: entry.is_directory,
            depth: entry.path.components().count().saturating_sub(1),
        });
    }
    nodes.extend(reserved);
    nodes.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| right.is_directory.cmp(&left.is_directory))
    });
    WorkspaceTreeListing { nodes, truncated }
}

fn workspace_tree_entry_is_visible(path: &Path, expanded: &HashSet<PathBuf>) -> bool {
    let mut parent = path.parent();
    while let Some(directory) = parent.filter(|parent| !parent.as_os_str().is_empty()) {
        if !expanded.contains(directory) {
            return false;
        }
        parent = directory.parent();
    }
    true
}

fn safe_tree_component(component: &std::ffi::OsStr) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let mut output = String::new();
        let mut remaining = component.as_bytes();
        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    output.push_str(&safe_tree_text(valid));
                    break;
                }
                Err(error) => {
                    let valid_bytes = error.valid_up_to();
                    if valid_bytes > 0 {
                        let valid = std::str::from_utf8(&remaining[..valid_bytes])
                            .expect("UTF-8 validator reported a valid prefix");
                        output.push_str(&safe_tree_text(valid));
                    }
                    let invalid_bytes = error
                        .error_len()
                        .unwrap_or_else(|| remaining.len().saturating_sub(valid_bytes));
                    for byte in &remaining[valid_bytes..valid_bytes + invalid_bytes] {
                        output.push_str(&format!("\\x{byte:02X}"));
                    }
                    remaining = &remaining[valid_bytes + invalid_bytes..];
                }
            }
        }
        output
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let mut output = String::new();
        for decoded in char::decode_utf16(component.encode_wide()) {
            match decoded {
                Ok(character) => push_safe_tree_character(&mut output, character),
                Err(error) => {
                    output.push_str(&format!("\\u{{{:04X}}}", error.unpaired_surrogate()))
                }
            }
        }
        output
    }

    #[cfg(not(any(unix, windows)))]
    safe_tree_text(&component.to_string_lossy())
}

fn safe_tree_path(path: &Path) -> String {
    path.iter()
        .map(safe_tree_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn safe_tree_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        push_safe_tree_character(&mut output, character);
    }
    output
}

fn push_safe_tree_character(output: &mut String, character: char) {
    if character == '\\' {
        output.push_str("\\\\");
    } else if character.is_control() || is_default_ignorable_tree_character(character) {
        output.push_str(&format!("\\u{{{:04X}}}", character as u32));
    } else {
        output.push(character);
    }
}

fn is_default_ignorable_tree_character(character: char) -> bool {
    matches!(
        character as u32,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// Resolve file identities at construction, explicit snapshot refresh, and
/// runtime file admission/path transitions. Workspace-tree key handling reads
/// this cache and never canonicalizes through the filesystem.
fn snapshot_workspace_tree_document_paths(
    workspace: &Workspace,
    index: Option<&ProjectIndex>,
) -> HashMap<u64, Option<PathBuf>> {
    workspace
        .buffers()
        .iter()
        .map(|editor| {
            let relative = editor.document.path().and_then(|path| {
                resolve_workspace_tree_document_path(&workspace.root, index, path)
            });
            (editor.id(), relative)
        })
        .collect()
}

fn resolve_workspace_tree_document_path(
    workspace_root: &Path,
    index: Option<&ProjectIndex>,
    path: &Path,
) -> Option<PathBuf> {
    let normalized_root;
    let root = if let Some(index) = index {
        index.root()
    } else {
        normalized_root = strict_workspace_tree_file_identity(workspace_root)?;
        &normalized_root
    };
    let identity = strict_workspace_tree_file_identity(path)?;
    let relative = lexical_descendant_path(root, &identity)?;
    workspace_tree_active_path_is_bounded(&relative).then_some(relative)
}

/// Canonicalize an existing path prefix and append only suffix components
/// proven absent. A dangling symlink is an existing unresolved component, not
/// a missing path, so it must never seed an in-workspace explorer identity.
fn strict_workspace_tree_file_identity(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing_suffix = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in missing_suffix.iter().rev() {
                    canonical.push(component);
                }
                return Some(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(ancestor) {
                    // The entry exists but could not be resolved. This covers
                    // dangling symlinks without following them.
                    Ok(_) => return None,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing_suffix.push(ancestor.file_name()?.to_os_string());
                        ancestor = ancestor.parent()?;
                    }
                    Err(_) => return None,
                }
            }
            Err(_) => return None,
        }
    }
}

fn lexical_descendant_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).ok()?;
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn workspace_tree_active_path_is_bounded(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().as_encoded_bytes().len() <= MAX_WORKSPACE_TREE_ACTIVE_PATH_BYTES
        && path.components().count() <= MAX_WORKSPACE_TREE_ACTIVE_PATH_COMPONENTS
}

fn append_workspace_outline_scan_notice(notice: &mut String, scan: &WorkspaceOutlineScan) {
    if scan.truncated_files || scan.truncated_symbols || scan.skipped > 0 {
        let mut details = Vec::new();
        if scan.truncated_files {
            details.push(format!("first {MAX_WORKSPACE_OUTLINE_FILES} files"));
        }
        if scan.truncated_symbols {
            details.push(format!("first {MAX_WORKSPACE_OUTLINE_SYMBOLS} symbols"));
        }
        if scan.skipped > 0 {
            details.push(format!("{} skipped", scan.skipped));
        }
        notice.push_str(" · ");
        notice.push_str(&details.join(", "));
    }
}

fn append_source_annotation_scan_notice(notice: &mut String, scan: &SourceAnnotationScan) {
    if scan.truncated_files || scan.truncated_matches || scan.skipped > 0 {
        let mut details = Vec::new();
        if scan.truncated_files {
            details.push(format!("first {MAX_SOURCE_ANNOTATION_FILES} files"));
        }
        if scan.truncated_matches {
            details.push(format!("first {MAX_SOURCE_ANNOTATIONS} annotations"));
        }
        if scan.skipped > 0 {
            details.push(format!("{} skipped", scan.skipped));
        }
        notice.push_str(" · ");
        notice.push_str(&details.join(", "));
    }
}

fn text_mentions_identifier(text: &str, identifier: &str) -> bool {
    if identifier.is_empty() {
        return false;
    }
    let characters = text.chars().collect::<Vec<_>>();
    let mut cursor = 0;
    while cursor < characters.len() {
        if !local_identifier_start(characters[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < characters.len() && local_identifier_continue(characters[cursor]) {
            cursor += 1;
        }
        if characters[start..cursor].iter().collect::<String>() == identifier {
            return true;
        }
    }
    false
}

fn identifier_columns_in_line(line: &str, identifier: &str) -> Vec<usize> {
    if identifier.is_empty() {
        return Vec::new();
    }
    let characters = line.chars().collect::<Vec<_>>();
    let mut columns = Vec::new();
    let mut cursor = 0;
    while cursor < characters.len() {
        if !local_identifier_start(characters[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < characters.len() && local_identifier_continue(characters[cursor]) {
            cursor += 1;
        }
        if characters[start..cursor].iter().collect::<String>() == identifier {
            columns.push(start);
        }
    }
    columns
}

fn annotation_columns_in_line(line: &str) -> Vec<(&'static str, usize)> {
    let uppercase = line.to_ascii_uppercase();
    let mut matches = Vec::new();
    for tag in SOURCE_ANNOTATION_TAGS {
        let mut search_start = 0;
        while let Some(offset) = uppercase[search_start..].find(tag) {
            let byte_start = search_start + offset;
            let byte_end = byte_start + tag.len();
            if source_annotation_boundary(uppercase.as_bytes().get(byte_start.wrapping_sub(1)))
                && source_annotation_boundary(uppercase.as_bytes().get(byte_end))
            {
                matches.push((*tag, line[..byte_start].chars().count()));
            }
            search_start = byte_end;
        }
    }
    matches.sort_by_key(|(_, column)| *column);
    matches
}

fn source_annotation_boundary(byte: Option<&u8>) -> bool {
    !matches!(byte, Some(b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn local_identifier_occurrences(document: &Document, identifier: &str) -> Vec<usize> {
    let mut occurrences = Vec::new();
    for line_index in 0..document.line_count() {
        let line = document.line(line_index);
        let line_start = document.line_start_char(line_index);
        occurrences.extend(
            identifier_columns_in_line(&line, identifier)
                .into_iter()
                .map(|column| line_start + column),
        );
    }
    occurrences
}

fn local_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn local_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn same_workspace(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn lsp_document_end(document: &Document) -> LspPosition {
    let snapshot = DocumentSnapshot::from_rope(document.rope(), DocumentVersion::INITIAL);
    snapshot
        .char_to_position(snapshot.len_chars())
        .expect("the end of an editor document is always a valid LSP position")
}

fn merge_startup_status(status: &mut Option<Status>, message: impl Into<String>, error: bool) {
    let message = message.into();
    if let Some(existing) = status {
        existing.message.push_str(" · ");
        existing.message.push_str(&message);
        existing.error |= error;
    } else {
        *status = Some(Status { message, error });
    }
}

fn push_bounded_jump(history: &mut Vec<JumpLocation>, location: JumpLocation) {
    if history.last() == Some(&location) {
        return;
    }
    if history.len() == MAX_JUMP_HISTORY {
        history.remove(0);
    }
    history.push(location);
}

fn same_bookmark_location(left: &JumpLocation, right: &JumpLocation) -> bool {
    if left.cursor != right.cursor {
        return false;
    }
    match (&left.path, &right.path) {
        (Some(left), Some(right)) => left == right,
        (None, None) => left.editor_id == right.editor_id,
        _ => false,
    }
}

fn recent_files_from_workspace(workspace: &Workspace) -> Vec<PathBuf> {
    let active_id = workspace.active().id();
    let mut paths = Vec::new();
    if let Some(path) = workspace.active().document.path() {
        paths.push(path.to_path_buf());
    }
    for editor in workspace.buffers() {
        if editor.id() == active_id {
            continue;
        }
        if let Some(path) = editor.document.path()
            && !paths.iter().any(|existing| same_workspace(existing, path))
        {
            paths.push(path.to_path_buf());
        }
        if paths.len() == MAX_RECENT_FILES_VIEW {
            break;
        }
    }
    paths
}

fn editor_location_position(editor: &Editor, char_index: usize) -> (usize, usize) {
    let line = editor.document.char_to_line(char_index);
    let column = char_index
        .min(editor.document.len_chars())
        .saturating_sub(editor.document.line_start_char(line));
    (line + 1, column + 1)
}

fn active_path_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn diagnostic_severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Information => "information",
        DiagnosticSeverity::Hint => "hint",
        DiagnosticSeverity::Unknown(_) => "diagnostic",
    }
}

fn trim_task_output(output: &mut String) {
    const MAX_BYTES: usize = 1024 * 1024;
    trim_bounded_text(output, MAX_BYTES, "[… earlier task output trimmed …]\n");
}

fn trim_bounded_text(output: &mut String, max_bytes: usize, marker: &str) {
    if output.len() <= max_bytes {
        return;
    }
    let mut start = output.len() - max_bytes;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    output.drain(..start);
    output.insert_str(0, marker);
}

fn lsp_operation_name(operation: LspOperation) -> &'static str {
    match operation {
        LspOperation::Initialize => "initialization",
        LspOperation::Completion => "completion",
        LspOperation::Hover => "hover",
        LspOperation::Definition => "definition",
        LspOperation::References => "references",
        LspOperation::DocumentSymbols => "document symbols",
        LspOperation::WorkspaceSymbols => "workspace symbols",
        LspOperation::Formatting => "formatting",
        LspOperation::Shutdown => "shutdown",
    }
}

fn task_problem_marker(severity: TaskProblemSeverity) -> char {
    match severity {
        TaskProblemSeverity::Error => 'E',
        TaskProblemSeverity::Warning => 'W',
        TaskProblemSeverity::Information => 'I',
        TaskProblemSeverity::Unknown => '?',
    }
}

fn merge_problem_notice(notice: &mut Option<String>, message: impl AsRef<str>) {
    if let Some(notice) = notice {
        notice.push_str(" · ");
        notice.push_str(message.as_ref());
    } else {
        *notice = Some(message.as_ref().to_owned());
    }
}

fn problem_entry_position(entry: &PromptEntry) -> Option<(PathBuf, usize, usize)> {
    match entry {
        PromptEntry::Location(location) | PromptEntry::ProblemLocation(location, _) => {
            let path = file_uri_to_path(&location.uri).ok()?;
            Some((
                crate::workspace::normalized_file_path(&path),
                location.range.start.line.get(),
                location.range.start.character.get(),
            ))
        }
        PromptEntry::TaskProblem(problem) => Some((
            crate::workspace::normalized_file_path(&problem.path),
            problem.line,
            problem.column,
        )),
        _ => None,
    }
}

fn problem_entry_is_error(entry: &PromptEntry) -> bool {
    match entry {
        PromptEntry::ProblemLocation(_, severity) => *severity == DiagnosticSeverity::Error,
        PromptEntry::TaskProblem(problem) => problem.severity == TaskProblemSeverity::Error,
        _ => false,
    }
}

fn env_value(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn route_transport() -> &'static str {
    if env::var_os("MOSH_CONNECTION").is_some() || env::var_os("MOSH_IP").is_some() {
        "mosh"
    } else if env::var_os("SSH_CONNECTION").is_some() || env::var_os("SSH_TTY").is_some() {
        "ssh"
    } else {
        "local"
    }
}

fn branch_label(head: &BranchHead) -> Option<String> {
    match head {
        BranchHead::Named(name) => Some(name.clone()),
        BranchHead::Detached => Some("detached".to_owned()),
        BranchHead::Unknown => None,
    }
}

fn git_mutation_summary(mutation: &GitMutation) -> String {
    match mutation {
        GitMutation::StageCurrent(path) => format!("stage {}", path.display()),
        GitMutation::UnstageCurrent(path) => format!("unstage {}", path.display()),
        GitMutation::CommitStaged { message } => format!("commit staged as {message:?}"),
    }
}

fn git_mutation_result_summary(result: &GitMutationResult) -> String {
    match result {
        GitMutationResult::Staged(path) => format!("Staged {}", path.display()),
        GitMutationResult::Unstaged(path) => format!("Unstaged {}", path.display()),
        GitMutationResult::Committed { message } => {
            format!("Committed staged changes as {message:?}")
        }
    }
}

fn git_state_glyph(state: FileState) -> char {
    match state {
        FileState::Unmodified => '·',
        FileState::Modified => 'M',
        FileState::TypeChanged => 'T',
        FileState::Added => 'A',
        FileState::Deleted => 'D',
        FileState::Renamed => 'R',
        FileState::Copied => 'C',
        FileState::UpdatedButUnmerged => 'U',
        FileState::Untracked => '?',
        FileState::Ignored => '!',
        FileState::Unknown(byte) => char::from(byte),
    }
}

fn git_state_label(state: FileState) -> String {
    match state {
        FileState::Unmodified => "unmodified".to_owned(),
        FileState::Modified => "modified".to_owned(),
        FileState::TypeChanged => "type changed".to_owned(),
        FileState::Added => "added".to_owned(),
        FileState::Deleted => "deleted".to_owned(),
        FileState::Renamed => "renamed".to_owned(),
        FileState::Copied => "copied".to_owned(),
        FileState::UpdatedButUnmerged => "unmerged".to_owned(),
        FileState::Untracked => "untracked".to_owned(),
        FileState::Ignored => "ignored".to_owned(),
        FileState::Unknown(byte) => format!("unknown status byte {byte}"),
    }
}

fn git_entry_kind_label(kind: crate::git::StatusEntryKind) -> String {
    match kind {
        crate::git::StatusEntryKind::Ordinary => "ordinary".to_owned(),
        crate::git::StatusEntryKind::Renamed { score } => format!("renamed ({score}%)"),
        crate::git::StatusEntryKind::Copied { score } => format!("copied ({score}%)"),
        crate::git::StatusEntryKind::Unmerged => "unmerged".to_owned(),
        crate::git::StatusEntryKind::Untracked => "untracked".to_owned(),
        crate::git::StatusEntryKind::Ignored => "ignored".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::ffi::OsStr;
    use std::process::Command;

    use crate::Document;
    use crate::config::LanguageServerConfig;
    use crate::lsp::{Line, LspPosition, Utf16Offset};
    use crate::lsp_client::LspRange;

    fn app_with_text(text: &str) -> App {
        let mut workspace = Workspace::new(None).unwrap();
        let replaced = workspace
            .replace_editor(0, crate::Editor::new(Document::from_text(text)))
            .expect("the initial test buffer exists");
        drop(replaced);
        App::new_ready_for_test(workspace, Config::default())
    }

    fn rust_lsp_config() -> Config {
        Config {
            language_servers: vec![LanguageServerConfig {
                name: "rust-test".to_owned(),
                extensions: vec!["rs".to_owned()],
                language_id: "rust".to_owned(),
                argv: vec!["unused-test-server".to_owned()],
            }],
            ..Config::default()
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn exit_action_layer(app: &mut App) {
        app.handle_event(key(KeyCode::Esc));
        assert!(app.ui.keymap.is_active());
        app.handle_event(key(KeyCode::Esc));
        assert!(!app.ui.keymap.is_active());
        assert!(app.ui.edit_transition.armed.is_some());
    }

    #[test]
    fn action_exit_cues_only_the_first_real_document_revision() {
        let mut app = app_with_text("abc");
        exit_action_layer(&mut app);

        app.handle_event(key(KeyCode::Right));
        assert!(app.ui.edit_transition.armed.is_some());
        assert!(!app.edit_transition_cue_active());

        app.handle_event(key(KeyCode::Char('x')));
        let first_deadline = app
            .ui
            .edit_transition
            .cue_until
            .expect("the first document edit starts the cue");
        assert!(app.edit_transition_cue_active_at(first_deadline - Duration::from_millis(1)));

        app.handle_event(key(KeyCode::Char('y')));
        assert_eq!(app.ui.edit_transition.cue_until, Some(first_deadline));
        assert!(app.poll_ui_transients_at(first_deadline));
        assert!(!app.poll_ui_transients_at(first_deadline));
        assert!(!app.edit_transition_cue_active_at(first_deadline));
    }

    #[test]
    fn action_edit_and_paste_share_the_revision_driven_cue() {
        let mut action_app = app_with_text("alpha\n");
        action_app.handle_event(key(KeyCode::Esc));
        action_app.handle_event(key(KeyCode::Char('D')));
        assert!(action_app.edit_transition_cue_active());
        assert_eq!(
            action_app.workspace.active().document.text(),
            "alpha\nalpha\n"
        );

        let mut paste_app = app_with_text("");
        exit_action_layer(&mut paste_app);
        paste_app.handle_event(Event::Paste("pasted".to_owned()));
        assert!(paste_app.edit_transition_cue_active());
        assert_eq!(paste_app.workspace.active().document.text(), "pasted");
    }

    #[test]
    fn rejected_read_only_edit_preserves_the_arm_and_error() {
        let mut app = app_with_text("editable");
        app.workspace.open_virtual("Read only", "snapshot");
        exit_action_layer(&mut app);
        let baseline = app.ui.edit_transition.armed;

        app.handle_event(key(KeyCode::Char('x')));

        assert_eq!(app.ui.edit_transition.armed, baseline);
        assert!(app.ui.edit_transition.cue_until.is_none());
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .is_some_and(|message| message.contains("read-only"))
        );
    }

    #[test]
    fn control_g_and_unknown_action_exit_arm_without_replacing_errors() {
        let mut control_g = app_with_text("");
        control_g.handle_event(key(KeyCode::Esc));
        control_g.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        )));
        assert!(control_g.ui.edit_transition.armed.is_some());

        let mut unknown = app_with_text("");
        unknown.handle_event(key(KeyCode::Esc));
        unknown.handle_event(key(KeyCode::Char('!')));
        assert!(unknown.ui.edit_transition.armed.is_some());
        assert!(unknown.status_is_error());
        unknown.handle_event(Event::Paste("x".to_owned()));
        assert!(unknown.edit_transition_cue_active());
        assert!(unknown.status_is_error());
    }

    fn test_git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn test_git<const N: usize>(root: &Path, arguments: [&OsStr; N]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn git_mutation_app() -> Option<(tempfile::TempDir, PathBuf, App)> {
        if !test_git_available() {
            return None;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return None;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app mutation test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app-mutation@example.invalid")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("commit.gpgSign"),
                OsStr::new("false")
            ]
        ));
        let path = directory.path().join("current file.rs");
        std::fs::write(&path, "pub fn base() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("current file.rs")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let app = App::new_ready_for_test(workspace, Config::default());
        Some((directory, path, app))
    }

    fn poll_git_mutation_to_refresh(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.git.pending.is_some() || app.git.status.is_pending() {
            app.poll_services();
            assert!(
                Instant::now() < deadline,
                "Git mutation or follow-up snapshot missed its deadline"
            );
            std::thread::yield_now();
        }
        app.poll_services();
    }

    #[test]
    fn git_stage_and_unstage_require_trust_and_run_asynchronously() {
        let Some((_directory, path, mut app)) = git_mutation_app() else {
            return;
        };
        app.workspace
            .active_mut()
            .insert("// changed\n", EditKind::Insert)
            .unwrap();
        app.save_current();
        assert!(!app.workspace.active().document.is_modified());

        app.execute_action(Action::GitStageCurrent);
        assert!(matches!(
            &app.ui.mode,
            UiMode::GitTrust(GitMutation::StageCurrent(path))
                if path == Path::new("current file.rs")
        ));
        assert!(app.footer_hint().contains("repository filters/hooks"));
        assert!(
            !app.git
                .repository
                .as_ref()
                .unwrap()
                .status_path(&path)
                .unwrap()
                .unwrap()
                .is_staged()
        );

        app.handle_event(key(KeyCode::Char('v')));
        assert_eq!(
            app.workspace.active().document.display_name(),
            "Git Operation Trust"
        );
        let details = app.workspace.active().document.text();
        assert!(details.contains(&format!(
            "Repository: {}",
            app.git.repository.as_ref().unwrap().root().display()
        )));
        assert!(details.contains("Repository path: current file.rs"));
        assert!(details.contains("This view did not run Git"));
        assert!(app.close_active_buffer(false).is_ok());

        app.execute_ex_command(ExCommand::GitStageCurrent);
        app.handle_event(key(KeyCode::Char('y')));
        assert!(app.git.pending.is_some());
        app.execute_ex_command(ExCommand::GitUnstageCurrent);
        assert_eq!(
            app.status_message(),
            Some("A Git operation is already running")
        );
        assert!(app.status_is_error());
        poll_git_mutation_to_refresh(&mut app);
        assert!(
            app.git
                .repository
                .as_ref()
                .unwrap()
                .status_path(&path)
                .unwrap()
                .unwrap()
                .is_staged()
        );

        app.execute_action(Action::GitUnstageCurrent);
        assert!(matches!(
            &app.ui.mode,
            UiMode::GitTrust(GitMutation::UnstageCurrent(path))
                if path == Path::new("current file.rs")
        ));
        app.handle_event(key(KeyCode::Char('y')));
        poll_git_mutation_to_refresh(&mut app);
        let status = app
            .git
            .repository
            .as_ref()
            .unwrap()
            .status_path(&path)
            .unwrap()
            .unwrap();
        assert!(!status.is_staged());
        assert!(status.has_worktree_change());
    }

    #[test]
    fn git_index_mutations_refuse_unsaved_buffers_without_entering_trust() {
        let Some((directory, path, mut app)) = git_mutation_app() else {
            return;
        };
        app.workspace
            .active_mut()
            .insert("// saved\n", EditKind::Insert)
            .unwrap();
        app.save_current();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("current file.rs")
            ]
        ));
        app.workspace
            .active_mut()
            .insert("// unsaved\n", EditKind::Insert)
            .unwrap();

        app.execute_action(Action::GitStageCurrent);
        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(
            app.status_message(),
            Some("Save the current buffer first; Git would otherwise use the disk version")
        );
        assert!(app.status_is_error());

        app.execute_ex_command(ExCommand::GitUnstageCurrent);
        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(
            app.status_message(),
            Some("Save the current buffer first; Git would otherwise use the disk version")
        );
        assert!(
            app.git
                .repository
                .as_ref()
                .unwrap()
                .status_path(&path)
                .unwrap()
                .unwrap()
                .is_staged(),
            "the staged disk version was preserved"
        );
    }

    #[test]
    fn git_commit_uses_bounded_message_prompt_trust_and_background_refresh() {
        let Some((directory, _path, mut app)) = git_mutation_app() else {
            return;
        };
        app.workspace
            .active_mut()
            .insert("// commit me\n", EditKind::Insert)
            .unwrap();
        app.save_current();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("current file.rs")
            ]
        ));

        app.execute_action(Action::GitCommitStaged);
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::GitCommitMessage,
                ..
            })
        ));
        app.handle_event(key(KeyCode::Enter));
        assert!(app.status_is_error());
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::GitCommitMessage,
                ..
            })
        ));

        app.handle_event(Event::Paste("trusted local commit".to_owned()));
        app.handle_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.ui.mode,
            UiMode::GitTrust(GitMutation::CommitStaged { message })
                if message == "trusted local commit"
        ));
        assert!(app.footer_hint().contains("trusted local commit"));
        app.handle_event(key(KeyCode::Char('y')));
        assert!(app.git.pending.is_some());
        poll_git_mutation_to_refresh(&mut app);

        let output = Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(["log", "-1", "--pretty=%s"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "trusted local commit"
        );
        assert!(app.status_message().is_some_and(|message| {
            message.contains("Committed staged changes as \"trusted local commit\"")
        }));
    }

    fn workspace_tree_app() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src/nested")).unwrap();
        std::fs::create_dir_all(directory.path().join("assets")).unwrap();
        std::fs::create_dir(directory.path().join("empty")).unwrap();
        std::fs::create_dir(directory.path().join("target")).unwrap();
        std::fs::write(directory.path().join("README.md"), "workspace\n").unwrap();
        std::fs::write(directory.path().join("assets/image.png"), [0, 1, 2, 3]).unwrap();
        std::fs::write(directory.path().join("target/ignored.rs"), "ignored\n").unwrap();
        std::fs::write(directory.path().join("src/lib.rs"), "pub fn library() {}\n").unwrap();
        let active = directory.path().join("src/nested/main.rs");
        std::fs::write(&active, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let app = App::new_ready_for_test(workspace, Config::default());
        (directory, app)
    }

    #[test]
    fn blocked_index_and_git_workers_cannot_delay_the_first_frame() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new(workspace, Config::default());
        let (index_started_tx, index_started_rx) = std::sync::mpsc::channel();
        let (index_release_tx, index_release_rx) = std::sync::mpsc::channel();
        let (git_started_tx, git_started_rx) = std::sync::mpsc::channel();
        let (git_release_tx, git_release_rx) = std::sync::mpsc::channel();
        let root = directory.path().to_path_buf();

        let index_generation = app.services.start_project_test_job(move || {
            index_started_tx.send(()).unwrap();
            index_release_rx.recv().unwrap();
            ProjectIndex::build(root).map_err(|error| error.to_string())
        });
        let git_generation = app.services.start_git_test_job(move || {
            git_started_tx.send(()).unwrap();
            git_release_rx.recv().unwrap();
            Ok(None)
        });
        app.project.status = ServiceStatus::Pending(index_generation);
        app.git.status = ServiceStatus::Pending(git_generation);
        index_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("index worker started");
        git_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("Git worker started");

        let mut renderer = crate::render::Renderer::default();
        let mut output = Vec::new();
        renderer.draw(&mut output, &mut app, (80, 24)).unwrap();
        assert!(!output.is_empty());
        assert!(String::from_utf8_lossy(&output).contains("wscrpt"));
        assert!(app.project.status.is_pending());
        assert!(app.git.status.is_pending());

        index_release_tx.send(()).unwrap();
        git_release_tx.send(()).unwrap();
    }

    #[test]
    fn stale_project_generation_cannot_replace_the_newer_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("kept.rs"), "fn kept() {}\n").unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let stale_directory = tempfile::tempdir().unwrap();
        std::fs::write(stale_directory.path().join("stale.rs"), "fn stale() {}\n").unwrap();
        let stale_index = ProjectIndex::build(stale_directory.path()).unwrap();

        let stale_generation = app
            .services
            .start_project_test_job(|| Err("superseded".to_owned()));
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let root = directory.path().to_path_buf();
        let current_generation = app.services.start_project_test_job(move || {
            release_receiver.recv().unwrap();
            ProjectIndex::build(root).map_err(|error| error.to_string())
        });
        app.project.status = ServiceStatus::Pending(current_generation);
        app.services.send_test_event(ServiceEvent::Project {
            tag: app.services.test_tag(stale_generation),
            result: Ok(stale_index),
        });

        app.poll_services();
        let files = app.project.index.as_ref().unwrap().files();
        assert!(files.iter().any(|path| path == Path::new("kept.rs")));
        assert!(!files.iter().any(|path| path == Path::new("stale.rs")));
        assert!(app.project.status.is_pending());
        release_sender.send(()).unwrap();
    }

    #[test]
    fn workspace_tree_listing_is_hierarchical_bounded_and_reserves_active_chain() {
        let mut entries = (0..5_000)
            .map(|index| ProjectTreeEntry {
                path: PathBuf::from(format!("file-{index:04}.rs")),
                is_directory: false,
            })
            .chain([
                ProjectTreeEntry {
                    path: PathBuf::from("zz"),
                    is_directory: true,
                },
                ProjectTreeEntry {
                    path: PathBuf::from("zz/deep"),
                    is_directory: true,
                },
                ProjectTreeEntry {
                    path: PathBuf::from("zz/deep/current.rs"),
                    is_directory: false,
                },
            ])
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let expanded = HashSet::from([PathBuf::from("zz"), PathBuf::from("zz/deep")]);
        let active = Path::new("zz/deep/current.rs");
        let listing = build_workspace_tree_listing(&entries, &expanded, Some(active), 4_096);

        assert_eq!(listing.nodes.len(), 4_096);
        assert!(listing.truncated);
        assert!(
            listing
                .nodes
                .iter()
                .any(|node| node.path == Path::new("zz"))
        );
        assert!(
            listing
                .nodes
                .iter()
                .any(|node| node.path == Path::new("zz/deep"))
        );
        assert!(listing.nodes.iter().any(|node| node.path == active));
    }

    #[test]
    fn workspace_tree_opens_on_the_active_file_with_its_ancestors_expanded() {
        let (_directory, mut app) = workspace_tree_app();
        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("src/nested/main.rs"))
        );

        app.execute_action(Action::WorkspaceTree);

        let prompt = app.prompt().unwrap();
        assert_eq!(prompt.kind, PromptFlow::WorkspaceTree);
        assert_eq!(app.overlay().unwrap().title, "WORKSPACE");
        assert!(app.project.tree_expanded.contains(Path::new("src")));
        assert!(app.project.tree_expanded.contains(Path::new("src/nested")));
        assert!(matches!(
            prompt.entries.get(prompt.selected),
            Some(PromptEntry::WorkspaceTree(node))
                if node.path == Path::new("src/nested/main.rs")
        ));
        assert!(prompt.labels.iter().any(|label| label.contains("▾ src/")));
        assert!(
            prompt
                .labels
                .iter()
                .any(|label| label.contains("● main.rs"))
        );
    }

    #[test]
    fn workspace_sidebar_toggle_persists_and_reveals_active_path() {
        let (_directory, mut app) = workspace_tree_app();

        app.execute_action(Action::WorkspaceSidebar);

        assert!(app.workspace_sidebar_visible());
        assert_eq!(
            app.status_message(),
            Some("Workspace sidebar on; Esc w t opens the navigable tree")
        );
        let sidebar = app.project_sidebar_view(16);
        assert!(!sidebar.unavailable);
        assert!(
            sidebar
                .lines
                .iter()
                .any(|line| { line.active && line.text.contains("main.rs") && !line.directory })
        );
        let session = app.session_snapshot();
        assert!(session.layout.workspace_tree_visible);

        let mut restored = app_with_text("");
        restored.apply_session_layout(session.layout);
        assert!(restored.workspace_sidebar_visible());

        app.execute_action(Action::WorkspaceSidebar);
        assert!(!app.workspace_sidebar_visible());
        assert_eq!(app.status_message(), Some("Workspace sidebar off"));
    }

    #[test]
    fn workspace_tree_right_left_and_enter_navigate_directories_without_leaving_prompt() {
        let (_directory, mut app) = workspace_tree_app();
        app.workspace.activate(0);
        app.workspace
            .open(app.workspace.root.join("README.md"))
            .unwrap();
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("src")));

        app.handle_event(key(KeyCode::Right));
        assert!(app.project.tree_expanded.contains(Path::new("src")));
        assert!(app.select_workspace_tree_path(Path::new("src/lib.rs")));

        app.handle_event(key(KeyCode::Left));
        assert!(matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("src")
        ));
        app.handle_event(key(KeyCode::Left));
        assert!(!app.project.tree_expanded.contains(Path::new("src")));
        assert!(matches!(app.ui.mode, UiMode::Prompt(_)));

        app.handle_event(key(KeyCode::Enter));
        assert!(app.project.tree_expanded.contains(Path::new("src")));
        assert!(matches!(app.ui.mode, UiMode::Prompt(_)));
    }

    #[test]
    fn workspace_tree_includes_binary_files_and_empty_dirs_but_omits_ignored_trees() {
        let (_directory, mut app) = workspace_tree_app();
        app.execute_action(Action::WorkspaceTree);

        assert!(app.select_workspace_tree_path(Path::new("empty")));
        assert!(!app.select_workspace_tree_path(Path::new("target")));
        assert!(app.select_workspace_tree_path(Path::new("assets")));
        app.handle_event(key(KeyCode::Right));
        assert!(app.select_workspace_tree_path(Path::new("assets/image.png")));
        app.handle_event(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Prompt(_)));
        assert!(app.status_is_error());
        assert!(
            app.overlay()
                .unwrap()
                .notice
                .is_some_and(|notice| notice.starts_with("Error: Could not open workspace file"))
        );

        app.handle_event(key(KeyCode::Char('i')));
        assert!(app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path == Path::new("assets/image.png"))
        }));
        assert!(!app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path.starts_with("target"))
        }));
    }

    #[test]
    fn workspace_tree_reveals_one_bounded_active_path_outside_the_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("node_modules/pkg/current.rs");
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, "active\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        assert!(
            !app.project
                .index
                .as_ref()
                .unwrap()
                .contains_tree_path("node_modules/pkg/current.rs")
        );

        app.execute_action(Action::WorkspaceTree);

        assert!(
            app.project
                .tree_expanded
                .contains(Path::new("node_modules"))
        );
        assert!(matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("node_modules/pkg/current.rs")
        ));
        assert!(
            app.prompt()
                .unwrap()
                .notice
                .as_deref()
                .unwrap()
                .contains("active buffer revealed outside snapshot")
        );

        app.handle_event(Event::Paste("current".to_owned()));
        assert!(app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path == Path::new("node_modules/pkg/current.rs"))
        }));
    }

    #[test]
    fn workspace_tree_reveals_a_new_in_root_buffer_without_claiming_it_is_indexed() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("new/deep/draft.rs");
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::WorkspaceTree);

        assert!(matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("new/deep/draft.rs")
        ));
        assert!(
            app.prompt()
                .unwrap()
                .notice
                .as_deref()
                .unwrap()
                .contains("outside snapshot")
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_does_not_seed_an_active_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&outside_file, "outside\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let workspace = Workspace::from_path(
            Some(root.path().join("escape/outside.rs")),
            Some(root.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        assert_eq!(app.workspace_tree_active_path(), None);
        app.execute_action(Action::WorkspaceTree);
        assert!(!app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path.starts_with("escape"))
        }));
    }

    #[test]
    fn workspace_tree_caches_post_start_edit_for_ignored_and_new_in_root_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("seed.rs"), "seed\n").unwrap();
        let ignored = directory.path().join("node_modules/pkg/ignored.rs");
        std::fs::create_dir_all(ignored.parent().unwrap()).unwrap();
        std::fs::write(&ignored, "ignored\n").unwrap();
        let workspace = Workspace::from_path(
            Some(directory.path().join("seed.rs")),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::Edit(ignored.clone()));
        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("node_modules/pkg/ignored.rs"))
        );
        app.execute_action(Action::WorkspaceTree);
        assert!(matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("node_modules/pkg/ignored.rs")
        ));

        app.cancel_prompt();
        let new_path = directory.path().join("drafts/new.rs");
        app.execute_ex_command(ExCommand::Edit(new_path));
        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("drafts/new.rs"))
        );
        app.execute_action(Action::WorkspaceTree);
        assert!(matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("drafts/new.rs")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_caches_post_start_edit_symlink_escape_as_outside() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("seed.rs"), "seed\n").unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();
        std::fs::write(root.path().join("inside/file.rs"), "trusted\n").unwrap();
        std::fs::write(outside.path().join("file.rs"), "outside\n").unwrap();
        let workspace = Workspace::from_path(
            Some(root.path().join("seed.rs")),
            Some(root.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        assert!(
            app.project
                .index
                .as_ref()
                .unwrap()
                .contains_tree_path("inside/file.rs")
        );
        std::fs::rename(root.path().join("inside"), root.path().join("held")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("inside")).unwrap();

        app.execute_ex_command(ExCommand::Edit(root.path().join("inside/file.rs")));

        let editor_id = app.workspace.active().id();
        assert_eq!(app.project.tree_document_paths.get(&editor_id), Some(&None));
        assert_eq!(app.workspace_tree_active_path(), None);
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("inside")));
        app.handle_event(key(KeyCode::Right));
        assert!(!matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("inside/file.rs")
        ));
        assert!(
            app.prompt()
                .unwrap()
                .labels
                .iter()
                .all(|label| !label.contains('●'))
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_rejects_dangling_ancestor_symlink_identity() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let outside_missing = parent.path().join("outside-missing");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("seed.rs"), "seed\n").unwrap();
        std::os::unix::fs::symlink(&outside_missing, root.join("escape")).unwrap();
        let workspace =
            Workspace::from_path(Some(root.join("seed.rs")), Some(root.clone())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::Edit(root.join("escape/new.rs")));

        assert_eq!(
            app.project
                .tree_document_paths
                .get(&app.workspace.active().id()),
            Some(&None)
        );
        assert_eq!(app.workspace_tree_active_path(), None);

        // Making the dangling target resolvable later must not turn the
        // previously cached negative identity into an in-root reveal.
        std::fs::create_dir(&outside_missing).unwrap();
        app.execute_action(Action::WorkspaceTree);
        assert!(!app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path.starts_with("escape"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_rejects_dangling_final_symlink_identity() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("seed.rs"), "seed\n").unwrap();
        std::os::unix::fs::symlink(parent.path().join("missing.rs"), root.join("draft.rs"))
            .unwrap();
        let workspace =
            Workspace::from_path(Some(root.join("seed.rs")), Some(root.clone())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::Edit(root.join("draft.rs")));

        assert_eq!(
            app.project
                .tree_document_paths
                .get(&app.workspace.active().id()),
            Some(&None)
        );
        assert_eq!(app.workspace_tree_active_path(), None);
        app.execute_action(Action::WorkspaceTree);
        assert!(!app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path == Path::new("draft.rs"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_strict_identity_still_reveals_ordinary_missing_nested_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("seed.rs"), "seed\n").unwrap();
        let workspace = Workspace::from_path(
            Some(root.path().join("seed.rs")),
            Some(root.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::Edit(root.path().join("drafts/deep/new.rs")));

        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("drafts/deep/new.rs"))
        );
        app.execute_action(Action::WorkspaceTree);
        assert!(matches!(
            app.selected_workspace_tree_node(),
            Some(node) if node.path == Path::new("drafts/deep/new.rs")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_selection_reuses_buffer_opened_through_symlink_alias() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::fs::write(root.path().join("real/value.rs"), "value\n").unwrap();
        std::fs::write(root.path().join("seed.rs"), "seed\n").unwrap();
        std::os::unix::fs::symlink(root.path().join("real"), root.path().join("alias")).unwrap();
        let workspace = Workspace::from_path(
            Some(root.path().join("seed.rs")),
            Some(root.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.execute_ex_command(ExCommand::Edit(root.path().join("alias/value.rs")));
        let editor_id = app.workspace.active().id();
        let buffer_count = app.workspace.len();

        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("real/value.rs"))
        );
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("real/value.rs")));
        app.handle_event(key(KeyCode::Enter));

        assert_eq!(app.workspace.len(), buffer_count);
        assert_eq!(app.workspace.active().id(), editor_id);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(root.path().join("alias/value.rs").as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_save_publication_recaches_a_retargeted_symlink_as_outside() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside = root.path().join("inside.rs");
        let alias = root.path().join("alias.rs");
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&inside, "inside\n").unwrap();
        std::fs::write(&outside_file, "outside\n").unwrap();
        std::os::unix::fs::symlink(&inside, &alias).unwrap();
        let workspace =
            Workspace::from_path(Some(alias.clone()), Some(root.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("inside.rs"))
        );
        app.workspace
            .active_mut()
            .insert("published ", EditKind::Insert)
            .unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside_file, &alias).unwrap();

        app.save_current();
        assert!(app.status_is_error());
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "outside\n");
        assert_eq!(
            app.workspace_tree_active_path(),
            Some(PathBuf::from("inside.rs"))
        );

        app.execute_ex_command(ExCommand::SaveForce);

        assert!(!app.status_is_error());
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            app.workspace.active().document.text()
        );
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "inside\n");
        assert_eq!(
            app.project
                .tree_document_paths
                .get(&app.workspace.active().id()),
            Some(&None)
        );
        assert_eq!(app.workspace_tree_active_path(), None);
    }

    #[test]
    fn workspace_tree_filter_reveals_active_file_replacing_snapshot_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("foo")).unwrap();
        std::fs::write(root.path().join("seed.rs"), "seed\n").unwrap();
        let workspace = Workspace::from_path(
            Some(root.path().join("seed.rs")),
            Some(root.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let index = app.project.index.as_ref().unwrap();
        assert!(index.contains_tree_path("foo"));
        assert!(!index.contains_tree_file("foo"));
        std::fs::remove_dir(root.path().join("foo")).unwrap();
        std::fs::write(root.path().join("foo"), "now a file\n").unwrap();
        app.execute_ex_command(ExCommand::Edit(root.path().join("foo")));

        app.execute_action(Action::WorkspaceTree);
        app.handle_event(Event::Paste("foo".to_owned()));

        assert!(app.prompt().unwrap().entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path == Path::new("foo") && !node.is_directory)
        }));
        assert!(
            app.prompt()
                .unwrap()
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("active buffer included outside snapshot"))
        );
    }

    #[test]
    fn workspace_tree_first_filter_input_selects_rank_zero_for_key_and_paste() {
        let (_directory, mut app) = workspace_tree_app();
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("src/nested/main.rs")));
        assert_ne!(app.prompt().unwrap().selected, 0);

        app.handle_event(key(KeyCode::Char('r')));
        assert_eq!(app.prompt().unwrap().selected, 0);
        app.handle_event(key(KeyCode::Backspace));
        assert!(app.select_workspace_tree_path(Path::new("src/nested/main.rs")));
        assert_ne!(app.prompt().unwrap().selected, 0);

        app.handle_event(Event::Paste("r".to_owned()));
        assert_eq!(app.prompt().unwrap().selected, 0);
    }

    #[test]
    fn quick_open_first_filter_input_selects_rank_zero() {
        let (_directory, mut app) = workspace_tree_app();
        app.execute_action(Action::QuickOpen);
        app.handle_event(key(KeyCode::Down));
        app.handle_event(key(KeyCode::Down));
        assert_eq!(app.prompt().unwrap().selected, 2);

        app.handle_event(Event::Paste("r".to_owned()));

        assert_eq!(app.prompt().unwrap().selected, 0);
    }

    #[test]
    fn workspace_tree_filtered_cap_is_labeled_partial() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..(crate::project::MAX_RESULTS + 1) {
            std::fs::write(
                directory.path().join(format!("match-{index:03}.rs")),
                "match\n",
            )
            .unwrap();
        }
        let workspace = Workspace::from_path(
            Some(directory.path().join("match-000.rs")),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::WorkspaceTree);
        app.handle_event(Event::Paste("match".to_owned()));

        assert_eq!(
            app.prompt().unwrap().entries.len(),
            crate::project::MAX_RESULTS
        );
        assert!(
            app.prompt()
                .unwrap()
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Partial filtered file view"))
        );
    }

    #[test]
    fn workspace_tree_filtered_active_overlay_reserves_a_slot_and_claims_only_inclusion() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..crate::project::MAX_RESULTS {
            std::fs::write(
                directory.path().join(format!("match-{index:03}.rs")),
                "match\n",
            )
            .unwrap();
        }
        let active = directory.path().join("node_modules/match-active.rs");
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, "active\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::WorkspaceTree);
        app.handle_event(Event::Paste("match".to_owned()));

        let prompt = app.prompt().unwrap();
        assert_eq!(prompt.entries.len(), crate::project::MAX_RESULTS);
        assert!(prompt.entries.iter().any(|entry| {
            matches!(entry, PromptEntry::WorkspaceTree(node) if node.path == Path::new("node_modules/match-active.rs"))
        }));
        assert!(
            prompt
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("active buffer included outside snapshot"))
        );

        app.handle_event(key(KeyCode::Backspace));
        app.handle_event(Event::Paste("seed".to_owned()));
        assert!(
            !app.prompt()
                .unwrap()
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("active buffer included outside snapshot"))
        );
    }

    #[test]
    fn workspace_tree_filter_searches_snapshot_and_clearing_restores_tree_selection() {
        let (_directory, mut app) = workspace_tree_app();
        app.workspace
            .open(app.workspace.root.join("README.md"))
            .unwrap();
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("src")));

        app.handle_event(key(KeyCode::Char('m')));
        let prompt = app.prompt().unwrap();
        assert_eq!(prompt.input, "m");
        assert_eq!(prompt.selected, 0);
        assert!(
            prompt.entries.iter().all(
                |entry| matches!(entry, PromptEntry::WorkspaceTree(node) if !node.is_directory)
            )
        );

        app.handle_event(key(KeyCode::Backspace));
        let prompt = app.prompt().unwrap();
        assert!(prompt.input.is_empty());
        assert!(matches!(
            prompt.entries.get(prompt.selected),
            Some(PromptEntry::WorkspaceTree(node)) if node.path == Path::new("src")
        ));
    }

    #[test]
    fn workspace_tree_filtered_file_commit_opens_exact_path_and_records_jump() {
        let (_directory, mut app) = workspace_tree_app();
        app.workspace
            .open(app.workspace.root.join("README.md"))
            .unwrap();
        let origin_id = app.workspace.active().id();
        let expected_path = app.workspace.root.join("src/nested/main.rs");
        app.execute_action(Action::WorkspaceTree);
        app.handle_event(Event::Paste("nestedmain".to_owned()));
        assert!(
            app.prompt()
                .unwrap()
                .labels
                .iter()
                .any(|label| label.contains("src/nested/main.rs"))
        );

        app.handle_event(key(KeyCode::Enter));

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(
            app.workspace.active().document.path(),
            Some(expected_path.as_path())
        );
        assert_eq!(
            app.ui.jump_back.last().map(|jump| jump.editor_id),
            Some(origin_id)
        );
    }

    #[test]
    fn workspace_tree_query_refuses_over_limit_paste_and_character_without_rebuilding() {
        let (_directory, mut app) = workspace_tree_app();
        app.execute_action(Action::WorkspaceTree);
        if let UiMode::Prompt(prompt) = &mut app.ui.mode {
            prompt.notice = Some("navigation sentinel".to_owned());
        }
        app.handle_event(key(KeyCode::Up));
        assert_eq!(
            app.prompt().unwrap().notice.as_deref(),
            Some("navigation sentinel")
        );

        app.handle_event(Event::Paste("x".repeat(MAX_WORKSPACE_TREE_QUERY_BYTES + 1)));
        assert!(app.prompt().unwrap().input.is_empty());
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("Workspace tree input is limited")
        );
        assert!(
            app.overlay()
                .unwrap()
                .notice
                .is_some_and(|notice| notice.starts_with("Error: Workspace tree input is limited"))
        );

        app.handle_event(Event::Paste("x".repeat(MAX_WORKSPACE_TREE_QUERY_BYTES)));
        assert_eq!(
            app.prompt().unwrap().input.len(),
            MAX_WORKSPACE_TREE_QUERY_BYTES
        );
        if let UiMode::Prompt(prompt) = &mut app.ui.mode {
            prompt.notice = Some("query cursor sentinel".to_owned());
        }
        app.handle_event(key(KeyCode::Left));
        assert_eq!(
            app.prompt().unwrap().notice.as_deref(),
            Some("query cursor sentinel")
        );
        app.handle_event(key(KeyCode::Char('y')));
        assert_eq!(
            app.prompt().unwrap().input.len(),
            MAX_WORKSPACE_TREE_QUERY_BYTES
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_tree_labels_escape_controls_invisible_unicode_and_invalid_bytes_injectively() {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff]);
        let invalid_label = safe_tree_component(&invalid);
        let literal_label = safe_tree_component(OsStr::new("\\xFF"));
        assert_eq!(invalid_label, "\\xFF");
        assert_eq!(literal_label, "\\\\xFF");
        assert_ne!(invalid_label, literal_label);

        let unsafe_label = safe_tree_text("evil\u{1b}[2J\n\t\u{202e}\u{200b}.rs");
        assert!(!unsafe_label.chars().any(char::is_control));
        assert!(!unsafe_label.contains('\u{202e}'));
        assert!(!unsafe_label.contains('\u{200b}'));
        assert!(unsafe_label.contains("\\u{001B}"));
        assert!(unsafe_label.contains("\\u{202E}"));

        let default_ignorables = safe_tree_text("a\u{fff0}\u{1bca0}\u{1d173}\u{e0061}\u{e0fff}b");
        assert_eq!(
            default_ignorables,
            "a\\u{FFF0}\\u{1BCA0}\\u{1D173}\\u{E0061}\\u{E0FFF}b"
        );
    }

    #[test]
    fn workspace_tree_refresh_prunes_removed_expansions() {
        let (directory, mut app) = workspace_tree_app();
        app.workspace
            .open(app.workspace.root.join("README.md"))
            .unwrap();
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("src")));
        app.handle_event(key(KeyCode::Right));
        assert!(app.project.tree_expanded.contains(Path::new("src")));

        std::fs::remove_file(directory.path().join("src/lib.rs")).unwrap();
        std::fs::remove_file(directory.path().join("src/nested/main.rs")).unwrap();
        std::fs::remove_dir(directory.path().join("src/nested")).unwrap();
        std::fs::remove_dir(directory.path().join("src")).unwrap();
        std::fs::create_dir(directory.path().join("new")).unwrap();
        std::fs::write(directory.path().join("new/file.rs"), "new\n").unwrap();

        app.refresh_workspace_snapshots();
        poll_app_until(&mut app, "workspace refresh", |app| {
            !app.project.status.is_pending()
        });
        assert!(app.project.tree_expanded.is_empty());
        app.cancel_prompt();
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("new")));
        app.handle_event(key(KeyCode::Right));
        assert!(app.project.tree_expanded.contains(Path::new("new")));
    }

    #[test]
    fn workspace_tree_open_from_pristine_initial_untitled_does_not_record_dead_jump() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("value.rs"), "value\n").unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let untitled_id = app.workspace.active().id();

        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("value.rs")));
        app.handle_event(key(KeyCode::Enter));

        assert_ne!(app.workspace.active().id(), untitled_id);
        assert!(app.ui.jump_back.is_empty());
    }

    #[test]
    fn workspace_new_file_prompt_creates_opens_and_refreshes_indexed_file() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("seed.rs"), "seed\n").unwrap();
        let workspace = Workspace::from_path(
            Some(directory.path().join("seed.rs")),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::NewFile);

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("new-file prompt missing");
        };
        assert_eq!(prompt.kind, PromptFlow::NewFilePath);
        prompt.input = "src/new.rs".to_owned();
        app.commit_prompt();

        let created = directory.path().join("src/new.rs");
        assert!(created.exists());
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "");
        assert_eq!(
            app.workspace.active().document.path(),
            Some(std::fs::canonicalize(&created).unwrap().as_path())
        );
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Created "));
        assert!(status.contains("src/new.rs"));
        assert!(status.ends_with("workspace reindexing"));
        poll_app_until(&mut app, "new-file reindex", |app| {
            !app.project.status.is_pending()
        });
        assert!(
            app.project
                .index
                .as_ref()
                .unwrap()
                .files()
                .iter()
                .any(|path| path == Path::new("src/new.rs"))
        );
    }

    #[test]
    fn workspace_new_file_refuses_existing_and_outside_targets() {
        let directory = tempfile::tempdir().unwrap();
        let seed = directory.path().join("seed.rs");
        std::fs::write(&seed, "seed\n").unwrap();
        let workspace =
            Workspace::from_path(Some(seed.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::NewFile(Some("seed.rs".into())));
        let status = app.status_message().unwrap();
        assert!(status.starts_with("New File target already exists: "));
        assert!(status.ends_with("seed.rs"));
        assert!(app.ui.status.as_ref().unwrap().error);

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        app.execute_ex_command(ExCommand::NewFile(Some(outside_file.clone())));
        assert_eq!(
            app.status_message(),
            Some("New File path must stay inside the workspace")
        );
        assert!(!outside_file.exists());
    }

    #[test]
    fn workspace_rename_file_prompt_moves_clean_file_and_refreshes_index() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("src/old.rs");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "fn old() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(old.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::RenameFile);

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("rename-file prompt missing");
        };
        assert_eq!(prompt.kind, PromptFlow::RenameFilePath);
        assert_eq!(prompt.input, "src/old.rs");
        prompt.input = "moved/new.rs".to_owned();
        app.commit_prompt();

        let new = directory.path().join("moved/new.rs");
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "fn old() {}\n");
        assert_eq!(
            app.workspace.active().document.path(),
            Some(std::fs::canonicalize(&new).unwrap().as_path())
        );
        assert!(!app.workspace.active().document.is_modified());
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Renamed "));
        assert!(status.contains("src/old.rs"));
        assert!(status.contains("moved/new.rs"));
        assert!(status.ends_with("workspace reindexing"));
        poll_app_until(&mut app, "rename reindex", |app| {
            !app.project.status.is_pending()
        });
        assert!(
            app.project
                .index
                .as_ref()
                .unwrap()
                .files()
                .iter()
                .any(|path| path == Path::new("moved/new.rs"))
        );
        assert!(
            !app.project
                .index
                .as_ref()
                .unwrap()
                .files()
                .iter()
                .any(|path| path == Path::new("src/old.rs"))
        );

        app.workspace
            .active_mut()
            .document
            .edit(0..0, "// renamed\n", 0, 11, crate::EditKind::Insert)
            .unwrap();
        app.save_current();
        assert!(
            std::fs::read_to_string(&new)
                .unwrap()
                .starts_with("// renamed\n")
        );
        assert!(!old.exists());
    }

    #[test]
    fn workspace_rename_file_refuses_dirty_existing_outside_and_untitled_sources() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let existing = directory.path().join("existing.rs");
        std::fs::write(&active, "active\n").unwrap();
        std::fs::write(&existing, "existing\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.workspace
            .active_mut()
            .document
            .edit(0..0, "dirty\n", 0, 6, crate::EditKind::Insert)
            .unwrap();
        app.execute_action(Action::RenameFile);
        assert_eq!(
            app.status_message(),
            Some("Save or discard changes before Rename File")
        );
        assert!(active.exists());

        app.workspace.active_mut().undo();
        app.execute_ex_command(ExCommand::RenameFile(Some("existing.rs".into())));
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Rename File target already exists: "));
        assert!(status.ends_with("existing.rs"));
        assert_eq!(std::fs::read_to_string(&active).unwrap(), "active\n");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "existing\n");

        app.execute_ex_command(ExCommand::RenameFile(Some("../outside.rs".into())));
        assert_eq!(
            app.status_message(),
            Some("Rename File path must stay inside the workspace")
        );
        assert!(!directory.path().join("../outside.rs").exists());

        app.execute_ex_command(ExCommand::New);
        app.execute_action(Action::RenameFile);
        assert_eq!(
            app.status_message(),
            Some("Rename File needs a file-backed buffer")
        );
    }

    #[test]
    fn workspace_save_copy_as_writes_dirty_buffer_without_retargeting_it() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        std::fs::write(&active, "active\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let active_id = app.workspace.active().id();

        app.workspace
            .active_mut()
            .document
            .edit(0..0, "// dirty\n", 0, 9, crate::EditKind::Insert)
            .unwrap();
        app.execute_action(Action::SaveCopyAs);

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("save-copy prompt missing");
        };
        assert_eq!(prompt.kind, PromptFlow::SaveCopyAsPath);
        assert_eq!(prompt.input, "active.rs");
        prompt.input = "copies/active-copy.rs".to_owned();
        app.commit_prompt();

        let copy = directory.path().join("copies/active-copy.rs");
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            "// dirty\nactive\n"
        );
        assert_eq!(
            app.workspace
                .active()
                .document
                .path()
                .map(crate::workspace::normalized_file_path),
            Some(crate::workspace::normalized_file_path(&active))
        );
        assert_eq!(app.workspace.active().id(), active_id);
        assert!(app.workspace.active().document.is_modified());
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Saved copy as "));
        assert!(status.contains("copies/active-copy.rs"));
        assert!(status.ends_with("workspace reindexing"));
        poll_app_until(&mut app, "save-copy reindex", |app| {
            !app.project.status.is_pending()
        });
        assert!(
            app.project
                .index
                .as_ref()
                .unwrap()
                .files()
                .iter()
                .any(|path| path == Path::new("copies/active-copy.rs"))
        );
    }

    #[test]
    fn workspace_save_copy_as_supports_untitled_and_refuses_existing_outside_read_only() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("existing.rs"), "existing\n").unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.workspace
            .active_mut()
            .document
            .edit(0..0, "scratch\n", 0, 8, crate::EditKind::Insert)
            .unwrap();
        app.execute_ex_command(ExCommand::SaveCopyAs(Some("scratch.rs".into())));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("scratch.rs")).unwrap(),
            "scratch\n"
        );
        assert!(app.workspace.active().document.path().is_none());
        assert!(app.workspace.active().document.is_modified());

        app.execute_ex_command(ExCommand::SaveCopyAs(Some("existing.rs".into())));
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Save Copy As target already exists: "));
        assert!(status.ends_with("existing.rs"));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("existing.rs")).unwrap(),
            "existing\n"
        );

        app.execute_ex_command(ExCommand::SaveCopyAs(Some("../outside.rs".into())));
        assert_eq!(
            app.status_message(),
            Some("Save Copy As path must stay inside the workspace")
        );
        assert!(!directory.path().join("../outside.rs").exists());

        app.open_workspace_info();
        app.execute_action(Action::SaveCopyAs);
        assert_eq!(
            app.status_message(),
            Some("Save Copy As is unavailable in read-only IDE views")
        );
    }

    #[test]
    fn workspace_open_path_opens_existing_file_without_requiring_clean_current_buffer() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let target = directory.path().join("nested/target.rs");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&active, "active\n").unwrap();
        std::fs::write(&target, "target\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let dirty_id = app.workspace.active().id();
        app.workspace
            .active_mut()
            .document
            .edit(0..0, "// dirty\n", 0, 9, crate::EditKind::Insert)
            .unwrap();

        app.execute_action(Action::OpenPath);

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("open-path prompt missing");
        };
        assert_eq!(prompt.kind, PromptFlow::OpenPath);
        prompt.input = "nested/target.rs".to_owned();
        app.commit_prompt();

        assert_eq!(app.workspace.active().document.text(), "target\n");
        assert_eq!(
            app.workspace
                .active()
                .document
                .path()
                .map(crate::workspace::normalized_file_path),
            Some(crate::workspace::normalized_file_path(&target))
        );
        assert!(
            app.workspace
                .editor_by_id(dirty_id)
                .unwrap()
                .document
                .is_modified()
        );
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Opened "));
        assert!(status.ends_with("nested/target.rs"));
    }

    #[test]
    fn workspace_open_path_opens_new_buffer_without_creating_disk_file_and_refuses_unsafe_targets()
    {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        std::fs::write(&active, "active\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::OpenPath(Some("draft.rs".into())));
        let draft = directory.path().join("draft.rs");
        assert!(!draft.exists());
        assert_eq!(
            app.workspace
                .active()
                .document
                .path()
                .map(crate::workspace::normalized_file_path),
            Some(crate::workspace::normalized_file_path(&draft))
        );
        assert!(!app.workspace.active().document.is_modified());
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Opened new path "));
        assert!(status.ends_with("draft.rs"));

        app.execute_ex_command(ExCommand::OpenPath(Some("../outside.rs".into())));
        assert_eq!(
            app.status_message(),
            Some("Open Path path must stay inside the workspace")
        );

        app.execute_ex_command(ExCommand::OpenPath(Some("missing/child.rs".into())));
        assert!(
            app.status_message()
                .unwrap()
                .starts_with("Open Path parent directory is unavailable")
        );

        std::fs::create_dir(directory.path().join("dir-target")).unwrap();
        app.execute_ex_command(ExCommand::OpenPath(Some("dir-target".into())));
        assert_eq!(
            app.status_message(),
            Some("Open Path target is not a regular file")
        );
    }

    #[test]
    fn workspace_tree_expansion_cap_and_empty_selection_errors_are_visible_notices() {
        let (_directory, mut app) = workspace_tree_app();
        app.execute_action(Action::WorkspaceTree);
        assert!(app.select_workspace_tree_path(Path::new("empty")));
        app.project.tree_expanded = (0..MAX_WORKSPACE_TREE_EXPANDED_DIRECTORIES)
            .map(|index| PathBuf::from(format!("retained-{index}")))
            .collect();

        app.handle_event(key(KeyCode::Right));
        assert!(
            app.overlay()
                .unwrap()
                .notice
                .is_some_and(|notice| notice.starts_with("Error: Workspace tree already retains"))
        );

        if let UiMode::Prompt(prompt) = &mut app.ui.mode {
            prompt.labels.clear();
            prompt.entries.clear();
        }
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(
            app.overlay().unwrap().notice,
            Some("Error: No workspace tree selection")
        );
    }

    #[test]
    fn save_as_recomputes_outside_identity_and_surfaces_refresh_failure() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("seed.rs"), "seed\n").unwrap();
        let workspace =
            Workspace::from_path(Some(root.join("seed.rs")), Some(root.clone())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let moved_root = parent.path().join("workspace-held");
        std::fs::rename(&root, &moved_root).unwrap();
        let outside = parent.path().join("outside.rs");

        app.execute_ex_command(ExCommand::Save(Some(outside.clone())));
        poll_app_until(&mut app, "failed Save As reindex", |app| {
            !app.project.status.is_pending()
        });

        assert_eq!(
            app.workspace.active().document.path(),
            Some(outside.as_path())
        );
        assert_eq!(
            app.project
                .tree_document_paths
                .get(&app.workspace.active().id()),
            Some(&None)
        );
        assert_eq!(app.workspace_tree_active_path(), None);
        assert!(app.status_message().is_some_and(|message| {
            message.contains("Workspace refresh failed; retained the previous snapshots")
        }));
        assert!(
            app.project
                .index
                .as_ref()
                .unwrap()
                .contains_tree_path("seed.rs")
        );
    }

    #[test]
    fn explicit_workspace_refresh_rebuilds_explorer_quick_open_and_search_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("seed.rs"), "seed\n").unwrap();
        let workspace = Workspace::from_path(
            Some(directory.path().join("seed.rs")),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        std::fs::write(directory.path().join("late-created.rs"), "late\n").unwrap();
        assert!(
            !app.project
                .index
                .as_ref()
                .unwrap()
                .contains_tree_path("late-created.rs")
        );

        app.execute_action(Action::WorkspaceRefresh);
        poll_app_until(&mut app, "explicit workspace refresh", |app| {
            !app.project.status.is_pending()
        });

        assert!(app.status_message().is_some_and(|message| {
            message.starts_with("Workspace snapshot refreshed:")
                && message.contains("text files")
                && message.contains("tree entries")
        }));
        assert!(
            app.project
                .index
                .as_ref()
                .unwrap()
                .contains_tree_path("late-created.rs")
        );
        app.execute_action(Action::QuickOpen);
        app.handle_event(Event::Paste("latecreated".to_owned()));
        assert!(
            app.prompt()
                .unwrap()
                .labels
                .iter()
                .any(|label| label == "late-created.rs")
        );
        assert!(app.project.search_worker.is_some());
    }

    #[test]
    fn failed_workspace_refresh_retains_previous_snapshots() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("seed.rs"), "seed\n").unwrap();
        let workspace =
            Workspace::from_path(Some(root.join("seed.rs")), Some(root.clone())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        std::fs::rename(&root, parent.path().join("workspace-held")).unwrap();

        app.execute_action(Action::WorkspaceRefresh);
        poll_app_until(&mut app, "failed workspace refresh", |app| {
            !app.project.status.is_pending()
        });

        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .is_some_and(|message| message.contains("retained the previous snapshots"))
        );
        assert_eq!(
            app.project.index.as_ref().unwrap().files(),
            &[PathBuf::from("seed.rs")]
        );
        assert!(app.project.search_worker.is_some());
    }

    #[cfg(unix)]
    fn poll_app_until(app: &mut App, description: &str, predicate: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !predicate(app) && std::time::Instant::now() < deadline {
            app.poll_services();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            predicate(app),
            "timed out waiting for {description}; active view was:\n{}",
            app.workspace.active().document.text()
        );
    }

    #[cfg(unix)]
    fn wire_log_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(unix)]
    fn wire_line_count(lines: &[String], expected: &str) -> usize {
        lines
            .iter()
            .filter(|line| line.as_str() == expected)
            .count()
    }

    fn zero_width_lsp_range(line: usize, character: usize) -> LspRange {
        let position = LspPosition::new(Line::new(line), Utf16Offset::new(character));
        LspRange::new(position, position)
    }

    fn track_lsp_edit_context(
        app: &mut App,
        uri: impl Into<String>,
        version: u64,
    ) -> LspDocumentRequestContext {
        let uri = uri.into();
        let editor = app.workspace.active();
        let incarnation = DocumentIncarnation::for_test(version.max(1));
        let context = LspDocumentRequestContext {
            editor_id: editor.id(),
            uri: uri.clone(),
            version: DocumentVersion::new(version),
            incarnation,
            state_id: editor.document.state_id(),
        };
        app.lsp
            .documents
            .insert(SynchronizedDocument::new(
                context.editor_id,
                uri,
                context.version,
                context.state_id,
                editor.document.saved_state_id(),
                editor.document.save_generation(),
            ))
            .unwrap();
        app.lsp
            .document_incarnations
            .insert(context.editor_id, incarnation);
        app.lsp.document_ends.insert(
            context.editor_id,
            lsp_document_end(&app.workspace.active().document),
        );
        context
    }

    fn json_object<const N: usize>(
        fields: [(&str, crate::lsp_client::JsonValue); N],
    ) -> crate::lsp_client::JsonValue {
        crate::lsp_client::JsonValue::Object(
            fields
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
        )
    }

    fn zero_width_text_edit_json(new_text: &str) -> crate::lsp_client::JsonValue {
        let position = || json_object([("line", 0_u64.into()), ("character", 0_u64.into())]);
        json_object([
            (
                "range",
                json_object([("start", position()), ("end", position())]),
            ),
            ("newText", new_text.into()),
        ])
    }

    fn workspace_symbol_result(
        name: &str,
        container: &str,
        uri: &str,
        line: usize,
        character: usize,
    ) -> crate::lsp_client::JsonValue {
        let position = || {
            json_object([
                ("line", (line as u64).into()),
                ("character", (character as u64).into()),
            ])
        };
        crate::lsp_client::JsonValue::Array(vec![json_object([
            ("name", name.into()),
            ("kind", 12_u64.into()),
            ("containerName", container.into()),
            (
                "location",
                json_object([
                    ("uri", uri.into()),
                    (
                        "range",
                        json_object([("start", position()), ("end", position())]),
                    ),
                ]),
            ),
        ])])
    }

    #[test]
    fn escape_action_never_inserts_command_characters() {
        let mut app = app_with_text("");
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(key(KeyCode::Char('s')));
        assert_eq!(app.workspace.active().document.text(), "");
        assert!(!app.ui.keymap.is_active());
    }

    #[test]
    fn bracketed_paste_is_one_literal_edit() {
        let mut app = app_with_text("");
        app.handle_event(Event::Paste("hello\x1bq\nworld".to_owned()));
        assert_eq!(app.workspace.active().document.text(), "hello\x1bq\nworld");
        app.workspace.active_mut().undo();
        assert_eq!(app.workspace.active().document.text(), "");
    }

    #[test]
    fn dirty_quit_is_guarded() {
        let mut app = app_with_text("");
        app.handle_event(key(KeyCode::Char('x')));
        app.execute_action(Action::Quit);
        assert!(!app.should_quit());
        assert!(matches!(app.ui.mode, UiMode::Confirm(ConfirmKind::Quit)));
    }

    #[test]
    fn legacy_escape_prefixed_key_resolves_as_an_action() {
        let mut app = app_with_text("");
        app.handle_event(key(KeyCode::Char('x')));
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::ALT,
        )));
        assert_eq!(app.workspace.active().document.text(), "");
        assert!(!app.ui.keymap.is_active());
    }

    #[test]
    fn prompt_editing_respects_graphemes() {
        let mut prompt = Prompt::new(PromptFlow::Search, "a👩‍💻b".to_owned(), 0, None);
        prompt.backspace();
        assert_eq!(prompt.input, "a👩‍💻");
        prompt.backspace();
        assert_eq!(prompt.input, "a");
    }

    #[test]
    fn in_buffer_search_highlight_never_leaks_to_another_buffer() {
        let mut app = app_with_text("needle");
        app.begin_prompt(PromptFlow::Search);
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("search prompt missing");
        };
        prompt.input = "needle".to_owned();
        app.prompt_changed();
        assert_eq!(app.search_match(), Some(0..6));

        app.ui.mode = UiMode::Edit;
        app.workspace.new_buffer();
        assert_eq!(app.search_match(), None);
    }

    #[test]
    fn interactive_literal_replace_all_is_one_undoable_action() {
        let mut app = app_with_text("café bird café");
        app.execute_action(Action::Replace);
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("replace-find prompt missing");
        };
        prompt.input = "café".to_owned();
        app.commit_prompt();
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("replace-with prompt missing");
        };
        prompt.input = "🪶".to_owned();
        app.commit_prompt();

        assert_eq!(app.workspace.active().document.text(), "🪶 bird 🪶");
        assert_eq!(
            app.status_message(),
            Some("Replaced 2 literal matches; Undo restores all")
        );
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), "café bird café");
    }

    #[test]
    fn soft_wrap_toggle_makes_arrow_keys_follow_terminal_rows() {
        let mut app = app_with_text(&"x".repeat(80));
        app.set_screen_size((40, 12));
        app.workspace.active_mut().viewport.left_column = 9;
        app.execute_action(Action::ToggleSoftWrap);
        assert!(app.soft_wrap_enabled());
        assert_eq!(app.status_message(), Some("Soft wrap on"));

        let layout = Layout::calculate(40, 12, 1, app.config.line_numbers);
        app.handle_event(key(KeyCode::Down));
        assert_eq!(app.workspace.active().cursor, layout.content_width);
        assert_eq!(app.workspace.active().viewport.left_column, 9);

        app.execute_action(Action::ToggleSoftWrap);
        assert!(!app.soft_wrap_enabled());
        assert_eq!(app.status_message(), Some("Soft wrap off"));
        assert_eq!(app.workspace.active().viewport.left_column, 9);
    }

    #[test]
    fn line_number_toggle_reclaims_editor_width_without_restart() {
        let mut app = app_with_text("one\ntwo\nthree\n");
        app.set_screen_size((40, 12));
        assert!(app.config.line_numbers);
        let numbered = Layout::calculate(
            app.ui.screen_size.0,
            app.ui.screen_size.1,
            app.workspace.active().document.line_count(),
            app.config.line_numbers,
        );

        app.execute_action(Action::ToggleLineNumbers);

        assert!(!app.config.line_numbers);
        assert_eq!(app.status_message(), Some("Line numbers off"));
        assert!(app.take_full_redraw());
        let unnumbered = Layout::calculate(
            app.ui.screen_size.0,
            app.ui.screen_size.1,
            app.workspace.active().document.line_count(),
            app.config.line_numbers,
        );
        assert_eq!(unnumbered.gutter_width, 0);
        assert!(unnumbered.content_width > numbered.content_width);

        app.execute_ex_command(ExCommand::SetLineNumbers(true));

        assert!(app.config.line_numbers);
        assert_eq!(app.status_message(), Some("Line numbers on"));
        assert!(app.take_full_redraw());
    }

    #[test]
    fn wrapped_mouse_click_and_wheel_share_the_visual_row_map() {
        let mut app = app_with_text(&"x".repeat(80));
        app.set_screen_size((40, 12));
        app.execute_action(Action::ToggleSoftWrap);
        let layout = Layout::calculate(40, 12, 1, app.config.line_numbers);

        app.place_mouse_cursor(
            (layout.gutter_width + 2) as u16,
            (layout.content_y + 1) as u16,
            layout,
            false,
        );
        assert_eq!(
            app.workspace.active().cursor,
            layout.content_width.saturating_add(2)
        );

        app.workspace.active_mut().set_cursor(9, false);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: layout.content_y as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.workspace.active().viewport.top_wrap_char, 74);
        app.prepare_viewport(layout);
        assert_eq!(app.workspace.active().viewport.top_wrap_char, 74);
    }

    #[test]
    fn sidebar_mouse_click_lands_on_the_clicked_character() {
        let mut app = app_with_text("abcdefghij\nklmnopqrst\n");
        app.set_screen_size((100, 24));
        app.execute_action(Action::WorkspaceSidebar);
        assert!(app.workspace_sidebar_visible());

        let line_count = app.workspace.active().document.line_count();
        let full_layout = Layout::calculate(100, 24, line_count, app.config.line_numbers);
        let sidebar_width = project_sidebar_width(&app, full_layout);
        assert!(sidebar_width > 0);
        let editor_layout = Layout::calculate(
            (full_layout.width - sidebar_width) as u16,
            24,
            line_count,
            app.config.line_numbers,
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (sidebar_width + editor_layout.gutter_width + 5) as u16,
            row: editor_layout.content_y as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.workspace.active().cursor, 5);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: editor_layout.content_y as u16,
            modifiers: KeyModifiers::NONE,
        });

        // A click inside the sidebar must not move the cursor or start a drag.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (sidebar_width / 2) as u16,
            row: editor_layout.content_y as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.workspace.active().cursor, 5);
        assert!(!app.ui.mouse_selecting);
    }

    #[test]
    fn transient_prompts_block_mouse_edits_behind_their_ui() {
        let mut app = app_with_text("first\nsecond\nthird\n");
        app.set_screen_size((80, 24));
        app.workspace.active_mut().set_cursor(3, false);
        app.execute_action(Action::Find);

        let layout = Layout::calculate(80, 24, 4, app.config.line_numbers);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (layout.gutter_width + 8) as u16,
            row: (layout.content_y + 2) as u16,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: layout.content_y as u16,
            modifiers: KeyModifiers::NONE,
        });

        assert!(matches!(app.ui.mode, UiMode::Prompt(_)));
        assert_eq!(app.workspace.active().cursor, 3);
        assert_eq!(app.workspace.active().viewport.top_line, 0);
    }

    #[test]
    fn candidate_overlay_mouse_uses_the_rendered_hit_map() {
        let mut app = app_with_text("first");
        app.set_screen_size((80, 24));
        let first_id = app.workspace.active().id();
        app.workspace.new_buffer();
        let second_id = app.workspace.active().id();
        app.workspace.activate(0);
        app.show_fixed_prompt(
            PromptFlow::Buffers,
            [
                ("first".to_owned(), PromptEntry::Buffer(0)),
                ("second".to_owned(), PromptEntry::Buffer(1)),
            ],
        );

        let layout = Layout::calculate(80, 24, 1, app.config.line_numbers);
        let overlay = CandidateOverlayLayout::calculate(layout, 2, 0, false).unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: overlay.x as u16,
            row: overlay.y as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(app.ui.mode, UiMode::Prompt(_)));
        assert_eq!(app.workspace.active().id(), first_id);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: overlay.x as u16,
            row: (overlay.y + 1) as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt) if prompt.selected == 1
        ));
        assert_eq!(app.workspace.active().id(), first_id);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: overlay.x as u16,
            row: (overlay.y + 2) as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(app.workspace.active().id(), second_id);
    }

    #[test]
    fn jump_history_returns_across_files_and_moves_forward_again() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.rs");
        let second = dir.path().join("second.rs");
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();
        let workspace =
            Workspace::from_path(Some(first.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().set_cursor(3, false);

        app.open_lsp_location(Location {
            uri: file_uri_identity(&second),
            range: zero_width_lsp_range(0, 2),
        });
        let canonical_second = std::fs::canonicalize(&second).unwrap();
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_second.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 2);

        app.execute_action(Action::JumpBack);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(first.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 3);
        app.execute_action(Action::JumpForward);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_second.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 2);
    }

    #[test]
    fn lsp_event_poll_budget_defers_oldest_and_makes_ordered_progress() {
        let large_payload_bytes = MAX_LSP_EVENT_BYTES_PER_POLL / 3;
        let oversize_payload_bytes = MAX_LSP_EVENT_BYTES_PER_POLL / 2 + 4 * 1024;
        let mut source = VecDeque::from([
            ("large-a", large_payload_bytes),
            ("large-b", large_payload_bytes),
            ("oversize", oversize_payload_bytes),
            ("tail", 64),
        ]);
        let mut deferred = None;
        let mut deferred_labels = Vec::new();
        let mut polls = Vec::new();

        while deferred.is_some() || !source.is_empty() {
            let mut budget = LspEventPollBudget::new();
            let mut batch = Vec::new();
            while budget.can_receive() {
                let event = if let Some(event) = deferred.take() {
                    event
                } else if let Some((label, payload_bytes)) = source.pop_front() {
                    LspEvent::ServerNotification {
                        method: label.to_owned(),
                        params: Some(crate::lsp_client::JsonValue::String(
                            "x".repeat(payload_bytes),
                        )),
                    }
                } else {
                    break;
                };
                let event_bytes = event.estimated_retained_bytes();
                if !budget.reserve(&event) {
                    let LspEvent::ServerNotification { method, .. } = &event else {
                        unreachable!("the synthetic stream contains notifications only");
                    };
                    deferred_labels.push(method.clone());
                    deferred = Some(event);
                    break;
                }
                let LspEvent::ServerNotification { method, .. } = event else {
                    unreachable!("the synthetic stream contains notifications only");
                };
                batch.push((method, event_bytes));
            }

            assert!(
                !batch.is_empty(),
                "a fresh budget must admit its first event, including an oversize event"
            );
            assert_eq!(budget.handled_events(), batch.len());
            assert_eq!(
                budget.retained_bytes(),
                batch.iter().map(|(_, bytes)| bytes).sum::<usize>()
            );
            polls.push(batch);
        }

        assert_eq!(
            deferred_labels,
            ["large-b", "oversize", "tail"],
            "the oldest over-budget event is carried into the next fresh poll"
        );
        assert_eq!(polls.len(), 4);
        assert_eq!(
            polls
                .iter()
                .flatten()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            ["large-a", "large-b", "oversize", "tail"]
        );
        let first_large_bytes = polls[0][0].1;
        let second_large_bytes = polls[1][0].1;
        assert!(first_large_bytes <= MAX_LSP_EVENT_BYTES_PER_POLL);
        assert!(second_large_bytes <= MAX_LSP_EVENT_BYTES_PER_POLL);
        assert!(
            first_large_bytes.saturating_add(second_large_bytes) > MAX_LSP_EVENT_BYTES_PER_POLL
        );
        assert!(polls[2][0].1 > MAX_LSP_EVENT_BYTES_PER_POLL);

        let mut oversize_polls = 0;
        for batch in &polls {
            let retained_bytes: usize = batch.iter().map(|(_, bytes)| bytes).sum();
            if retained_bytes > MAX_LSP_EVENT_BYTES_PER_POLL {
                oversize_polls += 1;
                assert_eq!(batch.len(), 1);
                assert!(batch[0].1 > MAX_LSP_EVENT_BYTES_PER_POLL);
            }
        }
        assert_eq!(oversize_polls, 1);
    }

    #[test]
    fn workspace_symbol_response_opens_a_filterable_picker_and_jump_target() {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin.rs");
        let target = directory.path().join("target.rs");
        std::fs::write(&origin, "origin\n").unwrap();
        std::fs::write(&target, "target\n").unwrap();
        let workspace =
            Workspace::from_path(Some(origin.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().set_cursor(3, false);
        app.lsp.server_name = Some("mock".to_owned());
        app.lsp.active_workspace_symbol_token = Some(7);
        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolPending,
            "bird".to_owned(),
            3,
            None,
        ));
        app.lsp.requests.insert(
            41,
            PendingLspRequest::WorkspaceSymbols {
                prompt_token: 7,
                query: "bird".to_owned(),
                server_name: "mock".to_owned(),
            },
        );

        assert!(app.handle_lsp_event(LspEvent::WorkspaceSymbols {
            request_id: 41,
            result: workspace_symbol_result(
                "bird_call",
                "crate",
                &file_uri_identity(&target),
                0,
                2
            ),
        }));
        let UiMode::Prompt(prompt) = &app.ui.mode else {
            panic!("workspace-symbol picker missing");
        };
        assert_eq!(prompt.kind, PromptFlow::WorkspaceSymbols);
        assert_eq!(prompt.labels.len(), 1);
        assert!(prompt.labels[0].contains("bird_call — Function · crate — target.rs:1:3"));
        assert_eq!(app.lsp.active_workspace_symbol_token, None);

        app.commit_prompt();
        let canonical_target = std::fs::canonicalize(&target).unwrap();
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_target.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 2);
        app.execute_action(Action::JumpBack);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(origin.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 3);
    }

    #[test]
    fn document_symbols_fall_back_to_local_outline_without_ready_lsp() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(
            &path,
            "pub struct Bird;\n\nimpl Bird {\n    pub fn fly(&self) {}\n}\n",
        )
        .unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::DocumentSymbols);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "DOCUMENT SYMBOLS");
        assert_eq!(overlay.items.len(), 3);
        assert!(overlay.items[0].contains("struct Bird"));
        assert!(overlay.items[1].contains("impl Bird"));
        assert!(overlay.items[2].contains("fn fly"));
        assert!(overlay.notice.unwrap().contains("Local outline fallback"));

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("document-symbol picker missing");
        };
        prompt.input = "fly".to_owned();
        app.prompt_changed();
        app.commit_prompt();

        assert_eq!(app.workspace.active().document.path(), Some(path.as_path()));
        assert_eq!(
            app.workspace.active().position(app.config.tab_width).line,
            3
        );
        assert_eq!(app.status_message(), Some("Jumped to document symbol"));
    }

    #[test]
    fn definition_falls_back_to_local_workspace_outline_without_ready_lsp() {
        let directory = tempfile::tempdir().unwrap();
        let src_dir = directory.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let origin = src_dir.join("main.rs");
        let target = src_dir.join("lib.rs");
        let origin_text = "fn main() {\n    call_search_target();\n}\n";
        std::fs::write(&origin, origin_text).unwrap();
        std::fs::write(&target, "pub fn call_search_target() -> i32 { 42 }\n").unwrap();
        let workspace =
            Workspace::from_path(Some(origin.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let cursor = origin_text
            .chars()
            .position(|character| character == 's')
            .unwrap();
        app.workspace.active_mut().set_cursor(cursor, false);

        app.execute_action(Action::Definition);

        let canonical_target = std::fs::canonicalize(&target).unwrap();
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_target.as_path())
        );
        assert_eq!(
            app.workspace.active().position(app.config.tab_width).line,
            0
        );
        assert_eq!(app.status_message(), Some("Jumped to local definition"));
        app.execute_action(Action::JumpBack);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(origin.as_path())
        );
        assert_eq!(app.workspace.active().cursor, cursor);
    }

    #[test]
    fn definition_fallback_reports_missing_identifier_without_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().set_cursor(9, false);

        app.execute_action(Action::Definition);

        assert!(app.overlay().is_none());
        assert_eq!(
            app.status_message(),
            Some("No identifier under cursor for local definition")
        );
    }

    #[test]
    fn references_fall_back_to_local_exact_identifier_picker_without_ready_lsp() {
        let directory = tempfile::tempdir().unwrap();
        let src_dir = directory.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let origin = src_dir.join("main.rs");
        let target = src_dir.join("lib.rs");
        let origin_text =
            "fn main() {\n    call_search_target();\n    call_search_target_extra();\n}\n";
        std::fs::write(&origin, origin_text).unwrap();
        std::fs::write(
            &target,
            "pub fn call_search_target() -> i32 { 42 }\nfn helper() { call_search_target(); }\n",
        )
        .unwrap();
        let workspace =
            Workspace::from_path(Some(origin.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let cursor = origin_text
            .chars()
            .position(|character| character == 's')
            .unwrap();
        app.workspace.active_mut().set_cursor(cursor, false);

        app.execute_action(Action::References);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "LOCAL REFERENCES");
        assert_eq!(overlay.items.len(), 3);
        assert!(overlay.notice.unwrap().contains("Local references"));
        assert!(
            overlay
                .items
                .iter()
                .all(|item| !item.contains("call_search_target_extra"))
        );

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("local references picker missing");
        };
        prompt.input = "lib.rs".to_owned();
        app.prompt_changed();
        app.commit_prompt();

        let canonical_target = std::fs::canonicalize(&target).unwrap();
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_target.as_path())
        );
        assert_eq!(app.status_message(), Some("Jumped to local reference"));
        app.execute_action(Action::JumpBack);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(origin.as_path())
        );
        assert_eq!(app.workspace.active().cursor, cursor);
    }

    #[test]
    fn references_fallback_reports_missing_identifier_without_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().set_cursor(9, false);

        app.execute_action(Action::References);

        assert!(app.overlay().is_none());
        assert_eq!(
            app.status_message(),
            Some("No identifier under cursor for local references")
        );
    }

    #[test]
    fn local_symbol_occurrence_navigation_moves_wraps_and_records_jump_history() {
        let text = "fn main() {\n    perch();\n    perch_extra();\n    perch();\n}\n";
        let mut app = app_with_text(text);
        let first = text.chars().position(|character| character == 'p').unwrap();
        let second = text.rmatch_indices("perch()").next().unwrap().0;
        app.workspace.active_mut().set_cursor(first, false);

        app.execute_action(Action::NextSymbolOccurrence);

        assert_eq!(app.workspace.active().cursor, second);
        assert_eq!(
            app.status_message(),
            Some("Next local occurrence of perch: 4:5")
        );
        app.execute_action(Action::NextSymbolOccurrence);
        assert_eq!(app.workspace.active().cursor, first);
        assert_eq!(
            app.status_message(),
            Some("Next local occurrence of perch: 2:5")
        );
        app.execute_action(Action::PreviousSymbolOccurrence);
        assert_eq!(app.workspace.active().cursor, second);
        assert_eq!(
            app.status_message(),
            Some("Previous local occurrence of perch: 4:5")
        );

        app.execute_action(Action::JumpBack);
        assert_eq!(app.workspace.active().cursor, first);
    }

    #[test]
    fn local_symbol_occurrence_navigation_reports_single_or_missing_identifier() {
        let mut app = app_with_text("fn main() {\n    only_once();\n}\n");
        let cursor = app
            .workspace
            .active()
            .document
            .text()
            .find("only_once")
            .unwrap();
        app.workspace.active_mut().set_cursor(cursor, false);

        app.execute_action(Action::NextSymbolOccurrence);

        assert_eq!(app.workspace.active().cursor, cursor);
        assert_eq!(
            app.status_message(),
            Some("Only one local occurrence for only_once")
        );

        app.workspace.active_mut().set_cursor(0, false);
        app.execute_action(Action::PreviousSymbolOccurrence);
        assert_eq!(
            app.status_message(),
            Some("Only one local occurrence for fn")
        );

        app.workspace.active_mut().set_cursor(9, false);
        app.execute_action(Action::NextSymbolOccurrence);
        assert_eq!(
            app.status_message(),
            Some("No identifier under cursor for local occurrence navigation")
        );
    }

    #[test]
    fn toggle_line_comment_uses_source_marker_and_is_undoable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        let text = "fn main() {\n    perch();\n    fly();\n}\n";
        std::fs::write(&path, text).unwrap();
        let workspace =
            Workspace::from_path(Some(path), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let anchor = text.find("perch").unwrap();
        let cursor = text.find("}\n").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);

        app.execute_action(Action::ToggleLineComment);

        assert_eq!(
            app.workspace.active().document.text(),
            "fn main() {\n    // perch();\n    // fly();\n}\n"
        );
        assert_eq!(app.status_message(), Some("Commented 2 lines"));
        app.execute_action(Action::ToggleLineComment);
        assert_eq!(app.workspace.active().document.text(), text);
        assert_eq!(app.status_message(), Some("Uncommented 2 lines"));

        app.execute_action(Action::Undo);
        assert_eq!(
            app.workspace.active().document.text(),
            "fn main() {\n    // perch();\n    // fly();\n}\n"
        );
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), text);
    }

    #[test]
    fn toggle_line_comment_reports_unsupported_and_blank_buffers() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("notes.md");
        std::fs::write(&markdown, "# Heading\n").unwrap();
        let workspace =
            Workspace::from_path(Some(markdown), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::ToggleLineComment);

        assert_eq!(
            app.status_message(),
            Some("No line-comment marker for this buffer")
        );

        let shell = directory.path().join("run.sh");
        std::fs::write(&shell, "\n   \n").unwrap();
        let index = app.workspace.open(&shell).unwrap();
        app.workspace.activate(index);

        app.execute_action(Action::ToggleLineComment);

        assert_eq!(app.status_message(), Some("No nonblank lines to comment"));
        assert_eq!(app.workspace.active().document.text(), "\n   \n");
    }

    #[test]
    fn duplicate_line_action_duplicates_current_line_selection_and_refuses_read_only_views() {
        let text = "alpha\nbeta\ngamma\n";
        let mut app = app_with_text(text);
        app.workspace
            .active_mut()
            .set_cursor(text.find("beta").unwrap() + 1, false);

        app.execute_action(Action::DuplicateLine);

        assert_eq!(
            app.workspace.active().document.text(),
            "alpha\nbeta\nbeta\ngamma\n"
        );
        assert_eq!(app.status_message(), Some("Duplicated 1 line"));
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), text);

        let anchor = text.find("alpha").unwrap();
        let cursor = text.find("gamma").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);
        app.execute_action(Action::DuplicateLine);
        assert_eq!(
            app.workspace.active().document.text(),
            "alpha\nbeta\nalpha\nbeta\ngamma\n"
        );
        assert_eq!(app.status_message(), Some("Duplicated 2 lines"));

        app.workspace.open_virtual("Read Only", "preview\n");
        app.execute_action(Action::DuplicateLine);
        assert_eq!(
            app.status_message(),
            Some("Read Only is a read-only IDE view")
        );
    }

    #[test]
    fn delete_line_action_deletes_current_line_selection_and_refuses_read_only_views() {
        let text = "alpha\nbeta\ngamma\n";
        let mut app = app_with_text(text);
        app.workspace
            .active_mut()
            .set_cursor(text.find("beta").unwrap() + 1, false);

        app.execute_action(Action::DeleteLine);

        assert_eq!(app.workspace.active().document.text(), "alpha\ngamma\n");
        assert_eq!(app.status_message(), Some("Deleted 1 line"));
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), text);

        let anchor = text.find("alpha").unwrap();
        let cursor = text.find("gamma").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);
        app.execute_action(Action::DeleteLine);
        assert_eq!(app.workspace.active().document.text(), "gamma\n");
        assert_eq!(app.status_message(), Some("Deleted 2 lines"));

        app.workspace.open_virtual("Read Only", "preview\n");
        app.execute_action(Action::DeleteLine);
        assert_eq!(
            app.status_message(),
            Some("Read Only is a read-only IDE view")
        );
    }

    #[test]
    fn move_line_actions_reorder_current_line_selection_and_refuse_read_only_views() {
        let text = "alpha\nbeta\ngamma\ndelta\n";
        let mut app = app_with_text(text);
        app.workspace
            .active_mut()
            .set_cursor(text.find("gamma").unwrap() + 1, false);

        app.execute_action(Action::MoveLinesUp);

        assert_eq!(
            app.workspace.active().document.text(),
            "alpha\ngamma\nbeta\ndelta\n"
        );
        assert_eq!(app.status_message(), Some("Moved 1 line up"));
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), text);

        let anchor = text.find("beta").unwrap();
        let cursor = text.find("delta").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);
        app.execute_action(Action::MoveLinesDown);
        assert_eq!(
            app.workspace.active().document.text(),
            "alpha\ndelta\nbeta\ngamma\n"
        );
        assert_eq!(app.status_message(), Some("Moved 2 lines down"));

        let end = app.workspace.active().document.len_chars();
        app.workspace.active_mut().set_cursor(end, false);
        app.execute_action(Action::MoveLinesDown);
        assert_eq!(app.status_message(), Some("Already at bottom"));

        app.workspace.open_virtual("Read Only", "preview\n");
        app.execute_action(Action::MoveLinesUp);
        assert_eq!(
            app.status_message(),
            Some("Read Only is a read-only IDE view")
        );
    }

    #[test]
    fn indent_actions_indent_outdent_selection_and_refuse_read_only_views() {
        let text = "alpha\n  beta\n\tgamma\ndelta\n";
        let mut app = app_with_text(text);
        app.workspace
            .active_mut()
            .set_cursor(text.find("alpha").unwrap() + 1, false);

        app.execute_action(Action::IndentLines);

        assert_eq!(
            app.workspace.active().document.text(),
            "    alpha\n  beta\n\tgamma\ndelta\n"
        );
        assert_eq!(app.status_message(), Some("Indented 1 line"));
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), text);

        let anchor = text.find("alpha").unwrap();
        let cursor = text.find("delta").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);
        app.execute_action(Action::OutdentLines);
        assert_eq!(
            app.workspace.active().document.text(),
            "alpha\nbeta\ngamma\ndelta\n"
        );
        assert_eq!(app.status_message(), Some("Outdented 2 lines"));

        app.execute_action(Action::OutdentLines);
        assert_eq!(app.status_message(), Some("No indentation to remove"));

        app.workspace.open_virtual("Read Only", "preview\n");
        app.execute_action(Action::IndentLines);
        assert_eq!(
            app.status_message(),
            Some("Read Only is a read-only IDE view")
        );
    }

    #[test]
    fn matching_bracket_action_jumps_records_history_and_reports_no_match() {
        let text = "fn main() {\n    call([1, {two: 2}]);\n}\n";
        let mut app = app_with_text(text);
        let open = text.find("call(").unwrap() + "call".len();
        app.workspace.active_mut().set_cursor(open, false);

        app.execute_action(Action::MatchingBracket);

        let target = text.find("]);").unwrap() + 1;
        assert_eq!(app.workspace.active().cursor, target);
        assert_eq!(
            app.status_message(),
            Some("Jumped to matching bracket: 2:23")
        );

        app.execute_action(Action::JumpBack);
        assert_eq!(app.workspace.active().cursor, open);

        app.workspace
            .active_mut()
            .set_cursor(text.find("main").unwrap(), false);
        app.execute_action(Action::MatchingBracket);
        assert_eq!(app.status_message(), Some("No matching bracket at cursor"));
    }

    #[test]
    fn select_line_action_expands_to_whole_lines_for_followup_actions() {
        let text = "alpha\nbeta\ngamma\n";
        let mut app = app_with_text(text);
        app.workspace
            .active_mut()
            .set_cursor(text.find("beta").unwrap() + 1, false);

        app.execute_action(Action::SelectLines);

        assert_eq!(app.status_message(), Some("Selected 1 line"));
        assert_eq!(
            app.workspace.active().selected_text().as_deref(),
            Some("beta\n")
        );

        app.execute_action(Action::DeleteLine);

        assert_eq!(app.workspace.active().document.text(), "alpha\ngamma\n");
        assert_eq!(app.status_message(), Some("Deleted 1 line"));
        app.execute_action(Action::Undo);
        assert_eq!(app.workspace.active().document.text(), text);

        let anchor = text.find("alpha").unwrap() + 2;
        let cursor = text.find("gamma").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);
        app.execute_action(Action::SelectLines);

        assert_eq!(app.status_message(), Some("Selected 2 lines"));
        assert_eq!(
            app.workspace.active().selected_text().as_deref(),
            Some("alpha\nbeta\n")
        );
    }

    #[test]
    fn copy_location_copies_file_position_or_selection_range_to_register() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        let path = directory.path().join("src/main.rs");
        let text = "alpha\nbeta\ngamma\n";
        std::fs::write(&path, text).unwrap();
        let workspace =
            Workspace::from_path(Some(path), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(
            workspace,
            Config {
                osc52_copy: false,
                ..Config::default()
            },
        );

        app.workspace
            .active_mut()
            .set_cursor(text.find("beta").unwrap() + 2, false);
        app.execute_action(Action::CopyLocation);

        assert_eq!(app.ui.clipboard.register(), "src/main.rs:2:3");
        assert_eq!(
            app.status_message(),
            Some("Copied location to internal register (OSC 52 copy is disabled)")
        );

        let anchor = text.find("alpha").unwrap() + 1;
        let cursor = text.find("gamma").unwrap();
        app.workspace.active_mut().set_cursor(anchor, false);
        app.workspace.active_mut().set_cursor(cursor, true);
        app.execute_action(Action::CopyLocation);

        assert_eq!(app.ui.clipboard.register(), "src/main.rs:1:2-3:1");

        app.workspace.new_buffer();
        app.execute_action(Action::CopyLocation);

        assert_eq!(app.ui.clipboard.register(), "Untitled:1:1");
    }

    #[test]
    fn copy_problem_copies_current_line_lsp_diagnostic_to_register() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        let path = directory.path().join("src/main.rs");
        let text = "alpha\nbeta\ngamma\n";
        std::fs::write(&path, text).unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(
            workspace,
            Config {
                osc52_copy: false,
                ..Config::default()
            },
        );
        let uri = file_uri_identity(&path);
        app.lsp.diagnostics.replace(
            uri.clone(),
            vec![Diagnostic {
                uri,
                range: zero_width_lsp_range(1, 1),
                severity: DiagnosticSeverity::Warning,
                message: "borrow exploded".to_owned(),
                source: Some("rustc".to_owned()),
                raw: crate::lsp_client::JsonValue::Null,
            }],
        );
        app.workspace
            .active_mut()
            .set_cursor(text.find("beta").unwrap(), false);

        app.execute_action(Action::CopyProblem);

        assert_eq!(
            app.ui.clipboard.register(),
            "src/main.rs:2:2: LSP warning [rustc]: borrow exploded"
        );
        assert_eq!(
            app.status_message(),
            Some("Copied problem to internal register (OSC 52 copy is disabled)")
        );
    }

    #[test]
    fn copy_problem_falls_back_to_current_line_task_problem() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        let path = directory.path().join("src/main.rs");
        let text = "alpha\nbeta\ngamma\n";
        std::fs::write(&path, text).unwrap();
        let workspace =
            Workspace::from_path(Some(path), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(
            workspace,
            Config {
                osc52_copy: false,
                ..Config::default()
            },
        );
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output = "src/main.rs:3:4: warning: task issue\n".to_owned();
        app.workspace
            .active_mut()
            .set_cursor(text.find("gamma").unwrap(), false);

        app.execute_action(Action::CopyProblem);

        assert_eq!(
            app.ui.clipboard.register(),
            "src/main.rs:3:4: task warning [check]: task issue"
        );

        app.workspace.active_mut().set_cursor(0, false);
        app.execute_action(Action::CopyProblem);

        assert_eq!(app.status_message(), Some("No problem at current line"));
    }

    #[test]
    fn workspace_outline_finds_local_symbols_across_indexed_files_without_lsp() {
        let directory = tempfile::tempdir().unwrap();
        let src_dir = directory.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let active = src_dir.join("main.rs");
        let notes = directory.path().join("README.md");
        std::fs::write(&active, "pub struct Bird;\n\nfn main() {}\n").unwrap();
        std::fs::write(&notes, "# Roost Plan\n\n## Perch Cache\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::WorkspaceOutline);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "WORKSPACE OUTLINE");
        assert!(
            overlay
                .items
                .iter()
                .any(|item| item.contains("src/main.rs:1") && item.contains("struct Bird"))
        );
        assert!(
            overlay
                .items
                .iter()
                .any(|item| item.contains("README.md:3") && item.contains("Perch Cache"))
        );
        assert!(overlay.notice.unwrap().contains("Local workspace outline"));

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("workspace-outline picker missing");
        };
        prompt.input = "perch".to_owned();
        app.prompt_changed();
        app.commit_prompt();

        let canonical_notes = std::fs::canonicalize(&notes).unwrap();
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_notes.as_path())
        );
        assert_eq!(
            app.workspace.active().position(app.config.tab_width).line,
            2
        );
        assert_eq!(
            app.status_message(),
            Some("Jumped to workspace outline symbol")
        );
    }

    #[test]
    fn workspace_outline_reports_no_symbols_without_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("notes.txt");
        std::fs::write(&active, "plain text only\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::WorkspaceOutline);

        assert!(app.overlay().is_none());
        assert_eq!(
            app.status_message(),
            Some("No workspace outline symbols found")
        );
    }

    #[test]
    fn source_annotations_picker_finds_todos_across_indexed_source_files() {
        let directory = tempfile::tempdir().unwrap();
        let src_dir = directory.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let active = src_dir.join("main.rs");
        let notes = directory.path().join("README.md");
        std::fs::write(&active, "fn main() {\n    // TODO: wire terminal IDE\n}\n").unwrap();
        std::fs::write(&notes, "# Notes\n\nFIXME: document Blink flow\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::SourceAnnotations);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "SOURCE ANNOTATIONS");
        assert!(
            overlay
                .items
                .iter()
                .any(|item| item.contains("src/main.rs:2") && item.contains("TODO"))
        );
        assert!(
            overlay
                .items
                .iter()
                .any(|item| item.contains("README.md:3") && item.contains("FIXME"))
        );
        assert!(overlay.notice.unwrap().contains("Source annotations"));

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("source-annotations picker missing");
        };
        prompt.input = "blink".to_owned();
        app.prompt_changed();
        app.commit_prompt();

        let canonical_notes = std::fs::canonicalize(&notes).unwrap();
        assert_eq!(
            app.workspace.active().document.path(),
            Some(canonical_notes.as_path())
        );
        assert_eq!(
            app.workspace.active().position(app.config.tab_width).line,
            2
        );
        assert_eq!(app.status_message(), Some("Jumped to source annotation"));
    }

    #[test]
    fn source_annotations_reports_no_project_or_no_matches_without_prompt() {
        let mut no_project = app_with_text("// TODO: not indexed\n");
        no_project.project.index = None;
        no_project.execute_action(Action::SourceAnnotations);
        assert!(no_project.overlay().is_none());
        assert_eq!(
            no_project.status_message(),
            Some("Source annotations need a project root")
        );

        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("main.rs");
        std::fs::write(&active, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::SourceAnnotations);

        assert!(app.overlay().is_none());
        assert_eq!(app.status_message(), Some("No source annotations found"));
    }

    #[test]
    fn source_annotation_scan_detects_tags_with_identifier_boundaries() {
        assert_eq!(
            annotation_columns_in_line("// TODO: first item FIXME later"),
            vec![("TODO", 3), ("FIXME", 20)]
        );
        assert!(annotation_columns_in_line("METHOD should not match TODO").contains(&("TODO", 24)));
        assert!(annotation_columns_in_line("METHOD only").is_empty());
        assert!(annotation_columns_in_line("noteworthy").is_empty());
    }

    #[test]
    fn cancelled_or_replaced_workspace_symbol_queries_never_reopen_ui() {
        let mut app = app_with_text("");
        app.lsp.server_name = Some("mock".to_owned());
        app.lsp.active_workspace_symbol_token = Some(10);
        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolPending,
            "old".to_owned(),
            0,
            None,
        ));
        app.lsp.requests.insert(
            51,
            PendingLspRequest::WorkspaceSymbols {
                prompt_token: 10,
                query: "old".to_owned(),
                server_name: "mock".to_owned(),
            },
        );
        app.cancel_prompt();
        assert!(app.lsp.requests.is_empty());
        assert!(!app.handle_lsp_event(LspEvent::WorkspaceSymbols {
            request_id: 51,
            result: crate::lsp_client::JsonValue::Array(Vec::new()),
        }));
        assert!(matches!(app.ui.mode, UiMode::Edit));

        app.lsp.active_workspace_symbol_token = Some(12);
        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolPending,
            "new".to_owned(),
            0,
            None,
        ));
        app.lsp.requests.insert(
            52,
            PendingLspRequest::WorkspaceSymbols {
                prompt_token: 11,
                query: "old".to_owned(),
                server_name: "mock".to_owned(),
            },
        );
        assert!(!app.handle_lsp_event(LspEvent::WorkspaceSymbols {
            request_id: 52,
            result: crate::lsp_client::JsonValue::Array(Vec::new()),
        }));
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind == PromptFlow::WorkspaceSymbolPending && prompt.input == "new"
        ));
        assert_eq!(app.lsp.active_workspace_symbol_token, Some(12));
    }

    #[test]
    fn workspace_symbol_query_validation_and_pending_prompt_are_bounded() {
        let mut app = app_with_text("");
        app.lsp.active_workspace_symbol_token = Some(1);
        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolQuery,
            String::new(),
            0,
            None,
        ));

        app.commit_prompt();
        assert!(app.status_message().unwrap().contains("non-empty"));
        assert!(matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::WorkspaceSymbolQuery,
                ..
            })
        ));

        if let UiMode::Prompt(prompt) = &mut app.ui.mode {
            prompt.input = "x".repeat(MAX_WORKSPACE_SYMBOL_QUERY_BYTES + 1);
        }
        app.commit_prompt();
        assert!(app.status_message().unwrap().contains("512 UTF-8 bytes"));

        if let UiMode::Prompt(prompt) = &mut app.ui.mode {
            prompt.kind = PromptFlow::WorkspaceSymbolPending;
            prompt.input = "frozen".to_owned();
        }
        app.handle_event(key(KeyCode::Char('x')));
        app.handle_event(Event::Paste("paste".to_owned()));
        app.handle_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind == PromptFlow::WorkspaceSymbolPending
                    && prompt.input == "frozen"
        ));

        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolQuery,
            "x".repeat(MAX_WORKSPACE_SYMBOL_QUERY_BYTES),
            0,
            None,
        ));
        app.handle_event(key(KeyCode::Char('y')));
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind == PromptFlow::WorkspaceSymbolQuery
                    && prompt.input.len() == MAX_WORKSPACE_SYMBOL_QUERY_BYTES
        ));
        assert!(app.status_is_error());

        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbols,
            "x".repeat(MAX_WORKSPACE_SYMBOL_QUERY_BYTES),
            0,
            None,
        ));
        app.handle_event(Event::Paste("more".to_owned()));
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt)
                if prompt.kind == PromptFlow::WorkspaceSymbols
                    && prompt.input.len() == MAX_WORKSPACE_SYMBOL_QUERY_BYTES
        ));
        assert!(app.status_is_error());
    }

    #[test]
    fn a_new_ui_action_invalidates_earlier_lsp_responses() {
        let mut app = app_with_text("origin");
        let editor_id = app.workspace.active().id();
        let context = track_lsp_edit_context(&mut app, "file:///ignored.rs", 1);
        app.lsp
            .requests
            .insert(70, PendingLspRequest::Hover { context });

        app.execute_action(Action::WorkspaceSymbols);
        assert!(app.lsp.requests.is_empty());
        assert!(!app.handle_lsp_event(LspEvent::Hover {
            request_id: 70,
            uri: "file:///ignored.rs".to_owned(),
            version: DocumentVersion::INITIAL,
            result: crate::lsp_client::JsonValue::parse(r#"{"contents":"late hover"}"#,).unwrap(),
        }));
        assert_eq!(app.workspace.active().id(), editor_id);
        assert_eq!(app.workspace.active().document.text(), "origin");
    }

    #[test]
    fn keyboard_cursor_move_cancels_position_scoped_lsp_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "origin\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&path);
        let context = track_lsp_edit_context(&mut app, uri.clone(), 1);
        app.lsp
            .requests
            .insert(72, PendingLspRequest::Definition { context });

        app.handle_event(key(KeyCode::Right));

        assert_eq!(app.workspace.active().cursor, 1);
        assert!(app.lsp.requests.is_empty());
        assert!(!app.handle_lsp_event(LspEvent::Definition {
            request_id: 72,
            uri,
            version: DocumentVersion::new(1),
            result: crate::lsp_client::JsonValue::Null,
        }));
        assert_eq!(app.workspace.active().document.path(), Some(path.as_path()));
    }

    #[test]
    fn restarting_a_service_dismisses_a_frozen_workspace_symbol_prompt() {
        let mut app = app_with_text("");
        app.lsp.active_workspace_symbol_token = Some(19);
        app.ui.mode = UiMode::Prompt(Prompt::new(
            PromptFlow::WorkspaceSymbolPending,
            "bird".to_owned(),
            0,
            None,
        ));
        app.lsp.requests.insert(
            71,
            PendingLspRequest::WorkspaceSymbols {
                prompt_token: 19,
                query: "bird".to_owned(),
                server_name: "old".to_owned(),
            },
        );

        app.restart_lsp();

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(app.lsp.active_workspace_symbol_token, None);
        assert!(app.lsp.requests.is_empty());
    }

    #[test]
    fn lsp_log_action_opens_bounded_read_only_log_view() {
        let mut app = app_with_text("");

        app.execute_action(Action::LspLog);

        assert_eq!(app.status_message(), Some("Language-server log is empty"));

        app.append_lsp_log("started mock server\n");
        app.execute_action(Action::LspLog);

        assert_eq!(app.workspace.active().document.display_name(), "LSP Log");
        assert!(app.workspace.active().document.is_read_only());
        assert!(
            app.workspace
                .active()
                .document
                .text()
                .contains("started mock")
        );
        assert_eq!(
            app.status_message(),
            Some("Language-server log opened as a read-only IDE view")
        );
    }

    #[test]
    fn lsp_restart_action_clears_service_state_and_reports_missing_config() {
        let mut app = app_with_text("");
        app.lsp.server_name = Some("mock".to_owned());
        app.lsp.workspace_symbols = Some(true);
        app.lsp.text_document_sync = Some(TextDocumentSyncCapability {
            open_close: true,
            full: true,
            ..TextDocumentSyncCapability::default()
        });
        let context = track_lsp_edit_context(&mut app, "file:///demo.rs", 1);
        app.lsp
            .requests
            .insert(5, PendingLspRequest::Hover { context });
        app.lsp.diagnostics.mark_partial();

        app.execute_action(Action::LspRestart);

        assert_eq!(
            app.status_message(),
            Some(
                "No language server is configured for the active file — run wscrpt --health and authorize one in ~/.config/wscrpt/config.toml",
            )
        );
        assert_eq!(app.lsp.server_name, None);
        assert_eq!(app.lsp.workspace_symbols, None);
        assert_eq!(app.lsp.text_document_sync, None);
        assert!(app.lsp.requests.is_empty());
        assert!(app.lsp.diagnostics.is_empty());
        assert!(!app.lsp.diagnostics.is_partial());
    }

    #[test]
    fn stale_lsp_location_is_rejected_before_switching_buffers() {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin.rs");
        let target = directory.path().join("target.rs");
        std::fs::write(&origin, "origin\n").unwrap();
        std::fs::write(&target, "target\n").unwrap();
        let workspace =
            Workspace::from_path(Some(origin.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let origin_id = app.workspace.active().id();

        app.open_lsp_location(Location {
            uri: file_uri_identity(&target),
            range: zero_width_lsp_range(99, 0),
        });

        assert_eq!(app.workspace.active().id(), origin_id);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(origin.as_path())
        );
        assert_eq!(app.workspace.len(), 1);
        assert!(app.status_is_error());
        assert!(app.status_message().unwrap().contains("Stale LSP location"));
    }

    #[test]
    fn ready_event_tracks_workspace_symbol_capability() {
        let mut app = app_with_text("");
        assert_eq!(app.lsp.workspace_symbols, None);
        assert!(app.handle_lsp_event(LspEvent::Ready {
            capabilities: crate::lsp_client::JsonValue::Null,
            workspace_symbols: true,
            text_document_sync: TextDocumentSyncCapability {
                open_close: true,
                full: true,
                incremental: false,
                save: true,
                save_include_text: false,
            },
        }));
        assert_eq!(app.lsp.workspace_symbols, Some(true));
        assert!(app.handle_lsp_event(LspEvent::Ready {
            capabilities: crate::lsp_client::JsonValue::Null,
            workspace_symbols: false,
            text_document_sync: TextDocumentSyncCapability {
                open_close: true,
                full: true,
                incremental: false,
                save: true,
                save_include_text: false,
            },
        }));
        assert_eq!(app.lsp.workspace_symbols, Some(false));

        assert!(app.handle_lsp_event(LspEvent::Ready {
            capabilities: crate::lsp_client::JsonValue::Null,
            workspace_symbols: true,
            text_document_sync: TextDocumentSyncCapability {
                open_close: true,
                full: false,
                incremental: true,
                save: false,
                save_include_text: false,
            },
        }));
        assert!(
            app.lsp
                .text_document_sync
                .is_some_and(|capability| capability.incremental && !capability.full)
        );

        assert!(app.handle_lsp_event(LspEvent::Ready {
            capabilities: crate::lsp_client::JsonValue::Null,
            workspace_symbols: false,
            text_document_sync: TextDocumentSyncCapability {
                open_close: true,
                full: false,
                incremental: false,
                save: false,
                save_include_text: false,
            },
        }));
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("full or incremental")
        );
    }

    #[test]
    fn same_server_discovery_is_bounded_and_captures_inactive_edit_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();
        let mut workspace =
            Workspace::from_path(Some(first.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let first_id = workspace.active().id();
        workspace.open(&second).unwrap();
        let active_id = workspace.active().id();
        let mut app = App::new_ready_for_test(workspace, rust_lsp_config());

        app.workspace
            .editor_by_id_mut(first_id)
            .unwrap()
            .insert("dirty ", EditKind::Insert)
            .unwrap();
        assert!(
            app.lsp_sync_target(active_id, "rust-test")
                .is_some_and(|target| target.editor_id == active_id)
        );
        let inactive = app.lsp_unsynchronized_targets("rust-test", active_id, 1);
        assert_eq!(inactive.len(), 1);
        assert_eq!(inactive[0].editor_id, first_id);
        assert_eq!(
            inactive[0].state_id,
            app.workspace
                .editor_by_id(first_id)
                .unwrap()
                .document
                .state_id()
        );
        assert_ne!(inactive[0].saved_state_id, Some(inactive[0].state_id));

        app.workspace
            .editor_by_id_mut(first_id)
            .unwrap()
            .document
            .save()
            .unwrap();
        let saved = app.lsp_unsynchronized_targets("rust-test", active_id, 1);
        assert_eq!(saved[0].saved_state_id, Some(saved[0].state_id));

        for index in 0..(MAX_SYNCHRONIZED_DOCUMENTS * 3) {
            app.workspace
                .open(directory.path().join(format!("buffer-{index}.rs")))
                .unwrap();
        }
        let active_id = app.workspace.active().id();
        let targets =
            app.lsp_unsynchronized_targets("rust-test", active_id, MAX_SYNCHRONIZED_DOCUMENTS + 1);
        assert_eq!(targets.len(), MAX_SYNCHRONIZED_DOCUMENTS + 1);
    }

    #[test]
    fn lsp_uri_identity_normalizes_dot_segments_for_sync_and_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let canonical_path = directory.path().join("main.rs");
        std::fs::write(&canonical_path, "fn main() {}\n").unwrap();
        let alias_path = nested.join("../main.rs");
        let workspace = Workspace::from_path(
            Some(alias_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            workspace.active().document.path(),
            Some(alias_path.as_path())
        );
        let mut app = App::new_ready_for_test(workspace, rust_lsp_config());
        let target = app
            .lsp_sync_target(app.workspace.active().id(), "rust-test")
            .unwrap();
        let normalized_uri = file_uri_identity(std::fs::canonicalize(&canonical_path).unwrap());
        assert_eq!(target.uri, normalized_uri);
        let context = track_lsp_edit_context(&mut app, normalized_uri.clone(), 1);
        let diagnostic = crate::lsp_client::JsonValue::parse(
            r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"severity":2,"message":"normalized"}"#,
        )
        .unwrap();

        assert!(app.handle_lsp_event(LspEvent::Diagnostics {
            uri: normalized_uri,
            version: Some(context.version),
            observed_version: Some(context.version),
            observed_incarnation: Some(context.incarnation),
            diagnostics: vec![diagnostic],
        }));
        assert_eq!(app.active_diagnostics().len(), 1);
        assert_eq!(app.active_diagnostics()[0].message, "normalized");
    }

    #[test]
    fn lsp_background_discovery_inspects_a_bounded_round_robin_slice() {
        let directory = tempfile::tempdir().unwrap();
        let active_path = directory.path().join("active.rs");
        std::fs::write(&active_path, "fn active() {}\n").unwrap();
        let mut workspace =
            Workspace::from_path(Some(active_path), Some(directory.path().to_path_buf())).unwrap();
        let active_id = workspace.active().id();
        for index in 0..MAX_LSP_DISCOVERY_INSPECTIONS_PER_POLL {
            workspace
                .open(directory.path().join(format!("unmapped-{index}.txt")))
                .unwrap();
        }
        let late_path = directory.path().join("late.rs");
        workspace.open(&late_path).unwrap();
        let late_id = workspace.active().id();
        workspace.activate(workspace.editor_index(active_id).unwrap());
        let mut app = App::new_ready_for_test(workspace, rust_lsp_config());

        let first = app.lsp_unsynchronized_targets("rust-test", active_id, 1);
        assert!(first.is_empty());
        assert_eq!(
            app.lsp.discovery_cursor,
            MAX_LSP_DISCOVERY_INSPECTIONS_PER_POLL
        );

        let second = app.lsp_unsynchronized_targets("rust-test", active_id, 1);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].editor_id, late_id);
    }

    #[test]
    fn unmapped_active_buffer_retains_same_server_document_registry() {
        let directory = tempfile::tempdir().unwrap();
        let mapped = directory.path().join("main.rs");
        let unmapped = directory.path().join("notes.txt");
        std::fs::write(&mapped, "fn main() {}\n").unwrap();
        std::fs::write(&unmapped, "notes\n").unwrap();
        let mut workspace =
            Workspace::from_path(Some(mapped.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mapped_id = workspace.active().id();
        workspace.open(&unmapped).unwrap();
        let mut app = App::new_ready_for_test(workspace, rust_lsp_config());
        app.lsp.server_name = Some("rust-test".to_owned());
        app.lsp
            .documents
            .insert(SynchronizedDocument::new(
                mapped_id,
                file_uri_identity(&mapped),
                DocumentVersion::new(3),
                app.workspace
                    .editor_by_id(mapped_id)
                    .unwrap()
                    .document
                    .state_id(),
                app.workspace
                    .editor_by_id(mapped_id)
                    .unwrap()
                    .document
                    .saved_state_id(),
                app.workspace
                    .editor_by_id(mapped_id)
                    .unwrap()
                    .document
                    .save_generation(),
            ))
            .unwrap();

        assert!(!app.ensure_lsp_service());
        assert_eq!(app.lsp.server_name.as_deref(), Some("rust-test"));
        assert!(app.lsp.documents.get_by_editor_id(mapped_id).is_some());
        assert!(app.lsp_editor_matches_service(
            mapped_id,
            &file_uri_identity(&mapped),
            "rust-test"
        ));
        let mapped_index = app.workspace.editor_index(mapped_id).unwrap();
        app.workspace.activate(mapped_index);
        app.workspace.close_active(true).unwrap();
        assert!(!app.lsp_editor_matches_service(
            mapped_id,
            &file_uri_identity(&mapped),
            "rust-test"
        ));
    }

    #[test]
    fn oversized_lsp_admission_is_rejected_before_client_or_text_access() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "small\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, rust_lsp_config());
        let mut target = app
            .lsp_sync_target(app.workspace.active().id(), "rust-test")
            .unwrap();
        target.document_bytes = DEFAULT_MAX_DOCUMENT_BYTES + 1;
        let mut error = None;
        let mut fatal = None;
        let mut budget = LspSyncBudget::new();
        let mut backpressured = false;

        assert!(app.admit_lsp_document(
            target,
            true,
            &mut error,
            &mut fatal,
            &mut budget,
            &mut backpressured,
        ));
        assert!(error.is_some());
        assert!(fatal.is_none());
        assert!(!backpressured);
        assert!(app.lsp.documents.is_empty());
        assert!(app.lsp.documents.is_partial());
        assert_eq!(app.lsp.quarantined_documents.len(), 1);
    }

    #[test]
    fn late_diagnostics_are_ignored_and_parser_loss_marks_cache_partial() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let editor = app.workspace.active();
        let editor_id = editor.id();
        let state_id = editor.document.state_id();
        let saved_state_id = editor.document.saved_state_id();
        let uri = file_uri_identity(&path);
        let incarnation = DocumentIncarnation::for_test(1);
        let publication = || LspEvent::Diagnostics {
            uri: uri.clone(),
            version: Some(DocumentVersion::new(1)),
            observed_version: Some(DocumentVersion::new(1)),
            observed_incarnation: Some(incarnation),
            diagnostics: vec![crate::lsp_client::JsonValue::Null],
        };

        assert!(!app.handle_lsp_event(publication()));
        assert!(app.lsp.diagnostics.is_empty());
        assert!(!app.lsp.diagnostics.is_partial());

        app.lsp
            .documents
            .insert(SynchronizedDocument::new(
                editor_id,
                uri.clone(),
                DocumentVersion::new(1),
                state_id,
                saved_state_id,
                app.workspace.active().document.save_generation(),
            ))
            .unwrap();
        app.lsp.document_incarnations.insert(editor_id, incarnation);
        assert!(app.handle_lsp_event(publication()));
        assert!(app.lsp.diagnostics.is_empty());
        assert!(app.lsp.diagnostics.is_partial());
        app.lsp.server_name = Some("rust-test".to_owned());
        assert!(
            app.lsp_summary()
                .is_some_and(|summary| summary.ends_with(" PARTIAL"))
        );
        app.open_problems();
        assert!(
            app.status_message()
                .is_some_and(|message| message.contains("Problems are partial"))
        );

        app.lsp.documents.remove_by_uri(&uri);
        app.lsp.document_incarnations.remove(&editor_id);
        app.lsp.diagnostics.clear();
        assert!(!app.handle_lsp_event(publication()));
        assert!(!app.lsp.diagnostics.is_partial());
    }

    #[test]
    fn malformed_diagnostics_publication_purges_affected_context_and_marks_partial() {
        let mut app = app_with_text("main\n");
        let uri = "file:///main.rs".to_owned();
        let other_uri = "file:///other.rs".to_owned();
        let range = zero_width_lsp_range(0, 1);
        let diagnostic = |diagnostic_uri: &str| Diagnostic {
            uri: diagnostic_uri.to_owned(),
            range,
            severity: DiagnosticSeverity::Warning,
            message: "cached".to_owned(),
            source: Some("mock".to_owned()),
            raw: json_object([("message", "cached".into())]),
        };
        app.lsp
            .diagnostics
            .replace(uri.clone(), vec![diagnostic(&uri)]);
        app.lsp
            .diagnostics
            .replace(other_uri.clone(), vec![diagnostic(&other_uri)]);

        assert!(app.handle_lsp_event(LspEvent::DiagnosticsRejected {
            uri: Some(uri.clone()),
            reason: "params.lsp.diagnostics was not an array".to_owned(),
        }));
        assert!(app.lsp.diagnostics.get(&uri).is_none());
        assert_eq!(app.lsp.diagnostics.get(&other_uri).map(<[_]>::len), Some(1));
        assert!(app.lsp.diagnostics.is_partial());
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("malformed diagnostics")
        );
        assert!(app.lsp.log.contains("rejected diagnostics publication"));

        assert!(app.handle_lsp_event(LspEvent::DiagnosticsRejected {
            uri: None,
            reason: "params was not an object".to_owned(),
        }));
        assert!(app.lsp.diagnostics.is_empty());
        assert!(app.lsp.diagnostics.is_partial());
    }

    #[test]
    fn queued_versionless_diagnostics_require_the_observed_document_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&path);
        let context = track_lsp_edit_context(&mut app, uri.clone(), 1);
        let incarnation = app.lsp.document_incarnations[&context.editor_id];
        let diagnostic = crate::lsp_client::JsonValue::parse(
            r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":2,"message":"observed"}"#,
        )
        .unwrap();

        assert!(!app.handle_lsp_event(LspEvent::Diagnostics {
            uri: uri.clone(),
            version: None,
            observed_version: Some(context.version),
            observed_incarnation: Some(DocumentIncarnation::for_test(
                incarnation.get().saturating_add(1),
            )),
            diagnostics: vec![diagnostic.clone()],
        }));
        assert!(app.lsp.diagnostics.is_empty());

        assert!(!app.handle_lsp_event(LspEvent::Diagnostics {
            uri: uri.clone(),
            version: None,
            observed_version: Some(DocumentVersion::new(2)),
            observed_incarnation: Some(incarnation),
            diagnostics: vec![diagnostic.clone()],
        }));
        assert!(app.lsp.diagnostics.is_empty());

        assert!(app.handle_lsp_event(LspEvent::Diagnostics {
            uri: uri.clone(),
            version: None,
            observed_version: Some(context.version),
            observed_incarnation: Some(incarnation),
            diagnostics: vec![diagnostic],
        }));
        assert_eq!(app.lsp.diagnostics.get(&uri).map(<[_]>::len), Some(1));
    }

    #[test]
    fn versionless_diagnostics_are_dropped_after_uri_state_becomes_ambiguous() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&path);
        let context = track_lsp_edit_context(&mut app, uri.clone(), 7);
        let incarnation = app.lsp.document_incarnations[&context.editor_id];
        app.mark_lsp_diagnostics_ambiguous(uri.clone());

        assert!(app.handle_lsp_event(LspEvent::Diagnostics {
            uri: uri.clone(),
            version: None,
            observed_version: Some(context.version),
            observed_incarnation: Some(incarnation),
            diagnostics: vec![crate::lsp_client::JsonValue::parse(
                r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"ambiguous"}"#,
            )
            .unwrap()],
        }));
        assert!(app.lsp.diagnostics.is_empty());
        assert!(app.lsp.diagnostics.is_partial());
    }

    #[test]
    fn problems_prompt_merges_task_and_lsp_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let task_path = directory.path().join("task.rs");
        let lsp_path = directory.path().join("lsp.rs");
        std::fs::write(&task_path, "one\ntwo\n").unwrap();
        std::fs::write(&lsp_path, "lsp\n").unwrap();
        let workspace = Workspace::from_path(
            Some(task_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output = "task.rs:2:2: error: task failure\n".to_owned();
        let uri = file_uri_identity(&lsp_path);
        app.lsp.diagnostics.replace(
            uri.clone(),
            vec![Diagnostic {
                uri,
                range: zero_width_lsp_range(0, 1),
                severity: DiagnosticSeverity::Warning,
                message: "LSP warning".to_owned(),
                source: Some("demo".to_owned()),
                raw: crate::lsp_client::JsonValue::Null,
            }],
        );

        app.open_problems();

        let UiMode::Prompt(prompt) = &app.ui.mode else {
            panic!("problems prompt missing");
        };
        assert_eq!(prompt.kind, PromptFlow::Problems);
        assert_eq!(prompt.labels.len(), 2);
        assert!(prompt.labels[0].starts_with("T E task.rs:2:2 [check]"));
        assert!(prompt.labels[1].starts_with("L W "));
        assert!(matches!(prompt.entries[0], PromptEntry::TaskProblem(_)));
        assert!(matches!(
            prompt.entries[1],
            PromptEntry::ProblemLocation(_, DiagnosticSeverity::Warning)
        ));
    }

    #[test]
    fn problems_prompt_has_a_global_candidate_bound_and_partial_notice() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("main.rs");
        std::fs::write(&source, "main\n").unwrap();
        let workspace =
            Workspace::from_path(Some(source.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&source);
        for bucket in 0..5 {
            let bucket_uri = format!("{uri}.{bucket}");
            let count = if bucket == 4 { 1 } else { 1_024 };
            app.lsp.diagnostics.replace(
                bucket_uri.clone(),
                (0..count)
                    .map(|index| Diagnostic {
                        uri: bucket_uri.clone(),
                        range: zero_width_lsp_range(index, 0),
                        severity: DiagnosticSeverity::Warning,
                        message: format!("warning {bucket}:{index}"),
                        source: None,
                        raw: crate::lsp_client::JsonValue::Null,
                    })
                    .collect(),
            );
        }

        app.open_problems();

        let UiMode::Prompt(prompt) = &app.ui.mode else {
            panic!("problems prompt missing");
        };
        assert_eq!(prompt.labels.len(), MAX_PROBLEM_CANDIDATES);
        assert!(prompt.notice.as_ref().is_some_and(|notice| {
            notice.contains("Problems list is partial")
                && notice.contains(&MAX_PROBLEM_CANDIDATES.to_string())
        }));
    }

    #[test]
    fn task_problem_navigation_preserves_dirty_buffer_and_jump_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("main.rs");
        std::fs::write(&source, "\tlet bird = 1;\n").unwrap();
        let workspace =
            Workspace::from_path(Some(source.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let editor_id = app.workspace.active().id();
        let end = app.workspace.active().document.len_chars();
        app.workspace.active_mut().set_cursor(end, false);
        app.workspace
            .active_mut()
            .insert("// dirty", EditKind::Insert)
            .unwrap();
        let origin = app.workspace.active().cursor;
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output = "main.rs:1:5: error: bad bird\n".to_owned();

        app.open_problems();
        app.commit_prompt();

        assert_eq!(app.workspace.active().id(), editor_id);
        assert!(app.workspace.active().document.is_modified());
        assert_eq!(app.workspace.active().cursor, 1);
        app.execute_action(Action::JumpBack);
        assert_eq!(app.workspace.active().id(), editor_id);
        assert_eq!(app.workspace.active().cursor, origin);
    }

    #[test]
    fn next_and_previous_problem_navigate_sorted_locations_and_wrap() {
        let directory = tempfile::tempdir().unwrap();
        let alpha = directory.path().join("alpha.rs");
        let beta = directory.path().join("beta.rs");
        std::fs::write(&alpha, "one\ntwo\n").unwrap();
        std::fs::write(&beta, "beta\n").unwrap();
        let alpha = std::fs::canonicalize(alpha).unwrap();
        let beta = std::fs::canonicalize(beta).unwrap();
        let workspace =
            Workspace::from_path(Some(alpha.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output = concat!(
            "beta.rs:1:1: error: beta\n",
            "alpha.rs:2:1: error: alpha two\n",
            "alpha.rs:1:1: error: alpha one\n",
        )
        .to_owned();

        app.execute_action(Action::PreviousProblem);
        assert_eq!(app.workspace.active().document.path(), Some(beta.as_path()));
        assert_eq!(app.workspace.active().cursor, 0);

        app.execute_action(Action::NextProblem);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(alpha.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 0);

        app.execute_action(Action::NextProblem);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(alpha.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 4);

        app.execute_action(Action::JumpBack);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(alpha.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 0);
    }

    #[test]
    fn next_and_previous_error_skip_warnings_and_wrap_across_lsp_and_tasks() {
        let directory = tempfile::tempdir().unwrap();
        let alpha = directory.path().join("alpha.rs");
        let beta = directory.path().join("beta.rs");
        std::fs::write(&alpha, "one\ntwo\n").unwrap();
        std::fs::write(&beta, "beta\n").unwrap();
        let alpha = std::fs::canonicalize(alpha).unwrap();
        let beta = std::fs::canonicalize(beta).unwrap();
        let workspace =
            Workspace::from_path(Some(alpha.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&alpha);
        app.lsp.diagnostics.replace(
            uri.clone(),
            vec![
                Diagnostic {
                    uri: uri.clone(),
                    range: zero_width_lsp_range(0, 0),
                    severity: DiagnosticSeverity::Warning,
                    message: "skip warning".to_owned(),
                    source: Some("mock".to_owned()),
                    raw: crate::lsp_client::JsonValue::Null,
                },
                Diagnostic {
                    uri,
                    range: zero_width_lsp_range(1, 0),
                    severity: DiagnosticSeverity::Error,
                    message: "take error".to_owned(),
                    source: Some("mock".to_owned()),
                    raw: crate::lsp_client::JsonValue::Null,
                },
            ],
        );
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output = "beta.rs:1:1: error: beta error\n".to_owned();

        app.workspace.active_mut().set_cursor(0, false);
        app.execute_action(Action::NextError);

        assert_eq!(
            app.workspace.active().document.path(),
            Some(alpha.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 4);

        app.execute_action(Action::NextError);

        assert_eq!(app.workspace.active().document.path(), Some(beta.as_path()));
        assert_eq!(app.workspace.active().cursor, 0);

        app.execute_action(Action::PreviousError);

        assert_eq!(
            app.workspace.active().document.path(),
            Some(alpha.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 4);
    }

    #[test]
    fn rustc_task_problem_uses_scalar_columns_with_tabs_and_unicode() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("main.rs");
        std::fs::write(&source, "\tlet café = nope;\n").unwrap();
        let workspace =
            Workspace::from_path(Some(source), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output =
            "error[E0425]: cannot find value `nope`\n  --> main.rs:1:13\n".to_owned();

        app.open_problems();
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt)
                if matches!(
                    prompt.entries.first(),
                    Some(PromptEntry::TaskProblem(problem))
                        if problem.column_kind == TaskProblemColumnKind::UnicodeScalar
                )
        ));
        app.commit_prompt();

        assert_eq!(app.workspace.active().cursor, 12);
        assert_eq!(app.workspace.active().document.slice(12..16), "nope");
    }

    #[test]
    fn split_utf8_task_output_still_opens_unicode_problem_paths() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("café.rs");
        std::fs::write(&source, "bird\n").unwrap();
        let workspace =
            Workspace::from_path(Some(source.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());

        for byte in "café.rs:1:1: error: unicode path\n".as_bytes() {
            app.append_task_output_chunk(OutputStream::Stdout, std::slice::from_ref(byte));
        }
        app.finish_task_output_decoders();
        app.open_problems();

        assert_eq!(app.tasks.output, "café.rs:1:1: error: unicode path\n");
        assert!(matches!(
            &app.ui.mode,
            UiMode::Prompt(prompt)
                if matches!(
                    prompt.entries.first(),
                    Some(PromptEntry::TaskProblem(problem))
                        if problem.path == std::fs::canonicalize(&source).unwrap()
                )
        ));
    }

    #[test]
    fn task_problem_target_is_revalidated_before_navigation() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let target = directory.path().join("target.rs");
        std::fs::write(&active, "active\n").unwrap();
        std::fs::write(&target, "target\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.tasks.cwd = Some(directory.path().to_path_buf());
        app.tasks.last = Some("check".to_owned());
        app.tasks.output = "target.rs:1:1: error: now stale\n".to_owned();
        app.open_problems();
        std::fs::remove_file(&target).unwrap();

        app.commit_prompt();

        assert_eq!(
            app.workspace.active().document.path(),
            Some(active.as_path())
        );
        assert!(
            app.status_message()
                .is_some_and(|message| message.contains("target is stale"))
        );
        assert!(!target.exists());
    }

    #[test]
    fn refreshed_project_index_also_replaces_search_worker_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let later = dir.path().join("later.txt");
        std::fs::write(&first, "old\n").unwrap();
        let workspace = Workspace::from_path(Some(first), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        std::fs::write(&later, "NEW_SEARCH_WORKER_MARKER\n").unwrap();

        app.refresh_workspace_snapshots();
        poll_app_until(&mut app, "search snapshot refresh", |app| {
            !app.project.status.is_pending()
        });
        let worker = app.project.search_worker.as_ref().expect("search worker");
        worker.request("NEW_SEARCH_WORKER_MARKER").unwrap();
        let result = worker
            .recv_latest_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, PathBuf::from("later.txt"));
    }

    #[test]
    fn session_snapshot_contains_navigation_but_never_document_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "secret unsaved text\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().cursor = 6;
        app.workspace.active_mut().anchor = Some(1);
        app.workspace.active_mut().viewport.top_line = 4;
        app.workspace.active_mut().viewport.top_wrap_char = 9;
        app.ui.soft_wrap = true;
        app.execute_action(Action::ToggleBookmark);

        let session = app.session_snapshot();
        assert_eq!(session.root, dir.path());
        assert_eq!(session.open_files.len(), 1);
        assert_eq!(session.open_files[0].path, path);
        assert_eq!(session.open_files[0].cursor, 6);
        assert_eq!(session.open_files[0].anchor, Some(1));
        assert_eq!(session.open_files[0].viewport.top_line, 4);
        assert_eq!(session.open_files[0].viewport.top_wrap_char, 9);
        assert_eq!(session.recent_files, vec![path.clone()]);
        assert_eq!(session.bookmarks, vec![BookmarkState { path, cursor: 6 }]);
        assert!(session.layout.soft_wrap);
        let encoded = toml::to_string(&session).unwrap();
        assert!(!encoded.contains("secret unsaved text"));
    }

    #[test]
    fn session_recent_files_keep_saved_and_restored_paths_without_text() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.rs");
        let second = dir.path().join("second.rs");
        let third = dir.path().join("third.rs");
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();
        std::fs::write(&third, "third\n").unwrap();
        let workspace =
            Workspace::from_path(Some(first.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.apply_session_recent_files(vec![second.clone(), third.clone()]);

        let session = app.session_snapshot();

        assert_eq!(session.open_files.len(), 1);
        assert_eq!(session.open_files[0].path, first);
        assert_eq!(session.recent_files, vec![first, second, third]);
        let encoded = toml::to_string(&session).unwrap();
        assert!(!encoded.contains("first\n"));
        assert!(!encoded.contains("second\n"));
        assert!(!encoded.contains("third\n"));
    }

    #[test]
    fn session_bookmarks_restore_existing_paths_and_skip_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.rs");
        let missing = dir.path().join("missing.rs");
        std::fs::write(&existing, "first\nsecond\n").unwrap();
        let workspace =
            Workspace::from_path(Some(existing.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.apply_session_bookmarks(vec![
            BookmarkState {
                path: missing,
                cursor: 0,
            },
            BookmarkState {
                path: existing.clone(),
                cursor: 6,
            },
        ]);

        assert_eq!(app.ui.bookmarks.len(), 1);
        assert_eq!(app.ui.bookmarks[0].path.as_ref(), Some(&existing));
        assert_eq!(app.ui.bookmarks[0].cursor, 6);

        app.execute_action(Action::Bookmarks);
        app.handle_event(key(KeyCode::Enter));

        assert_eq!(
            app.workspace
                .active()
                .document
                .char_to_line(app.workspace.active().cursor),
            1
        );
        assert_eq!(app.status_message(), Some("Jumped to bookmark"));
    }

    #[test]
    fn recovery_checkpoint_skips_unchanged_dirty_buffer_but_tracks_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecoveryStore::new(dir.path().join("recovery"));
        let mut app = app_with_text("");
        app.persistence.recovery_store = Some(store.clone());
        app.workspace
            .active_mut()
            .insert("unsaved", EditKind::Insert)
            .unwrap();

        assert!(!app.checkpoint_recovery());
        let journal = std::fs::read_dir(store.directory())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut bytes = std::fs::read(&journal).unwrap();
        bytes.extend_from_slice(b"# unchanged-checkpoint-marker\n");
        std::fs::write(&journal, &bytes).unwrap();

        assert!(!app.checkpoint_recovery());
        assert!(
            std::fs::read(&journal)
                .unwrap()
                .ends_with(b"# unchanged-checkpoint-marker\n")
        );

        app.workspace.active_mut().set_cursor(0, false);
        assert!(!app.checkpoint_recovery());
        assert!(
            !std::fs::read(&journal)
                .unwrap()
                .ends_with(b"# unchanged-checkpoint-marker\n")
        );
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn reload_clears_recovery_stamp_so_same_revision_is_checkpointed_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.txt");
        std::fs::write(&path, "base").unwrap();
        let workspace = Workspace::from_path(Some(path), Some(dir.path().to_path_buf())).unwrap();
        let store = RecoveryStore::new(dir.path().join("recovery"));
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.persistence.recovery_store = Some(store.clone());

        app.workspace
            .active_mut()
            .insert("x", EditKind::Insert)
            .unwrap();
        assert!(!app.checkpoint_recovery());
        assert_eq!(store.list().unwrap().len(), 1);
        app.reload_current(true);
        assert!(store.list().unwrap().is_empty());

        app.workspace
            .active_mut()
            .insert("x", EditKind::Insert)
            .unwrap();
        assert!(!app.checkpoint_recovery());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn recovery_never_creates_duplicate_session_paths() {
        let clean_dir = tempfile::tempdir().unwrap();
        let clean_path = clean_dir.path().join("main.txt");
        std::fs::write(&clean_path, "disk").unwrap();
        let clean_workspace = Workspace::from_path(
            Some(clean_path.clone()),
            Some(clean_dir.path().to_path_buf()),
        )
        .unwrap();
        let mut clean_app = App::new_ready_for_test(clean_workspace, Config::default());
        let clean_record = RecoveryRecord::new(
            clean_dir.path().to_path_buf(),
            Some(clean_path.clone()),
            "recovered".to_owned(),
            3,
            None,
            1,
            Some(0),
        );
        let clean_id = clean_record.id.clone();
        clean_app.persistence.recovery_records.push(clean_record);
        clean_app.restore_recovery(&clean_id);
        assert_eq!(clean_app.workspace.len(), 1);
        assert_eq!(clean_app.workspace.active().document.text(), "recovered");
        assert_eq!(
            clean_app.workspace.active().document.path(),
            Some(clean_path.as_path())
        );

        let dirty_dir = tempfile::tempdir().unwrap();
        let dirty_path = dirty_dir.path().join("main.txt");
        std::fs::write(&dirty_path, "disk").unwrap();
        let dirty_workspace = Workspace::from_path(
            Some(dirty_path.clone()),
            Some(dirty_dir.path().to_path_buf()),
        )
        .unwrap();
        let mut dirty_app = App::new_ready_for_test(dirty_workspace, Config::default());
        dirty_app
            .workspace
            .active_mut()
            .insert(" local", EditKind::Insert)
            .unwrap();
        let dirty_record = RecoveryRecord::new(
            dirty_dir.path().to_path_buf(),
            Some(dirty_path.clone()),
            "journal".to_owned(),
            0,
            None,
            1,
            Some(0),
        );
        let dirty_id = dirty_record.id.clone();
        dirty_app.persistence.recovery_records.push(dirty_record);
        dirty_app.restore_recovery(&dirty_id);
        assert_eq!(dirty_app.workspace.len(), 2);
        assert!(dirty_app.workspace.active().document.path().is_none());
        assert_eq!(dirty_app.workspace.active().document.text(), "journal");
        let session = dirty_app.session_snapshot();
        assert_eq!(session.open_files.len(), 1);
        assert_eq!(session.open_files[0].path, dirty_path);
    }

    #[test]
    fn terminal_action_is_a_one_shot_request() {
        let mut app = app_with_text("");
        app.execute_action(Action::Terminal);
        assert!(app.take_terminal_request());
        assert!(!app.take_terminal_request());
    }

    #[test]
    fn workspace_info_opens_a_read_only_context_snapshot() {
        let mut app = app_with_text("one\ntwo\n");
        app.set_screen_size((101, 37));
        app.execute_action(Action::WorkspaceInfo);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Workspace Info"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Workspace Info\n\n"));
        assert!(text.contains("Terminal: 101x37"));
        assert!(text.contains("Route snapshot: TERM="));
        assert!(text.contains("Active buffer\n"));
        assert!(text.contains("Workspace state\n"));
        assert!(text.contains("Tasks\n"));
        assert!(text.contains("Language server\n"));
        assert!(text.contains("Hardware gate\n"));
        assert_eq!(
            app.status_message(),
            Some("Workspace info opened as a read-only IDE view")
        );
    }

    #[test]
    fn buffer_info_opens_a_read_only_active_buffer_context_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, rust_lsp_config());
        app.workspace.active_mut().set_cursor(4, false);
        app.workspace.active_mut().set_cursor(11, true);
        app.workspace
            .active_mut()
            .insert("pub ", EditKind::Insert)
            .unwrap();

        app.execute_action(Action::BufferInfo);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Buffer Info"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Buffer Info\n\n"));
        assert!(text.contains("Identity\n"));
        assert!(text.contains("Name: main.rs"));
        assert!(text.contains(&format!("Path: {}", path.display())));
        assert!(text.contains("Text state\n"));
        assert!(text.contains("Selection:"));
        assert!(text.contains("Dirty: yes"));
        assert!(text.contains("File and language\n"));
        assert!(text.contains("Disk: "));
        assert!(text.contains("Syntax: Rust"));
        assert!(text.contains("Configured LSP: rust-test (rust)"));
        assert!(text.contains("Synchronized with active LSP: no"));
        assert!(text.contains("Diagnostics: 0 retained"));
        assert!(text.contains("Version control\n"));
        assert!(text.contains("Git: not a Git repository"));
        assert!(text.contains("Next actions\n"));
        assert_eq!(
            app.status_message(),
            Some("Buffer info opened as a read-only IDE view")
        );
    }

    #[test]
    fn dirty_buffers_view_lists_unsaved_buffers_without_switching_sources() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        workspace.open(&second_path).unwrap();
        let second_id = workspace.active().id();
        workspace
            .active_mut()
            .insert(" dirty", EditKind::Insert)
            .unwrap();
        workspace.activate(0);
        workspace
            .active_mut()
            .insert(" dirty", EditKind::Insert)
            .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::DirtyBuffers);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Dirty Buffers"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Dirty Buffers\n\n"));
        assert!(text.contains("Open buffers: 2  dirty: 2"));
        assert!(text.contains(&first_path.display().to_string()));
        assert!(text.contains(&second_path.display().to_string()));
        assert!(text.contains("read-only review snapshot"));
        assert!(text.contains("`Esc S` saves all dirty file-backed buffers"));
        assert_eq!(
            app.status_message(),
            Some("Dirty buffers opened as a read-only IDE view")
        );
        assert_eq!(
            app.workspace
                .editor_by_id(second_id)
                .unwrap()
                .document
                .text(),
            " dirtysecond\n"
        );
    }

    #[test]
    fn dirty_buffers_view_reports_clean_workspace() {
        let mut app = app_with_text("clean\n");

        app.execute_ex_command(ExCommand::DirtyBuffers);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Dirty Buffers"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Open buffers: 1  dirty: 0"));
        assert!(text.contains("No dirty buffers."));
    }

    #[test]
    fn dirty_buffer_navigation_cycles_only_unsaved_buffers() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        let third_path = directory.path().join("third.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        std::fs::write(&third_path, "third\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        workspace.open(&second_path).unwrap();
        workspace
            .active_mut()
            .insert("dirty ", EditKind::Insert)
            .unwrap();
        workspace.open(&third_path).unwrap();
        workspace
            .active_mut()
            .insert("dirty ", EditKind::Insert)
            .unwrap();
        workspace.activate(0);
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::NextDirtyBuffer);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second_path.as_path())
        );
        assert_eq!(app.status_message(), Some("Next dirty buffer: second.txt"));

        app.execute_action(Action::NextDirtyBuffer);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(third_path.as_path())
        );

        app.execute_action(Action::NextDirtyBuffer);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second_path.as_path())
        );

        app.execute_action(Action::PreviousDirtyBuffer);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(third_path.as_path())
        );
        assert_eq!(
            app.status_message(),
            Some("Previous dirty buffer: third.txt")
        );
    }

    #[test]
    fn dirty_buffer_navigation_reports_empty_and_singleton_states() {
        let mut app = app_with_text("clean\n");

        app.execute_action(Action::NextDirtyBuffer);
        assert_eq!(app.status_message(), Some("No dirty buffers"));

        app.workspace
            .active_mut()
            .insert("dirty ", EditKind::Insert)
            .unwrap();
        app.execute_action(Action::PreviousDirtyBuffer);
        assert_eq!(app.status_message(), Some("Only current buffer is dirty"));
    }

    #[test]
    fn close_other_buffers_closes_clean_neighbors_and_keeps_active_buffer() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        let third_path = directory.path().join("third.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        std::fs::write(&third_path, "third\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        workspace.open(&second_path).unwrap();
        let second_id = workspace.active().id();
        workspace.open(&third_path).unwrap();
        let active_id = workspace.active().id();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CloseOtherBuffers);

        assert_eq!(app.workspace.len(), 1);
        assert_eq!(app.workspace.active().id(), active_id);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(third_path.as_path())
        );
        assert!(app.workspace.editor_by_id(second_id).is_none());
        assert_eq!(app.status_message(), Some("Closed 2 other buffers"));
        assert!(!app.status_is_error());
    }

    #[test]
    fn close_other_buffers_refuses_dirty_neighbor_without_closing_anything() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let first_id = workspace.active().id();
        workspace
            .active_mut()
            .insert("dirty ", EditKind::Insert)
            .unwrap();
        workspace.open(&second_path).unwrap();
        let active_id = workspace.active().id();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CloseOtherBuffers);

        assert_eq!(app.workspace.len(), 2);
        assert_eq!(app.workspace.active().id(), active_id);
        assert!(
            app.workspace
                .editor_by_id(first_id)
                .unwrap()
                .document
                .is_modified()
        );
        assert_eq!(
            app.status_message(),
            Some("other buffers have unsaved changes")
        );
        assert!(app.status_is_error());
    }

    #[test]
    fn close_other_buffers_reports_singleton_noop_and_ex_command_route() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        let workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CloseOtherBuffers);
        assert_eq!(app.workspace.len(), 1);
        assert_eq!(app.status_message(), Some("No other buffers to close"));
        assert!(!app.status_is_error());

        app.workspace.open(&second_path).unwrap();
        let active_id = app.workspace.active().id();
        app.execute_ex_command(ExCommand::CloseOtherBuffers);

        assert_eq!(app.workspace.len(), 1);
        assert_eq!(app.workspace.active().id(), active_id);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second_path.as_path())
        );
        assert_eq!(app.status_message(), Some("Closed 1 other buffer"));
        assert!(!app.status_is_error());
    }

    #[test]
    fn reopen_closed_buffer_restores_file_cursor_selection_and_viewport() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second line\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        workspace.open(&second_path).unwrap();
        {
            let mut editor = workspace.active_mut();
            editor.cursor = 6;
            editor.anchor = Some(0);
            editor.viewport.top_line = 3;
            editor.viewport.top_wrap_char = 2;
            editor.viewport.left_column = 4;
        }
        let second_id = workspace.active().id();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CloseBuffer);
        assert_eq!(app.workspace.len(), 1);
        assert!(app.workspace.editor_by_id(second_id).is_none());

        app.execute_action(Action::ReopenClosedBuffer);

        assert_eq!(app.workspace.len(), 2);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second_path.as_path())
        );
        assert_eq!(app.workspace.active().cursor, 6);
        assert_eq!(app.workspace.active().anchor, Some(0));
        assert_eq!(app.workspace.active().viewport.top_line, 3);
        assert_eq!(app.workspace.active().viewport.top_wrap_char, 2);
        assert_eq!(app.workspace.active().viewport.left_column, 4);
        assert_eq!(
            app.status_message(),
            Some("Reopened closed buffer: second.txt")
        );
        assert!(!app.status_is_error());
    }

    #[test]
    fn reopen_closed_buffer_refuses_missing_file_without_switching_source() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        workspace.open(&second_path).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CloseBuffer);
        std::fs::remove_file(&second_path).unwrap();
        let active_id = app.workspace.active().id();
        app.execute_action(Action::ReopenClosedBuffer);

        assert_eq!(app.workspace.len(), 1);
        assert_eq!(app.workspace.active().id(), active_id);
        let status = app.status_message().unwrap();
        assert!(status.starts_with("Closed buffer is missing: "));
        assert!(status.ends_with("second.txt"));
        assert!(app.status_is_error());
    }

    #[test]
    fn reopen_closed_buffer_uses_close_others_history_and_ex_command_route() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        let third_path = directory.path().join("third.txt");
        std::fs::write(&first_path, "first\n").unwrap();
        std::fs::write(&second_path, "second\n").unwrap();
        std::fs::write(&third_path, "third\n").unwrap();
        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        workspace.open(&second_path).unwrap();
        workspace.open(&third_path).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CloseOtherBuffers);
        assert_eq!(app.workspace.len(), 1);

        app.execute_ex_command(ExCommand::ReopenClosedBuffer);

        assert_eq!(app.workspace.len(), 2);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second_path.as_path())
        );
        assert_eq!(
            app.status_message(),
            Some("Reopened closed buffer: second.txt")
        );
    }

    #[test]
    fn reopen_closed_buffer_reports_empty_stack() {
        let mut app = app_with_text("clean\n");

        app.execute_action(Action::ReopenClosedBuffer);

        assert_eq!(app.status_message(), Some("No closed file-backed buffers"));
        assert!(!app.status_is_error());
    }

    #[test]
    fn recent_files_view_lists_open_restored_and_missing_paths_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let restored = directory.path().join("restored.rs");
        let missing = directory.path().join("missing.rs");
        std::fs::write(&active, "fn active() {}\n").unwrap();
        std::fs::write(&restored, "fn restored() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.apply_session_recent_files(vec![restored.clone(), missing.clone()]);

        app.execute_action(Action::RecentFiles);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Recent Files"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Recent Files\n\n"));
        assert!(text.contains("read-only session/navigation snapshot"));
        assert!(text.contains(&active.display().to_string()));
        assert!(text.contains(&restored.display().to_string()));
        assert!(text.contains(&missing.display().to_string()));
        assert!(text.contains("file"));
        assert!(text.contains("missing"));
        assert!(text.contains("`:e PATH` opens a listed path"));
        assert_eq!(
            app.status_message(),
            Some("Recent files opened as a read-only IDE view")
        );
    }

    #[test]
    fn open_recent_picker_filters_and_opens_retained_file_path() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let restored = directory.path().join("restored.rs");
        std::fs::write(&active, "fn active() {}\n").unwrap();
        std::fs::write(&restored, "fn restored() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.apply_session_recent_files(vec![restored.clone()]);

        app.execute_action(Action::OpenRecentFile);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "OPEN RECENT");
        assert_eq!(overlay.items.len(), 2);
        assert!(overlay.items[0].contains(&restored.display().to_string()));
        assert!(overlay.items[1].contains(&active.display().to_string()));
        for character in "restored.rs".chars() {
            app.handle_event(key(KeyCode::Char(character)));
        }
        let overlay = app.overlay().unwrap();
        assert!(overlay.items[0].contains(&restored.display().to_string()));

        app.handle_event(key(KeyCode::Enter));

        assert_eq!(
            app.workspace.active().document.path(),
            Some(restored.as_path())
        );
        assert_eq!(app.workspace.active().document.text(), "fn restored() {}\n");
        assert_eq!(app.persistence.recent_files[0], restored);
        assert_eq!(app.status_message(), None);
    }

    #[test]
    fn open_recent_picker_reports_missing_retained_path_without_switching_source() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let missing = directory.path().join("missing.rs");
        std::fs::write(&active, "fn active() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.apply_session_recent_files(vec![missing.clone()]);

        app.execute_ex_command(ExCommand::OpenRecentFile);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "OPEN RECENT");
        assert!(overlay.items[0].contains("missing"));
        app.handle_event(key(KeyCode::Enter));

        assert_eq!(
            app.workspace.active().document.path(),
            Some(active.as_path())
        );
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("No such file or directory")
        );
    }

    #[test]
    fn jump_list_picker_restores_history_across_file_buffers() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        std::fs::write(&first, "one\norigin\n").unwrap();
        std::fs::write(&second, "two\nactive\n").unwrap();
        let workspace =
            Workspace::from_path(Some(first.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().goto_line(2);
        let origin = app.current_jump_location();
        app.workspace.open(second.clone()).unwrap();
        app.workspace.active_mut().goto_line(2);
        app.record_jump_origin(origin);

        app.execute_action(Action::JumpList);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "JUMP LIST");
        assert!(overlay.items[0].contains("older 1"));
        assert!(overlay.items[0].contains(&first.display().to_string()));
        assert!(overlay.items[1].contains("current"));
        app.handle_event(key(KeyCode::Enter));

        assert_eq!(
            app.workspace.active().document.path(),
            Some(first.as_path())
        );
        assert_eq!(
            app.workspace
                .active()
                .document
                .char_to_line(app.workspace.active().cursor),
            1
        );
        assert_eq!(
            app.status_message(),
            Some("Jumped to selected history location")
        );
    }

    #[test]
    fn jump_list_picker_reports_stale_closed_path_without_switching_source() {
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.rs");
        let missing = directory.path().join("missing.rs");
        std::fs::write(&active, "fn active() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.ui.jump_back.push(JumpLocation {
            editor_id: u64::MAX,
            path: Some(missing.clone()),
            cursor: 0,
        });

        app.execute_ex_command(ExCommand::JumpList);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "JUMP LIST");
        assert!(overlay.items[0].contains("missing.rs"));
        app.handle_event(key(KeyCode::Enter));

        assert_eq!(
            app.workspace.active().document.path(),
            Some(active.as_path())
        );
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("Could not restore jump: target is unavailable")
        );
    }

    #[test]
    fn bookmark_picker_restores_marked_source_locations() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        std::fs::write(&first, "one\nbookmarked\n").unwrap();
        std::fs::write(&second, "two\nactive\n").unwrap();
        let workspace =
            Workspace::from_path(Some(first.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().goto_line(2);

        app.execute_action(Action::ToggleBookmark);
        assert_eq!(app.status_message(), Some("Bookmark added"));
        app.workspace.open(second.clone()).unwrap();
        app.workspace.active_mut().goto_line(2);
        app.execute_action(Action::Bookmarks);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "BOOKMARKS");
        assert!(overlay.items[0].contains("bookmark 1"));
        assert!(overlay.items[0].contains(&first.display().to_string()));
        for character in "first".chars() {
            app.handle_event(key(KeyCode::Char(character)));
        }
        assert_eq!(app.overlay().unwrap().items.len(), 1);
        app.handle_event(key(KeyCode::Enter));

        assert_eq!(
            app.workspace.active().document.path(),
            Some(first.as_path())
        );
        assert_eq!(
            app.workspace
                .active()
                .document
                .char_to_line(app.workspace.active().cursor),
            1
        );
        assert_eq!(app.status_message(), Some("Jumped to bookmark"));
        assert_eq!(
            app.ui.jump_back.last().and_then(|jump| jump.path.as_ref()),
            Some(&second)
        );
    }

    #[test]
    fn bookmark_navigation_cycles_without_opening_picker() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        let third = directory.path().join("third.rs");
        std::fs::write(&first, "first\nmark\n").unwrap();
        std::fs::write(&second, "second\nmark\n").unwrap();
        std::fs::write(&third, "third\nactive\n").unwrap();
        let workspace =
            Workspace::from_path(Some(first.clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().goto_line(2);
        app.execute_action(Action::ToggleBookmark);
        app.workspace.open(second.clone()).unwrap();
        app.workspace.active_mut().goto_line(2);
        app.execute_action(Action::ToggleBookmark);
        app.workspace.open(third.clone()).unwrap();

        app.execute_action(Action::NextBookmark);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(first.as_path())
        );
        assert_eq!(app.status_message(), Some("Jumped to next bookmark"));

        app.execute_action(Action::NextBookmark);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second.as_path())
        );

        app.execute_action(Action::NextBookmark);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(first.as_path())
        );

        app.execute_action(Action::PreviousBookmark);
        assert_eq!(
            app.workspace.active().document.path(),
            Some(second.as_path())
        );
        assert_eq!(app.status_message(), Some("Jumped to previous bookmark"));
    }

    #[test]
    fn bookmark_navigation_reports_empty_and_current_singleton_states() {
        let mut app = app_with_text("one\ntwo\n");

        app.execute_action(Action::NextBookmark);
        assert_eq!(app.status_message(), Some("No bookmarks"));

        app.execute_action(Action::ToggleBookmark);
        app.execute_action(Action::PreviousBookmark);
        assert_eq!(app.status_message(), Some("Only current bookmark"));
    }

    #[test]
    fn bookmarks_toggle_remove_empty_and_refuse_read_only_views() {
        let mut app = app_with_text("one\ntwo\n");

        app.execute_action(Action::Bookmarks);
        assert_eq!(app.status_message(), Some("No bookmarks"));

        app.execute_action(Action::ToggleBookmark);
        assert_eq!(app.ui.bookmarks.len(), 1);
        app.execute_action(Action::ToggleBookmark);
        assert!(app.ui.bookmarks.is_empty());
        assert_eq!(app.status_message(), Some("Bookmark removed"));

        app.workspace.open_virtual("Read Only", "context");
        app.execute_action(Action::ToggleBookmark);
        assert!(app.ui.bookmarks.is_empty());
        assert_eq!(
            app.status_message(),
            Some("Bookmarks are for source buffers")
        );
    }

    #[test]
    fn keymap_reference_is_generated_from_all_registered_commands() {
        let mut app = app_with_text("keys\n");

        app.execute_action(Action::KeymapReference);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Keymap Reference"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("generated from the authoritative command registry"));
        assert!(text.contains("Core\n"));
        assert!(text.contains("Workspace\n"));
        assert!(text.contains("Code\n"));
        assert!(text.contains("Tasks\n"));
        assert!(text.contains("Version Control\n"));
        for command in keymap::COMMANDS {
            assert!(
                text.contains(command.id),
                "missing command id {} from keymap reference",
                command.id
            );
            assert!(
                text.contains(command.title),
                "missing command title {} from keymap reference",
                command.title
            );
        }
        let expected_status = format!(
            "{} command shortcut(s) opened read-only",
            keymap::COMMANDS.len()
        );
        assert_eq!(app.status_message(), Some(expected_status.as_str()));
    }

    #[test]
    fn keymap_reference_command_aliases_open_generated_view() {
        let mut app = app_with_text("keys\n");

        app.execute_ex_command(ExCommand::KeymapReference);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Keymap Reference"
        );
        assert!(app.workspace.active().document.text().contains(":keymap"));
    }

    #[test]
    fn current_file_git_status_opens_read_only_active_file_state() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("tracked.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("tracked.rs")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        std::fs::write(&path, "fn main() { println!(\"changed\"); }\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::CurrentFileStatus);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Git File Status"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Git File Status\n\n"));
        assert!(text.contains("Repo path: tracked.rs"));
        assert!(text.contains("Index: · (unmodified)"));
        assert!(text.contains("Worktree: M (modified)"));
        assert!(text.contains("Entry kind: ordinary"));
        assert!(text.contains("Working diff bytes: "));
        assert!(text.contains("`Esc v d` opens the staged/working patch"));
        assert_eq!(
            app.status_message(),
            Some("Current file Git status opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_log_opens_bounded_read_only_history_view() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("history.rs");
        for message in ["base", "feature"] {
            std::fs::write(&path, format!("pub fn {message}() {{}}\n")).unwrap();
            assert!(test_git(
                directory.path(),
                [
                    OsStr::new("add"),
                    OsStr::new("--"),
                    OsStr::new("history.rs")
                ]
            ));
            assert!(test_git(
                directory.path(),
                [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new(message)]
            ));
        }
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::GitLog);

        assert_eq!(app.workspace.active().document.display_name(), "Git Log");
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Git Log — master") || text.contains("Git Log — main"));
        assert!(text.contains("Recent commits (newest first, max 100)"));
        assert!(text.contains("feature"));
        assert!(text.contains("base"));
        assert!(text.find("feature").unwrap() < text.find("base").unwrap());
        assert!(text.contains("Branch switching, pull, and push are not implemented here."));
        assert_eq!(
            app.status_message(),
            Some("Git log opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_log_reports_unborn_repository_without_running_log() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitLog);

        assert_eq!(app.workspace.active().document.display_name(), "Git Log");
        let text = app.workspace.active().document.text();
        assert!(text.contains("State: unborn branch (no commits yet)"));
        assert!(text.contains("No commits yet."));
        assert_eq!(
            app.status_message(),
            Some("Git log opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_commit_picker_filters_and_opens_selected_commit() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("commits.rs");
        for message in ["zzbase commit", "feature commit"] {
            std::fs::write(
                &path,
                format!("pub fn {}() {{}}\n", message.replace(' ', "_")),
            )
            .unwrap();
            assert!(test_git(
                directory.path(),
                [
                    OsStr::new("add"),
                    OsStr::new("--"),
                    OsStr::new("commits.rs")
                ]
            ));
            assert!(test_git(
                directory.path(),
                [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new(message)]
            ));
        }
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::GitCommitPicker);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "GIT COMMITS");
        assert_eq!(overlay.items.len(), 2);
        assert!(overlay.items[0].contains("feature commit"));
        assert!(overlay.items[1].contains("zzbase commit"));
        assert!(
            overlay
                .notice
                .unwrap()
                .contains("Enter opens selected commit")
        );

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("commit picker missing");
        };
        prompt.input = "zzbase".to_owned();
        app.prompt_changed();
        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.items.len(), 1);
        assert!(overlay.items[0].contains("zzbase commit"));

        app.commit_prompt();

        assert_eq!(app.workspace.active().document.display_name(), "Git Commit");
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("zzbase commit"));
        assert!(text.contains("commits.rs"));
        assert!(!text.contains("feature commit"));
        assert_eq!(
            app.status_message(),
            Some("Git commit opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_commit_picker_reports_unborn_repository_without_prompt() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitCommitPicker);

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(app.status_message(), Some("No commits yet"));
    }

    #[test]
    fn git_file_history_opens_read_only_current_file_history_view() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("history.rs");
        let other = directory.path().join("other.rs");
        std::fs::write(&path, "pub fn base() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("history.rs")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("file base")
            ]
        ));
        std::fs::write(&other, "pub fn other() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("other.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("other file")
            ]
        ));
        std::fs::write(&path, "pub fn updated() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("history.rs")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("file update")
            ]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::GitFileHistory);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Git File History"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Git File History — history.rs"));
        assert!(text.contains("Recent commits touching this file (newest first, max 100)"));
        assert!(text.contains("file update"));
        assert!(text.contains("file base"));
        assert!(!text.contains("other file"));
        assert!(text.contains("This view is read-only and never mutates Git state."));
        assert_eq!(
            app.status_message(),
            Some("Git file history opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_file_history_refuses_virtual_buffers_before_running_git() {
        let mut app = app_with_text("scratch\n");

        app.execute_ex_command(ExCommand::GitFileHistory);

        assert!(app.status_is_error());
        assert_eq!(
            app.status_message(),
            Some("Open a file-backed buffer before requesting Git file history")
        );
    }

    #[test]
    fn git_head_opens_read_only_current_commit_view() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("head.rs");
        std::fs::write(&path, "pub fn head() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("head.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("inspect head")
            ]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::GitHead);

        assert_eq!(app.workspace.active().document.display_name(), "Git HEAD");
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Git HEAD — master") || text.contains("Git HEAD — main"));
        assert!(text.contains("Current HEAD commit"));
        assert!(text.contains("inspect head"));
        assert!(text.contains("head.rs"));
        assert!(text.contains("+pub fn head() {}"));
        assert!(text.contains("This view is read-only and never mutates Git state."));
        assert_eq!(
            app.status_message(),
            Some("Git HEAD opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_head_reports_unborn_repository_without_running_show() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitHead);

        assert_eq!(app.workspace.active().document.display_name(), "Git HEAD");
        let text = app.workspace.active().document.text();
        assert!(text.contains("State: unborn branch (no HEAD commit yet)"));
        assert!(text.contains("No HEAD commit yet."));
        assert_eq!(
            app.status_message(),
            Some("Git HEAD opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_commit_info_opens_read_only_explicit_commit_view() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("commit.rs");
        std::fs::write(&path, "pub fn commit_info() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("commit.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("inspect explicit commit")
            ]
        ));
        let rev_parse = Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(rev_parse.status.success());
        let commit = String::from_utf8_lossy(&rev_parse.stdout).trim().to_owned();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitCommitInfo(commit.clone()));

        assert_eq!(app.workspace.active().document.display_name(), "Git Commit");
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Git Commit — "));
        assert!(text.contains(&format!("Requested commit: {commit}")));
        assert!(text.contains("inspect explicit commit"));
        assert!(text.contains("commit.rs"));
        assert!(text.contains("+pub fn commit_info() {}"));
        assert!(text.contains("This view is read-only and never mutates Git state."));
        assert_eq!(
            app.status_message(),
            Some("Git commit opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_commit_info_rejects_symbolic_or_option_like_revisions() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitCommitInfo("--stat".to_owned()));

        assert!(app.status_is_error());
        assert_eq!(
            app.status_message(),
            Some("commit id must be 4 to 64 hexadecimal characters: --stat")
        );

        app.execute_ex_command(ExCommand::GitCommitInfo("HEAD".to_owned()));

        assert!(app.status_is_error());
        assert_eq!(
            app.status_message(),
            Some("commit id must be 4 to 64 hexadecimal characters: HEAD")
        );
    }

    #[test]
    fn git_blame_opens_read_only_current_line_view() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("blame.rs");
        std::fs::write(&path, "pub fn first() {}\npub fn second() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("blame.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("blame base")
            ]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.workspace.active_mut().goto_line(2);

        app.execute_action(Action::GitBlameLine);

        assert_eq!(app.workspace.active().document.display_name(), "Git Blame");
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Git Blame — blame.rs:2"));
        assert!(text.contains("Summary: blame base"));
        assert!(text.contains("Author: w app test"));
        assert!(text.contains("Original line: 2"));
        assert!(text.contains("Current line: 2"));
        assert!(text.contains("pub fn second() {}"));
        assert_eq!(
            app.status_message(),
            Some("Git blame opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_blame_refuses_virtual_buffers_before_running_git() {
        let mut app = app_with_text("scratch\n");

        app.execute_ex_command(ExCommand::GitBlameLine);

        assert!(app.status_is_error());
        assert_eq!(
            app.status_message(),
            Some("Open a file-backed buffer before requesting Git blame")
        );
    }

    #[test]
    fn file_status_command_reports_clean_tracked_file() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("clean.rs");
        std::fs::write(&path, "pub fn clean() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("clean.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitFileStatus);

        let text = app.workspace.active().document.text();
        assert!(text.contains("Repo path: clean.rs"));
        assert!(text.contains("Index: · (clean or tracked-clean)"));
        assert!(text.contains("Worktree: · (clean or tracked-clean)"));
        assert!(text.contains("Staged diff bytes: 0"));
        assert!(text.contains("Working diff bytes: 0"));
    }

    #[test]
    fn git_changes_picker_filters_and_opens_changed_file() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let base = directory.path().join("base.rs");
        let changed = directory.path().join("changed.rs");
        std::fs::write(&base, "pub fn base() {}\n").unwrap();
        std::fs::write(&changed, "pub fn changed() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("base.rs"),
                OsStr::new("changed.rs")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        std::fs::write(&changed, "pub fn changed_again() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(base.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::GitChanges);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "GIT CHANGES");
        assert_eq!(overlay.items.len(), 1);
        assert!(overlay.items[0].contains("changed.rs"));
        assert!(
            overlay
                .notice
                .unwrap()
                .contains("Enter opens selected path")
        );

        app.commit_prompt();

        assert_eq!(
            app.workspace
                .active()
                .document
                .path()
                .unwrap()
                .canonicalize()
                .unwrap(),
            changed.canonicalize().unwrap()
        );
        assert!(
            app.workspace
                .active()
                .document
                .text()
                .contains("changed_again")
        );
        assert_eq!(app.status_message(), None);
    }

    #[test]
    fn git_changes_picker_reports_clean_worktree_without_prompt() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("clean.rs");
        std::fs::write(&path, "pub fn clean() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("clean.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitChanges);

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(app.status_message(), Some("Working tree clean"));
    }

    #[test]
    fn git_diff_picker_filters_and_opens_changed_file_diff() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let base = directory.path().join("base.rs");
        let changed = directory.path().join("review.rs");
        std::fs::write(&base, "pub fn base() {}\n").unwrap();
        std::fs::write(&changed, "pub fn review() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("base.rs"),
                OsStr::new("review.rs")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        std::fs::write(&changed, "pub fn review_again() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(base.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::GitDiffPicker);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "GIT DIFFS");
        assert_eq!(overlay.items.len(), 1);
        assert!(overlay.items[0].contains("review.rs"));
        assert!(
            overlay
                .notice
                .unwrap()
                .contains("Enter opens selected diff")
        );

        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("diff picker missing");
        };
        prompt.input = "review".to_owned();
        app.prompt_changed();

        app.commit_prompt();

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Diff: review.rs"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("# Working-tree changes"));
        assert!(text.contains("-pub fn review() {}"));
        assert!(text.contains("+pub fn review_again() {}"));
        assert_eq!(
            app.status_message(),
            Some("Diff opened as a read-only IDE view")
        );
    }

    #[test]
    fn git_diff_picker_reports_clean_worktree_without_prompt() {
        if !test_git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !test_git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w app test")
            ]
        ));
        assert!(test_git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-app@example.invalid")
            ]
        ));
        let path = directory.path().join("clean.rs");
        std::fs::write(&path, "pub fn clean() {}\n").unwrap();
        assert!(test_git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("clean.rs")]
        ));
        assert!(test_git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::GitDiffPicker);

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(app.status_message(), Some("Working tree clean"));
    }

    #[test]
    fn task_trust_can_open_read_only_task_details_without_running() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::create_dir_all(directory.path().join("subdir")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.inspect]
argv = ["printf", "$(touch SHOULD_NOT_EXIST)\n"]
cwd = "subdir"
env = { TASK_MODE = "preview" }
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.request_task("inspect".to_owned());
        assert!(matches!(&app.ui.mode, UiMode::TaskTrust(name) if name == "inspect"));
        app.handle_task_trust_key(
            "inspect".to_owned(),
            KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE),
        );

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert!(app.tasks.handle.is_none());
        assert!(!directory.path().join("SHOULD_NOT_EXIST").exists());
        assert_eq!(
            app.workspace.active().document.display_name(),
            "Task Details: inspect"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Task Details: inspect"));
        assert!(text.contains("running still requires one-time trust"));
        assert!(text.contains("no shell is inserted or reparsed"));
        assert!(text.contains("[0] \"printf\""));
        assert!(text.contains("[1] \"$(touch SHOULD_NOT_EXIST)\\n\""));
        assert!(text.contains("configured: subdir"));
        assert!(text.contains("TASK_MODE=\"preview\""));
        assert_eq!(
            app.status_message(),
            Some("Task \"inspect\" details opened read-only; no task was started")
        );
    }

    #[test]
    fn task_catalog_lists_configured_tasks_without_running() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.check]
argv = ["cargo", "check"]
cwd = "."
env = { CARGO_TERM_COLOR = "never" }

[tasks.literal]
argv = ["printf", "$(touch SHOULD_NOT_EXIST)\n"]
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::TaskCatalog);

        assert!(app.tasks.handle.is_none());
        assert!(!directory.path().join("SHOULD_NOT_EXIST").exists());
        assert_eq!(
            app.workspace.active().document.display_name(),
            "Task Catalog"
        );
        assert!(app.workspace.active().document.is_read_only());
        let text = app.workspace.active().document.text();
        assert!(text.contains("Configured tasks: 2"));
        assert!(text.contains("check\n"));
        assert!(text.contains("command: \"cargo\""));
        assert!(text.contains("env overrides: 1"));
        assert!(text.contains("details: :task-info check"));
        assert!(text.contains("literal\n"));
        assert!(text.contains("argv is executed directly without an inserted shell"));
        assert_eq!(
            app.status_message(),
            Some("2 configured task(s) opened read-only; no task was started")
        );
    }

    #[test]
    fn task_info_command_opens_configured_task_details() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.check]
argv = ["cargo", "check"]
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::TaskInfo("check".to_owned()));

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Task Details: check"
        );
        assert!(app.workspace.active().document.is_read_only());
        assert!(
            app.workspace
                .active()
                .document
                .text()
                .contains("[1] \"check\"")
        );
    }

    #[test]
    fn task_catalog_command_opens_configured_task_list() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.test]
argv = ["cargo", "test"]
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::TaskCatalog);

        assert_eq!(
            app.workspace.active().document.display_name(),
            "Task Catalog"
        );
        assert!(app.workspace.active().document.is_read_only());
        assert!(app.workspace.active().document.text().contains("test\n"));
    }

    #[test]
    fn default_task_prefers_conventional_check_without_running() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.zeta]
argv = ["printf", "zeta\n"]

[tasks.check]
argv = ["printf", "check\n"]
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::RunDefaultTask);

        assert!(matches!(&app.ui.mode, UiMode::TaskTrust(name) if name == "check"));
        assert!(app.tasks.handle.is_none());
    }

    #[test]
    fn default_task_uses_single_configured_task_without_running() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.custom]
argv = ["printf", "custom\n"]
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::TaskDefault);

        assert!(matches!(&app.ui.mode, UiMode::TaskTrust(name) if name == "custom"));
        assert!(app.tasks.handle.is_none());
    }

    #[test]
    fn default_task_opens_picker_when_multiple_tasks_have_no_conventional_default() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".wscrpt")).unwrap();
        std::fs::write(
            directory.path().join(".wscrpt/tasks.toml"),
            r#"
version = 1

[tasks.alpha]
argv = ["printf", "alpha\n"]

[tasks.beta]
argv = ["printf", "beta\n"]
"#,
        )
        .unwrap();
        let workspace = Workspace::new(Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_action(Action::RunDefaultTask);

        let overlay = app.overlay().unwrap();
        assert_eq!(overlay.title, "TASKS");
        assert_eq!(overlay.items.len(), 2);
        assert_eq!(
            app.status_message(),
            Some("No conventional default task; choose a task")
        );
        assert!(app.tasks.handle.is_none());
    }

    #[test]
    fn save_all_resumes_across_multiple_untitled_save_as_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();
        workspace
            .active_mut()
            .insert("first", EditKind::Insert)
            .unwrap();
        workspace.new_buffer();
        workspace
            .active_mut()
            .insert("second", EditKind::Insert)
            .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.save_all();
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("first Save As prompt missing");
        };
        prompt.input = "one.txt".to_owned();
        app.commit_prompt();
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("second Save As prompt missing");
        };
        prompt.input = "two.txt".to_owned();
        app.commit_prompt();

        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert_eq!(app.workspace.modified_count(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("two.txt")).unwrap(),
            "second"
        );
    }

    #[test]
    fn close_dirty_untitled_resumes_after_save_as() {
        let dir = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();
        workspace
            .active_mut()
            .insert("keep me", EditKind::Insert)
            .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.ui.mode = UiMode::Confirm(ConfirmKind::CloseBuffer);

        app.handle_confirm_key(
            ConfirmKind::CloseBuffer,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        );
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("Save As prompt missing");
        };
        prompt.input = "kept.txt".to_owned();
        app.commit_prompt();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("kept.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(app.workspace.len(), 1);
        assert!(app.workspace.active().document.path().is_none());
        assert!(!app.workspace.active().document.is_modified());
    }

    #[test]
    fn pathless_write_quit_continues_after_untitled_save_as() {
        let dir = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();
        workspace
            .active_mut()
            .insert("done", EditKind::Insert)
            .unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());

        app.execute_ex_command(ExCommand::SaveQuit(None));
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("Save As prompt missing");
        };
        prompt.input = "done.txt".to_owned();
        app.commit_prompt();

        assert!(app.should_quit());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("done.txt")).unwrap(),
            "done"
        );
    }

    #[test]
    fn snippet_completion_is_refused_without_mutating_the_buffer() {
        let mut app = app_with_text("base");
        let context = track_lsp_edit_context(&mut app, "file:///main.rs", 1);
        let cursor = app.workspace.active().document.len_chars();
        app.workspace.active_mut().set_cursor(cursor, false);
        let state_id = app.workspace.active().document.state_id();

        app.apply_completion(
            CompletionItem {
                label: "snippet".to_owned(),
                detail: None,
                insert_text: "call(${1:value})$0".to_owned(),
                text_edit: None,
                is_snippet: true,
            },
            context,
            cursor,
            None,
        );

        assert_eq!(app.workspace.active().document.text(), "base");
        assert_eq!(app.workspace.active().document.state_id(), state_id);
        assert!(app.status_is_error());
        assert!(app.status_message().unwrap().contains("Snippet completion"));
    }

    #[test]
    fn unanswered_lsp_requests_are_bounded_and_dropped_correlations_clear() {
        let mut app = app_with_text("");
        let diagnostic_uri = "file:///main.rs".to_owned();
        let diagnostic_range = zero_width_lsp_range(0, 0);
        app.lsp.diagnostics.replace(
            diagnostic_uri.clone(),
            vec![Diagnostic {
                uri: diagnostic_uri.clone(),
                range: diagnostic_range,
                severity: DiagnosticSeverity::Warning,
                message: "stale after drop".to_owned(),
                source: None,
                raw: json_object([("message", "stale after drop".into())]),
            }],
        );
        let context = LspDocumentRequestContext {
            editor_id: app.workspace.active().id(),
            uri: "file:///main.rs".to_owned(),
            version: DocumentVersion::INITIAL,
            incarnation: DocumentIncarnation::for_test(1),
            state_id: app.workspace.active().document.state_id(),
        };
        for request_id in 0..MAX_PENDING_LSP_REQUESTS as u64 {
            app.lsp.requests.insert(
                request_id,
                PendingLspRequest::Hover {
                    context: context.clone(),
                },
            );
        }
        assert!(app.lsp_request_context().is_none());
        assert!(
            app.status_message()
                .unwrap()
                .contains("unanswered requests")
        );

        assert!(app.handle_lsp_event(LspEvent::RequestFailed {
            request_id: 0,
            operation: LspOperation::Hover,
            error: crate::lsp_client::JsonRpcError {
                code: -32098,
                message: "malformed JSON-RPC error object".to_owned(),
                data: None,
            },
        }));
        assert_eq!(app.lsp.requests.len(), MAX_PENDING_LSP_REQUESTS - 1);
        app.lsp.requests.insert(
            MAX_PENDING_LSP_REQUESTS as u64,
            PendingLspRequest::Hover {
                context: context.clone(),
            },
        );
        assert_eq!(app.lsp.requests.len(), MAX_PENDING_LSP_REQUESTS);
        app.lsp.server_name = Some("mock".to_owned());
        app.lsp.background_sync_due = true;
        app.lsp.deferred_event = Some(LspEvent::ServerNotification {
            method: "mock/deferred".to_owned(),
            params: None,
        });

        assert!(app.handle_lsp_event(LspEvent::EventsDropped { count: 3 }));
        assert!(app.lsp.requests.is_empty());
        assert!(app.lsp.diagnostics.is_empty());
        assert!(app.lsp.diagnostics.is_partial());
        assert!(app.status_message().unwrap().contains("dropped 3"));
        assert!(app.status_message().unwrap().contains(":lsp-restart"));
        assert_eq!(app.lsp.server_name, None);
        assert_eq!(app.lsp.failed_server.as_deref(), Some("mock"));
        assert!(!app.lsp.background_sync_due);
        assert!(app.lsp.deferred_event.is_none());
    }

    #[test]
    fn edit_after_request_rejects_formatting_responses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "base\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&path);
        let context = track_lsp_edit_context(&mut app, uri.clone(), 7);
        app.lsp.requests.insert(
            41,
            PendingLspRequest::Formatting {
                context: context.clone(),
            },
        );

        app.workspace
            .active_mut()
            .insert("user ", EditKind::Insert)
            .unwrap();
        let newer_text = app.workspace.active().document.text();

        assert!(app.handle_lsp_event(LspEvent::Formatting {
            request_id: 41,
            uri: uri.clone(),
            version: context.version,
            result: crate::lsp_client::JsonValue::Array(vec![zero_width_text_edit_json(
                "formatted ",
            )]),
        }));
        assert_eq!(app.workspace.active().document.text(), newer_text);
        assert!(app.status_message().unwrap().contains("stale formatting"));
    }

    #[test]
    fn edit_after_request_rejects_hover_definition_references_and_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "base\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(dir.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        let uri = file_uri_identity(&path);
        let context = track_lsp_edit_context(&mut app, uri.clone(), 7);
        app.lsp.requests.insert(
            61,
            PendingLspRequest::Hover {
                context: context.clone(),
            },
        );
        app.lsp.requests.insert(
            62,
            PendingLspRequest::Definition {
                context: context.clone(),
            },
        );
        app.lsp.requests.insert(
            63,
            PendingLspRequest::References {
                context: context.clone(),
            },
        );
        app.lsp.requests.insert(
            64,
            PendingLspRequest::DocumentSymbols {
                context: context.clone(),
            },
        );

        app.workspace
            .active_mut()
            .insert("user ", EditKind::Insert)
            .unwrap();
        let editor_id = app.workspace.active().id();
        let newer_text = app.workspace.active().document.text();
        let location = crate::lsp_client::JsonValue::parse(&format!(
            r#"[{{"uri":"{uri}","range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}}}}]"#
        ))
        .unwrap();

        assert!(app.handle_lsp_event(LspEvent::Hover {
            request_id: 61,
            uri: uri.clone(),
            version: context.version,
            result: crate::lsp_client::JsonValue::parse(r#"{"contents":"stale hover"}"#).unwrap(),
        }));
        assert!(app.status_message().unwrap().contains("stale hover"));

        assert!(app.handle_lsp_event(LspEvent::Definition {
            request_id: 62,
            uri: uri.clone(),
            version: context.version,
            result: location.clone(),
        }));
        assert!(app.status_message().unwrap().contains("stale definition"));

        assert!(app.handle_lsp_event(LspEvent::References {
            request_id: 63,
            uri: uri.clone(),
            version: context.version,
            result: location,
        }));
        assert!(app.status_message().unwrap().contains("stale references"));

        assert!(app.handle_lsp_event(LspEvent::DocumentSymbols {
            request_id: 64,
            uri,
            version: context.version,
            result: crate::lsp_client::JsonValue::parse(
                r#"[{"name":"stale","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}]"#,
            )
            .unwrap(),
        }));
        assert!(
            app.status_message()
                .unwrap()
                .contains("stale document symbols")
        );
        assert_eq!(app.workspace.active().id(), editor_id);
        assert_eq!(app.workspace.active().document.text(), newer_text);
        assert_eq!(app.workspace.len(), 1);
        assert!(matches!(app.ui.mode, UiMode::Edit));
    }

    #[test]
    fn completion_prompt_rechecks_request_state_before_commit() {
        let mut app = app_with_text("base");
        let uri = "file:///main.rs".to_owned();
        let context = track_lsp_edit_context(&mut app, uri.clone(), 3);
        let cursor = app.workspace.active().cursor;
        app.lsp.requests.insert(
            51,
            PendingLspRequest::Completion {
                context: context.clone(),
                cursor,
                anchor: app.workspace.active().anchor,
            },
        );
        assert!(
            app.handle_lsp_event(LspEvent::Completion {
                request_id: 51,
                uri,
                version: context.version,
                result: crate::lsp_client::JsonValue::parse(
                    r#"[{"label":"server","insertText":"server"}]"#,
                )
                .unwrap(),
            })
        );
        assert!(matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Completion,
                ..
            })
        ));

        app.workspace
            .active_mut()
            .insert("user ", EditKind::Insert)
            .unwrap();
        let newer_text = app.workspace.active().document.text();
        app.commit_prompt();

        assert_eq!(app.workspace.active().document.text(), newer_text);
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("Completion result expired")
        );
    }

    #[test]
    fn completion_prompt_expires_after_same_document_reopens_with_new_incarnation() {
        let mut app = app_with_text("base");
        let uri = "file:///main.rs".to_owned();
        let context = track_lsp_edit_context(&mut app, uri.clone(), 3);
        let cursor = app.workspace.active().cursor;
        app.lsp.requests.insert(
            53,
            PendingLspRequest::Completion {
                context: context.clone(),
                cursor,
                anchor: app.workspace.active().anchor,
            },
        );
        assert!(
            app.handle_lsp_event(LspEvent::Completion {
                request_id: 53,
                uri,
                version: context.version,
                result: crate::lsp_client::JsonValue::parse(
                    r#"[{"label":"server","insertText":"server"}]"#,
                )
                .unwrap(),
            })
        );
        assert!(matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Completion,
                ..
            })
        ));

        app.lsp.document_incarnations.insert(
            context.editor_id,
            DocumentIncarnation::for_test(context.incarnation.get() + 1),
        );
        app.commit_prompt();

        assert_eq!(app.workspace.active().document.text(), "base");
        assert!(app.status_is_error());
        assert!(
            app.status_message()
                .unwrap()
                .contains("Completion result expired")
        );
    }

    #[test]
    fn completion_response_expires_when_only_the_cursor_moves() {
        let mut app = app_with_text("base");
        let uri = "file:///main.rs".to_owned();
        let context = track_lsp_edit_context(&mut app, uri.clone(), 3);
        let cursor = app.workspace.active().cursor;
        let anchor = app.workspace.active().anchor;
        app.lsp.requests.insert(
            52,
            PendingLspRequest::Completion {
                context: context.clone(),
                cursor,
                anchor,
            },
        );
        app.workspace.active_mut().set_cursor(cursor + 1, false);

        assert!(
            app.handle_lsp_event(LspEvent::Completion {
                request_id: 52,
                uri,
                version: context.version,
                result: crate::lsp_client::JsonValue::parse(
                    r#"[{"label":"server","insertText":"server"}]"#,
                )
                .unwrap(),
            })
        );

        assert_eq!(app.workspace.active().document.text(), "base");
        assert!(matches!(app.ui.mode, UiMode::Edit));
        assert!(app.status_message().unwrap().contains("stale completion"));
    }

    #[test]
    fn unopenable_active_lsp_document_is_quarantined_without_registry_churn() {
        let directory = tempfile::tempdir().unwrap();
        let wire_log = directory.path().join("quarantine-wire.log");
        let small_paths = (0..MAX_SYNCHRONIZED_DOCUMENTS)
            .map(|index| directory.path().join(format!("small-{index:02}.rs")))
            .collect::<Vec<_>>();
        for path in &small_paths {
            std::fs::write(path, "fn small() {}\n").unwrap();
        }
        let mut workspace = Workspace::from_path(
            Some(small_paths[0].clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        for path in small_paths.iter().skip(1) {
            workspace.open(path).unwrap();
        }
        workspace.activate(0);
        let script = r#"
log=$1
: > "$log"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"textDocumentSync\":{\"openClose\":true,\"change\":1,\"save\":false}}}}" ;;
    *'"method":"textDocument/didOpen"'*) printf 'open\n' >> "$log" ;;
    *'"method":"textDocument/didClose"'*) printf 'close\n' >> "$log" ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "quarantine-mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                "--".to_owned(),
                wire_log.to_string_lossy().into_owned(),
            ],
        });
        let mut app = App::new_ready_for_test(workspace, config);
        poll_app_until(&mut app, "full synchronized registry", |app| {
            app.lsp.documents.len() == MAX_SYNCHRONIZED_DOCUMENTS
        });
        let registered_before = app
            .lsp
            .documents
            .iter()
            .map(|document| document.editor_id)
            .collect::<HashSet<_>>();

        let large_path = directory.path().join("too-large-for-frame.rs");
        let large_text = "a".repeat(9 * 1024 * 1024);
        std::fs::write(&large_path, &large_text).unwrap();
        app.workspace.open(&large_path).unwrap();
        let large_id = app.workspace.active().id();
        let large_state = app.workspace.active().document.state_id();
        let mut first_budget = LspSyncBudget::new();
        assert!(app.sync_lsp_document_with_budget(&mut first_budget));
        assert_eq!(first_budget.remaining_documents, 7);
        assert_eq!(
            first_budget.remaining_bytes,
            MAX_LSP_TEXT_BYTES_PER_SYNC_POLL - large_text.len()
        );
        assert_eq!(app.lsp.quarantined_documents.len(), 1);
        assert_eq!(app.lsp.quarantined_documents[0].editor_id, large_id);
        assert_eq!(app.lsp.quarantined_documents[0].state_id, large_state);
        assert_eq!(
            app.lsp
                .documents
                .iter()
                .map(|document| document.editor_id)
                .collect::<HashSet<_>>(),
            registered_before
        );

        let mut retry_budget = LspSyncBudget::new();
        app.sync_lsp_document_with_budget(&mut retry_budget);
        assert_eq!(
            retry_budget.remaining_documents,
            MAX_LSP_TEXT_DOCUMENTS_PER_SYNC_POLL
        );
        assert_eq!(
            retry_budget.remaining_bytes,
            MAX_LSP_TEXT_BYTES_PER_SYNC_POLL
        );

        app.workspace
            .active_mut()
            .insert("b", EditKind::Insert)
            .unwrap();
        let changed_state = app.workspace.active().document.state_id();
        let mut changed_budget = LspSyncBudget::new();
        assert!(app.sync_lsp_document_with_budget(&mut changed_budget));
        assert_eq!(app.lsp.quarantined_documents.len(), 1);
        assert_eq!(app.lsp.quarantined_documents[0].state_id, changed_state);
        assert_eq!(wire_line_count(&wire_log_lines(&wire_log), "close"), 0);

        if let Some(mut client) = app.lsp.client.take() {
            let _ = client.shutdown();
        }
    }

    #[cfg(unix)]
    #[test]
    fn lsp_sync_budget_retries_every_background_without_stale_diagnostics() {
        const DOCUMENT_BYTES: usize = 4 * 1024;
        let directory = tempfile::tempdir().unwrap();
        let paths = (0..MAX_SYNCHRONIZED_DOCUMENTS)
            .map(|index| directory.path().join(format!("fair-{index:02}.rs")))
            .collect::<Vec<_>>();
        let source = "a".repeat(DOCUMENT_BYTES);
        for path in &paths {
            std::fs::write(path, &source).unwrap();
        }
        let mut workspace =
            Workspace::from_path(Some(paths[0].clone()), Some(directory.path().to_path_buf()))
                .unwrap();
        let mut editor_ids = vec![workspace.active().id()];
        for path in paths.iter().skip(1) {
            workspace.open(path).unwrap();
            editor_ids.push(workspace.active().id());
        }
        workspace.activate(0);
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"textDocumentSync\":{\"openClose\":true,\"change\":1,\"save\":false}}}}" ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "fairness-mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        });
        let mut app = App::new_ready_for_test(workspace, config);
        poll_app_until(&mut app, "fairness registry admission", |app| {
            app.lsp.documents.len() == MAX_SYNCHRONIZED_DOCUMENTS
        });

        for (&editor_id, path) in editor_ids.iter().zip(paths.iter()) {
            let uri = file_uri_identity(path);
            app.lsp.diagnostics.replace(
                uri.clone(),
                vec![Diagnostic {
                    uri,
                    range: zero_width_lsp_range(0, 0),
                    severity: DiagnosticSeverity::Warning,
                    message: "stale".to_owned(),
                    source: None,
                    raw: crate::lsp_client::JsonValue::Null,
                }],
            );
            app.workspace
                .editor_by_id_mut(editor_id)
                .unwrap()
                .insert("b", EditKind::Insert)
                .unwrap();
        }
        assert_eq!(app.lsp.diagnostics.len(), MAX_SYNCHRONIZED_DOCUMENTS);

        for _ in 0..20 {
            app.workspace
                .editor_by_id_mut(editor_ids[0])
                .unwrap()
                .insert("c", EditKind::Insert)
                .unwrap();
            let mut budget = LspSyncBudget {
                remaining_documents: MAX_LSP_TEXT_DOCUMENTS_PER_SYNC_POLL,
                remaining_bytes: 5 * (DOCUMENT_BYTES + 64),
            };
            app.sync_lsp_document_with_budget(&mut budget);
        }
        poll_app_until(&mut app, "all fair background retries", |app| {
            editor_ids.iter().all(|editor_id| {
                let editor_state = app
                    .workspace
                    .editor_by_id(*editor_id)
                    .unwrap()
                    .document
                    .state_id();
                app.lsp
                    .documents
                    .get_by_editor_id(*editor_id)
                    .is_some_and(|document| document.state_id == editor_state)
            })
        });

        assert!(app.lsp.diagnostics.is_empty());
        for editor_id in editor_ids {
            let editor_state = app
                .workspace
                .editor_by_id(editor_id)
                .unwrap()
                .document
                .state_id();
            assert_eq!(
                app.lsp
                    .documents
                    .get_by_editor_id(editor_id)
                    .unwrap()
                    .state_id,
                editor_state,
                "editor {editor_id} starved behind the sync cursor"
            );
        }

        if let Some(mut client) = app.lsp.client.take() {
            let _ = client.shutdown();
        }
    }

    #[cfg(unix)]
    #[test]
    fn incremental_only_server_receives_full_document_range_replacements() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("incremental.rs");
        let wire_log = directory.path().join("incremental-wire.log");
        std::fs::write(&path, "one\n🦀").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let script = r#"
log=$1
: > "$log"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"textDocumentSync\":{\"openClose\":true,\"change\":2}}}}" ;;
    *'"method":"textDocument/didChange"'*)
      case "$body" in
        *'"range":{"start":{"line":0,"character":0},"end":{"line":1,"character":2}}'*) printf 'end-astral\n' >> "$log" ;;
        *'"range":{"start":{"line":0,"character":0},"end":{"line":2,"character":0}}'*) printf 'end-trailing-newline\n' >> "$log" ;;
        *) printf 'wrong-range\n' >> "$log" ;;
      esac ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "incremental-mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                "--".to_owned(),
                wire_log.to_string_lossy().into_owned(),
            ],
        });
        let mut app = App::new_ready_for_test(workspace, config);
        let editor_id = app.workspace.active().id();
        poll_app_until(&mut app, "incremental document admission", |app| {
            app.lsp.documents.get_by_editor_id(editor_id).is_some()
        });
        assert!(
            app.lsp
                .text_document_sync
                .is_some_and(|capability| capability.incremental && !capability.full)
        );
        let astral_end = LspPosition::new(Line::new(1), Utf16Offset::new(2));
        assert_eq!(app.lsp.document_ends.get(&editor_id), Some(&astral_end));

        let end = app.workspace.active().document.len_chars();
        app.workspace.active_mut().set_cursor(end, false);
        app.workspace
            .active_mut()
            .insert("\n", EditKind::Insert)
            .unwrap();
        poll_app_until(&mut app, "first incremental replacement", |_| {
            !wire_log_lines(&wire_log).is_empty()
        });
        assert_eq!(
            app.lsp.document_ends.get(&editor_id),
            Some(&LspPosition::new(Line::new(2), Utf16Offset::new(0)))
        );
        app.workspace.active_mut().set_cursor(0, false);
        app.workspace
            .active_mut()
            .insert("prefix ", EditKind::Insert)
            .unwrap();
        poll_app_until(&mut app, "second incremental replacement", |_| {
            wire_log_lines(&wire_log).len() >= 2
        });

        assert_eq!(
            wire_log_lines(&wire_log),
            vec!["end-astral", "end-trailing-newline"]
        );
        assert_eq!(
            app.lsp.document_ends.get(&editor_id),
            Some(&lsp_document_end(&app.workspace.active().document))
        );

        if let Some(mut client) = app.lsp.client.take() {
            let _ = client.shutdown();
            assert!(client.wait_stopped(std::time::Duration::from_secs(1)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn lsp_save_capability_shapes_and_defers_include_text_notifications() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.rs");
        let wire_log = directory.path().join("save-wire.log");
        std::fs::write(&path, "base\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let script = r#"
log=$1
: > "$log"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"textDocumentSync\":{\"openClose\":true,\"change\":1,\"save\":{\"includeText\":true}}}}}" ;;
    *'"method":"textDocument/didChange"'*) printf 'change\n' >> "$log" ;;
    *'"method":"textDocument/didSave"'*)
      case "$body" in *'"text":"changed base'*) printf 'save-text\n' >> "$log" ;; *) printf 'save-missing-text\n' >> "$log" ;; esac ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "save-mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                "--".to_owned(),
                wire_log.to_string_lossy().into_owned(),
            ],
        });
        let mut app = App::new_ready_for_test(workspace, config);
        let editor_id = app.workspace.active().id();
        poll_app_until(&mut app, "save document admission", |app| {
            app.lsp.documents.get_by_editor_id(editor_id).is_some()
        });

        app.workspace
            .active_mut()
            .insert("changed ", EditKind::Insert)
            .unwrap();
        app.workspace.active_mut().document.save().unwrap();
        let current_state = app.workspace.active().document.state_id();
        let current_save_generation = app.workspace.active().document.save_generation();
        let mut change_only_budget = LspSyncBudget {
            remaining_documents: 1,
            remaining_bytes: MAX_LSP_TEXT_BYTES_PER_SYNC_POLL,
        };
        app.sync_lsp_document_with_budget(&mut change_only_budget);
        let synchronized = app.lsp.documents.get_by_editor_id(editor_id).unwrap();
        assert_eq!(synchronized.state_id, current_state);
        assert_ne!(synchronized.save_generation, current_save_generation);

        let mut save_budget = LspSyncBudget {
            remaining_documents: 1,
            remaining_bytes: MAX_LSP_TEXT_BYTES_PER_SYNC_POLL,
        };
        app.sync_lsp_document_with_budget(&mut save_budget);
        assert_eq!(
            app.lsp
                .documents
                .get_by_editor_id(editor_id)
                .unwrap()
                .save_generation,
            current_save_generation
        );
        poll_app_until(&mut app, "ordered include-text save", |_| {
            wire_log_lines(&wire_log).len() >= 2
        });
        assert_eq!(wire_log_lines(&wire_log), vec!["change", "save-text"]);

        app.lsp.text_document_sync.as_mut().unwrap().save = false;
        app.save_current();
        let suppressed_generation = app.workspace.active().document.save_generation();
        app.sync_lsp_document();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(wire_log_lines(&wire_log), vec!["change", "save-text"]);
        assert_eq!(
            app.lsp
                .documents
                .get_by_editor_id(editor_id)
                .unwrap()
                .save_generation,
            suppressed_generation
        );

        let client = app.lsp.client.as_mut().expect("mock client remains live");
        client.shutdown().unwrap();
        assert!(client.wait_stopped(std::time::Duration::from_secs(1)));
        app.lsp.client = None;
    }

    #[cfg(unix)]
    #[test]
    fn clean_initial_reload_resynchronizes_text_without_synthesizing_a_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        let wire_log = directory.path().join("reload-wire.log");
        std::fs::write(&path, "fn before() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(path.clone()), Some(directory.path().to_path_buf())).unwrap();
        let uri = file_uri_identity(&path);
        let script = r#"
log=$1
: > "$log"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"textDocumentSync\":{\"openClose\":true,\"change\":1,\"save\":{\"includeText\":false}}}}}" ;;
    *'"method":"textDocument/didOpen"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      version=$(printf '%s' "$body" | sed -n 's/.*"version":\([0-9][0-9]*\).*/\1/p')
      printf 'open\t%s\t%s\n' "$uri" "$version" >> "$log"
      send_message "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"version\":$version,\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":2}},\"severity\":2,\"message\":\"before reload\"}]}}" ;;
    *'"method":"textDocument/didChange"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      version=$(printf '%s' "$body" | sed -n 's/.*"version":\([0-9][0-9]*\).*/\1/p')
      printf 'change\t%s\t%s\n' "$uri" "$version" >> "$log"
      send_message "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"version\":$version,\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":2}},\"severity\":2,\"message\":\"after reload\"}]}}" ;;
    *'"method":"textDocument/didSave"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      printf 'save\t%s\n' "$uri" >> "$log" ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "reload-mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                "--".to_owned(),
                wire_log.to_string_lossy().into_owned(),
            ],
        });
        let mut app = App::new_ready_for_test(workspace, config);

        poll_app_until(&mut app, "initial synchronized diagnostic", |app| {
            app.lsp
                .documents
                .get_by_uri(&uri)
                .is_some_and(|document| document.version == DocumentVersion::new(1))
                && app.lsp.diagnostics.get(&uri).is_some_and(|diagnostics| {
                    diagnostics
                        .first()
                        .is_some_and(|diagnostic| diagnostic.message == "before reload")
                })
        });
        assert_eq!(app.workspace.active().document.state_id(), 0);

        std::fs::write(&path, "fn externally_reloaded() {}\n").unwrap();
        app.workspace
            .active_mut()
            .document
            .reload_from_disk()
            .unwrap();
        assert_ne!(app.workspace.active().document.state_id(), 0);
        poll_app_until(&mut app, "reloaded document and diagnostic", |app| {
            app.lsp
                .documents
                .get_by_uri(&uri)
                .is_some_and(|document| document.version == DocumentVersion::new(2))
                && app.lsp.diagnostics.get(&uri).is_some_and(|diagnostics| {
                    diagnostics
                        .first()
                        .is_some_and(|diagnostic| diagnostic.message == "after reload")
                })
                && wire_line_count(&wire_log_lines(&wire_log), &format!("change\t{uri}\t2")) == 1
        });
        assert_eq!(
            app.workspace.active().document.text(),
            "fn externally_reloaded() {}\n"
        );
        for _ in 0..20 {
            app.poll_services();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            wire_line_count(&wire_log_lines(&wire_log), &format!("save\t{uri}")),
            0
        );

        let client = app.lsp.client.as_mut().expect("mock client remains live");
        client.shutdown().unwrap();
        assert!(client.wait_stopped(std::time::Duration::from_secs(3)));
        app.lsp.client = None;
    }

    #[cfg(unix)]
    #[test]
    fn live_lsp_preserves_same_server_documents_until_the_buffer_closes() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.rs");
        let second_path = directory.path().join("second.rs");
        let unmapped_path = directory.path().join("notes.txt");
        let wire_log = directory.path().join("lsp-wire.log");
        std::fs::write(&first_path, "fn first() {}\n").unwrap();
        std::fs::write(&second_path, "fn second() {}\n").unwrap();
        std::fs::write(&unmapped_path, "notes\n").unwrap();

        let mut workspace = Workspace::from_path(
            Some(first_path.clone()),
            Some(directory.path().to_path_buf()),
        )
        .unwrap();
        let first_id = workspace.active().id();
        workspace.open(&second_path).unwrap();
        let second_id = workspace.active().id();
        workspace.open(&unmapped_path).unwrap();
        let unmapped_id = workspace.active().id();
        workspace.activate(workspace.editor_index(first_id).unwrap());

        let first_uri = file_uri_identity(&first_path);
        let second_uri = file_uri_identity(&second_path);
        let unmapped_uri = file_uri_identity(&unmapped_path);
        let script = r#"
log=$1
first_uri=$2
second_uri=$3
: > "$log"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"textDocumentSync\":{\"openClose\":true,\"change\":1,\"save\":{\"includeText\":false}}}}}" ;;
    *'"method":"textDocument/didOpen"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      version=$(printf '%s' "$body" | sed -n 's/.*"version":\([0-9][0-9]*\).*/\1/p')
      printf 'open\t%s\t%s\n' "$uri" "$version" >> "$log"
      if [ "$uri" = "$first_uri" ]; then message='first diagnostic'; else message='second diagnostic'; fi
      send_message "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"version\":$version,\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":2}},\"severity\":2,\"message\":\"$message\"}]}}" ;;
    *'"method":"textDocument/didChange"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      version=$(printf '%s' "$body" | sed -n 's/.*"version":\([0-9][0-9]*\).*/\1/p')
      printf 'change\t%s\t%s\n' "$uri" "$version" >> "$log"
      if [ "$uri" = "$first_uri" ]; then message='first diagnostic'; else message='second diagnostic'; fi
      send_message "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"version\":$version,\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":2}},\"severity\":2,\"message\":\"$message\"}]}}" ;;
    *'"method":"textDocument/didSave"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      printf 'save\t%s\n' "$uri" >> "$log" ;;
    *'"method":"textDocument/didClose"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      printf 'close\t%s\n' "$uri" >> "$log" ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf 'shutdown\n' >> "$log"
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*)
      printf 'exit\n' >> "$log"
      exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "multi-buffer-mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                "--".to_owned(),
                wire_log.to_string_lossy().into_owned(),
                first_uri.clone(),
                second_uri.clone(),
            ],
        });
        let mut app = App::new_ready_for_test(workspace, config);

        poll_app_until(&mut app, "both documents and diagnostics", |app| {
            app.lsp.documents.len() == 2
                && app.lsp.diagnostics.get(&first_uri).is_some()
                && app.lsp.diagnostics.get(&second_uri).is_some()
        });
        let open_lines = wire_log_lines(&wire_log);
        assert_eq!(
            wire_line_count(&open_lines, &format!("open\t{first_uri}\t1")),
            1
        );
        assert_eq!(
            wire_line_count(&open_lines, &format!("open\t{second_uri}\t2")),
            1
        );
        assert_eq!(app.lsp.document_ends.len(), 2);
        assert!(app.lsp.document_ends.contains_key(&first_id));
        assert!(app.lsp.document_ends.contains_key(&second_id));
        assert!(!open_lines.iter().any(|line| line.contains(&unmapped_uri)));

        // An explicit save of an already-clean buffer is still an LSP save
        // event; saved_state_id alone cannot distinguish it.
        app.save_current();
        poll_app_until(&mut app, "clean save notification", |app| {
            let editor = app.workspace.editor_by_id(first_id).unwrap();
            app.lsp
                .documents
                .get_by_editor_id(first_id)
                .is_some_and(|document| {
                    document.save_generation == editor.document.save_generation()
                })
                && wire_line_count(&wire_log_lines(&wire_log), &format!("save\t{first_uri}")) == 1
        });

        app.workspace
            .activate(app.workspace.editor_index(second_id).unwrap());
        poll_app_until(&mut app, "second document activation", |app| {
            app.lsp.documents.active_editor_id() == Some(second_id)
        });
        app.workspace
            .activate(app.workspace.editor_index(first_id).unwrap());
        poll_app_until(&mut app, "first document reactivation", |app| {
            app.lsp.documents.active_editor_id() == Some(first_id)
        });

        app.workspace
            .editor_by_id_mut(first_id)
            .unwrap()
            .insert("// first edit\n", EditKind::Insert)
            .unwrap();
        app.workspace
            .editor_by_id_mut(second_id)
            .unwrap()
            .insert("// inactive second edit\n", EditKind::Insert)
            .unwrap();
        let first_state = app
            .workspace
            .editor_by_id(first_id)
            .unwrap()
            .document
            .state_id();
        let second_state = app
            .workspace
            .editor_by_id(second_id)
            .unwrap()
            .document
            .state_id();
        poll_app_until(&mut app, "independent document changes", |app| {
            app.lsp
                .documents
                .get_by_editor_id(first_id)
                .is_some_and(|document| {
                    document.version == DocumentVersion::new(3) && document.state_id == first_state
                })
                && app
                    .lsp
                    .documents
                    .get_by_editor_id(second_id)
                    .is_some_and(|document| {
                        document.version == DocumentVersion::new(4)
                            && document.state_id == second_state
                    })
        });
        poll_app_until(&mut app, "versioned changes on the wire", |_| {
            let lines = wire_log_lines(&wire_log);
            wire_line_count(&lines, &format!("change\t{first_uri}\t3")) == 1
                && wire_line_count(&lines, &format!("change\t{second_uri}\t4")) == 1
        });

        app.save_all();
        poll_app_until(&mut app, "both saves", |app| {
            [first_id, second_id].into_iter().all(|editor_id| {
                let editor = app.workspace.editor_by_id(editor_id).unwrap();
                app.lsp
                    .documents
                    .get_by_editor_id(editor_id)
                    .is_some_and(|document| {
                        document.saved_state_id == Some(editor.document.state_id())
                            && document.save_generation == editor.document.save_generation()
                    })
            })
        });
        poll_app_until(&mut app, "save notifications on the wire", |_| {
            let lines = wire_log_lines(&wire_log);
            wire_line_count(&lines, &format!("save\t{first_uri}")) == 2
                && wire_line_count(&lines, &format!("save\t{second_uri}")) == 1
        });

        std::fs::write(&first_path, "external replacement\n").unwrap();
        app.workspace
            .activate(app.workspace.editor_index(first_id).unwrap());
        app.execute_ex_command(ExCommand::SaveForce);
        poll_app_until(&mut app, "force-save notification", |app| {
            let editor = app.workspace.editor_by_id(first_id).unwrap();
            app.lsp
                .documents
                .get_by_editor_id(first_id)
                .is_some_and(|document| {
                    document.save_generation == editor.document.save_generation()
                })
                && wire_line_count(&wire_log_lines(&wire_log), &format!("save\t{first_uri}")) == 3
        });

        app.workspace
            .activate(app.workspace.editor_index(second_id).unwrap());
        poll_app_until(&mut app, "second document before Problems", |app| {
            app.lsp.documents.active_editor_id() == Some(second_id)
        });
        app.open_problems();
        let UiMode::Prompt(prompt) = &app.ui.mode else {
            panic!("Problems prompt missing");
        };
        assert!(
            prompt
                .labels
                .iter()
                .any(|label| label.contains("first diagnostic"))
        );
        assert!(
            prompt
                .labels
                .iter()
                .any(|label| label.contains("second diagnostic"))
        );
        app.ui.mode = UiMode::Edit;

        app.workspace
            .activate(app.workspace.editor_index(unmapped_id).unwrap());
        for _ in 0..20 {
            app.poll_services();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.lsp.documents.len(), 2);
        assert!(
            wire_log_lines(&wire_log)
                .iter()
                .all(|line| !line.starts_with("close\t"))
        );

        app.workspace
            .activate(app.workspace.editor_index(first_id).unwrap());
        app.workspace.close_active(false).unwrap();
        poll_app_until(&mut app, "first document close", |app| {
            app.lsp.documents.get_by_editor_id(first_id).is_none()
                && app.lsp.diagnostics.get(&first_uri).is_none()
                && wire_line_count(&wire_log_lines(&wire_log), &format!("close\t{first_uri}")) == 1
        });
        assert!(app.lsp.documents.get_by_editor_id(second_id).is_some());
        assert!(!app.lsp.document_ends.contains_key(&first_id));
        assert!(app.lsp.document_ends.contains_key(&second_id));

        let client = app.lsp.client.as_mut().expect("mock client remains live");
        client.shutdown().unwrap();
        assert!(client.wait_stopped(std::time::Duration::from_secs(3)));
        let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while wire_line_count(&wire_log_lines(&wire_log), "exit") == 0
            && std::time::Instant::now() < exit_deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.lsp.client = None;
        let final_lines = wire_log_lines(&wire_log);
        assert_eq!(
            wire_line_count(&final_lines, &format!("open\t{first_uri}\t1")),
            1
        );
        assert_eq!(
            wire_line_count(&final_lines, &format!("open\t{second_uri}\t2")),
            1
        );
        assert_eq!(
            wire_line_count(&final_lines, &format!("change\t{first_uri}\t3")),
            1
        );
        assert_eq!(
            wire_line_count(&final_lines, &format!("change\t{second_uri}\t4")),
            1
        );
        assert_eq!(
            wire_line_count(&final_lines, &format!("save\t{first_uri}")),
            3
        );
        assert_eq!(
            wire_line_count(&final_lines, &format!("save\t{second_uri}")),
            1
        );
        assert_eq!(
            wire_line_count(&final_lines, &format!("close\t{first_uri}")),
            1
        );
        let second_close = format!("close\t{second_uri}");
        assert!(
            !final_lines
                .iter()
                .any(|line| { line == &second_close || line.contains(&unmapped_uri) })
        );
        assert_eq!(wire_line_count(&final_lines, "shutdown"), 1);
        assert_eq!(wire_line_count(&final_lines, "exit"), 1);
    }

    #[cfg(unix)]
    #[test]
    fn live_lsp_reaches_diagnostics_and_completion_through_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let workspace = Workspace::from_path(Some(path), Some(dir.path().to_path_buf())).unwrap();
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"workspaceSymbolProvider":true,"textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":false}}}}}' ;;
    *'"method":"textDocument/didOpen"'*)
      uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"version\":1,\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":2}},\"severity\":2,\"message\":\"demo warning\"}]}}" ;;
    *'"method":"textDocument/completion"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"label\":\"done\",\"insertText\":\"done\"}]}" ;;
    *'"method":"workspace/symbol"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"main\",\"kind\":12,\"containerName\":\"demo\",\"location\":{\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":0,\"character\":3},\"end\":{\"line\":0,\"character\":3}}}}]}" ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let mut config = Config::default();
        config.language_servers.push(LanguageServerConfig {
            name: "mock".to_owned(),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        });
        let mut app = App::new_ready_for_test(workspace, config);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while app.active_diagnostics().is_empty() && std::time::Instant::now() < deadline {
            app.poll_services();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.active_diagnostics().len(), 1);
        assert_eq!(app.active_diagnostics()[0].message, "demo warning");
        assert!(!app.diagnostic_highlights().is_empty());

        let end = app.workspace.active().document.len_chars();
        app.workspace.active_mut().cursor = end;
        app.request_lsp_completion();
        while !matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Completion,
                ..
            })
        ) && std::time::Instant::now() < deadline
        {
            app.poll_services();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::Completion,
                ..
            })
        ));
        app.commit_prompt();
        assert!(app.workspace.active().document.text().ends_with("done"));

        app.begin_workspace_symbol_query();
        let UiMode::Prompt(prompt) = &mut app.ui.mode else {
            panic!("workspace-symbol query prompt missing");
        };
        assert_eq!(prompt.kind, PromptFlow::WorkspaceSymbolQuery);
        prompt.input = "main".to_owned();
        prompt.cursor = prompt.input.chars().count();
        app.commit_prompt();
        assert!(matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::WorkspaceSymbolPending,
                ..
            })
        ));
        while !matches!(
            app.ui.mode,
            UiMode::Prompt(Prompt {
                kind: PromptFlow::WorkspaceSymbols,
                ..
            })
        ) && std::time::Instant::now() < deadline
        {
            app.poll_services();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let UiMode::Prompt(prompt) = &app.ui.mode else {
            panic!("workspace-symbol results missing");
        };
        assert_eq!(prompt.kind, PromptFlow::WorkspaceSymbols);
        assert!(prompt.labels[0].contains("main — Function · demo"));
        app.commit_prompt();
        assert_eq!(app.workspace.active().cursor, 3);
    }

    #[test]
    fn agent_run_refuses_dirty_unsaved_buffers() {
        let mut app = app_with_text("unsaved");
        app.workspace
            .active_mut()
            .insert("x", EditKind::Insert)
            .expect("edit");
        assert!(app.workspace.active().document.is_modified());
        app.start_agent_run("do a thing".to_owned());
        assert!(app.agent.job.is_none());
        assert!(
            app.ui
                .status
                .as_ref()
                .is_some_and(|status| { status.error && status.message.contains("dirty buffer") }),
            "expected dirty-buffer error, got {:?}",
            app.ui.status
        );
    }

    #[test]
    fn agent_run_confirms_when_git_worktree_is_dirty() {
        let temp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .status()
            .expect("git init");
        let repo = crate::git::GitRepository::discover(temp.path()).expect("discover repo");
        let mut workspace = Workspace::new(Some(temp.path().to_path_buf())).unwrap();
        let replaced = workspace
            .replace_editor(0, crate::Editor::new(Document::from_text("ok")))
            .expect("buffer");
        drop(replaced);
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.git.status = ServiceStatus::Ready;
        app.git.repository = Some(repo);
        app.git.changes = 3;
        app.start_agent_run("do a thing".to_owned());
        assert!(app.agent.job.is_none());
        assert!(matches!(
            app.ui.mode,
            UiMode::Confirm(ConfirmKind::AgentDirtyTree { git_changes: 3, .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.agent.job.is_some(), "Y should start the agent run");
    }
}
