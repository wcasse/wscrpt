//! Small, bounded synchronous Git service for editor-facing version-control features.
//!
//! Every Git invocation is built with [`Command`] arguments. Pathspecs are
//! passed after `--` with Git's literal-pathspec mode enabled, so a filename is
//! never interpreted as a shell fragment, option, or pathspec expression.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_STDOUT_LIMIT: usize = 8 * 1024 * 1024;
const GIT_STDERR_LIMIT: usize = 256 * 1024;
const GIT_WAIT_INTERVAL: Duration = Duration::from_millis(10);
const MAX_BRANCH_NAME_BYTES: usize = 255;

#[derive(Clone, Copy, Debug)]
struct CommandLimits {
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

const GIT_COMMAND_LIMITS: CommandLimits = CommandLimits {
    timeout: GIT_COMMAND_TIMEOUT,
    stdout_bytes: GIT_STDOUT_LIMIT,
    stderr_bytes: GIT_STDERR_LIMIT,
};

/// A repository discovered by Git itself (including linked worktrees).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRepository {
    root: PathBuf,
}

/// Parsed output from `git status --porcelain=v2 -z --branch`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryStatus {
    pub branch: BranchInfo,
    pub files: Vec<FileStatus>,
}

/// Branch metadata emitted by porcelain-v2 status.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BranchInfo {
    pub head: BranchHead,
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub unborn: bool,
}

impl BranchInfo {
    /// The local branch name, or `None` for a detached/unknown HEAD.
    pub fn name(&self) -> Option<&str> {
        match &self.head {
            BranchHead::Named(name) => Some(name),
            BranchHead::Detached | BranchHead::Unknown => None,
        }
    }

    pub const fn is_detached(&self) -> bool {
        matches!(self.head, BranchHead::Detached)
    }
}

/// The symbolic state of HEAD.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum BranchHead {
    Named(String),
    Detached,
    #[default]
    Unknown,
}

/// One path reported by porcelain-v2 status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileStatus {
    /// Current repository-relative pathname.
    pub path: PathBuf,
    /// Previous pathname for a rename or copy.
    pub original_path: Option<PathBuf>,
    pub index: FileState,
    pub worktree: FileState,
    pub kind: StatusEntryKind,
    pub submodule: SubmoduleStatus,
}

/// Focused blame metadata for one committed line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameLine {
    pub commit: String,
    pub original_line: usize,
    pub final_line: usize,
    pub author: Option<String>,
    pub author_mail: Option<String>,
    pub author_time: Option<String>,
    pub summary: Option<String>,
    pub filename: Option<PathBuf>,
    pub content: String,
}

impl FileStatus {
    pub const fn is_staged(&self) -> bool {
        !matches!(self.index, FileState::Unmodified)
    }

    pub const fn has_worktree_change(&self) -> bool {
        !matches!(self.worktree, FileState::Unmodified)
    }
}

/// A typed index or worktree state from porcelain-v2's `XY` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileState {
    Unmodified,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    UpdatedButUnmerged,
    Untracked,
    Ignored,
    /// A state introduced by a newer Git version that this build does not yet
    /// name. Keeping the byte makes status forward-compatible.
    Unknown(u8),
}

/// The porcelain-v2 record kind, with rename/copy similarity retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusEntryKind {
    Ordinary,
    Renamed { score: u16 },
    Copied { score: u16 },
    Unmerged,
    Untracked,
    Ignored,
}

/// Submodule flags from porcelain-v2's four-character `<sub>` field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubmoduleStatus {
    pub is_submodule: bool,
    pub commit_changed: bool,
    pub tracked_changes: bool,
    pub untracked_changes: bool,
}

/// Failures from repository discovery or a Git operation.
#[derive(Debug)]
pub enum GitError {
    GitUnavailable(io::Error),
    Io {
        action: &'static str,
        source: io::Error,
    },
    NotRepository(PathBuf),
    CommandFailed {
        action: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    TimedOut {
        action: &'static str,
        timeout: Duration,
    },
    OutputLimitExceeded {
        action: &'static str,
        stream: &'static str,
        limit: usize,
    },
    InvalidPath(PathBuf),
    InvalidRevision(String),
    Parse(StatusParseError),
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitUnavailable(_) => write!(formatter, "Git is not installed or not on PATH"),
            Self::Io { action, .. } => write!(formatter, "could not {action}"),
            Self::NotRepository(path) => {
                write!(
                    formatter,
                    "{} is not inside a Git repository",
                    path.display()
                )
            }
            Self::CommandFailed {
                action,
                status,
                stderr,
            } => {
                write!(formatter, "Git could not {action} ({status})")?;
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
            Self::TimedOut { action, timeout } => {
                write!(
                    formatter,
                    "Git timed out while trying to {action} after {timeout:?}"
                )
            }
            Self::OutputLimitExceeded {
                action,
                stream,
                limit,
            } => write!(
                formatter,
                "Git {stream} exceeded the {limit}-byte limit while trying to {action}"
            ),
            Self::InvalidPath(path) => write!(
                formatter,
                "path must name an item inside the repository: {}",
                path.display()
            ),
            Self::InvalidRevision(revision) => write!(
                formatter,
                "commit id must be 4 to 64 hexadecimal characters: {revision}"
            ),
            Self::Parse(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::GitUnavailable(source) | Self::Io { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::NotRepository(_)
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::OutputLimitExceeded { .. }
            | Self::InvalidRevision(_)
            | Self::InvalidPath(_) => None,
        }
    }
}

impl From<StatusParseError> for GitError {
    fn from(error: StatusParseError) -> Self {
        Self::Parse(error)
    }
}

/// A malformed porcelain-v2 status stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusParseError {
    pub record: usize,
    pub message: String,
}

impl StatusParseError {
    fn new(record: usize, message: impl Into<String>) -> Self {
        Self {
            record,
            message: message.into(),
        }
    }
}

impl fmt::Display for StatusParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid porcelain-v2 status record {}: {}",
            self.record, self.message
        )
    }
}

impl std::error::Error for StatusParseError {}

impl GitRepository {
    /// Ask Git for the top-level repository containing `start`.
    ///
    /// `start` may be a file, directory, or not-yet-created path. In the last
    /// case, discovery starts at its closest existing ancestor.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, GitError> {
        let start = discovery_directory(start.as_ref())?;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&start)
            .args(["rev-parse", "--show-toplevel"])
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = run_output(command, "discover repository")?;
        if !output.status.success() {
            let stderr = stderr_text(&output);
            if stderr.contains("not a git repository") {
                return Err(GitError::NotRepository(start));
            }
            return Err(command_failed("discover repository", output));
        }

        let root = trim_one_line_ending(&output.stdout);
        if root.is_empty() {
            return Err(GitError::CommandFailed {
                action: "discover repository",
                status: output.status,
                stderr: "git returned an empty repository root".to_owned(),
            });
        }
        Ok(Self {
            root: path_from_bytes(root),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Normalize a user or filesystem path to Git's repository-relative path
    /// using the same safety checks as diff/stage operations.
    pub fn relative_path(&self, path: impl AsRef<Path>) -> Result<PathBuf, GitError> {
        self.safe_path(path.as_ref())
    }

    /// Read branch metadata and all tracked/untracked file states.
    pub fn status(&self) -> Result<RepositoryStatus, GitError> {
        let mut command = self.command();
        command.args([
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ]);
        let output = checked_output(command, "read repository status")?;
        parse_porcelain_v2(&output.stdout).map_err(Into::into)
    }

    /// Read the porcelain status entry for one explicit path, when Git reports
    /// one. A clean tracked file returns `Ok(None)`.
    pub fn status_path(&self, path: impl AsRef<Path>) -> Result<Option<FileStatus>, GitError> {
        let relative = self.safe_path(path.as_ref())?;
        Ok(self
            .status()?
            .files
            .into_iter()
            .find(|file| file.path == relative || file.original_path.as_ref() == Some(&relative)))
    }

    /// Read only the branch-facing portion of status.
    pub fn current_branch(&self) -> Result<BranchInfo, GitError> {
        Ok(self.status()?.branch)
    }

    /// Return a bounded, decorated one-line commit history for the current branch.
    pub fn recent_log(&self, max_count: usize) -> Result<Vec<u8>, GitError> {
        let max_count = max_count.to_string();
        let mut command = self.command();
        command.args([
            "log",
            "--no-color",
            "--decorate=short",
            "--date=short",
            "--pretty=format:%h %ad %d %s",
            "--max-count",
            max_count.as_str(),
        ]);
        Ok(checked_output(command, "read log")?.stdout)
    }

    /// Return a bounded, decorated one-line commit history for one repository path.
    pub fn file_history(
        &self,
        path: impl AsRef<Path>,
        max_count: usize,
    ) -> Result<Vec<u8>, GitError> {
        let path = self.safe_path(path.as_ref())?;
        let max_count = max_count.to_string();
        let mut command = self.command();
        command
            .args([
                "log",
                "--no-color",
                "--decorate=short",
                "--date=short",
                "--pretty=format:%h %ad %an %s",
                "--max-count",
                max_count.as_str(),
                "--follow",
                "--",
            ])
            .arg(path);
        Ok(checked_output(command, "read file history")?.stdout)
    }

    /// Return the no-color stat and patch for the current HEAD commit.
    pub fn head_details(&self) -> Result<Vec<u8>, GitError> {
        let mut command = self.command();
        command.args([
            "show",
            "--no-color",
            "--stat",
            "--patch",
            "--no-ext-diff",
            "--decorate=short",
            "--date=short",
            "HEAD",
        ]);
        Ok(checked_output(command, "read HEAD commit")?.stdout)
    }

    /// Return the no-color stat and patch for one explicit hexadecimal commit id.
    pub fn commit_details(&self, commit: &str) -> Result<Vec<u8>, GitError> {
        let commit = validate_commit_id(commit)?;
        let mut command = self.command();
        command.args([
            "show",
            "--no-color",
            "--stat",
            "--patch",
            "--no-ext-diff",
            "--decorate=short",
            "--date=short",
            commit,
        ]);
        Ok(checked_output(command, "read commit")?.stdout)
    }

    /// Return line-porcelain blame metadata for one committed line.
    pub fn blame_line(&self, path: impl AsRef<Path>, line: usize) -> Result<BlameLine, GitError> {
        let path = self.safe_path(path.as_ref())?;
        let line = line.max(1).to_string();
        let range = format!("{line},{line}");
        let mut command = self.command();
        command
            .args(["blame", "--line-porcelain", "-L", range.as_str(), "--"])
            .arg(path);
        let output = checked_output(command, "read line blame")?;
        parse_blame_line(&output.stdout)
    }

    /// Return the no-color patch for one working-tree or staged path.
    pub fn diff_path(&self, path: impl AsRef<Path>, staged: bool) -> Result<Vec<u8>, GitError> {
        self.diff_paths([path], staged)
    }

    /// Return the no-color patch for explicit paths.
    pub fn diff_paths<I, P>(&self, paths: I, staged: bool) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = self.safe_paths(paths)?;
        let mut command = self.command();
        command.args(["diff", "--no-ext-diff", "--no-color"]);
        if staged {
            command.arg("--cached");
        }
        command.arg("--").args(paths);
        Ok(checked_output(command, "read diff")?.stdout)
    }

    /// List local branch names (no remotes), newest-checked-out first.
    pub fn list_local_branches(&self, max_count: usize) -> Result<Vec<String>, GitError> {
        let max_count = max_count.clamp(1, 500);
        let mut command = self.command();
        command.args([
            "for-each-ref",
            "--format=%(refname:short)",
            "--sort=-committerdate",
            "refs/heads",
        ]);
        let output = checked_output(command, "list local branches")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut branches = Vec::new();
        for line in text.lines() {
            let name = line.trim();
            if name.is_empty() {
                continue;
            }
            if validate_branch_name(name).is_ok() {
                branches.push(name.to_owned());
            }
            if branches.len() >= max_count {
                break;
            }
        }
        Ok(branches)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("--literal-pathspecs")
            .arg("-C")
            .arg(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat");
        command
    }

    fn safe_paths<I, P>(&self, paths: I) -> Result<Vec<PathBuf>, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut safe = Vec::new();
        for path in paths {
            safe.push(self.safe_path(path.as_ref())?);
        }
        if safe.is_empty() {
            return Err(GitError::InvalidPath(PathBuf::new()));
        }
        Ok(safe)
    }

    fn safe_path(&self, path: &Path) -> Result<PathBuf, GitError> {
        let relative = if path.is_absolute() {
            // `tempfile` and user-entered paths commonly use `/var` on macOS
            // while Git reports the physical `/private/var` root. Resolve the
            // parent (not the final item, which may itself be a symlink) so
            // both spellings compare correctly and deleted paths still work.
            let physical_root = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
            let physical_path = physical_parent_path(path).unwrap_or_else(|| path.to_path_buf());
            physical_path
                .strip_prefix(&physical_root)
                .map_err(|_| GitError::InvalidPath(path.to_path_buf()))?
                .to_path_buf()
        } else {
            path.to_path_buf()
        };

        let mut normalized = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => normalized.push(part),
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(GitError::InvalidPath(path.to_path_buf()));
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(GitError::InvalidPath(path.to_path_buf()));
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            return Err(GitError::InvalidPath(path.to_path_buf()));
        }
        Ok(normalized)
    }
}

/// Resolve the closest existing parent, then put any missing tail back. The
/// final component is intentionally never canonicalized: staging a symlink
/// stored in the repository is valid, while a path *through* an escaping
/// symlink is rejected once its parent is resolved outside the root.
fn physical_parent_path(path: &Path) -> Option<PathBuf> {
    let final_component = path.file_name()?.to_os_string();
    let mut parent = path.parent()?.to_path_buf();
    let mut missing = Vec::<OsString>::new();

    let physical_parent = loop {
        match fs::canonicalize(&parent) {
            Ok(parent) => break parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(parent.file_name()?.to_os_string());
                if !parent.pop() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    };

    let mut physical = physical_parent;
    for component in missing.iter().rev() {
        physical.push(component);
    }
    physical.push(final_component);
    Some(physical)
}

fn validate_commit_id(commit: &str) -> Result<&str, GitError> {
    let commit = commit.trim();
    if (4..=64).contains(&commit.len()) && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(commit)
    } else {
        Err(GitError::InvalidRevision(commit.to_owned()))
    }
}

fn parse_blame_line(input: &[u8]) -> Result<BlameLine, GitError> {
    let mut lines = input.split(|byte| *byte == b'\n');
    let header = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| StatusParseError::new(1, "blame output is empty"))?;
    let header = String::from_utf8_lossy(header);
    let mut fields = header.split_whitespace();
    let commit = fields
        .next()
        .filter(|commit| !commit.is_empty())
        .ok_or_else(|| StatusParseError::new(1, "blame header has no commit"))?
        .to_owned();
    let original_line = fields
        .next()
        .and_then(|line| line.parse::<usize>().ok())
        .ok_or_else(|| StatusParseError::new(1, "blame header has no original line"))?;
    let final_line = fields
        .next()
        .and_then(|line| line.parse::<usize>().ok())
        .ok_or_else(|| StatusParseError::new(1, "blame header has no final line"))?;

    let mut blame = BlameLine {
        commit,
        original_line,
        final_line,
        author: None,
        author_mail: None,
        author_time: None,
        summary: None,
        filename: None,
        content: String::new(),
    };

    for (index, line) in lines.enumerate() {
        if let Some(content) = line.strip_prefix(b"\t") {
            blame.content = String::from_utf8_lossy(content).into_owned();
            return Ok(blame);
        }
        let line_number = index + 2;
        if let Some(value) = line.strip_prefix(b"author ") {
            blame.author = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = line.strip_prefix(b"author-mail ") {
            blame.author_mail = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = line.strip_prefix(b"author-time ") {
            blame.author_time = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = line.strip_prefix(b"summary ") {
            blame.summary = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = line.strip_prefix(b"filename ") {
            blame.filename = Some(PathBuf::from(String::from_utf8_lossy(value).into_owned()));
        } else if line.is_empty() {
            continue;
        } else if line_number > 10_000 {
            return Err(StatusParseError::new(line_number, "blame output is too long").into());
        }
    }
    Err(StatusParseError::new(1, "blame output has no line content").into())
}

/// Discover a repository without treating a non-repository directory as an
/// exceptional condition.
pub fn discover_repository(start: impl AsRef<Path>) -> Result<Option<GitRepository>, GitError> {
    match GitRepository::discover(start) {
        Ok(repository) => Ok(Some(repository)),
        Err(GitError::NotRepository(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Parse NUL-delimited porcelain-v2 output.
///
/// Pathname fields are retained as platform-native bytes on Unix. In
/// particular, spaces, tabs, newlines, leading dashes, and rename pairs do not
/// need quoting or special cases.
pub fn parse_porcelain_v2(input: &[u8]) -> Result<RepositoryStatus, StatusParseError> {
    let records: Vec<&[u8]> = input.split(|byte| *byte == 0).collect();
    let mut status = RepositoryStatus::default();
    let mut index = 0;

    while index < records.len() {
        let record = records[index];
        if record.is_empty() {
            index += 1;
            continue;
        }
        let record_number = index + 1;

        match record[0] {
            b'#' => parse_header(record, record_number, &mut status.branch)?,
            b'1' => status.files.push(parse_ordinary(record, record_number)?),
            b'2' => {
                let original = records.get(index + 1).copied().ok_or_else(|| {
                    StatusParseError::new(record_number, "rename/copy record has no original path")
                })?;
                if original.is_empty() {
                    return Err(StatusParseError::new(
                        record_number,
                        "rename/copy record has an empty original path",
                    ));
                }
                status
                    .files
                    .push(parse_rename(record, original, record_number)?);
                index += 1;
            }
            b'u' => status.files.push(parse_unmerged(record, record_number)?),
            b'?' => status
                .files
                .push(parse_simple_path(record, record_number, false)?),
            b'!' => status
                .files
                .push(parse_simple_path(record, record_number, true)?),
            tag => {
                return Err(StatusParseError::new(
                    record_number,
                    format!("unknown record tag {:?}", char::from(tag)),
                ));
            }
        }
        index += 1;
    }
    Ok(status)
}

fn parse_header(
    record: &[u8],
    record_number: usize,
    branch: &mut BranchInfo,
) -> Result<(), StatusParseError> {
    let body = record.strip_prefix(b"# ").ok_or_else(|| {
        StatusParseError::new(record_number, "branch header must start with `# `")
    })?;
    let Some(separator) = body.iter().position(|byte| *byte == b' ') else {
        return Err(StatusParseError::new(
            record_number,
            "branch header has no value",
        ));
    };
    let key = &body[..separator];
    let value = &body[separator + 1..];

    match key {
        b"branch.oid" => {
            if value == b"(initial)" {
                branch.oid = None;
                branch.unborn = true;
            } else {
                branch.oid = Some(as_text(value));
                branch.unborn = false;
            }
        }
        b"branch.head" => {
            branch.head = if value == b"(detached)" {
                BranchHead::Detached
            } else {
                BranchHead::Named(as_text(value))
            };
        }
        b"branch.upstream" => branch.upstream = Some(as_text(value)),
        b"branch.ab" => {
            let mut counts = value.split(|byte| *byte == b' ');
            let ahead = counts.next().ok_or_else(|| {
                StatusParseError::new(record_number, "branch.ab has no ahead count")
            })?;
            let behind = counts.next().ok_or_else(|| {
                StatusParseError::new(record_number, "branch.ab has no behind count")
            })?;
            if counts.next().is_some() {
                return Err(StatusParseError::new(
                    record_number,
                    "branch.ab has extra fields",
                ));
            }
            branch.ahead = parse_prefixed_count(ahead, b'+', record_number, "ahead")?;
            branch.behind = parse_prefixed_count(behind, b'-', record_number, "behind")?;
        }
        // Porcelain-v2 may add headers (for example `# stash N`) when callers
        // request them. Unknown headers are explicitly safe to ignore.
        _ => {}
    }
    Ok(())
}

fn parse_ordinary(record: &[u8], record_number: usize) -> Result<FileStatus, StatusParseError> {
    let (fields, path) = split_metadata(record, 8, record_number)?;
    expect_tag(fields[0], b"1", record_number)?;
    let (index, worktree) = parse_xy(fields[1], record_number)?;
    Ok(FileStatus {
        path: nonempty_path(path, record_number)?,
        original_path: None,
        index,
        worktree,
        kind: StatusEntryKind::Ordinary,
        submodule: parse_submodule(fields[2], record_number)?,
    })
}

fn parse_rename(
    record: &[u8],
    original: &[u8],
    record_number: usize,
) -> Result<FileStatus, StatusParseError> {
    let (fields, path) = split_metadata(record, 9, record_number)?;
    expect_tag(fields[0], b"2", record_number)?;
    let (index, worktree) = parse_xy(fields[1], record_number)?;
    let score = fields[8];
    let (&operation, digits) = score
        .split_first()
        .ok_or_else(|| StatusParseError::new(record_number, "rename/copy score is empty"))?;
    let score = parse_ascii_u16(digits, record_number, "rename/copy score")?;
    let kind = match operation {
        b'R' => StatusEntryKind::Renamed { score },
        b'C' => StatusEntryKind::Copied { score },
        _ => {
            return Err(StatusParseError::new(
                record_number,
                "rename/copy score must start with R or C",
            ));
        }
    };
    Ok(FileStatus {
        path: nonempty_path(path, record_number)?,
        original_path: Some(nonempty_path(original, record_number)?),
        index,
        worktree,
        kind,
        submodule: parse_submodule(fields[2], record_number)?,
    })
}

fn parse_unmerged(record: &[u8], record_number: usize) -> Result<FileStatus, StatusParseError> {
    let (fields, path) = split_metadata(record, 10, record_number)?;
    expect_tag(fields[0], b"u", record_number)?;
    let (index, worktree) = parse_xy(fields[1], record_number)?;
    Ok(FileStatus {
        path: nonempty_path(path, record_number)?,
        original_path: None,
        index,
        worktree,
        kind: StatusEntryKind::Unmerged,
        submodule: parse_submodule(fields[2], record_number)?,
    })
}

fn parse_simple_path(
    record: &[u8],
    record_number: usize,
    ignored: bool,
) -> Result<FileStatus, StatusParseError> {
    let prefix: &[u8] = if ignored { b"! " } else { b"? " };
    let path = record.strip_prefix(prefix).ok_or_else(|| {
        StatusParseError::new(record_number, "path record must contain a tag and space")
    })?;
    Ok(FileStatus {
        path: nonempty_path(path, record_number)?,
        original_path: None,
        index: FileState::Unmodified,
        worktree: if ignored {
            FileState::Ignored
        } else {
            FileState::Untracked
        },
        kind: if ignored {
            StatusEntryKind::Ignored
        } else {
            StatusEntryKind::Untracked
        },
        submodule: SubmoduleStatus::default(),
    })
}

/// Split a fixed number of ASCII metadata fields while leaving every byte in
/// the pathname tail intact (including leading whitespace).
fn split_metadata(
    record: &[u8],
    field_count: usize,
    record_number: usize,
) -> Result<(Vec<&[u8]>, &[u8]), StatusParseError> {
    let mut remaining = record;
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let separator = remaining
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| {
                StatusParseError::new(record_number, "record has too few metadata fields")
            })?;
        fields.push(&remaining[..separator]);
        remaining = &remaining[separator + 1..];
    }
    Ok((fields, remaining))
}

fn expect_tag(field: &[u8], expected: &[u8], record: usize) -> Result<(), StatusParseError> {
    if field == expected {
        Ok(())
    } else {
        Err(StatusParseError::new(record, "record tag is malformed"))
    }
}

fn parse_xy(field: &[u8], record: usize) -> Result<(FileState, FileState), StatusParseError> {
    if field.len() != 2 {
        return Err(StatusParseError::new(
            record,
            "XY state must contain exactly two bytes",
        ));
    }
    Ok((parse_state(field[0]), parse_state(field[1])))
}

const fn parse_state(state: u8) -> FileState {
    match state {
        b'.' | b' ' => FileState::Unmodified,
        b'M' => FileState::Modified,
        b'T' => FileState::TypeChanged,
        b'A' => FileState::Added,
        b'D' => FileState::Deleted,
        b'R' => FileState::Renamed,
        b'C' => FileState::Copied,
        b'U' => FileState::UpdatedButUnmerged,
        other => FileState::Unknown(other),
    }
}

fn parse_submodule(field: &[u8], record: usize) -> Result<SubmoduleStatus, StatusParseError> {
    match field {
        b"N..." => Ok(SubmoduleStatus::default()),
        [b'S', commit, tracked, untracked]
            if matches!(*commit, b'C' | b'.')
                && matches!(*tracked, b'M' | b'.')
                && matches!(*untracked, b'U' | b'.') =>
        {
            Ok(SubmoduleStatus {
                is_submodule: true,
                commit_changed: *commit == b'C',
                tracked_changes: *tracked == b'M',
                untracked_changes: *untracked == b'U',
            })
        }
        _ => Err(StatusParseError::new(record, "invalid submodule state")),
    }
}

fn parse_prefixed_count(
    value: &[u8],
    prefix: u8,
    record: usize,
    name: &str,
) -> Result<u64, StatusParseError> {
    let digits = value.strip_prefix(&[prefix]).ok_or_else(|| {
        StatusParseError::new(record, format!("{name} count has the wrong prefix"))
    })?;
    let text = std::str::from_utf8(digits)
        .map_err(|_| StatusParseError::new(record, format!("{name} count is not ASCII")))?;
    text.parse::<u64>()
        .map_err(|_| StatusParseError::new(record, format!("{name} count is invalid")))
}

fn parse_ascii_u16(value: &[u8], record: usize, name: &str) -> Result<u16, StatusParseError> {
    let text = std::str::from_utf8(value)
        .map_err(|_| StatusParseError::new(record, format!("{name} is not ASCII")))?;
    text.parse::<u16>()
        .map_err(|_| StatusParseError::new(record, format!("{name} is invalid")))
}

fn nonempty_path(path: &[u8], record: usize) -> Result<PathBuf, StatusParseError> {
    if path.is_empty() {
        Err(StatusParseError::new(record, "pathname is empty"))
    } else {
        Ok(path_from_bytes(path))
    }
}

fn as_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn discovery_directory(start: &Path) -> Result<PathBuf, GitError> {
    let mut current = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| GitError::Io {
                action: "read the current directory",
                source,
            })?
            .join(start)
    };

    loop {
        match fs::metadata(&current) {
            Ok(metadata) => {
                if metadata.is_file() {
                    current.pop();
                }
                return Ok(current);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !current.pop() {
                    return Err(GitError::Io {
                        action: "find an existing discovery directory",
                        source: error,
                    });
                }
            }
            Err(source) => {
                return Err(GitError::Io {
                    action: "inspect the repository path",
                    source,
                });
            }
        }
    }
}

fn checked_output(command: Command, action: &'static str) -> Result<Output, GitError> {
    checked_output_with_limits(command, action, GIT_COMMAND_LIMITS)
}

fn checked_output_with_limits(
    command: Command,
    action: &'static str,
    limits: CommandLimits,
) -> Result<Output, GitError> {
    let output = run_output_with_limits(command, action, limits)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failed(action, output))
    }
}

fn validate_branch_name(name: &str) -> Result<&str, GitError> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_BRANCH_NAME_BYTES {
        return Err(GitError::InvalidRevision(format!(
            "branch name must be 1..{MAX_BRANCH_NAME_BYTES} bytes after trimming"
        )));
    }
    if name == "HEAD" || name == "@" || name.starts_with('-') {
        return Err(GitError::InvalidRevision(format!(
            "refusing unsafe branch name {name:?}"
        )));
    }
    if name.contains("..")
        || name.contains("//")
        || name.contains('@')
        || name.contains('\\')
        || name.contains('\0')
        || name.ends_with('.')
        || name.ends_with('/')
        || name.contains("/.")
        || name.contains(".lock")
    {
        return Err(GitError::InvalidRevision(format!(
            "refusing unsafe branch name {name:?}"
        )));
    }
    if !name.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '/' | '+')
    }) {
        return Err(GitError::InvalidRevision(format!(
            "branch name contains unsupported characters: {name:?}"
        )));
    }
    Ok(name)
}

fn run_output(command: Command, action: &'static str) -> Result<Output, GitError> {
    run_output_with_limits(command, action, GIT_COMMAND_LIMITS)
}

#[derive(Debug)]
struct CapturedStream {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

type CaptureHandle = thread::JoinHandle<io::Result<CapturedStream>>;

/// Run one direct-argv command while bounding time and retained pipe data.
///
/// Both pipes are drained concurrently even after their retention limits are
/// reached. That keeps a noisy Git process from blocking on a full pipe while
/// also preventing its output from consuming unbounded editor memory.
fn run_output_with_limits(
    mut command: Command,
    action: &'static str,
    limits: CommandLimits,
) -> Result<Output, GitError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);

    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            GitError::GitUnavailable(source)
        } else {
            GitError::Io { action, source }
        }
    })?;
    let stdout = child.stdout.take().expect("configured piped stdout");
    let stderr = child.stderr.take().expect("configured piped stderr");
    let stdout_thread = thread::spawn(move || capture_stream(stdout, limits.stdout_bytes));
    let stderr_thread = thread::spawn(move || capture_stream(stderr, limits.stderr_bytes));

    let status = match wait_for_child(&mut child, started, limits.timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate_and_reap(&mut child);
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(GitError::TimedOut {
                action,
                timeout: limits.timeout,
            });
        }
        Err(source) => {
            terminate_and_reap(&mut child);
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(GitError::Io { action, source });
        }
    };

    // Git can exit after a hook or filter backgrounds a descendant that still
    // owns one of its pipes. Include pipe closure in the same deadline instead
    // of letting `join` turn that inherited descriptor into an editor hang.
    if !wait_for_captures(&stdout_thread, &stderr_thread, started, limits.timeout) {
        let _ = force_terminate_process(&mut child);
        drop(stdout_thread);
        drop(stderr_thread);
        return Err(GitError::TimedOut {
            action,
            timeout: limits.timeout,
        });
    }

    // Join both readers before propagating either error so neither pipe-reader
    // thread is accidentally detached from a completed command.
    let stdout = join_capture(stdout_thread);
    let stderr = join_capture(stderr_thread);
    let stdout = stdout.map_err(|source| GitError::Io { action, source })?;
    let stderr = stderr.map_err(|source| GitError::Io { action, source })?;

    if stdout.exceeded_limit {
        return Err(GitError::OutputLimitExceeded {
            action,
            stream: "stdout",
            limit: limits.stdout_bytes,
        });
    }
    if stderr.exceeded_limit {
        return Err(GitError::OutputLimitExceeded {
            action,
            stream: "stderr",
            limit: limits.stderr_bytes,
        });
    }

    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

fn capture_stream(mut stream: impl Read, limit: usize) -> io::Result<CapturedStream> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded_limit = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let retained = read.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded_limit |= retained < read;
    }
    Ok(CapturedStream {
        bytes,
        exceeded_limit,
    })
}

fn wait_for_child(
    child: &mut Child,
    started: Instant,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Ok(None);
        }
        thread::sleep(GIT_WAIT_INTERVAL.min(timeout.saturating_sub(elapsed)));
    }
}

fn wait_for_captures(
    stdout: &CaptureHandle,
    stderr: &CaptureHandle,
    started: Instant,
    timeout: Duration,
) -> bool {
    loop {
        if stdout.is_finished() && stderr.is_finished() {
            return true;
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return false;
        }
        thread::sleep(GIT_WAIT_INTERVAL.min(timeout.saturating_sub(elapsed)));
    }
}

fn join_capture(handle: CaptureHandle) -> io::Result<CapturedStream> {
    handle
        .join()
        .map_err(|_| io::Error::other("Git output reader thread panicked"))?
}

fn terminate_and_reap(child: &mut Child) {
    if force_terminate_process(child).is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // Keep Git and any hooks or filters it starts out of the editor's process
    // group so a timeout can terminate the whole command tree safely.
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn force_terminate_process(child: &mut Child) -> io::Result<()> {
    let pid = std::ffi::c_int::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child PID exceeds c_int"))?;
    // SAFETY: `pid` is the freshly spawned process-group leader configured
    // immediately before spawn, and SIGKILL contains no borrowed data.
    let result = unsafe { unix_kill(-pid, SIGKILL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn force_terminate_process(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
const SIGKILL: std::ffi::c_int = 9;

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn unix_kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
}

fn command_failed(action: &'static str, output: Output) -> GitError {
    GitError::CommandFailed {
        action,
        status: output.status,
        stderr: stderr_text(&output),
    }
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(trim_one_line_ending(&output.stderr)).into_owned()
}

fn trim_one_line_ending(mut bytes: &[u8]) -> &[u8] {
    if let Some(stripped) = bytes.strip_suffix(b"\n") {
        bytes = stripped;
    }
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Write as _;

    const RUNNER_HELPER_ENV: &str = "WSCRPT_GIT_RUNNER_TEST_MODE";

    // The `orphan` mode deliberately drops a live child so the outer runner
    // can prove that an inherited pipe cannot escape its process-group timeout.
    #[allow(clippy::zombie_processes)]
    #[test]
    fn bounded_runner_helper_process() {
        let Ok(mode) = env::var(RUNNER_HELPER_ENV) else {
            return;
        };
        match mode.as_str() {
            "stdout" => {
                std::io::stdout()
                    .write_all(&vec![b'o'; 128 * 1024])
                    .unwrap();
                std::io::stdout().flush().unwrap();
            }
            "stderr" => {
                std::io::stderr()
                    .write_all(&vec![b'e'; 128 * 1024])
                    .unwrap();
                std::io::stderr().flush().unwrap();
            }
            "fail" => {
                std::io::stderr()
                    .write_all(b"deliberate failure\n")
                    .unwrap();
                std::io::stderr().flush().unwrap();
                std::process::exit(7);
            }
            #[cfg(unix)]
            "orphan" => {
                let _descendant = runner_helper_command("sleep").spawn().unwrap();
            }
            "sleep" => thread::sleep(Duration::from_secs(30)),
            other => panic!("unknown bounded-runner helper mode {other:?}"),
        }
    }

    #[test]
    fn bounded_runner_caps_stdout_and_stderr() {
        for (mode, expected_stream) in [("stdout", "stdout"), ("stderr", "stderr")] {
            let limits = CommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: if mode == "stdout" { 128 } else { 512 * 1024 },
                stderr_bytes: if mode == "stderr" { 128 } else { 512 * 1024 },
            };
            let error = run_output_with_limits(
                runner_helper_command(mode),
                "exercise output limits",
                limits,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                GitError::OutputLimitExceeded {
                    action: "exercise output limits",
                    stream,
                    limit: 128,
                } if stream == expected_stream
            ));
        }
    }

    #[test]
    fn bounded_runner_preserves_failed_status_and_stderr() {
        let error =
            checked_output(runner_helper_command("fail"), "exercise command failure").unwrap_err();
        match error {
            GitError::CommandFailed {
                action,
                status,
                stderr,
            } => {
                assert_eq!(action, "exercise command failure");
                assert_eq!(status.code(), Some(7));
                assert!(stderr.contains("deliberate failure"));
            }
            other => panic!("expected preserved command failure, got {other:?}"),
        }
    }

    #[test]
    fn bounded_runner_times_out_and_reaps_the_child() {
        let timeout = Duration::from_millis(150);
        let started = Instant::now();
        let error = run_output_with_limits(
            runner_helper_command("sleep"),
            "exercise timeout",
            CommandLimits {
                timeout,
                stdout_bytes: 64 * 1024,
                stderr_bytes: 64 * 1024,
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GitError::TimedOut {
                action: "exercise timeout",
                timeout: actual,
            } if actual == timeout
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout cleanup did not return promptly"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_runner_times_out_when_a_descendant_keeps_its_pipes_open() {
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let error = run_output_with_limits(
            runner_helper_command("orphan"),
            "exercise inherited pipe timeout",
            CommandLimits {
                timeout,
                stdout_bytes: 64 * 1024,
                stderr_bytes: 64 * 1024,
            },
        )
        .unwrap_err();

        assert!(matches!(error, GitError::TimedOut { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "an inherited pipe escaped the command deadline"
        );
    }

    fn runner_helper_command(mode: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "git::tests::bounded_runner_helper_process",
                "--nocapture",
            ])
            .env(RUNNER_HELPER_ENV, mode);
        command
    }

    #[test]
    fn parses_branch_ordinary_rename_and_special_paths() {
        let input = concat!(
            "# branch.oid 0123456789abcdef\0",
            "# branch.head feature/ipad\0",
            "# branch.upstream origin/feature/ipad\0",
            "# branch.ab +12 -3\0",
            "1 M. N... 100644 100644 100644 abc def src/a file.rs\0",
            "2 R. N... 100644 100644 100644 abc def R087 src/new name.rs\0",
            "src/old name.rs\0",
            "? notes/line\nbreak.md\0",
            "! target/cache file\0",
        );
        let status = parse_porcelain_v2(input.as_bytes()).unwrap();

        assert_eq!(status.branch.name(), Some("feature/ipad"));
        assert_eq!(status.branch.oid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(
            status.branch.upstream.as_deref(),
            Some("origin/feature/ipad")
        );
        assert_eq!((status.branch.ahead, status.branch.behind), (12, 3));
        assert_eq!(status.files.len(), 4);
        assert_eq!(status.files[0].path, PathBuf::from("src/a file.rs"));
        assert_eq!(status.files[0].index, FileState::Modified);
        assert_eq!(status.files[1].kind, StatusEntryKind::Renamed { score: 87 });
        assert_eq!(status.files[1].path, PathBuf::from("src/new name.rs"));
        assert_eq!(
            status.files[1].original_path.as_deref(),
            Some(Path::new("src/old name.rs"))
        );
        assert_eq!(status.files[2].path, PathBuf::from("notes/line\nbreak.md"));
        assert_eq!(status.files[2].worktree, FileState::Untracked);
        assert_eq!(status.files[3].kind, StatusEntryKind::Ignored);
    }

    #[test]
    fn preserves_a_path_that_begins_with_spaces() {
        let status =
            parse_porcelain_v2(b"1 .M N... 100644 100644 100644 abc def   leading spaces.txt\0")
                .unwrap();
        assert_eq!(status.files[0].path, PathBuf::from("  leading spaces.txt"));
        assert_eq!(status.files[0].worktree, FileState::Modified);
    }

    #[test]
    fn parses_unborn_detached_and_unmerged_states() {
        let unborn = parse_porcelain_v2(
            b"# branch.oid (initial)\0# branch.head main\0u UU N... 100644 100644 100644 100644 a b c conflict.txt\0",
        )
        .unwrap();
        assert!(unborn.branch.unborn);
        assert_eq!(unborn.branch.name(), Some("main"));
        assert_eq!(unborn.files[0].kind, StatusEntryKind::Unmerged);
        assert_eq!(unborn.files[0].index, FileState::UpdatedButUnmerged);

        let detached = parse_porcelain_v2(b"# branch.oid abc\0# branch.head (detached)\0").unwrap();
        assert!(detached.branch.is_detached());
        assert_eq!(detached.branch.name(), None);
    }

    #[test]
    fn rejects_truncated_rename_records() {
        let error = parse_porcelain_v2(b"2 R. N... 100644 100644 100644 abc def R100 new.rs\0")
            .unwrap_err();
        assert!(error.message.contains("original path"));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let status = parse_porcelain_v2(b"? bad-\xff-name\0").unwrap();
        assert_eq!(
            status.files[0].path.as_os_str().as_bytes(),
            b"bad-\xff-name"
        );
    }

    #[test]
    fn real_repository_supports_read_only_discovery_status_and_diff() {
        if !git_available() {
            // Git is optional at build/test time.
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        if !git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            // An installed but administratively disabled Git should not make
            // the editor's parser test suite unusable.
            return;
        }
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w test")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w@example.invalid")
            ]
        ));

        let tracked = directory.path().join("tracked file.txt");
        fs::write(&tracked, "one\n").unwrap();
        assert!(git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                tracked.file_name().unwrap()
            ]
        ));
        assert!(git(
            directory.path(),
            [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new("base")]
        ));

        let nested = directory.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        let repository = GitRepository::discover(&nested).unwrap();
        assert_eq!(
            fs::canonicalize(repository.root()).unwrap(),
            fs::canonicalize(directory.path()).unwrap()
        );

        fs::write(&tracked, "one\ntwo\n").unwrap();
        let odd = directory.path().join("odd ; -- name.txt");
        fs::write(&odd, "safe\n").unwrap();
        let status = repository.status().unwrap();
        assert!(
            status
                .files
                .iter()
                .any(|file| file.path == Path::new("tracked file.txt"))
        );
        assert!(
            status
                .files
                .iter()
                .any(|file| file.path == Path::new("odd ; -- name.txt"))
        );
        assert!(!repository.diff_path(&tracked, false).unwrap().is_empty());
        assert!(odd.exists());
        let unchanged = repository.status().unwrap();
        assert!(unchanged.files.iter().any(|file| {
            file.path == Path::new("odd ; -- name.txt") && file.worktree == FileState::Untracked
        }));
    }

    #[test]
    fn recent_log_is_bounded_newest_first_and_read_only() {
        if !git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w git test")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-git@example.invalid")
            ]
        ));
        let file = directory.path().join("history.txt");
        for message in ["first", "second", "third"] {
            fs::write(&file, format!("{message}\n")).unwrap();
            assert!(git(
                directory.path(),
                [
                    OsStr::new("add"),
                    OsStr::new("--"),
                    OsStr::new("history.txt")
                ]
            ));
            assert!(git(
                directory.path(),
                [OsStr::new("commit"), OsStr::new("-qm"), OsStr::new(message)]
            ));
        }
        let repository = GitRepository::discover(directory.path()).unwrap();

        let log = String::from_utf8_lossy(&repository.recent_log(2).unwrap()).into_owned();

        assert!(log.contains("third"));
        assert!(log.contains("second"));
        assert!(!log.contains("first"));
        assert!(log.find("third").unwrap() < log.find("second").unwrap());
        assert!(repository.status().unwrap().files.is_empty());
    }

    #[test]
    fn file_history_is_scoped_to_the_requested_path() {
        if !git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w git test")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-git@example.invalid")
            ]
        ));
        let tracked = directory.path().join("tracked.txt");
        let other = directory.path().join("other.txt");
        fs::write(&tracked, "one\n").unwrap();
        assert!(git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("tracked.txt")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("tracked base")
            ]
        ));
        fs::write(&other, "other\n").unwrap();
        assert!(git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("other.txt")]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("other only")
            ]
        ));
        fs::write(&tracked, "two\n").unwrap();
        assert!(git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("tracked.txt")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("tracked update")
            ]
        ));
        let repository = GitRepository::discover(directory.path()).unwrap();

        let history =
            String::from_utf8_lossy(&repository.file_history(&tracked, 100).unwrap()).into_owned();

        assert!(history.contains("tracked update"));
        assert!(history.contains("tracked base"));
        assert!(!history.contains("other only"));
        assert!(history.find("tracked update").unwrap() < history.find("tracked base").unwrap());
        assert!(repository.status().unwrap().files.is_empty());
    }

    #[test]
    fn head_details_show_current_commit_stat_and_patch() {
        if !git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w git test")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-git@example.invalid")
            ]
        ));
        let file = directory.path().join("head.txt");
        fs::write(&file, "first\n").unwrap();
        assert!(git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("head.txt")]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("inspect head")
            ]
        ));
        let repository = GitRepository::discover(directory.path()).unwrap();

        let details = String::from_utf8_lossy(&repository.head_details().unwrap()).into_owned();

        assert!(details.contains("inspect head"));
        assert!(details.contains("head.txt"));
        assert!(details.contains("+first"));
        assert!(repository.status().unwrap().files.is_empty());
    }

    #[test]
    fn commit_details_accepts_only_hex_ids_and_shows_that_commit() {
        if !git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w git test")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-git@example.invalid")
            ]
        ));
        let file = directory.path().join("commit.txt");
        fs::write(&file, "first\n").unwrap();
        assert!(git(
            directory.path(),
            [
                OsStr::new("add"),
                OsStr::new("--"),
                OsStr::new("commit.txt")
            ]
        ));
        assert!(git(
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
        let repository = GitRepository::discover(directory.path()).unwrap();

        let details =
            String::from_utf8_lossy(&repository.commit_details(&commit).unwrap()).into_owned();

        assert!(details.contains("inspect explicit commit"));
        assert!(details.contains("commit.txt"));
        assert!(details.contains("+first"));
        assert!(matches!(
            repository.commit_details("--stat"),
            Err(GitError::InvalidRevision(_))
        ));
        assert!(matches!(
            repository.commit_details("HEAD"),
            Err(GitError::InvalidRevision(_))
        ));
        assert!(repository.status().unwrap().files.is_empty());
    }

    #[test]
    fn blame_line_reports_the_requested_committed_line() {
        if !git_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if !git(directory.path(), [OsStr::new("init"), OsStr::new("-q")]) {
            return;
        }
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("w git test")
            ]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("w-git@example.invalid")
            ]
        ));
        let file = directory.path().join("blame.txt");
        fs::write(&file, "first\nsecond\nthird\n").unwrap();
        assert!(git(
            directory.path(),
            [OsStr::new("add"), OsStr::new("--"), OsStr::new("blame.txt")]
        ));
        assert!(git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("-qm"),
                OsStr::new("blame base")
            ]
        ));
        let repository = GitRepository::discover(directory.path()).unwrap();

        let blame = repository.blame_line(&file, 2).unwrap();

        assert_eq!(blame.original_line, 2);
        assert_eq!(blame.final_line, 2);
        assert_eq!(blame.author.as_deref(), Some("w git test"));
        assert_eq!(blame.summary.as_deref(), Some("blame base"));
        assert_eq!(blame.filename.as_deref(), Some(Path::new("blame.txt")));
        assert_eq!(blame.content, "second");
    }

    fn git<const N: usize>(root: &Path, arguments: [&OsStr; N]) -> bool {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1");
        run_output(command, "run test Git command").is_ok_and(|output| output.status.success())
    }

    fn git_available() -> bool {
        let mut command = Command::new("git");
        command.arg("--version");
        run_output(command, "inspect Git availability").is_ok_and(|output| output.status.success())
    }
}
