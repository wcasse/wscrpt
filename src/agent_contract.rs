//! Pure agent-native contracts: work packets, events, Stickies, and review packets.
//!
//! These types are the host-side boundary for agent orchestration. They do not
//! speak ACP wire format, launch processes, or render UI. Validation is
//! deterministic and size-bounded so a coordinator can reject unsafe input
//! before any workspace mutation is considered.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// On-disk / in-memory schema version for agent contracts in this build.
pub const AGENT_CONTRACT_VERSION: u32 = 1;

/// Maximum UTF-8 bytes for a work-packet goal.
pub const MAX_GOAL_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes for a short event summary.
pub const MAX_SUMMARY_BYTES: usize = 512;
/// Maximum UTF-8 bytes for one relative path string.
pub const MAX_PATH_BYTES: usize = 4 * 1024;
/// Maximum path scopes (writable or protected) on one work packet.
pub const MAX_PATH_SCOPES: usize = 256;
/// Maximum required verification command entries on one work packet.
pub const MAX_REQUIRED_CHECKS: usize = 32;
/// Maximum argv elements per required check.
pub const MAX_CHECK_ARGV: usize = 64;
/// Maximum UTF-8 bytes for one argv element.
pub const MAX_ARGV_ELEMENT_BYTES: usize = 1_024;
/// Maximum UTF-8 bytes for identifiers (packet id, session id, sticky id).
pub const MAX_ID_BYTES: usize = 128;
/// Maximum UTF-8 bytes for a Git object id / ref string.
pub const MAX_GIT_OBJECT_BYTES: usize = 256;
/// Maximum UTF-8 bytes for an artifact reference (local path or opaque handle).
pub const MAX_ARTIFACT_REF_BYTES: usize = 1_024;
/// Maximum UTF-8 bytes for creator labels and sticky titles.
pub const MAX_LABEL_BYTES: usize = 256;
/// Maximum UTF-8 bytes for sticky Markdown body (content file, not layout).
pub const MAX_STICKY_BODY_BYTES: usize = 64 * 1024;
/// Maximum line comments retained on one review packet revision.
pub const MAX_REVIEW_COMMENTS: usize = 512;
/// Maximum UTF-8 bytes for one review comment body.
pub const MAX_COMMENT_BODY_BYTES: usize = 8 * 1024;
/// Maximum changed paths listed on one review packet.
pub const MAX_REVIEW_PATHS: usize = 1_024;
/// Maximum artifact refs on one review packet.
pub const MAX_REVIEW_ARTIFACTS: usize = 64;
/// Maximum sticky ids attached to one work packet.
pub const MAX_STICKIES_PER_PACKET: usize = 8;
/// Maximum UTF-8 bytes for a sticky body snapshot included as agent brief.
pub const MAX_STICKY_BRIEF_BYTES: usize = 4 * 1024;

/// Five-state activity model for one agent run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRunState {
    Brief,
    Working,
    NeedsYou,
    Review,
    Closed,
}

impl AgentRunState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Brief => "brief",
            Self::Working => "working",
            Self::NeedsYou => "needs_you",
            Self::Review => "review",
            Self::Closed => "closed",
        }
    }
}

/// Explicit authority granted by a work packet. Push remains a field for later
/// phases; v1 coordinators must not grant network merge/push side effects.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgentAuthority {
    pub edit: bool,
    pub command: bool,
    pub network: bool,
    pub commit: bool,
    pub push: bool,
}

impl AgentAuthority {
    /// Conservative default for a review-oriented beta run: edit + command only.
    pub const fn review_oriented() -> Self {
        Self {
            edit: true,
            command: true,
            network: false,
            commit: false,
            push: false,
        }
    }
}

/// Where the agent is allowed to write relative to the workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeBinding {
    /// Operate on the user's current tree (requires explicit choice later).
    CurrentTree { root: PathBuf },
    /// Isolated linked worktree at `path` under or beside the main root.
    LinkedWorktree { root: PathBuf, path: PathBuf },
}

impl WorktreeBinding {
    pub fn root(&self) -> &Path {
        match self {
            Self::CurrentTree { root } | Self::LinkedWorktree { root, .. } => root,
        }
    }

    pub fn work_path(&self) -> &Path {
        match self {
            Self::CurrentTree { root } => root,
            Self::LinkedWorktree { path, .. } => path,
        }
    }
}

/// A relative path prefix that defines scope. Empty path means the workspace
/// root. Absolute paths, `..`, and empty components after normalize are invalid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathScope {
    relative: PathBuf,
}

impl PathScope {
    /// Build a scope from a relative path. `.` and empty become the root scope.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ContractError> {
        let relative = normalize_relative_path(path.as_ref(), AllowEmpty::Yes)?;
        if path_utf8_len(&relative) > MAX_PATH_BYTES {
            return Err(ContractError::Oversized {
                field: "path_scope",
                limit: MAX_PATH_BYTES,
            });
        }
        Ok(Self { relative })
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }

    pub fn is_root(&self) -> bool {
        self.relative.as_os_str().is_empty()
    }

    /// True when `candidate` is this scope or a nested path under it.
    pub fn covers(&self, candidate: &Path) -> bool {
        if self.is_root() {
            return true;
        }
        candidate == self.relative.as_path() || candidate.starts_with(&self.relative)
    }
}

/// Contract between the user, wscrpt, and one agent run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkPacket {
    pub id: String,
    pub workspace_id: u64,
    pub goal: String,
    pub base_commit: Option<String>,
    pub worktree: WorktreeBinding,
    pub writable_paths: Vec<PathScope>,
    pub protected_paths: Vec<PathScope>,
    pub required_checks: Vec<Vec<String>>,
    pub authority: AgentAuthority,
    pub creator: String,
    pub created_at_unix_ms: u64,
    /// Sticky note ids explicitly included as agent context (user-attached only).
    pub sticky_ids: Vec<String>,
    /// Bounded snapshot of sticky body text for the agent brief (not a live link).
    pub sticky_brief: Option<String>,
}

impl WorkPacket {
    /// Validate sizes, path hygiene, and required fields. Does not touch disk.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_id(&self.id, "packet_id")?;
        validate_text_len(&self.goal, MAX_GOAL_BYTES, "goal")?;
        if self.goal.trim().is_empty() {
            return Err(ContractError::EmptyField("goal"));
        }
        if let Some(commit) = &self.base_commit {
            validate_text_len(commit, MAX_GIT_OBJECT_BYTES, "base_commit")?;
        }
        if self.writable_paths.len() > MAX_PATH_SCOPES {
            return Err(ContractError::TooMany {
                field: "writable_paths",
                limit: MAX_PATH_SCOPES,
            });
        }
        if self.protected_paths.len() > MAX_PATH_SCOPES {
            return Err(ContractError::TooMany {
                field: "protected_paths",
                limit: MAX_PATH_SCOPES,
            });
        }
        if self.writable_paths.is_empty() {
            return Err(ContractError::EmptyField("writable_paths"));
        }
        if self.required_checks.len() > MAX_REQUIRED_CHECKS {
            return Err(ContractError::TooMany {
                field: "required_checks",
                limit: MAX_REQUIRED_CHECKS,
            });
        }
        for (index, argv) in self.required_checks.iter().enumerate() {
            if argv.is_empty() {
                return Err(ContractError::EmptyField("required_checks.argv"));
            }
            if argv.len() > MAX_CHECK_ARGV {
                return Err(ContractError::TooMany {
                    field: "required_checks.argv",
                    limit: MAX_CHECK_ARGV,
                });
            }
            for element in argv {
                validate_text_len(element, MAX_ARGV_ELEMENT_BYTES, "required_checks.argv")?;
            }
            let _ = index;
        }
        validate_text_len(&self.creator, MAX_LABEL_BYTES, "creator")?;
        if self.creator.trim().is_empty() {
            return Err(ContractError::EmptyField("creator"));
        }
        // Push is recorded for later phases but must not be enabled in v1 packets.
        if self.authority.push {
            return Err(ContractError::AuthorityNotGranted("push"));
        }
        if self.sticky_ids.len() > MAX_STICKIES_PER_PACKET {
            return Err(ContractError::TooMany {
                field: "sticky_ids",
                limit: MAX_STICKIES_PER_PACKET,
            });
        }
        for id in &self.sticky_ids {
            validate_id(id, "sticky_id")?;
        }
        if let Some(brief) = &self.sticky_brief {
            validate_text_len(brief, MAX_STICKY_BRIEF_BYTES, "sticky_brief")?;
        }
        Ok(())
    }

    /// Whether a relative path may be reported/touched under this packet.
    ///
    /// Protected scopes win. A path must fall under at least one writable
    /// scope. Invalid relative paths are rejected.
    pub fn allows_path(&self, path: &Path) -> Result<bool, ContractError> {
        let relative = normalize_relative_path(path, AllowEmpty::No)?;
        if path_utf8_len(&relative) > MAX_PATH_BYTES {
            return Err(ContractError::Oversized {
                field: "path",
                limit: MAX_PATH_BYTES,
            });
        }
        if self
            .protected_paths
            .iter()
            .any(|scope| scope.covers(&relative))
        {
            return Ok(false);
        }
        Ok(self
            .writable_paths
            .iter()
            .any(|scope| scope.covers(&relative)))
    }
}

/// Kinds carried on the bounded agent event stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentEventKind {
    State,
    Plan,
    Approval,
    PathTouched,
    CheckResult,
    Artifact,
    ReviewReady,
    Notice,
}

impl AgentEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Plan => "plan",
            Self::Approval => "approval",
            Self::PathTouched => "path_touched",
            Self::CheckResult => "check_result",
            Self::Artifact => "artifact",
            Self::ReviewReady => "review_ready",
            Self::Notice => "notice",
        }
    }

    /// Path-touched events require a path; other kinds may carry optional refs.
    pub const fn requires_path(self) -> bool {
        matches!(self, Self::PathTouched)
    }
}

/// One bounded receipt event from an agent run.
///
/// Detailed tool payloads and source text stay out of this shape. References
/// open local artifacts on demand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEvent {
    pub workspace_id: u64,
    pub session_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    pub kind: AgentEventKind,
    pub summary: String,
    pub path: Option<PathBuf>,
    pub git_object: Option<String>,
    pub artifact_ref: Option<String>,
    pub check_ok: Option<bool>,
    pub run_state: Option<AgentRunState>,
    pub sensitive: bool,
}

impl AgentEvent {
    /// Structural validation only (sizes and kind constraints). Scope and
    /// generation checks belong to the coordinator.
    pub fn validate_structure(&self) -> Result<(), ContractError> {
        validate_id(&self.session_id, "session_id")?;
        if self.sequence == 0 {
            return Err(ContractError::InvalidSequence);
        }
        validate_text_len(&self.summary, MAX_SUMMARY_BYTES, "summary")?;
        if self.summary.trim().is_empty() {
            return Err(ContractError::EmptyField("summary"));
        }
        if let Some(path) = &self.path {
            let relative = normalize_relative_path(path, AllowEmpty::No)?;
            if path_utf8_len(&relative) > MAX_PATH_BYTES {
                return Err(ContractError::Oversized {
                    field: "path",
                    limit: MAX_PATH_BYTES,
                });
            }
        } else if self.kind.requires_path() {
            return Err(ContractError::MissingPath);
        }
        if let Some(object) = &self.git_object {
            validate_text_len(object, MAX_GIT_OBJECT_BYTES, "git_object")?;
        }
        if let Some(artifact) = &self.artifact_ref {
            validate_text_len(artifact, MAX_ARTIFACT_REF_BYTES, "artifact_ref")?;
        }
        if self.kind == AgentEventKind::State && self.run_state.is_none() {
            return Err(ContractError::MissingRunState);
        }
        if self.kind == AgentEventKind::CheckResult && self.check_ok.is_none() {
            return Err(ContractError::MissingCheckResult);
        }
        Ok(())
    }
}

/// Sticky notes are spatial working context backed by Markdown files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StickyStore {
    /// `.wscrpt/stickies/<id>.md` — shareable with the repository when committed.
    Team,
    /// `$XDG_STATE_HOME/wscrpt/stickies/<workspace-id>/<id>.md` — personal only.
    Personal,
}

/// What a sticky is pinned to. Layout (x/y/size) is device-local and never part
/// of this content contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StickyAnchor {
    Workspace,
    File {
        path: PathBuf,
    },
    Selection {
        path: PathBuf,
        base_blob: String,
        start_line: u32,
        end_line: u32,
        context_hash: String,
    },
    Commit {
        object: String,
    },
    /// Preview session identity only — never an auth token or raw media URL.
    PreviewSession {
        session_id: String,
    },
}

/// Content-side sticky contract (geometry lives in a separate layout store).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickyNote {
    pub id: String,
    pub store: StickyStore,
    pub title: String,
    pub body_markdown: String,
    pub anchor: StickyAnchor,
    pub archived: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl StickyNote {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_id(&self.id, "sticky_id")?;
        validate_text_len(&self.title, MAX_LABEL_BYTES, "sticky_title")?;
        validate_text_len(&self.body_markdown, MAX_STICKY_BODY_BYTES, "sticky_body")?;
        match &self.anchor {
            StickyAnchor::Workspace => {}
            StickyAnchor::File { path } => {
                normalize_relative_path(path, AllowEmpty::No)?;
            }
            StickyAnchor::Selection {
                path,
                base_blob,
                start_line,
                end_line,
                context_hash,
            } => {
                normalize_relative_path(path, AllowEmpty::No)?;
                validate_text_len(base_blob, MAX_GIT_OBJECT_BYTES, "base_blob")?;
                validate_text_len(context_hash, MAX_GIT_OBJECT_BYTES, "context_hash")?;
                if *end_line < *start_line {
                    return Err(ContractError::InvalidSelectionRange);
                }
            }
            StickyAnchor::Commit { object } => {
                validate_text_len(object, MAX_GIT_OBJECT_BYTES, "commit")?;
            }
            StickyAnchor::PreviewSession { session_id } => {
                validate_id(session_id, "preview_session_id")?;
            }
        }
        Ok(())
    }
}

/// Review lifecycle for asynchronous Git/worktree packets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewState {
    Open,
    Approved,
    ChangesRequested,
    Superseded,
    Closed,
}

/// One line-level review comment. Offline replay is idempotent via `client_nonce`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewComment {
    pub packet_revision: u32,
    pub path: PathBuf,
    pub base_blob: String,
    pub line: Option<u32>,
    pub context_hash: Option<String>,
    pub body_markdown: String,
    pub client_nonce: String,
    pub created_at_unix_ms: u64,
}

impl ReviewComment {
    pub fn validate(&self) -> Result<(), ContractError> {
        normalize_relative_path(&self.path, AllowEmpty::No)?;
        validate_text_len(&self.base_blob, MAX_GIT_OBJECT_BYTES, "base_blob")?;
        if let Some(hash) = &self.context_hash {
            validate_text_len(hash, MAX_GIT_OBJECT_BYTES, "context_hash")?;
        }
        validate_text_len(&self.body_markdown, MAX_COMMENT_BODY_BYTES, "comment_body")?;
        if self.body_markdown.trim().is_empty() {
            return Err(ContractError::EmptyField("comment_body"));
        }
        validate_id(&self.client_nonce, "client_nonce")?;
        Ok(())
    }
}

/// Asynchronous review packet naming exact base/head and changed paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPacket {
    pub id: String,
    pub workspace_id: u64,
    pub base_object: String,
    pub head_object: String,
    pub changed_paths: Vec<PathBuf>,
    pub check_results: Vec<(String, bool)>,
    pub artifact_refs: Vec<String>,
    pub revision: u32,
    pub state: ReviewState,
    pub comments: Vec<ReviewComment>,
    pub created_at_unix_ms: u64,
}

impl ReviewPacket {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_id(&self.id, "review_id")?;
        validate_text_len(&self.base_object, MAX_GIT_OBJECT_BYTES, "base_object")?;
        validate_text_len(&self.head_object, MAX_GIT_OBJECT_BYTES, "head_object")?;
        if self.changed_paths.len() > MAX_REVIEW_PATHS {
            return Err(ContractError::TooMany {
                field: "changed_paths",
                limit: MAX_REVIEW_PATHS,
            });
        }
        for path in &self.changed_paths {
            normalize_relative_path(path, AllowEmpty::No)?;
        }
        if self.artifact_refs.len() > MAX_REVIEW_ARTIFACTS {
            return Err(ContractError::TooMany {
                field: "artifact_refs",
                limit: MAX_REVIEW_ARTIFACTS,
            });
        }
        for artifact in &self.artifact_refs {
            validate_text_len(artifact, MAX_ARTIFACT_REF_BYTES, "artifact_ref")?;
        }
        if self.comments.len() > MAX_REVIEW_COMMENTS {
            return Err(ContractError::TooMany {
                field: "comments",
                limit: MAX_REVIEW_COMMENTS,
            });
        }
        for comment in &self.comments {
            comment.validate()?;
            if comment.packet_revision > self.revision {
                return Err(ContractError::CommentRevisionAhead);
            }
        }
        Ok(())
    }
}

/// Contract validation failures (no I/O).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    EmptyField(&'static str),
    Oversized { field: &'static str, limit: usize },
    TooMany { field: &'static str, limit: usize },
    InvalidId(&'static str),
    InvalidPath(PathBuf),
    MissingPath,
    MissingRunState,
    MissingCheckResult,
    InvalidSequence,
    InvalidSelectionRange,
    CommentRevisionAhead,
    AuthorityNotGranted(&'static str),
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "{field} must not be empty"),
            Self::Oversized { field, limit } => {
                write!(f, "{field} exceeds limit of {limit} bytes")
            }
            Self::TooMany { field, limit } => {
                write!(f, "{field} exceeds limit of {limit} entries")
            }
            Self::InvalidId(field) => write!(f, "{field} is not a valid identifier"),
            Self::InvalidPath(path) => write!(f, "invalid relative path: {}", path.display()),
            Self::MissingPath => write!(f, "event kind requires a path"),
            Self::MissingRunState => write!(f, "state event requires run_state"),
            Self::MissingCheckResult => write!(f, "check_result event requires check_ok"),
            Self::InvalidSequence => write!(f, "sequence must be non-zero"),
            Self::InvalidSelectionRange => write!(f, "selection end_line must be >= start_line"),
            Self::CommentRevisionAhead => {
                write!(f, "comment revision cannot exceed packet revision")
            }
            Self::AuthorityNotGranted(name) => {
                write!(
                    f,
                    "authority `{name}` is not granted in this contract version"
                )
            }
        }
    }
}

impl std::error::Error for ContractError {}

/// Current wall-clock milliseconds since UNIX epoch (non-critical for tests).
pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy)]
enum AllowEmpty {
    Yes,
    No,
}

/// Normalize a workspace-relative path: no absolute roots, no escaping `..`.
fn normalize_relative_path(path: &Path, empty: AllowEmpty) -> Result<PathBuf, ContractError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                if part.is_empty() {
                    return Err(ContractError::InvalidPath(path.to_path_buf()));
                }
                normalized.push(part);
            }
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ContractError::InvalidPath(path.to_path_buf()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ContractError::InvalidPath(path.to_path_buf()));
            }
        }
    }
    if matches!(empty, AllowEmpty::No) && normalized.as_os_str().is_empty() {
        return Err(ContractError::InvalidPath(path.to_path_buf()));
    }
    Ok(normalized)
}

fn path_utf8_len(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn validate_text_len(text: &str, limit: usize, field: &'static str) -> Result<(), ContractError> {
    if text.len() > limit {
        return Err(ContractError::Oversized { field, limit });
    }
    Ok(())
}

/// Validate a packet/session/sticky identifier (shared with the coordinator).
pub fn validate_id(id: &str, field: &'static str) -> Result<(), ContractError> {
    validate_text_len(id, MAX_ID_BYTES, field)?;
    if id.is_empty() {
        return Err(ContractError::EmptyField(field));
    }
    let valid = id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.');
    if !valid {
        return Err(ContractError::InvalidId(field));
    }
    Ok(())
}

/// Normalize a non-empty workspace-relative path (no absolute roots, no `..` escape).
pub fn normalize_relative(path: &Path) -> Result<PathBuf, ContractError> {
    normalize_relative_path(path, AllowEmpty::No)
}

/// Build a minimal valid packet for tests and the fake agent.
pub fn sample_work_packet(workspace_id: u64) -> WorkPacket {
    WorkPacket {
        id: "pkt-sample-1".to_owned(),
        workspace_id,
        goal: "Fix the bounded admission tests".to_owned(),
        base_commit: Some("abc1234".to_owned()),
        worktree: WorktreeBinding::CurrentTree {
            root: PathBuf::from("/tmp/wscrpt-workspace"),
        },
        writable_paths: vec![PathScope::new("src").expect("scope")],
        protected_paths: vec![PathScope::new(".wscrpt").expect("scope")],
        required_checks: vec![vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--locked".to_owned(),
        ]],
        authority: AgentAuthority::review_oriented(),
        creator: "local-user".to_owned(),
        created_at_unix_ms: 1_700_000_000_000,
        sticky_ids: Vec::new(),
        sticky_brief: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_packet_validates() {
        sample_work_packet(1).validate().expect("valid sample");
    }

    #[test]
    fn path_scope_covers_nested_and_rejects_siblings() {
        let scope = PathScope::new("src/agent").unwrap();
        assert!(scope.covers(Path::new("src/agent")));
        assert!(scope.covers(Path::new("src/agent/mod.rs")));
        assert!(!scope.covers(Path::new("src/app.rs")));
        assert!(!scope.covers(Path::new("src")));
    }

    #[test]
    fn protected_paths_win_over_writable() {
        let packet = sample_work_packet(1);
        assert_eq!(packet.allows_path(Path::new("src/main.rs")), Ok(true));
        assert_eq!(
            packet.allows_path(Path::new(".wscrpt/tasks.toml")),
            Ok(false)
        );
        assert_eq!(packet.allows_path(Path::new("docs/README.md")), Ok(false));
    }

    #[test]
    fn absolute_and_parent_paths_are_invalid() {
        assert!(PathScope::new("/etc/passwd").is_err());
        assert!(PathScope::new("../escape").is_err());
        assert!(
            sample_work_packet(1)
                .allows_path(Path::new("../secrets"))
                .is_err()
        );
    }

    #[test]
    fn push_authority_is_rejected_in_v1() {
        let mut packet = sample_work_packet(1);
        packet.authority.push = true;
        assert_eq!(
            packet.validate(),
            Err(ContractError::AuthorityNotGranted("push"))
        );
    }

    #[test]
    fn oversized_summary_fails_event_validation() {
        let event = AgentEvent {
            workspace_id: 1,
            session_id: "run-1".to_owned(),
            generation: 1,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::Notice,
            summary: "x".repeat(MAX_SUMMARY_BYTES + 1),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert!(matches!(
            event.validate_structure(),
            Err(ContractError::Oversized {
                field: "summary",
                ..
            })
        ));
    }

    #[test]
    fn sticky_and_review_contracts_validate() {
        let sticky = StickyNote {
            id: "sticky-1".to_owned(),
            store: StickyStore::Personal,
            title: "Remember".to_owned(),
            body_markdown: "- ship W0\n".to_owned(),
            anchor: StickyAnchor::File {
                path: PathBuf::from("src/agent.rs"),
            },
            archived: false,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        sticky.validate().expect("sticky");

        let review = ReviewPacket {
            id: "rev-1".to_owned(),
            workspace_id: 1,
            base_object: "aaa".to_owned(),
            head_object: "bbb".to_owned(),
            changed_paths: vec![PathBuf::from("src/agent.rs")],
            check_results: vec![("cargo test".to_owned(), true)],
            artifact_refs: vec![],
            revision: 1,
            state: ReviewState::Open,
            comments: vec![ReviewComment {
                packet_revision: 1,
                path: PathBuf::from("src/agent.rs"),
                base_blob: "aaa".to_owned(),
                line: Some(10),
                context_hash: Some("ctx".to_owned()),
                body_markdown: "nit: name".to_owned(),
                client_nonce: "nonce-1".to_owned(),
                created_at_unix_ms: 1,
            }],
            created_at_unix_ms: 1,
        };
        review.validate().expect("review");
    }
}
