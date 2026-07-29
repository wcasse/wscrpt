//! The keyboard-independent `wscrpt` action layer.
//!
//! Bare `Esc` or `Ctrl-K` enters the layer. While it is active, `Esc` cancels
//! and `Ctrl-K` restarts at the root. The `g`, `w`, `c`, `t`, and `v` keys are
//! prefixes which wait indefinitely; there is deliberately no timeout in this
//! module. Unknown actions are consumed, reported, and return to editing.
//!
//! This module intentionally knows nothing about Crossterm. The input adapter
//! is responsible for producing [`Key::Escape`] and [`Key::ControlK`], and for
//! handling globally reserved keys such as `Ctrl-\\` (terminal passthrough) and
//! optional `Ctrl-L` (redraw) before invoking this keymap.

use std::fmt::{self, Write as _};
use std::sync::OnceLock;

/// A normalized key understood by the action layer.
///
/// Modifier-heavy keys are intentionally absent. They are not required by the
/// iPad/Blink contract and should be handled outside this portable layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Key {
    Escape,
    ControlK,
    Character(char),
    Left,
    Right,
    Up,
    Down,
}

/// A key which can appear after the action-layer entry key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ActionKey {
    Character(char),
    Left,
    Right,
    Up,
    Down,
}

impl From<ActionKey> for Key {
    fn from(key: ActionKey) -> Self {
        match key {
            ActionKey::Character(character) => Self::Character(character),
            ActionKey::Left => Self::Left,
            ActionKey::Right => Self::Right,
            ActionKey::Up => Self::Up,
            ActionKey::Down => Self::Down,
        }
    }
}

impl fmt::Display for ActionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Character(' ') => formatter.write_str("Space"),
            Self::Character(character) => write!(formatter, "{character}"),
            Self::Left => formatter.write_str("Left"),
            Self::Right => formatter.write_str("Right"),
            Self::Up => formatter.write_str("Up"),
            Self::Down => formatter.write_str("Down"),
        }
    }
}

/// A complete sequence after either `Esc` or `Ctrl-K` has entered the layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Sequence {
    pub prefix: Option<char>,
    pub key: ActionKey,
}

impl Sequence {
    pub const fn direct(key: ActionKey) -> Self {
        Self { prefix: None, key }
    }

    pub const fn prefixed(prefix: char, key: ActionKey) -> Self {
        Self {
            prefix: Some(prefix),
            key,
        }
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(prefix) = self.prefix {
            write!(formatter, "{prefix} {}", self.key)
        } else {
            self.key.fmt(formatter)
        }
    }
}

/// Command-palette grouping. `g` navigation commands remain core actions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Namespace {
    Core,
    Workspace,
    Code,
    Tasks,
    VersionControl,
}

impl Namespace {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Core => "Core",
            Self::Workspace => "Workspace",
            Self::Code => "Code",
            Self::Tasks => "Tasks",
            Self::VersionControl => "Version Control",
        }
    }
}

/// Current no-timeout prefix state.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum PrefixState {
    #[default]
    Inactive,
    Action,
    GoTo,
    Workspace,
    Code,
    Tasks,
    VersionControl,
}

impl PrefixState {
    pub const fn breadcrumb(self) -> &'static str {
        match self {
            Self::Inactive => "",
            Self::Action => "ACTION",
            Self::GoTo => "ACTION › GO",
            Self::Workspace => "ACTION › WORKSPACE",
            Self::Code => "ACTION › CODE",
            Self::Tasks => "ACTION › TASKS",
            Self::VersionControl => "ACTION › VERSION CONTROL",
        }
    }

    pub const fn namespace(self) -> Option<Namespace> {
        match self {
            Self::Inactive => None,
            Self::Action | Self::GoTo => Some(Namespace::Core),
            Self::Workspace => Some(Namespace::Workspace),
            Self::Code => Some(Namespace::Code),
            Self::Tasks => Some(Namespace::Tasks),
            Self::VersionControl => Some(Namespace::VersionControl),
        }
    }

    const fn prefix(self) -> Option<char> {
        match self {
            Self::GoTo => Some('g'),
            Self::Workspace => Some('w'),
            Self::Code => Some('c'),
            Self::Tasks => Some('t'),
            Self::VersionControl => Some('v'),
            Self::Inactive | Self::Action => None,
        }
    }
}

/// Stable built-in action identities.
///
/// Persist the string returned by [`Action::id`], not the enum discriminant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Action {
    Save,
    SaveAll,
    Quit,
    QuickOpen,
    BufferSwitcher,
    PreviousBuffer,
    NextBuffer,
    CloseBuffer,
    CloseOtherBuffers,
    ReopenClosedBuffer,
    OpenPath,
    Find,
    Replace,
    NextMatch,
    PreviousMatch,
    Undo,
    Redo,
    DuplicateLine,
    DeleteLine,
    MoveLinesUp,
    MoveLinesDown,
    IndentLines,
    OutdentLines,
    Yank,
    Cut,
    Paste,
    SelectLines,
    SelectAll,
    ToggleSoftWrap,
    ToggleLineNumbers,
    PreviousWord,
    NextWord,
    PreviousViewport,
    NextViewport,
    JumpBack,
    JumpForward,
    JumpList,
    ToggleBookmark,
    Bookmarks,
    PreviousBookmark,
    NextBookmark,
    GoToLine,
    FileTop,
    FileBottom,
    MatchingBracket,
    DocumentSymbols,
    WorkspaceSymbols,
    WorkspaceOutline,
    SourceAnnotations,
    Help,
    KeymapReference,
    CommandPalette,
    CommandLine,
    WorkspaceTree,
    WorkspaceSidebar,
    WorkspaceInfo,
    BufferInfo,
    DirtyBuffers,
    PreviousDirtyBuffer,
    NextDirtyBuffer,
    RecentFiles,
    OpenRecentFile,
    WorkspaceRefresh,
    GlobalSearch,
    NewFile,
    RenameFile,
    SaveCopyAs,
    Recovery,
    Completion,
    Definition,
    References,
    NextSymbolOccurrence,
    PreviousSymbolOccurrence,
    ToggleLineComment,
    CopyLocation,
    CopyProblem,
    LspLog,
    LspRestart,
    Hover,
    Format,
    Problems,
    NextProblem,
    PreviousProblem,
    NextError,
    PreviousError,
    Terminal,
    TaskCatalog,
    TaskOutput,
    RunDefaultTask,
    TaskPicker,
    RerunLastTask,
    StopTask,
    VersionControlStatus,
    GitChanges,
    CurrentFileStatus,
    CurrentDiff,
    GitDiffPicker,
    GitLog,
    GitCommitPicker,
    GitFileHistory,
    GitHead,
    GitBlameLine,
    Branches,
}

impl Action {
    pub const ALL: &'static [Self] = &[
        Self::Save,
        Self::SaveAll,
        Self::Quit,
        Self::QuickOpen,
        Self::BufferSwitcher,
        Self::PreviousBuffer,
        Self::NextBuffer,
        Self::CloseBuffer,
        Self::CloseOtherBuffers,
        Self::ReopenClosedBuffer,
        Self::OpenPath,
        Self::Find,
        Self::Replace,
        Self::NextMatch,
        Self::PreviousMatch,
        Self::Undo,
        Self::Redo,
        Self::DuplicateLine,
        Self::DeleteLine,
        Self::MoveLinesUp,
        Self::MoveLinesDown,
        Self::IndentLines,
        Self::OutdentLines,
        Self::Yank,
        Self::Cut,
        Self::Paste,
        Self::SelectLines,
        Self::SelectAll,
        Self::ToggleSoftWrap,
        Self::ToggleLineNumbers,
        Self::PreviousWord,
        Self::NextWord,
        Self::PreviousViewport,
        Self::NextViewport,
        Self::JumpBack,
        Self::JumpForward,
        Self::JumpList,
        Self::ToggleBookmark,
        Self::Bookmarks,
        Self::PreviousBookmark,
        Self::NextBookmark,
        Self::GoToLine,
        Self::FileTop,
        Self::FileBottom,
        Self::MatchingBracket,
        Self::DocumentSymbols,
        Self::WorkspaceSymbols,
        Self::WorkspaceOutline,
        Self::SourceAnnotations,
        Self::Help,
        Self::KeymapReference,
        Self::CommandPalette,
        Self::CommandLine,
        Self::WorkspaceTree,
        Self::WorkspaceSidebar,
        Self::WorkspaceInfo,
        Self::BufferInfo,
        Self::DirtyBuffers,
        Self::PreviousDirtyBuffer,
        Self::NextDirtyBuffer,
        Self::RecentFiles,
        Self::OpenRecentFile,
        Self::WorkspaceRefresh,
        Self::GlobalSearch,
        Self::NewFile,
        Self::RenameFile,
        Self::SaveCopyAs,
        Self::Recovery,
        Self::Completion,
        Self::Definition,
        Self::References,
        Self::NextSymbolOccurrence,
        Self::PreviousSymbolOccurrence,
        Self::ToggleLineComment,
        Self::CopyLocation,
        Self::CopyProblem,
        Self::LspLog,
        Self::LspRestart,
        Self::Hover,
        Self::Format,
        Self::Problems,
        Self::NextProblem,
        Self::PreviousProblem,
        Self::NextError,
        Self::PreviousError,
        Self::Terminal,
        Self::TaskCatalog,
        Self::TaskOutput,
        Self::RunDefaultTask,
        Self::TaskPicker,
        Self::RerunLastTask,
        Self::StopTask,
        Self::VersionControlStatus,
        Self::GitChanges,
        Self::CurrentFileStatus,
        Self::CurrentDiff,
        Self::GitDiffPicker,
        Self::GitLog,
        Self::GitCommitPicker,
        Self::GitFileHistory,
        Self::GitHead,
        Self::GitBlameLine,
        Self::Branches,
    ];

    pub fn command(self) -> &'static Command {
        COMMANDS
            .iter()
            .find(|command| command.action == self)
            .expect("every Action must have command metadata")
    }

    pub fn id(self) -> &'static str {
        self.command().id
    }

    pub fn title(self) -> &'static str {
        self.command().title
    }

    pub fn namespace(self) -> Namespace {
        self.command().namespace
    }

    pub fn sequence(self) -> Sequence {
        self.command().sequence
    }

    pub fn from_id(id: &str) -> Option<Self> {
        command_by_id(id).map(|command| command.action)
    }
}

/// Palette metadata and the authoritative default binding for one action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Command {
    pub action: Action,
    pub id: &'static str,
    pub title: &'static str,
    pub namespace: Namespace,
    pub sequence: Sequence,
    pub keywords: &'static [&'static str],
}

const fn direct(character: char) -> Sequence {
    Sequence::direct(ActionKey::Character(character))
}

const fn prefixed(prefix: char, character: char) -> Sequence {
    Sequence::prefixed(prefix, ActionKey::Character(character))
}

macro_rules! command {
    ($action:ident, $id:literal, $title:literal, $namespace:ident, $sequence:expr, [$($keyword:literal),* $(,)?]) => {
        Command {
            action: Action::$action,
            id: $id,
            title: $title,
            namespace: Namespace::$namespace,
            sequence: $sequence,
            keywords: &[$($keyword),*],
        }
    };
}

/// All built-in commands in stable palette order.
pub static COMMANDS: &[Command] = &[
    command!(
        Save,
        "core.save",
        "Save Current",
        Core,
        direct('s'),
        ["write", "file"]
    ),
    command!(
        SaveAll,
        "core.save-all",
        "Save All",
        Core,
        direct('S'),
        ["write", "files"]
    ),
    command!(
        Quit,
        "core.quit",
        "Quit Workspace",
        Core,
        direct('q'),
        ["exit", "close"]
    ),
    command!(
        QuickOpen,
        "core.quick-open",
        "Quick Open",
        Core,
        direct('o'),
        ["file", "fuzzy"]
    ),
    command!(
        BufferSwitcher,
        "core.buffer-switcher",
        "Buffer Switcher",
        Core,
        direct('b'),
        ["files", "tabs"]
    ),
    command!(
        PreviousBuffer,
        "core.previous-buffer",
        "Previous Buffer",
        Core,
        direct('['),
        ["back", "tab"]
    ),
    command!(
        NextBuffer,
        "core.next-buffer",
        "Next Buffer",
        Core,
        direct(']'),
        ["forward", "tab"]
    ),
    command!(
        CloseBuffer,
        "core.close-buffer",
        "Close Buffer",
        Core,
        direct('k'),
        ["file", "tab"]
    ),
    command!(
        CloseOtherBuffers,
        "core.close-other-buffers",
        "Close Other Buffers",
        Core,
        direct('K'),
        ["file", "tab", "cleanup", "others"]
    ),
    command!(
        ReopenClosedBuffer,
        "core.reopen-closed-buffer",
        "Reopen Closed Buffer",
        Core,
        direct('U'),
        ["file", "tab", "undo", "restore"]
    ),
    command!(
        Find,
        "core.find",
        "Find in Buffer",
        Core,
        direct('/'),
        ["search", "text"]
    ),
    command!(
        Replace,
        "core.replace",
        "Replace All in Buffer",
        Core,
        direct('R'),
        ["search", "change", "literal"]
    ),
    command!(
        NextMatch,
        "core.next-match",
        "Next Match",
        Core,
        direct('n'),
        ["find", "search"]
    ),
    command!(
        PreviousMatch,
        "core.previous-match",
        "Previous Match",
        Core,
        direct('N'),
        ["find", "search"]
    ),
    command!(Undo, "core.undo", "Undo", Core, direct('u'), ["history"]),
    command!(Redo, "core.redo", "Redo", Core, direct('r'), ["history"]),
    command!(
        DuplicateLine,
        "core.duplicate-line",
        "Duplicate Line/Selection",
        Core,
        direct('D'),
        ["duplicate", "copy", "line", "selection", "edit"]
    ),
    command!(
        DeleteLine,
        "core.delete-line",
        "Delete Line/Selection",
        Core,
        direct('d'),
        ["delete", "line", "selection", "edit"]
    ),
    command!(
        MoveLinesUp,
        "core.move-lines-up",
        "Move Line/Selection Up",
        Core,
        direct('-'),
        ["move", "line", "selection", "up", "edit"]
    ),
    command!(
        MoveLinesDown,
        "core.move-lines-down",
        "Move Line/Selection Down",
        Core,
        direct('='),
        ["move", "line", "selection", "down", "edit"]
    ),
    command!(
        IndentLines,
        "core.indent-lines",
        "Indent Line/Selection",
        Core,
        direct('>'),
        ["indent", "line", "selection", "edit"]
    ),
    command!(
        OutdentLines,
        "core.outdent-lines",
        "Outdent Line/Selection",
        Core,
        direct('<'),
        ["outdent", "dedent", "line", "selection", "edit"]
    ),
    command!(
        Yank,
        "core.yank",
        "Yank Selection or Line",
        Core,
        direct('y'),
        ["copy", "register"]
    ),
    command!(
        Cut,
        "core.cut",
        "Cut Selection or Line",
        Core,
        direct('x'),
        ["delete", "register"]
    ),
    command!(
        Paste,
        "core.paste",
        "Paste Register",
        Core,
        direct('p'),
        ["insert", "clipboard"]
    ),
    command!(
        SelectLines,
        "core.select-lines",
        "Select Line/Selection",
        Core,
        direct('L'),
        ["selection", "line", "block", "edit"]
    ),
    command!(
        SelectAll,
        "core.select-all",
        "Select All",
        Core,
        direct('a'),
        ["selection", "document"]
    ),
    command!(
        ToggleSoftWrap,
        "core.toggle-soft-wrap",
        "Toggle Soft Wrap",
        Core,
        direct('z'),
        ["line", "display"]
    ),
    command!(
        ToggleLineNumbers,
        "core.toggle-line-numbers",
        "Toggle Line Numbers",
        Core,
        direct('l'),
        ["gutter", "line", "display", "ipad", "width"]
    ),
    command!(
        PreviousWord,
        "core.previous-word",
        "Previous Word",
        Core,
        Sequence::direct(ActionKey::Left),
        ["move", "navigate"]
    ),
    command!(
        NextWord,
        "core.next-word",
        "Next Word",
        Core,
        Sequence::direct(ActionKey::Right),
        ["move", "navigate"]
    ),
    command!(
        PreviousViewport,
        "core.previous-viewport",
        "Previous Viewport",
        Core,
        Sequence::direct(ActionKey::Up),
        ["page", "scroll"]
    ),
    command!(
        NextViewport,
        "core.next-viewport",
        "Next Viewport",
        Core,
        Sequence::direct(ActionKey::Down),
        ["page", "scroll"]
    ),
    command!(
        JumpBack,
        "core.jump-back",
        "Jump Back",
        Core,
        prefixed('g', 'o'),
        ["history", "older", "navigate"]
    ),
    command!(
        JumpForward,
        "core.jump-forward",
        "Jump Forward",
        Core,
        prefixed('g', 'i'),
        ["history", "newer", "navigate"]
    ),
    command!(
        JumpList,
        "core.jump-list",
        "Jump List",
        Core,
        prefixed('g', 'j'),
        ["history", "locations", "navigate", "picker"]
    ),
    command!(
        ToggleBookmark,
        "core.toggle-bookmark",
        "Toggle Bookmark",
        Core,
        prefixed('g', 'k'),
        ["mark", "bookmark", "location", "remember"]
    ),
    command!(
        Bookmarks,
        "core.bookmarks",
        "Bookmarks",
        Core,
        prefixed('g', 'K'),
        ["marks", "bookmarks", "locations", "picker"]
    ),
    command!(
        PreviousBookmark,
        "core.previous-bookmark",
        "Previous Bookmark",
        Core,
        prefixed('g', '['),
        ["bookmark", "mark", "previous", "navigate"]
    ),
    command!(
        NextBookmark,
        "core.next-bookmark",
        "Next Bookmark",
        Core,
        prefixed('g', ']'),
        ["bookmark", "mark", "next", "navigate"]
    ),
    command!(
        GoToLine,
        "core.goto-line",
        "Go to Line",
        Core,
        prefixed('g', 'l'),
        ["jump", "number"]
    ),
    command!(
        FileTop,
        "core.file-top",
        "Go to File Top",
        Core,
        prefixed('g', 't'),
        ["first", "start"]
    ),
    command!(
        FileBottom,
        "core.file-bottom",
        "Go to File Bottom",
        Core,
        prefixed('g', 'b'),
        ["last", "end"]
    ),
    command!(
        MatchingBracket,
        "core.matching-bracket",
        "Go to Matching Bracket",
        Core,
        prefixed('g', 'm'),
        ["bracket", "brace", "paren", "match", "navigate"]
    ),
    command!(
        DocumentSymbols,
        "core.document-symbols",
        "Document Symbols",
        Core,
        prefixed('g', 's'),
        ["outline", "functions"]
    ),
    command!(
        WorkspaceSymbols,
        "core.workspace-symbols",
        "Workspace Symbols",
        Core,
        prefixed('g', 'w'),
        ["project", "search", "functions", "types", "lsp"]
    ),
    command!(
        WorkspaceOutline,
        "core.workspace-outline",
        "Workspace Outline",
        Core,
        prefixed('g', 'O'),
        ["project", "outline", "symbols", "tags", "local"]
    ),
    command!(
        SourceAnnotations,
        "code.source-annotations",
        "Source Annotations",
        Code,
        prefixed('c', 't'),
        ["todo", "fixme", "hack", "note", "comments"]
    ),
    command!(
        Help,
        "core.help",
        "Help",
        Core,
        direct('h'),
        ["keys", "manual"]
    ),
    command!(
        KeymapReference,
        "core.keymap-reference",
        "Keymap Reference",
        Core,
        direct('?'),
        ["keys", "shortcuts", "bindings", "reference", "ipad"]
    ),
    command!(
        CommandPalette,
        "core.command-palette",
        "Command Palette",
        Core,
        direct(' '),
        ["actions", "commands"]
    ),
    command!(
        CommandLine,
        "core.command-line",
        "Open Command Line",
        Core,
        direct(':'),
        ["colon", "write", "quit"]
    ),
    command!(
        WorkspaceTree,
        "workspace.tree",
        "Workspace Tree",
        Workspace,
        prefixed('w', 't'),
        ["files", "explorer"]
    ),
    command!(
        WorkspaceSidebar,
        "workspace.sidebar",
        "Toggle Workspace Sidebar",
        Workspace,
        prefixed('w', 'S'),
        ["files", "explorer", "tree", "sidebar", "layout"]
    ),
    command!(
        WorkspaceInfo,
        "workspace.info",
        "Workspace Info",
        Workspace,
        prefixed('w', 'i'),
        ["status", "route", "buffers", "project", "lsp", "task"]
    ),
    command!(
        BufferInfo,
        "workspace.buffer-info",
        "Buffer Info",
        Workspace,
        prefixed('w', 'b'),
        ["active", "file", "context", "sidebar", "ipad"]
    ),
    command!(
        DirtyBuffers,
        "workspace.dirty-buffers",
        "Dirty Buffers",
        Workspace,
        prefixed('w', 'd'),
        ["unsaved", "modified", "save", "quit", "review"]
    ),
    command!(
        PreviousDirtyBuffer,
        "workspace.previous-dirty-buffer",
        "Previous Dirty Buffer",
        Workspace,
        prefixed('w', '['),
        ["unsaved", "modified", "dirty", "buffer", "previous"]
    ),
    command!(
        NextDirtyBuffer,
        "workspace.next-dirty-buffer",
        "Next Dirty Buffer",
        Workspace,
        prefixed('w', ']'),
        ["unsaved", "modified", "dirty", "buffer", "next"]
    ),
    command!(
        OpenPath,
        "workspace.open-path",
        "Open Path",
        Workspace,
        prefixed('w', 'o'),
        ["edit", "path", "file", "typed"]
    ),
    command!(
        RecentFiles,
        "workspace.recent-files",
        "Recent Files",
        Workspace,
        prefixed('w', 'e'),
        ["session", "history", "opened", "files", "restore"]
    ),
    command!(
        OpenRecentFile,
        "workspace.open-recent-file",
        "Open Recent File",
        Workspace,
        prefixed('w', 'E'),
        ["session", "history", "opened", "files", "restore", "picker"]
    ),
    command!(
        WorkspaceRefresh,
        "workspace.refresh",
        "Refresh Workspace Snapshot",
        Workspace,
        prefixed('w', 'R'),
        [
            "refresh",
            "reindex",
            "files",
            "explorer",
            "quick-open",
            "search"
        ]
    ),
    command!(
        GlobalSearch,
        "workspace.global-search",
        "Global Search",
        Workspace,
        prefixed('w', 's'),
        ["project", "find"]
    ),
    command!(
        NewFile,
        "workspace.new-file",
        "New File",
        Workspace,
        prefixed('w', 'n'),
        ["create", "buffer", "touch"]
    ),
    command!(
        RenameFile,
        "workspace.rename-file",
        "Rename File",
        Workspace,
        prefixed('w', 'm'),
        ["move", "rename", "path"]
    ),
    command!(
        SaveCopyAs,
        "workspace.save-copy-as",
        "Save Copy As",
        Workspace,
        prefixed('w', 'c'),
        ["copy", "duplicate", "fork", "file"]
    ),
    command!(
        Recovery,
        "workspace.recovery",
        "Recovery Journals",
        Workspace,
        prefixed('w', 'r'),
        ["crash", "restore", "unsaved"]
    ),
    command!(
        Completion,
        "code.completion",
        "Completion",
        Code,
        prefixed('c', 'c'),
        ["complete", "suggest"]
    ),
    command!(
        Definition,
        "code.definition",
        "Go to Definition",
        Code,
        prefixed('c', 'd'),
        ["symbol", "lsp"]
    ),
    command!(
        References,
        "code.references",
        "Find References",
        Code,
        prefixed('c', 'r'),
        ["usages", "lsp"]
    ),
    command!(
        NextSymbolOccurrence,
        "code.next-symbol-occurrence",
        "Next Symbol Occurrence",
        Code,
        prefixed('c', '.'),
        ["symbol", "identifier", "occurrence", "next", "local"]
    ),
    command!(
        PreviousSymbolOccurrence,
        "code.previous-symbol-occurrence",
        "Previous Symbol Occurrence",
        Code,
        prefixed('c', ','),
        ["symbol", "identifier", "occurrence", "previous", "local"]
    ),
    command!(
        ToggleLineComment,
        "code.toggle-line-comment",
        "Toggle Line Comment",
        Code,
        prefixed('c', '/'),
        ["comment", "line", "toggle", "source", "edit"]
    ),
    command!(
        CopyLocation,
        "code.copy-location",
        "Copy File Location",
        Code,
        prefixed('c', 'y'),
        [
            "copy",
            "path",
            "line",
            "column",
            "selection",
            "context",
            "clipboard"
        ]
    ),
    command!(
        CopyProblem,
        "code.copy-problem",
        "Copy Current Problem",
        Code,
        prefixed('c', 'Y'),
        [
            "copy",
            "diagnostic",
            "problem",
            "error",
            "warning",
            "message",
            "context",
            "clipboard"
        ]
    ),
    command!(
        LspLog,
        "code.lsp-log",
        "Language Server Log",
        Code,
        prefixed('c', 'l'),
        ["lsp", "language", "server", "log", "debug"]
    ),
    command!(
        LspRestart,
        "code.lsp-restart",
        "Restart Language Server",
        Code,
        prefixed('c', 'R'),
        ["lsp", "language", "server", "restart", "retry"]
    ),
    command!(
        Hover,
        "code.hover",
        "Hover Information",
        Code,
        prefixed('c', 'h'),
        ["documentation", "type"]
    ),
    command!(
        Format,
        "code.format",
        "Format Document",
        Code,
        prefixed('c', 'f'),
        ["style", "lsp"]
    ),
    command!(
        Problems,
        "code.problems",
        "Problems (LSP + Tasks)",
        Code,
        prefixed('c', 'p'),
        ["diagnostics", "errors", "compiler", "build", "task"]
    ),
    command!(
        NextProblem,
        "code.next-problem",
        "Next Problem",
        Code,
        prefixed('c', ']'),
        ["diagnostics", "errors", "compiler", "build", "task", "next"]
    ),
    command!(
        PreviousProblem,
        "code.previous-problem",
        "Previous Problem",
        Code,
        prefixed('c', '['),
        [
            "diagnostics",
            "errors",
            "compiler",
            "build",
            "task",
            "previous"
        ]
    ),
    command!(
        NextError,
        "code.next-error",
        "Next Error",
        Code,
        prefixed('c', 'e'),
        ["diagnostics", "errors", "compiler", "build", "next"]
    ),
    command!(
        PreviousError,
        "code.previous-error",
        "Previous Error",
        Code,
        prefixed('c', 'E'),
        ["diagnostics", "errors", "compiler", "build", "previous"]
    ),
    command!(
        Terminal,
        "task.terminal",
        "Workspace Shell",
        Tasks,
        prefixed('t', 't'),
        ["terminal", "shell", "handoff"]
    ),
    command!(
        TaskCatalog,
        "task.catalog",
        "Task Catalog",
        Tasks,
        prefixed('t', 'i'),
        ["list", "inspect", "build", "test", "details"]
    ),
    command!(
        TaskOutput,
        "task.output",
        "Task Output",
        Tasks,
        prefixed('t', 'o'),
        ["log", "build", "test"]
    ),
    command!(
        RunDefaultTask,
        "task.run-default",
        "Run Default Task",
        Tasks,
        prefixed('t', 'd'),
        ["check", "test", "build", "default", "run"]
    ),
    command!(
        TaskPicker,
        "task.picker",
        "Task Picker and Run",
        Tasks,
        prefixed('t', 'r'),
        ["execute", "command"]
    ),
    command!(
        RerunLastTask,
        "task.rerun-last",
        "Rerun Last Task",
        Tasks,
        prefixed('t', 'l'),
        ["repeat", "execute"]
    ),
    command!(
        StopTask,
        "task.stop",
        "Stop Task",
        Tasks,
        prefixed('t', 's'),
        ["cancel", "kill"]
    ),
    command!(
        VersionControlStatus,
        "vcs.status",
        "Git Status",
        VersionControl,
        prefixed('v', 's'),
        ["version control", "changes"]
    ),
    command!(
        GitChanges,
        "vcs.changes",
        "Open Git Changed File",
        VersionControl,
        prefixed('v', 'o'),
        ["git", "changes", "open", "modified", "status"]
    ),
    command!(
        CurrentFileStatus,
        "vcs.file-status",
        "Current File Status",
        VersionControl,
        prefixed('v', 'f'),
        ["git", "file", "state", "staged", "working"]
    ),
    command!(
        CurrentDiff,
        "vcs.current-diff",
        "Current Diff",
        VersionControl,
        prefixed('v', 'd'),
        ["git", "changes"]
    ),
    command!(
        GitDiffPicker,
        "vcs.diff-picker",
        "Open Changed Diff",
        VersionControl,
        prefixed('v', 'D'),
        ["git", "diff", "changes", "open", "review"]
    ),
    command!(
        GitLog,
        "vcs.log",
        "Git Log",
        VersionControl,
        prefixed('v', 'l'),
        ["git", "history", "commits", "recent"]
    ),
    command!(
        GitCommitPicker,
        "vcs.commit-picker",
        "Open Git Commit",
        VersionControl,
        prefixed('v', 'L'),
        ["git", "history", "commit", "open", "picker"]
    ),
    command!(
        GitFileHistory,
        "vcs.file-history",
        "Current File History",
        VersionControl,
        prefixed('v', 'H'),
        ["git", "history", "commits", "current", "file"]
    ),
    command!(
        GitHead,
        "vcs.head",
        "Git HEAD Commit",
        VersionControl,
        prefixed('v', 'h'),
        ["git", "history", "commit", "patch", "head"]
    ),
    command!(
        GitBlameLine,
        "vcs.blame-line",
        "Git Blame Current Line",
        VersionControl,
        prefixed('v', 'a'),
        ["git", "blame", "author", "line", "history"]
    ),
    command!(
        Branches,
        "vcs.branches",
        "Git Branches",
        VersionControl,
        prefixed('v', 'b'),
        ["checkout", "switch", "branch", "info"]
    ),
];

/// Render the public command reference from the same registry used by the
/// keymap, command palette, and in-editor reference view.
pub fn command_reference_markdown() -> String {
    let mut output = String::from(
        "# Command reference\n\n\
         Generated from the built-in command registry. Do not edit this file by hand.\n\n\
         Enter the no-timeout action layer with `Esc` or `Ctrl-K`, then type the listed sequence.\n",
    );
    for namespace in [
        Namespace::Core,
        Namespace::Workspace,
        Namespace::Code,
        Namespace::Tasks,
        Namespace::VersionControl,
    ] {
        let _ = write!(
            output,
            "\n## {}\n\n| Sequence | Command ID | Action |\n| --- | --- | --- |\n",
            namespace.title()
        );
        for command in COMMANDS
            .iter()
            .filter(|command| command.namespace == namespace)
        {
            let _ = writeln!(
                output,
                "| `Esc {}` | `{}` | {} |",
                command.sequence, command.id, command.title
            );
        }
    }
    output
}

fn hint_actions(prefix: PrefixState) -> &'static [Action] {
    match prefix {
        PrefixState::Inactive => &[],
        PrefixState::Action => &[
            Action::Save,
            Action::QuickOpen,
            Action::Find,
            Action::KeymapReference,
            Action::CommandPalette,
        ],
        PrefixState::GoTo => &[
            Action::JumpBack,
            Action::JumpForward,
            Action::GoToLine,
            Action::MatchingBracket,
            Action::DocumentSymbols,
            Action::WorkspaceSymbols,
        ],
        PrefixState::Workspace => &[
            Action::WorkspaceTree,
            Action::WorkspaceSidebar,
            Action::WorkspaceRefresh,
            Action::GlobalSearch,
            Action::NewFile,
            Action::RenameFile,
            Action::Recovery,
        ],
        PrefixState::Code => &[
            Action::Completion,
            Action::Definition,
            Action::References,
            Action::Problems,
            Action::Format,
            Action::Hover,
            Action::LspRestart,
            Action::LspLog,
        ],
        PrefixState::Tasks => &[
            Action::RunDefaultTask,
            Action::TaskPicker,
            Action::RerunLastTask,
            Action::TaskCatalog,
            Action::TaskOutput,
            Action::StopTask,
            Action::Terminal,
        ],
        PrefixState::VersionControl => &[
            Action::VersionControlStatus,
            Action::GitChanges,
            Action::CurrentFileStatus,
            Action::CurrentDiff,
            Action::GitDiffPicker,
            Action::GitLog,
            Action::GitCommitPicker,
            Action::GitFileHistory,
            Action::GitHead,
            Action::GitBlameLine,
            Action::Branches,
        ],
    }
}

/// Render the action-layer footer from the same command metadata used by the
/// keymap, palette, help overlay, and generated reference.
pub fn action_hint(prefix: PrefixState) -> String {
    if prefix == PrefixState::Inactive {
        return " Esc actions ".to_owned();
    }

    let mut output = format!(" {}  ", prefix.breadcrumb());
    for (index, action) in hint_actions(prefix).iter().enumerate() {
        let command = action.command();
        if index > 0 {
            output.push_str(" · ");
        }
        let _ = write!(output, "{} {}", command.sequence.key, command.title);
    }
    output.push_str(" · Esc Back ");
    output
}

fn help_command_line(actions: &[Action]) -> String {
    let mut output = String::new();
    for (index, action) in actions.iter().enumerate() {
        let command = action.command();
        if index > 0 {
            output.push_str("   ");
        }
        let _ = write!(output, "Esc {} {}", command.sequence, command.title);
    }
    output
}

/// Render the compact in-editor help overlay from command metadata.
pub fn action_layer_help_lines() -> &'static [String] {
    static LINES: OnceLock<Vec<String>> = OnceLock::new();
    LINES.get_or_init(|| {
        vec![
            "wscrpt — first hour".to_owned(),
            String::new(),
            "Type normally. Press Esc (or Ctrl-K) for the no-timeout ACTION layer.".to_owned(),
            "Prefixes wait forever — delayed Esc over mosh will not dump keys into the file.".to_owned(),
            String::new(),
            "Essentials".to_owned(),
            help_command_line(&[Action::Save, Action::QuickOpen, Action::Quit]),
            help_command_line(&[Action::Find, Action::CommandPalette]),
            help_command_line(&[Action::WorkspaceTree, Action::GlobalSearch]),
            help_command_line(&[Action::Completion, Action::Problems]),
            help_command_line(&[Action::Format, Action::Hover]),
            help_command_line(&[Action::Terminal, Action::RunDefaultTask]),
            help_command_line(&[Action::VersionControlStatus, Action::KeymapReference]),
            String::new(),
            "Language servers".to_owned(),
            "LSP starts only from ~/.config/wscrpt/config.toml (never from the project).".to_owned(),
            "Run: wscrpt --health   then: wscrpt --print-default-config".to_owned(),
            "Paste/uncomment a discovered [[language_servers]] block and restart wscrpt.".to_owned(),
            "Esc c f formats · set format_on_save = true to format before each Save.".to_owned(),
            String::new(),
            "Remote notes: mouse off by default for Blink · paste is one undo · Esc/Ctrl-G closes help."
                .to_owned(),
        ]
    })
}

pub fn command_by_id(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

pub fn command_by_sequence(sequence: Sequence) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.sequence == sequence)
}

/// Case-insensitive command-palette search over titles, IDs, sequences, and
/// keywords. Every whitespace-separated query term must match.
pub fn search_commands(query: &str) -> Vec<&'static Command> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_ascii_lowercase())
        .collect();
    if terms.is_empty() {
        return COMMANDS.iter().collect();
    }

    COMMANDS
        .iter()
        .filter(|command| {
            let title = command.title.to_ascii_lowercase();
            let sequence = command.sequence.to_string().to_ascii_lowercase();
            terms.iter().all(|term| {
                title.contains(term)
                    || command.id.contains(term)
                    || sequence.contains(term)
                    || command
                        .keywords
                        .iter()
                        .any(|keyword| keyword.to_ascii_lowercase().contains(term))
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownSequence {
    pub sequence: Sequence,
}

impl UnknownSequence {
    pub fn message(self) -> String {
        format!("Unknown action: {}", self.sequence)
    }
}

/// A consumed action-layer result.
///
/// [`Keymap::feed`] returns `None` only when the layer is inactive and the key
/// should continue through the normal editing input path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution {
    Pending(PrefixState),
    Command(Action),
    Unknown(UnknownSequence),
    Cancel,
}

/// Stateful, no-timeout resolver for the Action layer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Keymap {
    state: PrefixState,
}

impl Keymap {
    pub const fn new() -> Self {
        Self {
            state: PrefixState::Inactive,
        }
    }

    pub const fn state(&self) -> PrefixState {
        self.state
    }

    pub const fn is_active(&self) -> bool {
        !matches!(self.state, PrefixState::Inactive)
    }

    pub fn cancel(&mut self) -> bool {
        let was_active = self.is_active();
        self.state = PrefixState::Inactive;
        was_active
    }

    /// Feed one already-normalized key.
    ///
    /// A `Some` result means the key was consumed. In particular, an unknown
    /// action must not be inserted into the document.
    pub fn feed(&mut self, key: Key) -> Option<Resolution> {
        if self.state == PrefixState::Inactive {
            return match key {
                Key::Escape | Key::ControlK => {
                    self.state = PrefixState::Action;
                    Some(Resolution::Pending(self.state))
                }
                _ => None,
            };
        }

        match key {
            Key::Escape => {
                self.state = PrefixState::Inactive;
                Some(Resolution::Cancel)
            }
            Key::ControlK => {
                self.state = PrefixState::Action;
                Some(Resolution::Pending(self.state))
            }
            key => Some(self.resolve_action_key(key)),
        }
    }

    fn resolve_action_key(&mut self, key: Key) -> Resolution {
        let key = match key {
            Key::Character(character) => ActionKey::Character(character),
            Key::Left => ActionKey::Left,
            Key::Right => ActionKey::Right,
            Key::Up => ActionKey::Up,
            Key::Down => ActionKey::Down,
            Key::Escape | Key::ControlK => unreachable!("entry keys handled by feed"),
        };

        if self.state == PrefixState::Action
            && let ActionKey::Character(character) = key
            && let Some(prefix) = prefix_state(character)
        {
            self.state = prefix;
            return Resolution::Pending(prefix);
        }

        let sequence = Sequence {
            prefix: self.state.prefix(),
            key,
        };
        self.state = PrefixState::Inactive;
        command_by_sequence(sequence).map_or(
            Resolution::Unknown(UnknownSequence { sequence }),
            |command| Resolution::Command(command.action),
        )
    }
}

const fn prefix_state(character: char) -> Option<PrefixState> {
    match character {
        'g' => Some(PrefixState::GoTo),
        'w' => Some(PrefixState::Workspace),
        'c' => Some(PrefixState::Code),
        't' => Some(PrefixState::Tasks),
        'v' => Some(PrefixState::VersionControl),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn char_key(character: char) -> ActionKey {
        ActionKey::Character(character)
    }

    fn invoke(entry: Key, sequence: Sequence) -> (Resolution, PrefixState) {
        let mut keymap = Keymap::new();
        assert_eq!(
            keymap.feed(entry),
            Some(Resolution::Pending(PrefixState::Action))
        );
        if let Some(prefix) = sequence.prefix {
            let expected = prefix_state(prefix).expect("documented prefix");
            assert_eq!(
                keymap.feed(Key::Character(prefix)),
                Some(Resolution::Pending(expected))
            );
        }
        let resolution = keymap
            .feed(sequence.key.into())
            .expect("active layer consumes every key");
        (resolution, keymap.state())
    }

    #[test]
    fn exact_documented_sequences_are_complete() {
        let expected = [
            (direct('s'), Action::Save),
            (direct('S'), Action::SaveAll),
            (direct('q'), Action::Quit),
            (direct('o'), Action::QuickOpen),
            (direct('b'), Action::BufferSwitcher),
            (direct('['), Action::PreviousBuffer),
            (direct(']'), Action::NextBuffer),
            (direct('k'), Action::CloseBuffer),
            (direct('K'), Action::CloseOtherBuffers),
            (direct('U'), Action::ReopenClosedBuffer),
            (prefixed('w', 'o'), Action::OpenPath),
            (direct('/'), Action::Find),
            (direct('R'), Action::Replace),
            (direct('n'), Action::NextMatch),
            (direct('N'), Action::PreviousMatch),
            (direct('u'), Action::Undo),
            (direct('r'), Action::Redo),
            (direct('D'), Action::DuplicateLine),
            (direct('d'), Action::DeleteLine),
            (direct('-'), Action::MoveLinesUp),
            (direct('='), Action::MoveLinesDown),
            (direct('>'), Action::IndentLines),
            (direct('<'), Action::OutdentLines),
            (direct('y'), Action::Yank),
            (direct('x'), Action::Cut),
            (direct('p'), Action::Paste),
            (direct('L'), Action::SelectLines),
            (direct('a'), Action::SelectAll),
            (direct('z'), Action::ToggleSoftWrap),
            (direct('l'), Action::ToggleLineNumbers),
            (Sequence::direct(ActionKey::Left), Action::PreviousWord),
            (Sequence::direct(ActionKey::Right), Action::NextWord),
            (Sequence::direct(ActionKey::Up), Action::PreviousViewport),
            (Sequence::direct(ActionKey::Down), Action::NextViewport),
            (direct('h'), Action::Help),
            (direct('?'), Action::KeymapReference),
            (direct(' '), Action::CommandPalette),
            (direct(':'), Action::CommandLine),
            (prefixed('g', 'o'), Action::JumpBack),
            (prefixed('g', 'i'), Action::JumpForward),
            (prefixed('g', 'j'), Action::JumpList),
            (prefixed('g', 'k'), Action::ToggleBookmark),
            (prefixed('g', 'K'), Action::Bookmarks),
            (prefixed('g', '['), Action::PreviousBookmark),
            (prefixed('g', ']'), Action::NextBookmark),
            (prefixed('g', 'l'), Action::GoToLine),
            (prefixed('g', 't'), Action::FileTop),
            (prefixed('g', 'b'), Action::FileBottom),
            (prefixed('g', 'm'), Action::MatchingBracket),
            (prefixed('g', 's'), Action::DocumentSymbols),
            (prefixed('g', 'w'), Action::WorkspaceSymbols),
            (prefixed('g', 'O'), Action::WorkspaceOutline),
            (prefixed('w', 't'), Action::WorkspaceTree),
            (prefixed('w', 'S'), Action::WorkspaceSidebar),
            (prefixed('w', 'i'), Action::WorkspaceInfo),
            (prefixed('w', 'b'), Action::BufferInfo),
            (prefixed('w', 'd'), Action::DirtyBuffers),
            (prefixed('w', '['), Action::PreviousDirtyBuffer),
            (prefixed('w', ']'), Action::NextDirtyBuffer),
            (prefixed('w', 'e'), Action::RecentFiles),
            (prefixed('w', 'E'), Action::OpenRecentFile),
            (prefixed('w', 'R'), Action::WorkspaceRefresh),
            (prefixed('w', 's'), Action::GlobalSearch),
            (prefixed('w', 'n'), Action::NewFile),
            (prefixed('w', 'm'), Action::RenameFile),
            (prefixed('w', 'c'), Action::SaveCopyAs),
            (prefixed('w', 'r'), Action::Recovery),
            (prefixed('c', 'c'), Action::Completion),
            (prefixed('c', 'd'), Action::Definition),
            (prefixed('c', 'r'), Action::References),
            (prefixed('c', '.'), Action::NextSymbolOccurrence),
            (prefixed('c', ','), Action::PreviousSymbolOccurrence),
            (prefixed('c', '/'), Action::ToggleLineComment),
            (prefixed('c', 'y'), Action::CopyLocation),
            (prefixed('c', 'Y'), Action::CopyProblem),
            (prefixed('c', 'l'), Action::LspLog),
            (prefixed('c', 'R'), Action::LspRestart),
            (prefixed('c', 'h'), Action::Hover),
            (prefixed('c', 'f'), Action::Format),
            (prefixed('c', 'p'), Action::Problems),
            (prefixed('c', 't'), Action::SourceAnnotations),
            (prefixed('c', ']'), Action::NextProblem),
            (prefixed('c', '['), Action::PreviousProblem),
            (prefixed('c', 'e'), Action::NextError),
            (prefixed('c', 'E'), Action::PreviousError),
            (prefixed('t', 't'), Action::Terminal),
            (prefixed('t', 'i'), Action::TaskCatalog),
            (prefixed('t', 'o'), Action::TaskOutput),
            (prefixed('t', 'd'), Action::RunDefaultTask),
            (prefixed('t', 'r'), Action::TaskPicker),
            (prefixed('t', 'l'), Action::RerunLastTask),
            (prefixed('t', 's'), Action::StopTask),
            (prefixed('v', 's'), Action::VersionControlStatus),
            (prefixed('v', 'o'), Action::GitChanges),
            (prefixed('v', 'f'), Action::CurrentFileStatus),
            (prefixed('v', 'd'), Action::CurrentDiff),
            (prefixed('v', 'D'), Action::GitDiffPicker),
            (prefixed('v', 'l'), Action::GitLog),
            (prefixed('v', 'L'), Action::GitCommitPicker),
            (prefixed('v', 'H'), Action::GitFileHistory),
            (prefixed('v', 'h'), Action::GitHead),
            (prefixed('v', 'a'), Action::GitBlameLine),
            (prefixed('v', 'b'), Action::Branches),
        ];

        assert_eq!(COMMANDS.len(), expected.len());
        let actual: HashSet<_> = COMMANDS
            .iter()
            .map(|command| (command.sequence, command.action))
            .collect();
        assert_eq!(actual, expected.into_iter().collect());
    }

    #[test]
    fn every_sequence_resolves_from_both_entry_keys() {
        for command in COMMANDS {
            for entry in [Key::Escape, Key::ControlK] {
                assert_eq!(
                    invoke(entry, command.sequence),
                    (Resolution::Command(command.action), PrefixState::Inactive),
                    "{} via {entry:?}",
                    command.id
                );
            }
        }
    }

    #[test]
    fn removed_0_2_actions_have_no_registry_or_keymap_route() {
        for id in [
            "workspace.replace",
            "code.actions",
            "code.rename",
            "task.terminal-live",
            "task.terminal-split",
            "task.terminal-panel",
            "task.terminal-probe",
            "task.terminal-stop",
            "vcs.stage-current-file",
            "vcs.unstage-current-file",
            "vcs.commit-staged",
            "vcs.checkout",
            "vcs.pull",
            "vcs.push",
        ] {
            assert_eq!(Action::from_id(id), None, "removed action {id}");
        }
        for sequence in [
            prefixed('w', 'p'),
            prefixed('c', 'a'),
            prefixed('c', 'n'),
            prefixed('t', 'T'),
            prefixed('t', 'v'),
            prefixed('t', 'p'),
            prefixed('t', 'P'),
            prefixed('t', 'S'),
            prefixed('v', 'S'),
            prefixed('v', 'U'),
            prefixed('v', 'c'),
            prefixed('v', 'k'),
            prefixed('v', 'p'),
            prefixed('v', 'P'),
        ] {
            assert_eq!(command_by_sequence(sequence), None, "removed {sequence}");
        }
    }

    #[test]
    fn workspace_symbols_has_stable_navigation_metadata() {
        let sequence = prefixed('g', 'w');
        for entry in [Key::Escape, Key::ControlK] {
            assert_eq!(
                invoke(entry, sequence),
                (
                    Resolution::Command(Action::WorkspaceSymbols),
                    PrefixState::Inactive
                )
            );
        }

        let command = Action::WorkspaceSymbols.command();
        assert_eq!(command.id, "core.workspace-symbols");
        assert_eq!(command.title, "Workspace Symbols");
        assert_eq!(command.namespace, Namespace::Core);
        assert_eq!(command.sequence, sequence);
        assert_eq!(Action::from_id(command.id), Some(Action::WorkspaceSymbols));
    }

    #[test]
    fn workspace_outline_has_stable_navigation_metadata() {
        let sequence = prefixed('g', 'O');
        for entry in [Key::Escape, Key::ControlK] {
            assert_eq!(
                invoke(entry, sequence),
                (
                    Resolution::Command(Action::WorkspaceOutline),
                    PrefixState::Inactive
                )
            );
        }

        let command = Action::WorkspaceOutline.command();
        assert_eq!(command.id, "core.workspace-outline");
        assert_eq!(command.title, "Workspace Outline");
        assert_eq!(command.namespace, Namespace::Core);
        assert_eq!(command.sequence, sequence);
        assert_eq!(Action::from_id(command.id), Some(Action::WorkspaceOutline));
    }

    #[test]
    fn source_annotations_has_stable_navigation_metadata() {
        let sequence = prefixed('c', 't');
        for entry in [Key::Escape, Key::ControlK] {
            assert_eq!(
                invoke(entry, sequence),
                (
                    Resolution::Command(Action::SourceAnnotations),
                    PrefixState::Inactive
                )
            );
        }

        let command = Action::SourceAnnotations.command();
        assert_eq!(command.id, "code.source-annotations");
        assert_eq!(command.title, "Source Annotations");
        assert_eq!(command.namespace, Namespace::Code);
        assert_eq!(command.sequence, sequence);
        assert_eq!(Action::from_id(command.id), Some(Action::SourceAnnotations));
    }

    #[test]
    fn ids_actions_and_sequences_never_collide() {
        let mut actions = HashSet::new();
        let mut ids = HashSet::new();
        let mut sequences = HashSet::new();
        for command in COMMANDS {
            assert!(
                actions.insert(command.action),
                "duplicate action: {:?}",
                command.action
            );
            assert!(ids.insert(command.id), "duplicate id: {}", command.id);
            assert!(
                sequences.insert(command.sequence),
                "duplicate sequence: {}",
                command.sequence
            );
            assert!(!command.title.is_empty());
        }
        assert_eq!(actions, Action::ALL.iter().copied().collect());
    }

    #[test]
    fn only_intentional_namespace_nodes_are_prefixes() {
        let prefixes = ['g', 'w', 'c', 't', 'v'];
        for prefix in prefixes {
            assert!(
                COMMANDS
                    .iter()
                    .any(|command| command.sequence.prefix == Some(prefix))
            );
            assert!(command_by_sequence(direct(prefix)).is_none());
        }
        assert!(
            COMMANDS
                .iter()
                .filter_map(|command| command.sequence.prefix)
                .all(|prefix| prefixes.contains(&prefix))
        );
    }

    #[test]
    fn case_sensitive_bindings_stay_distinct() {
        assert_eq!(
            command_by_sequence(direct('s')).unwrap().action,
            Action::Save
        );
        assert_eq!(
            command_by_sequence(direct('S')).unwrap().action,
            Action::SaveAll
        );
        assert_eq!(
            command_by_sequence(direct('n')).unwrap().action,
            Action::NextMatch
        );
        assert_eq!(
            command_by_sequence(direct('N')).unwrap().action,
            Action::PreviousMatch
        );
        assert_eq!(
            command_by_sequence(direct('l')).unwrap().action,
            Action::ToggleLineNumbers
        );
    }

    #[test]
    fn same_final_key_is_scoped_by_its_prefix() {
        assert_eq!(
            command_by_sequence(direct('r')).unwrap().action,
            Action::Redo
        );
        assert_eq!(
            command_by_sequence(prefixed('c', 'r')).unwrap().action,
            Action::References
        );
        assert_eq!(
            command_by_sequence(direct('s')).unwrap().action,
            Action::Save
        );
        assert_eq!(
            command_by_sequence(prefixed('v', 's')).unwrap().action,
            Action::VersionControlStatus
        );
        assert_eq!(
            command_by_sequence(prefixed('w', 's')).unwrap().action,
            Action::GlobalSearch
        );
        assert_eq!(
            command_by_sequence(prefixed('w', 'S')).unwrap().action,
            Action::WorkspaceSidebar
        );
        assert_eq!(
            command_by_sequence(prefixed('t', 's')).unwrap().action,
            Action::StopTask
        );
        assert!(
            search_commands("workspace sidebar")
                .iter()
                .any(|command| command.action == Action::WorkspaceSidebar)
        );
        assert_eq!(
            command_by_sequence(direct('p')).unwrap().action,
            Action::Paste
        );
    }

    #[test]
    fn namespace_waits_without_a_clock_or_timeout() {
        let mut keymap = Keymap::new();
        keymap.feed(Key::Escape);
        assert_eq!(
            keymap.feed(Key::Character('c')),
            Some(Resolution::Pending(PrefixState::Code))
        );
        for _ in 0..10_000 {
            assert_eq!(keymap.state(), PrefixState::Code);
        }
        assert_eq!(
            keymap.feed(Key::Character('d')),
            Some(Resolution::Command(Action::Definition))
        );
    }

    #[test]
    fn escape_cancels_root_or_namespace_all_the_way_to_editing() {
        let mut root = Keymap::new();
        root.feed(Key::Escape);
        assert_eq!(root.feed(Key::Escape), Some(Resolution::Cancel));
        assert_eq!(root.state(), PrefixState::Inactive);

        let mut namespace = Keymap::new();
        namespace.feed(Key::Escape);
        namespace.feed(Key::Character('w'));
        assert_eq!(namespace.feed(Key::Escape), Some(Resolution::Cancel));
        assert_eq!(namespace.state(), PrefixState::Inactive);
    }

    #[test]
    fn control_k_is_an_alias_and_restarts_the_shared_layer() {
        let mut keymap = Keymap::new();
        assert_eq!(
            keymap.feed(Key::ControlK),
            Some(Resolution::Pending(PrefixState::Action))
        );
        keymap.feed(Key::Character('v'));
        assert_eq!(keymap.state(), PrefixState::VersionControl);
        assert_eq!(
            keymap.feed(Key::ControlK),
            Some(Resolution::Pending(PrefixState::Action))
        );
        assert_eq!(
            keymap.feed(Key::Character('s')),
            Some(Resolution::Command(Action::Save))
        );
    }

    #[test]
    fn unknown_actions_are_consumed_and_return_to_editing() {
        let mut keymap = Keymap::new();
        assert_eq!(keymap.feed(Key::Character('x')), None);
        keymap.feed(Key::Escape);
        let resolution = keymap.feed(Key::Character('@'));
        assert_eq!(
            resolution,
            Some(Resolution::Unknown(UnknownSequence {
                sequence: direct('@')
            }))
        );
        assert_eq!(
            match resolution.unwrap() {
                Resolution::Unknown(unknown) => unknown.message(),
                _ => unreachable!(),
            },
            "Unknown action: @"
        );
        assert_eq!(keymap.state(), PrefixState::Inactive);

        keymap.feed(Key::Escape);
        keymap.feed(Key::Character('w'));
        assert_eq!(
            keymap.feed(Key::Character('x')),
            Some(Resolution::Unknown(UnknownSequence {
                sequence: prefixed('w', 'x')
            }))
        );
        assert_eq!(keymap.state(), PrefixState::Inactive);
    }

    #[test]
    fn palette_searches_titles_ids_sequences_and_keywords() {
        assert_eq!(search_commands("save all")[0].action, Action::SaveAll);
        assert_eq!(
            search_commands("duplicate line")[0].action,
            Action::DuplicateLine
        );
        assert_eq!(search_commands("delete line")[0].action, Action::DeleteLine);
        assert_eq!(search_commands("move up")[0].action, Action::MoveLinesUp);
        assert_eq!(
            search_commands("move down")[0].action,
            Action::MoveLinesDown
        );
        assert_eq!(
            search_commands("indent line")[0].action,
            Action::IndentLines
        );
        assert_eq!(
            search_commands("outdent line")[0].action,
            Action::OutdentLines
        );
        assert_eq!(
            search_commands("matching bracket")[0].action,
            Action::MatchingBracket
        );
        assert_eq!(
            search_commands("core.select-lines")[0].action,
            Action::SelectLines
        );
        assert_eq!(
            search_commands("vcs.status")[0].action,
            Action::VersionControlStatus
        );
        assert!(
            search_commands("c d")
                .iter()
                .any(|command| command.action == Action::Definition)
        );
        assert!(
            search_commands("c .")
                .iter()
                .any(|command| command.action == Action::NextSymbolOccurrence)
        );
        assert!(
            search_commands("previous local occurrence")
                .iter()
                .any(|command| command.action == Action::PreviousSymbolOccurrence)
        );
        assert!(
            search_commands("toggle comment")
                .iter()
                .any(|command| command.action == Action::ToggleLineComment)
        );
        assert!(
            search_commands("copy location")
                .iter()
                .any(|command| command.action == Action::CopyLocation)
        );
        assert!(
            search_commands("copy diagnostic")
                .iter()
                .any(|command| command.action == Action::CopyProblem)
        );
        assert!(
            search_commands("lsp log")
                .iter()
                .any(|command| command.action == Action::LspLog)
        );
        assert!(
            search_commands("restart server")
                .iter()
                .any(|command| command.action == Action::LspRestart)
        );
        assert!(
            search_commands("diagnostics errors")
                .iter()
                .any(|command| command.action == Action::Problems)
        );
        assert!(
            search_commands("next error")
                .iter()
                .any(|command| command.action == Action::NextError)
        );
        assert!(
            search_commands("previous error")
                .iter()
                .any(|command| command.action == Action::PreviousError)
        );
        assert!(
            search_commands("reindex explorer")
                .iter()
                .any(|command| command.action == Action::WorkspaceRefresh)
        );
        assert!(
            search_commands("rename file")
                .iter()
                .any(|command| command.action == Action::RenameFile)
        );
        assert!(
            search_commands("open path")
                .iter()
                .any(|command| command.action == Action::OpenPath)
        );
        assert!(
            search_commands("close other buffers")
                .iter()
                .any(|command| command.action == Action::CloseOtherBuffers)
        );
        assert!(
            search_commands("reopen closed buffer")
                .iter()
                .any(|command| command.action == Action::ReopenClosedBuffer)
        );
        assert!(
            search_commands("save copy")
                .iter()
                .any(|command| command.action == Action::SaveCopyAs)
        );
        assert!(
            search_commands("line numbers")
                .iter()
                .any(|command| command.action == Action::ToggleLineNumbers)
        );
        assert!(
            search_commands("buffer context")
                .iter()
                .any(|command| command.action == Action::BufferInfo)
        );
        assert!(
            search_commands("unsaved review")
                .iter()
                .any(|command| command.action == Action::DirtyBuffers)
        );
        assert!(
            search_commands("next dirty")
                .iter()
                .any(|command| command.action == Action::NextDirtyBuffer)
        );
        assert!(
            search_commands("previous dirty")
                .iter()
                .any(|command| command.action == Action::PreviousDirtyBuffer)
        );
        assert!(
            search_commands("recent session")
                .iter()
                .any(|command| command.action == Action::RecentFiles)
        );
        assert!(
            search_commands("open recent picker")
                .iter()
                .any(|command| command.action == Action::OpenRecentFile)
        );
        assert!(
            search_commands("default test")
                .iter()
                .any(|command| command.action == Action::RunDefaultTask)
        );
        assert!(
            search_commands("key bindings")
                .iter()
                .any(|command| command.action == Action::KeymapReference)
        );
        assert!(
            search_commands("jump locations")
                .iter()
                .any(|command| command.action == Action::JumpList)
        );
        assert!(
            search_commands("toggle bookmark")
                .iter()
                .any(|command| command.action == Action::ToggleBookmark)
        );
        assert!(
            search_commands("bookmark picker")
                .iter()
                .any(|command| command.action == Action::Bookmarks)
        );
        assert!(
            search_commands("next bookmark")
                .iter()
                .any(|command| command.action == Action::NextBookmark)
        );
        assert!(
            search_commands("previous bookmark")
                .iter()
                .any(|command| command.action == Action::PreviousBookmark)
        );
        assert!(
            search_commands("recent commits")
                .iter()
                .any(|command| command.action == Action::GitLog)
        );
        assert!(
            search_commands("open commit")
                .iter()
                .any(|command| command.action == Action::GitCommitPicker)
        );
        assert!(
            search_commands("open changes")
                .iter()
                .any(|command| command.action == Action::GitChanges)
        );
        assert!(
            search_commands("open diff")
                .iter()
                .any(|command| command.action == Action::GitDiffPicker)
        );
        assert!(
            search_commands("file history")
                .iter()
                .any(|command| command.action == Action::GitFileHistory)
        );
        assert!(
            search_commands("head patch")
                .iter()
                .any(|command| command.action == Action::GitHead)
        );
        assert!(
            search_commands("blame author")
                .iter()
                .any(|command| command.action == Action::GitBlameLine)
        );
        assert!(search_commands("definitely-not-a-command").is_empty());
        assert_eq!(search_commands("  ").len(), COMMANDS.len());
    }

    #[test]
    fn palette_finds_workspace_symbols_by_scope_and_intent() {
        for query in [
            "workspace symbols",
            "project functions",
            "types lsp",
            "core.workspace-symbols",
            "g w",
        ] {
            assert!(
                search_commands(query)
                    .iter()
                    .any(|command| command.action == Action::WorkspaceSymbols),
                "workspace symbols missing for palette query {query:?}"
            );
        }
    }

    #[test]
    fn palette_finds_workspace_outline_by_scope_and_intent() {
        for query in [
            "workspace outline",
            "project tags",
            "local symbols",
            "core.workspace-outline",
            "g O",
        ] {
            assert!(
                search_commands(query)
                    .iter()
                    .any(|command| command.action == Action::WorkspaceOutline),
                "workspace outline missing for palette query {query:?}"
            );
        }
    }

    #[test]
    fn palette_finds_source_annotations_by_scope_and_intent() {
        for query in [
            "source annotations",
            "todo comments",
            "fixme",
            "code.source-annotations",
            "c t",
        ] {
            assert!(
                search_commands(query)
                    .iter()
                    .any(|command| command.action == Action::SourceAnnotations),
                "source annotations missing for palette query {query:?}"
            );
        }
    }

    #[test]
    fn metadata_round_trips_stable_ids_and_actions() {
        for action in Action::ALL {
            let command = action.command();
            assert_eq!(Action::from_id(command.id), Some(*action));
            assert_eq!(action.title(), command.title);
            assert_eq!(action.namespace(), command.namespace);
            assert_eq!(action.sequence(), command.sequence);
        }
        assert_eq!(Action::from_id("core.not-real"), None);
    }

    #[test]
    fn breadcrumbs_identify_every_pending_state() {
        assert_eq!(PrefixState::Action.breadcrumb(), "ACTION");
        assert_eq!(PrefixState::GoTo.breadcrumb(), "ACTION › GO");
        assert_eq!(
            PrefixState::Workspace.namespace(),
            Some(Namespace::Workspace)
        );
        assert_eq!(PrefixState::Code.namespace(), Some(Namespace::Code));
        assert_eq!(PrefixState::Tasks.namespace(), Some(Namespace::Tasks));
        assert_eq!(
            PrefixState::VersionControl.namespace(),
            Some(Namespace::VersionControl)
        );
        assert_eq!(PrefixState::Inactive.namespace(), None);
    }

    #[test]
    fn cancel_method_reports_whether_the_layer_was_active() {
        let mut keymap = Keymap::new();
        assert!(!keymap.cancel());
        keymap.feed(Key::Escape);
        assert!(keymap.cancel());
        assert_eq!(keymap.state(), PrefixState::Inactive);
    }

    #[test]
    fn helper_creates_character_keys() {
        assert_eq!(char_key('x'), ActionKey::Character('x'));
    }

    #[test]
    fn committed_command_reference_is_generated_from_registry() {
        assert_eq!(
            include_str!("../docs/COMMANDS.md"),
            command_reference_markdown()
        );
    }

    #[test]
    fn action_hints_and_help_are_rendered_from_command_metadata() {
        for prefix in [
            PrefixState::Action,
            PrefixState::GoTo,
            PrefixState::Workspace,
            PrefixState::Code,
            PrefixState::Tasks,
            PrefixState::VersionControl,
        ] {
            let hint = action_hint(prefix);
            for action in hint_actions(prefix) {
                let command = action.command();
                assert!(
                    hint.contains(&format!("{} {}", command.sequence.key, command.title)),
                    "missing {} from {prefix:?} hint",
                    command.id
                );
            }
        }

        let help = action_layer_help_lines().join("\n");
        for action in [
            Action::Save,
            Action::QuickOpen,
            Action::Completion,
            Action::Format,
            Action::Terminal,
            Action::VersionControlStatus,
            Action::KeymapReference,
        ] {
            let command = action.command();
            assert!(
                help.contains(&format!("Esc {} {}", command.sequence, command.title)),
                "missing {} from help",
                command.id
            );
        }
        assert!(
            help.contains("Language servers") && help.contains("format_on_save"),
            "first-hour help should cover LSP authorization and format_on_save"
        );
    }
}
