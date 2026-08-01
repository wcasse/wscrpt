use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExCommand {
    Save(Option<PathBuf>),
    SaveForce,
    Quit { force: bool },
    SaveQuit(Option<PathBuf>),
    Edit(PathBuf),
    OpenPath(Option<PathBuf>),
    New,
    CloseOtherBuffers,
    ReopenClosedBuffer,
    NewFile(Option<PathBuf>),
    RenameFile(Option<PathBuf>),
    SaveCopyAs(Option<PathBuf>),
    GoTo(usize),
    JumpList,
    Bookmarks,
    SetLineEnding(super::LineEnding),
    SetLineNumbers(bool),
    Reload { force: bool },
    RefreshWorkspace,
    WorkspaceSidebar,
    WorkspaceInfo,
    BufferInfo,
    DirtyBuffers,
    RecentFiles,
    OpenRecentFile,
    Terminal,
    Tasks,
    TaskCatalog,
    TaskDefault,
    Task(String),
    TaskInfo(String),
    WorkspaceOutline,
    SourceAnnotations,
    GitStatus,
    GitChanges,
    GitFileStatus,
    GitDiff,
    GitDiffPicker,
    GitLog,
    GitCommitPicker,
    GitFileHistory,
    GitHead,
    GitCommitInfo(String),
    GitBlameLine,
    GitBranches,
    GitStageCurrent,
    GitUnstageCurrent,
    GitCommitStaged,
    LspLog,
    LspRestart,
    KeymapReference,
    Help,
    Stickies,
    NewSticky,
    AgentRun,
    AgentCancel,
    AgentDashboard,
}

pub fn parse(input: &str) -> Result<ExCommand, String> {
    let input = input.trim().trim_start_matches(':').trim();
    if input.is_empty() {
        return Err("type a command; try help".to_owned());
    }
    let (name, argument) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(name, rest)| (name, rest.trim()));

    match name {
        "w" | "write" => Ok(ExCommand::Save(path_argument(argument))),
        "w!" | "write!" => no_argument(argument, ExCommand::SaveForce),
        "q" | "quit" => no_argument(argument, ExCommand::Quit { force: false }),
        "q!" | "quit!" => no_argument(argument, ExCommand::Quit { force: true }),
        "wq" | "x" => Ok(ExCommand::SaveQuit(path_argument(argument))),
        "e" | "edit" if !argument.is_empty() => Ok(ExCommand::Edit(argument.into())),
        "e" | "edit" => Err("edit needs a path".to_owned()),
        "open-path" | "edit-path" | "open" if argument.is_empty() => Ok(ExCommand::OpenPath(None)),
        "open-path" | "edit-path" | "open" => Ok(ExCommand::OpenPath(Some(argument.into()))),
        "new" => no_argument(argument, ExCommand::New),
        "close-others" | "close-other-buffers" | "only" => {
            no_argument(argument, ExCommand::CloseOtherBuffers)
        }
        "reopen-closed" | "reopen-closed-buffer" | "undo-close" => {
            no_argument(argument, ExCommand::ReopenClosedBuffer)
        }
        "new-file" | "create-file" | "touch" if argument.is_empty() => Ok(ExCommand::NewFile(None)),
        "new-file" | "create-file" | "touch" => Ok(ExCommand::NewFile(Some(argument.into()))),
        "rename-file" | "move-file" | "mv" if argument.is_empty() => {
            Ok(ExCommand::RenameFile(None))
        }
        "rename-file" | "move-file" | "mv" => Ok(ExCommand::RenameFile(Some(argument.into()))),
        "save-copy" | "save-copy-as" | "copy-file" | "duplicate-file" if argument.is_empty() => {
            Ok(ExCommand::SaveCopyAs(None))
        }
        "save-copy" | "save-copy-as" | "copy-file" | "duplicate-file" => {
            Ok(ExCommand::SaveCopyAs(Some(argument.into())))
        }
        "goto" | "line" => argument
            .parse::<usize>()
            .ok()
            .filter(|line| *line > 0)
            .map(ExCommand::GoTo)
            .ok_or_else(|| "goto needs a line number greater than zero".to_owned()),
        "jumps" | "jump-list" | "jumplist" | "jump-history" => {
            no_argument(argument, ExCommand::JumpList)
        }
        "bookmarks" | "bookmark-list" | "marks" => no_argument(argument, ExCommand::Bookmarks),
        "set" if argument == "ff=unix" || argument == "fileformat=unix" => {
            Ok(ExCommand::SetLineEnding(super::LineEnding::Lf))
        }
        "set" if argument == "ff=dos" || argument == "fileformat=dos" => {
            Ok(ExCommand::SetLineEnding(super::LineEnding::CrLf))
        }
        "set" if argument == "number" || argument == "nu" => Ok(ExCommand::SetLineNumbers(true)),
        "set" if argument == "nonumber" || argument == "nonu" => {
            Ok(ExCommand::SetLineNumbers(false))
        }
        "set" => Err("set supports ff=unix, ff=dos, number, and nonumber".to_owned()),
        "reload" => no_argument(argument, ExCommand::Reload { force: false }),
        "reload!" => no_argument(argument, ExCommand::Reload { force: true }),
        "refresh" => no_argument(argument, ExCommand::RefreshWorkspace),
        "sidebar" | "workspace-sidebar" | "project-sidebar" | "tree-sidebar" => {
            no_argument(argument, ExCommand::WorkspaceSidebar)
        }
        "info" | "workspace-info" => no_argument(argument, ExCommand::WorkspaceInfo),
        "buffer-info" | "bufferinfo" | "file-info" | "fileinfo" => {
            no_argument(argument, ExCommand::BufferInfo)
        }
        "dirty" | "dirty-buffers" | "dirtybuffers" | "modified" | "unsaved" => {
            no_argument(argument, ExCommand::DirtyBuffers)
        }
        "recent" | "recent-files" | "recentfiles" => no_argument(argument, ExCommand::RecentFiles),
        "open-recent" | "open-recent-file" | "recent-open" | "recent-picker" => {
            no_argument(argument, ExCommand::OpenRecentFile)
        }
        "terminal" | "term" | "shell" => no_argument(argument, ExCommand::Terminal),
        "tasks" => no_argument(argument, ExCommand::Tasks),
        "task-catalog" | "taskcatalog" | "task-list" | "tasklist" => {
            no_argument(argument, ExCommand::TaskCatalog)
        }
        "task-default" | "default-task" | "run-default" | "check" => {
            no_argument(argument, ExCommand::TaskDefault)
        }
        "task" if !argument.is_empty() => Ok(ExCommand::Task(argument.to_owned())),
        "task" => Err("task needs a configured task name".to_owned()),
        "task-info" | "taskinfo" if !argument.is_empty() => {
            Ok(ExCommand::TaskInfo(argument.to_owned()))
        }
        "task-info" | "taskinfo" => Err("task-info needs a configured task name".to_owned()),
        "outline" | "workspace-outline" | "project-outline" | "local-symbols" => {
            no_argument(argument, ExCommand::WorkspaceOutline)
        }
        "todos" | "todo" | "source-annotations" | "annotations" | "fixmes" | "fixme" => {
            no_argument(argument, ExCommand::SourceAnnotations)
        }
        "git" | "status" => no_argument(argument, ExCommand::GitStatus),
        "changes" | "git-changes" | "changed-files" | "open-change" => {
            no_argument(argument, ExCommand::GitChanges)
        }
        "file-status" | "filestatus" | "git-file" | "gitfile" => {
            no_argument(argument, ExCommand::GitFileStatus)
        }
        "diff" => no_argument(argument, ExCommand::GitDiff),
        "diffs" | "git-diffs" | "changed-diffs" | "open-diff" => {
            no_argument(argument, ExCommand::GitDiffPicker)
        }
        "log" | "git-log" | "history" => no_argument(argument, ExCommand::GitLog),
        "commits" | "commit-picker" | "git-commits" | "open-commit" => {
            no_argument(argument, ExCommand::GitCommitPicker)
        }
        "file-history" | "filehistory" | "git-file-history" | "history-file" => {
            no_argument(argument, ExCommand::GitFileHistory)
        }
        "head" | "git-head" | "show-head" | "head-commit" => {
            no_argument(argument, ExCommand::GitHead)
        }
        "commit-info" | "commitinfo" | "show-commit" | "git-show" if !argument.is_empty() => {
            Ok(ExCommand::GitCommitInfo(argument.to_owned()))
        }
        "commit-info" | "commitinfo" | "show-commit" | "git-show" => {
            Err("commit-info needs a commit id".to_owned())
        }
        "blame" | "git-blame" | "line-blame" | "blame-line" => {
            no_argument(argument, ExCommand::GitBlameLine)
        }
        "branches" => no_argument(argument, ExCommand::GitBranches),
        "stage-current" => no_argument(argument, ExCommand::GitStageCurrent),
        "unstage-current" => no_argument(argument, ExCommand::GitUnstageCurrent),
        "commit" | "commit-staged" => no_argument(argument, ExCommand::GitCommitStaged),
        "lsp-log" => no_argument(argument, ExCommand::LspLog),
        "lsp-restart" => no_argument(argument, ExCommand::LspRestart),
        "keys" | "keymap" | "shortcuts" | "bindings" => {
            no_argument(argument, ExCommand::KeymapReference)
        }
        "help" | "h" | "?" => no_argument(argument, ExCommand::Help),
        "stickies" | "sticky" | "notes" | "sticky-list" => {
            no_argument(argument, ExCommand::Stickies)
        }
        "new-sticky" | "sticky-new" | "note-new" | "new-note" => {
            no_argument(argument, ExCommand::NewSticky)
        }
        "agent" | "agent-run" | "run-agent" => no_argument(argument, ExCommand::AgentRun),
        "agent-cancel" | "cancel-agent" | "stop-agent" => {
            no_argument(argument, ExCommand::AgentCancel)
        }
        // Activity / receipt / status aliases open the same bottom Agents dashboard.
        "agent-dashboard" | "dashboard" | "agents-dashboard" | "agent-panel" | "agent-activity"
        | "agent-status" | "agent-receipt" | "agents" => {
            no_argument(argument, ExCommand::AgentDashboard)
        }
        _ => Err(format!("unknown command: {name}")),
    }
}

fn path_argument(argument: &str) -> Option<PathBuf> {
    (!argument.is_empty()).then(|| PathBuf::from(argument))
}

fn no_argument(argument: &str, command: ExCommand) -> Result<ExCommand, String> {
    if argument.is_empty() {
        Ok(command)
    } else {
        Err("this command does not take an argument".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_commands() {
        assert_eq!(parse(":w").unwrap(), ExCommand::Save(None));
        assert_eq!(parse(":w!").unwrap(), ExCommand::SaveForce);
        assert_eq!(
            parse("write notes/today.md").unwrap(),
            ExCommand::Save(Some("notes/today.md".into()))
        );
        assert_eq!(parse("open-path").unwrap(), ExCommand::OpenPath(None));
        assert_eq!(
            parse("open src/lib.rs").unwrap(),
            ExCommand::OpenPath(Some("src/lib.rs".into()))
        );
        assert_eq!(parse("close-others").unwrap(), ExCommand::CloseOtherBuffers);
        assert_eq!(
            parse("reopen-closed").unwrap(),
            ExCommand::ReopenClosedBuffer
        );
        assert_eq!(
            parse("only now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("undo-close now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("q!").unwrap(), ExCommand::Quit { force: true });
        assert_eq!(parse("new-file").unwrap(), ExCommand::NewFile(None));
        assert_eq!(
            parse("new-file src/new.rs").unwrap(),
            ExCommand::NewFile(Some("src/new.rs".into()))
        );
        assert_eq!(parse("rename-file").unwrap(), ExCommand::RenameFile(None));
        assert_eq!(
            parse("rename-file src/renamed.rs").unwrap(),
            ExCommand::RenameFile(Some("src/renamed.rs".into()))
        );
        assert_eq!(parse("save-copy").unwrap(), ExCommand::SaveCopyAs(None));
        assert_eq!(
            parse("copy-file src/copy.rs").unwrap(),
            ExCommand::SaveCopyAs(Some("src/copy.rs".into()))
        );
        assert_eq!(parse("42"), Err("unknown command: 42".to_owned()));
        assert_eq!(parse("goto 42").unwrap(), ExCommand::GoTo(42));
        assert_eq!(parse("jumps").unwrap(), ExCommand::JumpList);
        assert_eq!(parse("bookmarks").unwrap(), ExCommand::Bookmarks);
        assert_eq!(
            parse("jump-list now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("set number").unwrap(),
            ExCommand::SetLineNumbers(true)
        );
        assert_eq!(
            parse("set nonumber").unwrap(),
            ExCommand::SetLineNumbers(false)
        );
        assert_eq!(parse("log").unwrap(), ExCommand::GitLog);
        assert_eq!(parse("diffs").unwrap(), ExCommand::GitDiffPicker);
        assert_eq!(
            parse("open-diff now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("history now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("commits").unwrap(), ExCommand::GitCommitPicker);
        assert_eq!(
            parse("open-commit now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("file-history").unwrap(), ExCommand::GitFileHistory);
        assert_eq!(
            parse("history-file now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("head").unwrap(), ExCommand::GitHead);
        assert_eq!(
            parse("show-head now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("commit-info abc123").unwrap(),
            ExCommand::GitCommitInfo("abc123".into())
        );
        assert_eq!(
            parse("show-commit"),
            Err("commit-info needs a commit id".to_owned())
        );
        assert_eq!(parse("blame").unwrap(), ExCommand::GitBlameLine);
        assert_eq!(
            parse("line-blame now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("task check").unwrap(),
            ExCommand::Task("check".into())
        );
        assert_eq!(parse("task-catalog").unwrap(), ExCommand::TaskCatalog);
        assert_eq!(parse("task-list").unwrap(), ExCommand::TaskCatalog);
        assert_eq!(parse("task-default").unwrap(), ExCommand::TaskDefault);
        assert_eq!(parse("check").unwrap(), ExCommand::TaskDefault);
        assert_eq!(
            parse("run-default now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("task-list now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("task-info check").unwrap(),
            ExCommand::TaskInfo("check".into())
        );
        assert_eq!(
            parse("taskinfo check").unwrap(),
            ExCommand::TaskInfo("check".into())
        );
        assert_eq!(
            parse("task-info"),
            Err("task-info needs a configured task name".to_owned())
        );
        assert_eq!(parse("outline").unwrap(), ExCommand::WorkspaceOutline);
        assert_eq!(
            parse("workspace-outline").unwrap(),
            ExCommand::WorkspaceOutline
        );
        assert_eq!(
            parse("project-outline now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("todos").unwrap(), ExCommand::SourceAnnotations);
        assert_eq!(parse("fixme").unwrap(), ExCommand::SourceAnnotations);
        assert_eq!(
            parse("source-annotations now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("terminal").unwrap(), ExCommand::Terminal);
        assert_eq!(parse("changes").unwrap(), ExCommand::GitChanges);
        assert_eq!(parse("stage-current").unwrap(), ExCommand::GitStageCurrent);
        assert_eq!(
            parse("unstage-current").unwrap(),
            ExCommand::GitUnstageCurrent
        );
        assert_eq!(parse("commit").unwrap(), ExCommand::GitCommitStaged);
        assert_eq!(parse("commit-staged").unwrap(), ExCommand::GitCommitStaged);
        assert_eq!(
            parse("commit direct message"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(
            parse("open-change now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("file-status").unwrap(), ExCommand::GitFileStatus);
        assert_eq!(parse("git-file").unwrap(), ExCommand::GitFileStatus);
        assert_eq!(parse("lsp-restart").unwrap(), ExCommand::LspRestart);
        assert_eq!(parse("keys").unwrap(), ExCommand::KeymapReference);
        assert_eq!(parse("keymap").unwrap(), ExCommand::KeymapReference);
        assert_eq!(
            parse("shortcuts now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("reload!").unwrap(), ExCommand::Reload { force: true });
        assert_eq!(parse("info").unwrap(), ExCommand::WorkspaceInfo);
        assert_eq!(parse("sidebar").unwrap(), ExCommand::WorkspaceSidebar);
        assert_eq!(
            parse("project-sidebar now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse("buffer-info").unwrap(), ExCommand::BufferInfo);
        assert_eq!(parse("fileinfo").unwrap(), ExCommand::BufferInfo);
        assert_eq!(parse("dirty-buffers").unwrap(), ExCommand::DirtyBuffers);
        assert_eq!(parse("unsaved").unwrap(), ExCommand::DirtyBuffers);
        assert_eq!(parse("recent-files").unwrap(), ExCommand::RecentFiles);
        assert_eq!(parse("recent").unwrap(), ExCommand::RecentFiles);
        assert_eq!(parse("open-recent").unwrap(), ExCommand::OpenRecentFile);
        assert_eq!(
            parse("recent-picker now"),
            Err("this command does not take an argument".to_owned())
        );
        assert_eq!(parse(":refresh").unwrap(), ExCommand::RefreshWorkspace);
        assert_eq!(
            parse("refresh now"),
            Err("this command does not take an argument".to_owned())
        );
    }

    #[test]
    fn removed_0_2_aliases_are_unknown() {
        for alias in [
            "terminal-live",
            "term-live",
            "shell-live",
            "terminal-embed",
            "embedded-terminal",
            "terminal-split",
            "term-split",
            "shell-split",
            "terminal-pane",
            "term-pane",
            "terminal-panel",
            "term-panel",
            "shell-panel",
            "pty-panel",
            "terminal-probe",
            "term-probe",
            "shell-probe",
            "pty-probe",
            "terminal-stop",
            "term-stop",
            "shell-stop",
            "pty-stop",
            "checkout",
            "switch",
            "git-checkout",
            "git-switch",
            "pull",
            "git-pull",
            "push",
            "git-push",
            "stage-current-file",
            "stage-file",
            "unstage-current-file",
            "unstage-file",
            "git-commit",
            "stage",
            "unstage",
        ] {
            assert_eq!(parse(alias), Err(format!("unknown command: {alias}")));
        }
    }
}
