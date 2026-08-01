//! Versioned persistence for the last workspace's navigational and UI state.
//!
//! A session remembers *where* the user was: the workspace root, disk-backed
//! files, selections, viewports, recent paths, and a few layout choices. It
//! intentionally never stores document contents. Dirty and untitled buffers
//! belong to [`crate::recovery`] journals, whose lifetime and safety rules are
//! different from this best-effort convenience state.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// On-disk workspace-session schema understood by this build.
pub const SESSION_FORMAT_VERSION: u32 = 2;
const LEGACY_SESSION_FORMAT_VERSION: u32 = 1;

const APPLICATION_DIRECTORY: &str = "wscrpt";
const SESSION_FILENAME: &str = "session.toml";
const MAX_SESSION_BYTES: u64 = 1024 * 1024;
const MAX_OPEN_FILES: usize = 4_096;
const MAX_RECENT_FILES: usize = 4_096;
const MAX_BOOKMARKS: usize = 4_096;
const TEMP_CREATE_ATTEMPTS: usize = 32;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Everything needed to restore one disk-backed editor buffer except its text.
///
/// Cursor and anchor are Unicode scalar-value indices, matching `Editor`.
/// They cannot be checked against document length until the file is opened, so
/// restoration code must clamp them to the freshly loaded document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenFileState {
    pub path: PathBuf,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub viewport: ViewportState,
}

/// One persisted source bookmark. Bookmarks deliberately store only disk path
/// and cursor, never editor text or virtual/untitled buffer identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookmarkState {
    pub path: PathBuf,
    pub cursor: usize,
}

/// Scroll offsets for one open file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewportState {
    pub top_line: usize,
    /// Line-local character offset near the first soft-wrapped row. The
    /// renderer normalizes it after restoring at the current terminal width.
    #[serde(default)]
    pub top_wrap_char: usize,
    pub left_column: usize,
}

/// Persisted UI choices that affect the workspace layout.
///
/// These are flags rather than transient dimensions: terminal sizes frequently
/// change across Blink, mosh, and SSH reconnects, so pixel/cell geometry must be
/// recomputed by the renderer instead of restored.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutFlags {
    pub workspace_tree_visible: bool,
    pub problems_visible: bool,
    pub soft_wrap: bool,
    /// Bottom agent dashboard panel (Grok Build–style roster strip).
    #[serde(default)]
    pub agent_dashboard_visible: bool,
}

#[derive(Deserialize)]
struct SessionVersion {
    version: u32,
}

/// Decoder for the 0.1 session schema. Embedded-terminal visibility was
/// transient UI state, so migration intentionally drops those two flags while
/// retaining every navigational and supported layout field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySessionV1 {
    version: u32,
    root: PathBuf,
    open_files: Vec<OpenFileState>,
    active_index: usize,
    recent_files: Vec<PathBuf>,
    #[serde(default)]
    bookmarks: Vec<BookmarkState>,
    #[serde(default)]
    layout: LegacyLayoutFlagsV1,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacyLayoutFlagsV1 {
    workspace_tree_visible: bool,
    terminal_visible: bool,
    terminal_split_visible: bool,
    problems_visible: bool,
    soft_wrap: bool,
}

impl From<LegacySessionV1> for Session {
    fn from(legacy: LegacySessionV1) -> Self {
        let LegacyLayoutFlagsV1 {
            workspace_tree_visible,
            terminal_visible,
            terminal_split_visible,
            problems_visible,
            soft_wrap,
        } = legacy.layout;
        let _ = (terminal_visible, terminal_split_visible);
        Self {
            version: SESSION_FORMAT_VERSION,
            root: legacy.root,
            open_files: legacy.open_files,
            active_index: legacy.active_index,
            recent_files: legacy.recent_files,
            bookmarks: legacy.bookmarks,
            layout: LayoutFlags {
                workspace_tree_visible,
                problems_visible,
                soft_wrap,
                agent_dashboard_visible: false,
            },
        }
    }
}

/// A versioned snapshot of workspace navigation state.
///
/// `open_files` only names files on disk. There is deliberately no serialized
/// buffer text in this type; unsaved content is owned by the recovery module.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub version: u32,
    pub root: PathBuf,
    pub open_files: Vec<OpenFileState>,
    pub active_index: usize,
    pub recent_files: Vec<PathBuf>,
    #[serde(default)]
    pub bookmarks: Vec<BookmarkState>,
    #[serde(default)]
    pub layout: LayoutFlags,
}

/// Descriptive aliases for callers that prefer workspace-specific names.
pub type WorkspaceSession = Session;
pub type SessionFile = OpenFileState;
pub type SessionViewport = ViewportState;
pub type SessionLayout = LayoutFlags;

impl Session {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            version: SESSION_FORMAT_VERSION,
            root: root.into(),
            open_files: Vec::new(),
            active_index: 0,
            recent_files: Vec::new(),
            bookmarks: Vec::new(),
            layout: LayoutFlags::default(),
        }
    }

    /// Validate structural invariants without touching any remembered path.
    ///
    /// Files are allowed to have moved or disappeared between sessions. The
    /// loader therefore validates representation and indexing, not existence.
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion {
                found: self.version,
                supported: SESSION_FORMAT_VERSION,
            });
        }
        validate_absolute_path("root", &self.root)?;

        if self.open_files.len() > MAX_OPEN_FILES {
            return Err(SessionError::InvalidSession(format!(
                "open_files contains {} entries; the limit is {MAX_OPEN_FILES}",
                self.open_files.len()
            )));
        }
        if self.recent_files.len() > MAX_RECENT_FILES {
            return Err(SessionError::InvalidSession(format!(
                "recent_files contains {} entries; the limit is {MAX_RECENT_FILES}",
                self.recent_files.len()
            )));
        }
        if self.bookmarks.len() > MAX_BOOKMARKS {
            return Err(SessionError::InvalidSession(format!(
                "bookmarks contains {} entries; the limit is {MAX_BOOKMARKS}",
                self.bookmarks.len()
            )));
        }

        if self.open_files.is_empty() {
            if self.active_index != 0 {
                return Err(SessionError::InvalidSession(
                    "active_index must be 0 when no disk-backed files are open".to_owned(),
                ));
            }
        } else if self.active_index >= self.open_files.len() {
            return Err(SessionError::InvalidSession(format!(
                "active_index {} is outside {} open files",
                self.active_index,
                self.open_files.len()
            )));
        }

        for (index, file) in self.open_files.iter().enumerate() {
            validate_absolute_path(&format!("open_files[{index}].path"), &file.path)?;
        }
        for (index, path) in self.recent_files.iter().enumerate() {
            validate_absolute_path(&format!("recent_files[{index}]"), path)?;
        }
        for (index, bookmark) in self.bookmarks.iter().enumerate() {
            validate_absolute_path(&format!("bookmarks[{index}].path"), &bookmark.path)?;
        }

        reject_duplicate_paths(
            "open_files",
            self.open_files.iter().map(|file| file.path.as_path()),
        )?;
        reject_duplicate_paths(
            "recent_files",
            self.recent_files.iter().map(PathBuf::as_path),
        )?;
        reject_duplicate_bookmarks(&self.bookmarks)?;
        Ok(())
    }
}

/// Storage for the single last-workspace session file.
///
/// `SessionStore::new` accepts the full `session.toml` path, which makes tests
/// independent of process environment. Application code should normally use
/// [`SessionStore::from_env`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// Resolve the XDG session path without creating it.
    pub fn from_env() -> Result<Self, SessionError> {
        Ok(Self::new(session_path()?))
    }

    /// Store a session at an explicit file path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load and validate the session, returning `None` when it does not exist.
    ///
    /// Reads are size-bounded and non-regular entries (including symlinks) are
    /// rejected. Unknown fields are rejected by the schema, which also prevents
    /// a recovery journal containing document text from being accepted here.
    pub fn load(&self) -> Result<Option<Session>, SessionError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(SessionError::io("inspect session", &self.path, source)),
        };
        if !metadata.file_type().is_file() {
            return Err(SessionError::InvalidSessionFile(self.path.clone()));
        }
        if metadata.len() > MAX_SESSION_BYTES {
            return Err(SessionError::SessionTooLarge {
                path: self.path.clone(),
                limit: MAX_SESSION_BYTES,
            });
        }

        let file = File::open(&self.path)
            .map_err(|source| SessionError::io("open session", &self.path, source))?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_SESSION_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| SessionError::io("read session", &self.path, source))?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            return Err(SessionError::SessionTooLarge {
                path: self.path.clone(),
                limit: MAX_SESSION_BYTES,
            });
        }
        let source =
            String::from_utf8(bytes).map_err(|_| SessionError::InvalidUtf8(self.path.clone()))?;
        let header: SessionVersion =
            toml::from_str(&source).map_err(|source| SessionError::Decode {
                path: self.path.clone(),
                source,
            })?;
        let session = match header.version {
            SESSION_FORMAT_VERSION => {
                toml::from_str::<Session>(&source).map_err(|source| SessionError::Decode {
                    path: self.path.clone(),
                    source,
                })?
            }
            LEGACY_SESSION_FORMAT_VERSION => {
                let legacy = toml::from_str::<LegacySessionV1>(&source).map_err(|source| {
                    SessionError::Decode {
                        path: self.path.clone(),
                        source,
                    }
                })?;
                debug_assert_eq!(legacy.version, LEGACY_SESSION_FORMAT_VERSION);
                Session::from(legacy)
            }
            found => {
                return Err(SessionError::UnsupportedVersion {
                    found,
                    supported: SESSION_FORMAT_VERSION,
                });
            }
        };
        session.validate()?;
        Ok(Some(session))
    }

    /// Atomically create or replace the session after syncing it to disk.
    ///
    /// The temporary file is created beside the destination, written, flushed,
    /// and fsynced before rename. The containing directory is fsynced after the
    /// rename so the publication itself is durable across a power loss.
    pub fn save(&self, session: &Session) -> Result<PathBuf, SessionError> {
        session.validate()?;
        let mut encoded = toml::to_string_pretty(session).map_err(SessionError::Encode)?;
        encoded.push('\n');
        if encoded.len() as u64 > MAX_SESSION_BYTES {
            return Err(SessionError::SessionTooLarge {
                path: self.path.clone(),
                limit: MAX_SESSION_BYTES,
            });
        }

        let directory = self.ensure_directory()?;
        let (mut file, temporary) = self.create_temporary_file(&directory)?;
        let mut cleanup = TemporaryCleanup::new(temporary.clone());

        file.write_all(encoded.as_bytes())
            .map_err(|source| SessionError::io("write session", &temporary, source))?;
        file.flush()
            .map_err(|source| SessionError::io("flush session", &temporary, source))?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| SessionError::io("set session permissions", &temporary, source))?;
        file.sync_all()
            .map_err(|source| SessionError::io("sync session", &temporary, source))?;
        drop(file);

        fs::rename(&temporary, &self.path)
            .map_err(|source| SessionError::io("publish session", &self.path, source))?;
        cleanup.disarm();
        sync_directory(&directory)?;
        Ok(self.path.clone())
    }

    /// Remove the convenience session without touching recovery journals.
    pub fn remove(&self) -> Result<bool, SessionError> {
        let directory = parent_directory(&self.path)?;
        match fs::remove_file(&self.path) {
            Ok(()) => {
                sync_directory(&directory)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SessionError::io("remove session", &self.path, source)),
        }
    }

    fn ensure_directory(&self) -> Result<PathBuf, SessionError> {
        let directory = parent_directory(&self.path)?;
        fs::create_dir_all(&directory)
            .map_err(|source| SessionError::io("create session directory", &directory, source))?;
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|source| SessionError::io("inspect session directory", &directory, source))?;
        if !metadata.file_type().is_dir() {
            return Err(SessionError::InvalidStateDirectory(directory));
        }
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|source| {
            SessionError::io("set session directory permissions", &directory, source)
        })?;
        Ok(directory)
    }

    fn create_temporary_file(&self, directory: &Path) -> Result<(File, PathBuf), SessionError> {
        let stem = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(SESSION_FILENAME);
        let mut last_collision = None;
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temporary =
                directory.join(format!(".{stem}.tmp-{:08x}-{sequence:016x}", process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&temporary) {
                Ok(file) => return Ok((file, temporary)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                Err(source) => {
                    return Err(SessionError::io(
                        "create temporary session",
                        temporary,
                        source,
                    ));
                }
            }
        }
        Err(SessionError::io(
            "create temporary session",
            directory,
            last_collision.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "temporary session name collision",
                )
            }),
        ))
    }
}

/// Resolve `$XDG_STATE_HOME/wscrpt/session.toml`, falling back to
/// `$HOME/.local/state/wscrpt/session.toml` per the XDG Base Directory spec.
///
/// This is a sibling of, not a replacement for, the `recovery/` directory.
pub fn session_path() -> Result<PathBuf, SessionError> {
    session_path_from(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))
}

/// Save using the standard XDG session path.
pub fn save_session(session: &Session) -> Result<PathBuf, SessionError> {
    SessionStore::from_env()?.save(session)
}

/// Load using the standard XDG session path.
pub fn load_session() -> Result<Option<Session>, SessionError> {
    SessionStore::from_env()?.load()
}

/// Remove the standard XDG session file, leaving recovery journals intact.
pub fn remove_session() -> Result<bool, SessionError> {
    SessionStore::from_env()?.remove()
}

fn session_path_from(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, SessionError> {
    if let Some(value) = xdg_state_home.filter(|value| !value.is_empty()) {
        let root = PathBuf::from(value);
        if root.is_absolute() {
            return Ok(root.join(APPLICATION_DIRECTORY).join(SESSION_FILENAME));
        }
        // Relative XDG paths are ignored by specification.
    }
    let home = home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(SessionError::StateDirectoryUnavailable)?;
    Ok(home
        .join(".local/state")
        .join(APPLICATION_DIRECTORY)
        .join(SESSION_FILENAME))
}

fn parent_directory(path: &Path) -> Result<PathBuf, SessionError> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| SessionError::InvalidSessionPath(path.to_path_buf()))
}

fn validate_absolute_path(field: &str, path: &Path) -> Result<(), SessionError> {
    if path.as_os_str().is_empty() {
        return Err(SessionError::InvalidSession(format!(
            "{field} must not be empty"
        )));
    }
    if !path.is_absolute() {
        return Err(SessionError::InvalidSession(format!(
            "{field} must be absolute: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_duplicate_paths<'a>(
    field: &str,
    paths: impl Iterator<Item = &'a Path>,
) -> Result<(), SessionError> {
    let mut seen: Vec<&Path> = Vec::new();
    for path in paths {
        if seen.contains(&path) {
            return Err(SessionError::InvalidSession(format!(
                "{field} contains duplicate path {}",
                path.display()
            )));
        }
        seen.push(path);
    }
    Ok(())
}

fn reject_duplicate_bookmarks(bookmarks: &[BookmarkState]) -> Result<(), SessionError> {
    let mut seen: Vec<(&Path, usize)> = Vec::new();
    for bookmark in bookmarks {
        let identity = (bookmark.path.as_path(), bookmark.cursor);
        if seen.contains(&identity) {
            return Err(SessionError::InvalidSession(format!(
                "bookmarks contains duplicate location {} @ char {}",
                bookmark.path.display(),
                bookmark.cursor + 1
            )));
        }
        seen.push(identity);
    }
    Ok(())
}

fn sync_directory(directory: &Path) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        let file = File::open(directory).map_err(|source| {
            SessionError::io("open session directory for sync", directory, source)
        })?;
        file.sync_all()
            .map_err(|source| SessionError::io("sync session directory", directory, source))?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

struct TemporaryCleanup {
    path: PathBuf,
    armed: bool,
}

impl TemporaryCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Errors produced while resolving, validating, or persisting a session.
#[derive(Debug)]
pub enum SessionError {
    StateDirectoryUnavailable,
    InvalidSessionPath(PathBuf),
    InvalidStateDirectory(PathBuf),
    InvalidSessionFile(PathBuf),
    InvalidSession(String),
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    SessionTooLarge {
        path: PathBuf,
        limit: u64,
    },
    InvalidUtf8(PathBuf),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Encode(toml::ser::Error),
    Decode {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl SessionError {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateDirectoryUnavailable => formatter.write_str(
                "could not resolve session state directory: XDG_STATE_HOME and HOME are unavailable or invalid",
            ),
            Self::InvalidSessionPath(path) => {
                write!(formatter, "session path has no containing directory: {}", path.display())
            }
            Self::InvalidStateDirectory(path) => write!(
                formatter,
                "session state path is not a real directory: {}",
                path.display()
            ),
            Self::InvalidSessionFile(path) => write!(
                formatter,
                "session path is not a regular file: {}",
                path.display()
            ),
            Self::InvalidSession(reason) => write!(formatter, "invalid workspace session: {reason}"),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported session format version {found}; this build supports version {supported}"
            ),
            Self::SessionTooLarge { path, limit } => write!(
                formatter,
                "workspace session {} exceeds the {limit}-byte safety limit",
                path.display()
            ),
            Self::InvalidUtf8(path) => write!(
                formatter,
                "workspace session is not valid UTF-8: {}",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "could not {operation} {}: {source}", path.display()),
            Self::Encode(source) => write!(formatter, "could not encode workspace session: {source}"),
            Self::Decode { path, source } => write!(
                formatter,
                "could not decode workspace session {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Encode(source) => Some(source),
            Self::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session(root: &Path) -> Session {
        let first = root.join("src/main.rs");
        let second = root.join("README.md");
        Session {
            version: SESSION_FORMAT_VERSION,
            root: root.to_path_buf(),
            open_files: vec![
                OpenFileState {
                    path: first.clone(),
                    cursor: 24,
                    anchor: Some(7),
                    viewport: ViewportState {
                        top_line: 3,
                        top_wrap_char: 24,
                        left_column: 2,
                    },
                },
                OpenFileState {
                    path: second.clone(),
                    cursor: 4,
                    anchor: None,
                    viewport: ViewportState::default(),
                },
            ],
            active_index: 1,
            recent_files: vec![second, first],
            bookmarks: vec![BookmarkState {
                path: root.join("src/main.rs"),
                cursor: 24,
            }],
            layout: LayoutFlags {
                workspace_tree_visible: true,
                problems_visible: true,
                soft_wrap: true,
                agent_dashboard_visible: false,
            },
        }
    }

    #[test]
    fn xdg_resolution_uses_state_home_and_documented_fallback() {
        let xdg = PathBuf::from("/tmp/wscrpt-session-xdg");
        let home = PathBuf::from("/tmp/wscrpt-session-home");
        assert_eq!(
            session_path_from(
                Some(xdg.clone().into_os_string()),
                Some(home.clone().into_os_string())
            )
            .unwrap(),
            xdg.join("wscrpt/session.toml")
        );
        assert_eq!(
            session_path_from(None, Some(home.clone().into_os_string())).unwrap(),
            home.join(".local/state/wscrpt/session.toml")
        );
        assert_eq!(
            session_path_from(
                Some(OsString::from("relative/state")),
                Some(home.clone().into_os_string())
            )
            .unwrap(),
            home.join(".local/state/wscrpt/session.toml")
        );
        assert!(matches!(
            session_path_from(Some(OsString::new()), None),
            Err(SessionError::StateDirectoryUnavailable)
        ));
    }

    #[test]
    fn round_trip_preserves_workspace_navigation_without_buffer_text() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let path = temp.path().join("state/wscrpt/session.toml");
        let store = SessionStore::new(&path);
        let session = sample_session(&root);

        assert_eq!(store.save(&session).unwrap(), path);
        assert_eq!(store.load().unwrap(), Some(session));

        let encoded = fs::read_to_string(path).unwrap();
        assert!(!encoded.lines().any(|line| line.starts_with("text =")));
        assert!(!encoded.contains("recovery"));
    }

    #[test]
    fn version_one_migration_drops_terminal_flags_and_preserves_supported_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let path = temp.path().join("state/session.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let expected = sample_session(&root);
        let mut legacy: toml::Value = toml::Value::try_from(&expected).unwrap();
        legacy["version"] = toml::Value::Integer(i64::from(LEGACY_SESSION_FORMAT_VERSION));
        let layout = legacy["layout"].as_table_mut().unwrap();
        layout.insert("terminal_visible".to_owned(), toml::Value::Boolean(true));
        layout.insert(
            "terminal_split_visible".to_owned(),
            toml::Value::Boolean(true),
        );
        fs::write(&path, toml::to_string_pretty(&legacy).unwrap()).unwrap();

        let migrated = SessionStore::new(&path).load().unwrap().unwrap();
        assert_eq!(migrated, expected);
        assert_eq!(migrated.version, SESSION_FORMAT_VERSION);
        assert_eq!(migrated.open_files.len(), 2);
        assert_eq!(migrated.active_index, 1);
        assert_eq!(migrated.bookmarks.len(), 1);
        assert!(migrated.layout.workspace_tree_visible);
        assert!(migrated.layout.problems_visible);
        assert!(migrated.layout.soft_wrap);

        SessionStore::new(&path).save(&migrated).unwrap();
        let rewritten = fs::read_to_string(path).unwrap();
        assert!(rewritten.contains(&format!("version = {SESSION_FORMAT_VERSION}")));
        assert!(!rewritten.contains("terminal_visible"));
        assert!(!rewritten.contains("terminal_split_visible"));
    }

    #[test]
    fn unknown_future_version_is_recoverably_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.toml");
        let mut future: toml::Value =
            toml::Value::try_from(sample_session(&temp.path().join("workspace"))).unwrap();
        future["version"] = toml::Value::Integer(i64::from(SESSION_FORMAT_VERSION + 1));
        fs::write(&path, toml::to_string_pretty(&future).unwrap()).unwrap();

        assert!(matches!(
            SessionStore::new(path).load(),
            Err(SessionError::UnsupportedVersion {
                found,
                supported: SESSION_FORMAT_VERSION,
            }) if found == SESSION_FORMAT_VERSION + 1
        ));
    }

    #[test]
    fn legacy_viewport_without_wrap_anchor_defaults_to_zero() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("main.rs");
        let encoded = format!(
            "version = {SESSION_FORMAT_VERSION}\nroot = {:?}\nactive_index = 0\nrecent_files = []\n\n[[open_files]]\npath = {:?}\ncursor = 0\n\n[open_files.viewport]\ntop_line = 2\nleft_column = 7\n",
            root.path(),
            path
        );
        let session: Session = toml::from_str(&encoded).unwrap();
        assert_eq!(session.open_files[0].viewport.top_wrap_char, 0);
        assert_eq!(session.open_files[0].viewport.left_column, 7);
        assert!(session.bookmarks.is_empty());
    }

    #[test]
    fn a_recovery_style_text_field_is_rejected_instead_of_silently_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/session.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let session = sample_session(&temp.path().join("workspace"));
        let mut encoded = toml::to_string_pretty(&session).unwrap();
        encoded.push_str("\ntext = \"unsaved contents\"\n");
        fs::write(&path, encoded).unwrap();

        assert!(matches!(
            store_for(&path).load(),
            Err(SessionError::Decode { .. })
        ));
    }

    #[test]
    fn missing_session_is_none_and_does_not_create_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing/wscrpt/session.toml");
        let store = SessionStore::new(&path);
        assert_eq!(store.load().unwrap(), None);
        assert!(!path.parent().unwrap().exists());
        assert!(!store.remove().unwrap());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn invalid_version_index_and_paths_are_rejected_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/session.toml");
        let store = SessionStore::new(&path);
        let mut session = sample_session(&temp.path().join("workspace"));

        session.version += 1;
        assert!(matches!(
            store.save(&session),
            Err(SessionError::UnsupportedVersion { .. })
        ));
        session.version = SESSION_FORMAT_VERSION;
        session.active_index = session.open_files.len();
        assert!(matches!(
            store.save(&session),
            Err(SessionError::InvalidSession(_))
        ));
        session.active_index = 0;
        session.open_files[0].path = PathBuf::from("relative.rs");
        assert!(matches!(
            store.save(&session),
            Err(SessionError::InvalidSession(_))
        ));
        session.open_files[0].path = temp.path().join("workspace/src/main.rs");
        session.bookmarks.push(BookmarkState {
            path: PathBuf::from("relative.rs"),
            cursor: 0,
        });
        assert!(matches!(
            store.save(&session),
            Err(SessionError::InvalidSession(_))
        ));
        assert!(!path.exists());
    }

    #[test]
    fn duplicate_paths_are_rejected_without_canonicalizing_missing_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = sample_session(&temp.path().join("workspace"));
        session.open_files.push(session.open_files[0].clone());
        assert!(matches!(
            session.validate(),
            Err(SessionError::InvalidSession(_))
        ));
        session.open_files.pop();
        session.bookmarks.push(session.bookmarks[0].clone());
        assert!(matches!(
            session.validate(),
            Err(SessionError::InvalidSession(_))
        ));
    }

    #[test]
    fn replacement_is_atomic_at_the_directory_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/session.toml");
        let store = SessionStore::new(&path);
        let mut session = sample_session(&temp.path().join("workspace"));
        store.save(&session).unwrap();

        session.active_index = 0;
        session.open_files[0].cursor = 900;
        store.save(&session).unwrap();
        assert_eq!(store.load().unwrap(), Some(session));

        let entries: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![OsString::from("session.toml")]);
    }

    #[cfg(unix)]
    #[test]
    fn state_is_private_and_symlink_sessions_are_not_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/session.toml");
        let store = SessionStore::new(&path);
        store
            .save(&sample_session(&temp.path().join("workspace")))
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let target = temp.path().join("other.toml");
        fs::write(&target, "version = 1").unwrap();
        let linked = temp.path().join("linked.toml");
        symlink(target, &linked).unwrap();
        assert!(matches!(
            SessionStore::new(linked).load(),
            Err(SessionError::InvalidSessionFile(_))
        ));
    }

    #[test]
    fn oversized_and_non_utf8_files_are_rejected_by_bounded_loader() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.toml");
        fs::write(&path, vec![b'x'; MAX_SESSION_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            store_for(&path).load(),
            Err(SessionError::SessionTooLarge { .. })
        ));

        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            store_for(&path).load(),
            Err(SessionError::InvalidUtf8(_))
        ));
    }

    fn store_for(path: &Path) -> SessionStore {
        SessionStore::new(path)
    }
}
