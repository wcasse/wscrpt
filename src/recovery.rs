//! Crash-recovery journals for unsaved editor buffers.
//!
//! Each dirty buffer is stored as one versioned TOML record. A record keeps the
//! complete UTF-8 buffer rather than a patch so recovery does not depend on the
//! original file still existing or having the same contents. Writes use a
//! mode-0600 temporary file in the journal directory followed by an atomic
//! rename.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// On-disk schema version understood by this build.
pub const RECOVERY_FORMAT_VERSION: u32 = 1;

const APPLICATION_DIRECTORY: &str = "wscrpt";
const RECOVERY_DIRECTORY: &str = "recovery";
const JOURNAL_EXTENSION: &str = "toml";
const MAX_ID_BYTES: usize = 128;
const TEMP_CREATE_ATTEMPTS: usize = 32;

/// Maximum encoded size of one recovery journal.
///
/// Recovery snapshots contain complete buffers, so this is deliberately larger
/// than the normal session file limit while still preventing an unbounded read
/// during startup.
pub const MAX_RECOVERY_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum number of directory entries inspected by one recovery scan.
pub const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 4_096;
/// Maximum aggregate encoded journal bytes considered by one recovery scan.
pub const MAX_RECOVERY_LIST_BYTES: u64 = 256 * 1024 * 1024;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A non-fatal problem encountered while enumerating recovery journals.
///
/// The path and already-formatted message make warnings cheap for startup code
/// to surface without retaining filesystem or parser error objects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryWarning {
    pub path: PathBuf,
    pub message: String,
}

impl RecoveryWarning {
    fn from_error(path: impl Into<PathBuf>, error: RecoveryError) -> Self {
        Self {
            path: path.into(),
            message: error.to_string(),
        }
    }

    fn message(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for RecoveryWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.message)
    }
}

/// Bounded result of scanning a recovery directory.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryListing {
    pub records: Vec<RecoveryRecord>,
    pub warnings: Vec<RecoveryWarning>,
}

impl RecoveryListing {
    pub fn is_clean(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// A complete snapshot of one dirty UTF-8 buffer.
///
/// Cursor and anchor are character indices, matching `Editor` rather than UTF-8
/// byte offsets. `saved_revision == Some(revision)` describes a clean buffer and
/// is rejected by [`RecoveryStore::write`]. `None` is useful for a new untitled
/// buffer that has never had a saved revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    /// Schema version for forwards-compatible rejection of unknown formats.
    pub version: u32,
    /// Filesystem-safe identity used as the journal filename.
    pub id: String,
    /// Project root that was active when the snapshot was captured.
    pub workspace_root: PathBuf,
    /// File the buffer came from, or `None` for an untitled buffer.
    pub original_path: Option<PathBuf>,
    /// Complete normalized UTF-8 document contents.
    pub text: String,
    /// Cursor position as a Unicode scalar-value (character) index.
    pub cursor: usize,
    /// Selection anchor as a character index, when a selection is active.
    pub anchor: Option<usize>,
    /// Current document state/revision identifier.
    pub revision: u64,
    /// Last saved state/revision identifier, if the buffer has ever been saved.
    pub saved_revision: Option<u64>,
    /// Snapshot time in milliseconds since the Unix epoch.
    pub recorded_at_unix_millis: u64,
}

impl RecoveryRecord {
    /// Build a new recovery record with a fresh safe ID and current timestamp.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        original_path: Option<PathBuf>,
        text: impl Into<String>,
        cursor: usize,
        anchor: Option<usize>,
        revision: u64,
        saved_revision: Option<u64>,
    ) -> Self {
        Self {
            version: RECOVERY_FORMAT_VERSION,
            id: generate_id(),
            workspace_root: workspace_root.into(),
            original_path,
            text: text.into(),
            cursor,
            anchor,
            revision,
            saved_revision,
            recorded_at_unix_millis: unix_millis(),
        }
    }

    /// Refresh the timestamp before replacing an existing journal snapshot.
    pub fn mark_recorded_now(&mut self) {
        self.recorded_at_unix_millis = unix_millis();
    }

    /// Validate invariants required for a safe, recoverable dirty-buffer record.
    pub fn validate(&self) -> Result<(), RecoveryError> {
        if self.version != RECOVERY_FORMAT_VERSION {
            return Err(RecoveryError::UnsupportedVersion {
                found: self.version,
                supported: RECOVERY_FORMAT_VERSION,
            });
        }
        validate_id(&self.id)?;
        if self.workspace_root.as_os_str().is_empty() {
            return Err(RecoveryError::InvalidRecord(
                "workspace_root must not be empty".to_owned(),
            ));
        }
        if self.saved_revision == Some(self.revision) {
            return Err(RecoveryError::InvalidRecord(
                "recovery journals may only contain dirty buffers".to_owned(),
            ));
        }

        let character_count = self.text.chars().count();
        if self.cursor > character_count {
            return Err(RecoveryError::InvalidRecord(format!(
                "cursor {} exceeds buffer length {character_count}",
                self.cursor
            )));
        }
        if let Some(anchor) = self.anchor
            && anchor > character_count
        {
            return Err(RecoveryError::InvalidRecord(format!(
                "anchor {anchor} exceeds buffer length {character_count}"
            )));
        }
        Ok(())
    }
}

/// A recovery journal rooted at an explicit directory.
///
/// Use [`RecoveryStore::from_env`] in the application and [`RecoveryStore::new`]
/// with a temporary directory in tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryStore {
    directory: PathBuf,
}

impl RecoveryStore {
    /// Resolve the standard XDG recovery directory without creating it.
    pub fn from_env() -> Result<Self, RecoveryError> {
        Ok(Self::new(recovery_dir()?))
    }

    /// Use an explicit journal directory. It is created on the first write.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Atomically create or replace the journal for `record`.
    ///
    /// The returned path is the final journal path, never the temporary path.
    pub fn write(&self, record: &RecoveryRecord) -> Result<PathBuf, RecoveryError> {
        record.validate()?;
        let mut encoded = toml::to_string_pretty(record).map_err(RecoveryError::Encode)?;
        encoded.push('\n');
        if encoded.len() as u64 > MAX_RECOVERY_JOURNAL_BYTES {
            return Err(RecoveryError::JournalTooLarge {
                path: self.journal_path(&record.id)?,
                limit: MAX_RECOVERY_JOURNAL_BYTES,
            });
        }
        self.ensure_directory()?;

        let target = self.journal_path(&record.id)?;
        let (mut file, temporary) = self.create_temporary_file(&record.id)?;
        let mut cleanup = TemporaryCleanup::new(temporary.clone());

        write_all(&mut file, encoded.as_bytes(), &temporary)?;
        file.flush()
            .map_err(|source| RecoveryError::io("flush journal", &temporary, source))?;

        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| RecoveryError::io("set journal permissions", &temporary, source))?;

        file.sync_all()
            .map_err(|source| RecoveryError::io("sync journal", &temporary, source))?;
        drop(file);

        fs::rename(&temporary, &target)
            .map_err(|source| RecoveryError::io("publish journal", &target, source))?;
        cleanup.disarm();
        sync_directory_best_effort(&self.directory);
        Ok(target)
    }

    /// Load and validate one journal by its safe ID.
    pub fn load(&self, id: &str) -> Result<RecoveryRecord, RecoveryError> {
        let path = self.journal_path(id)?;
        self.load_bounded(id, &path, MAX_RECOVERY_JOURNAL_BYTES)
    }

    fn load_bounded(
        &self,
        id: &str,
        path: &Path,
        byte_limit: u64,
    ) -> Result<RecoveryRecord, RecoveryError> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|source| RecoveryError::io("inspect journal", path, source))?;
        if !metadata.file_type().is_file() {
            return Err(RecoveryError::InvalidJournalFile(path.to_path_buf()));
        }
        if metadata.len() > byte_limit {
            return Err(RecoveryError::JournalTooLarge {
                path: path.to_path_buf(),
                limit: byte_limit,
            });
        }

        let file =
            File::open(path).map_err(|source| RecoveryError::io("open journal", path, source))?;
        let initial_capacity = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let mut bytes = Vec::with_capacity(initial_capacity);
        file.take(byte_limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| RecoveryError::io("read journal", path, source))?;
        if bytes.len() as u64 > byte_limit {
            return Err(RecoveryError::JournalTooLarge {
                path: path.to_path_buf(),
                limit: byte_limit,
            });
        }
        let source =
            String::from_utf8(bytes).map_err(|_| RecoveryError::InvalidUtf8(path.to_path_buf()))?;
        let record: RecoveryRecord =
            toml::from_str(&source).map_err(|source| RecoveryError::Decode {
                path: path.to_path_buf(),
                source,
            })?;

        if record.id != id {
            return Err(RecoveryError::InvalidRecord(format!(
                "journal filename ID {id:?} does not match record ID {:?}",
                record.id
            )));
        }
        record.validate()?;
        Ok(record)
    }

    /// Load all valid journal files, newest snapshot first.
    ///
    /// A missing directory is an empty journal. Temporary files left by a
    /// process that died before rename and unrelated directory entries are
    /// ignored.
    pub fn list(&self) -> Result<Vec<RecoveryRecord>, RecoveryError> {
        Ok(self.list_with_warnings()?.records)
    }

    /// Load valid journal files while preserving a warning for every skipped
    /// journal-looking entry.
    ///
    /// Opening the recovery directory itself is the only fatal scan error.
    /// Individual entry, metadata, bounded-read, UTF-8, TOML, version, and
    /// record-validation failures are isolated so they cannot hide other valid
    /// recovery records. Directory entries and aggregate encoded bytes are
    /// capped to keep startup work and the returned result bounded.
    pub fn list_with_warnings(&self) -> Result<RecoveryListing, RecoveryError> {
        let directory_metadata = match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RecoveryListing::default());
            }
            Err(source) => {
                return Err(RecoveryError::io(
                    "inspect recovery directory",
                    &self.directory,
                    source,
                ));
            }
        };
        if !directory_metadata.file_type().is_dir() {
            return Err(RecoveryError::InvalidStateDirectory(self.directory.clone()));
        }

        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RecoveryListing::default());
            }
            Err(source) => {
                return Err(RecoveryError::io(
                    "list recovery directory",
                    &self.directory,
                    source,
                ));
            }
        };

        let mut listing = RecoveryListing::default();
        let mut considered_bytes = 0_u64;
        for (scanned_entries, entry) in entries.enumerate() {
            if scanned_entries == MAX_RECOVERY_DIRECTORY_ENTRIES {
                listing.warnings.push(RecoveryWarning::message(
                    &self.directory,
                    format!(
                        "recovery scan stopped after {MAX_RECOVERY_DIRECTORY_ENTRIES} directory entries"
                    ),
                ));
                break;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    listing.warnings.push(RecoveryWarning::from_error(
                        &self.directory,
                        RecoveryError::io("read recovery directory entry", &self.directory, source),
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(source) => {
                    listing.warnings.push(RecoveryWarning::from_error(
                        &path,
                        RecoveryError::io("inspect recovery directory entry", &path, source),
                    ));
                    continue;
                }
            };
            if !file_type.is_file() {
                if filename_has_journal_extension(&entry.file_name()) {
                    listing.warnings.push(RecoveryWarning::from_error(
                        &path,
                        RecoveryError::InvalidJournalFile(path.clone()),
                    ));
                }
                continue;
            }
            let Some(id) = journal_id_from_filename(&entry.file_name()) else {
                if filename_has_journal_extension(&entry.file_name()) {
                    listing.warnings.push(RecoveryWarning::message(
                        &path,
                        "recovery journal filename does not contain a safe ID",
                    ));
                }
                continue;
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) => {
                    listing.warnings.push(RecoveryWarning::from_error(
                        &path,
                        RecoveryError::io("inspect recovery journal", &path, source),
                    ));
                    continue;
                }
            };
            if metadata.len() > MAX_RECOVERY_JOURNAL_BYTES {
                listing.warnings.push(RecoveryWarning::from_error(
                    &path,
                    RecoveryError::JournalTooLarge {
                        path: path.clone(),
                        limit: MAX_RECOVERY_JOURNAL_BYTES,
                    },
                ));
                continue;
            }
            let Some(next_considered_bytes) = considered_bytes.checked_add(metadata.len()) else {
                listing.warnings.push(RecoveryWarning::message(
                    &path,
                    format!(
                        "recovery scan byte limit of {MAX_RECOVERY_LIST_BYTES} bytes would be exceeded"
                    ),
                ));
                continue;
            };
            if next_considered_bytes > MAX_RECOVERY_LIST_BYTES {
                listing.warnings.push(RecoveryWarning::message(
                    &path,
                    format!(
                        "recovery scan byte limit of {MAX_RECOVERY_LIST_BYTES} bytes would be exceeded"
                    ),
                ));
                continue;
            }
            considered_bytes = next_considered_bytes;

            match self.load_bounded(&id, &path, metadata.len()) {
                Ok(record) => listing.records.push(record),
                Err(RecoveryError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    // The journal was removed between read_dir and load.
                }
                Err(error) => listing
                    .warnings
                    .push(RecoveryWarning::from_error(&path, error)),
            }
        }

        listing.records.sort_by(|left, right| {
            right
                .recorded_at_unix_millis
                .cmp(&left.recorded_at_unix_millis)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(listing)
    }

    /// Remove one journal. Returns `false` when it was already absent.
    pub fn remove(&self, id: &str) -> Result<bool, RecoveryError> {
        let path = self.journal_path(id)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_directory_best_effort(&self.directory);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(RecoveryError::io("remove journal", path, source)),
        }
    }

    fn ensure_directory(&self) -> Result<(), RecoveryError> {
        fs::create_dir_all(&self.directory).map_err(|source| {
            RecoveryError::io("create recovery directory", &self.directory, source)
        })?;

        let metadata = fs::symlink_metadata(&self.directory).map_err(|source| {
            RecoveryError::io("inspect recovery directory", &self.directory, source)
        })?;
        if !metadata.file_type().is_dir() {
            return Err(RecoveryError::InvalidStateDirectory(self.directory.clone()));
        }

        #[cfg(unix)]
        fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700)).map_err(
            |source| {
                RecoveryError::io(
                    "set recovery directory permissions",
                    &self.directory,
                    source,
                )
            },
        )?;

        Ok(())
    }

    fn journal_path(&self, id: &str) -> Result<PathBuf, RecoveryError> {
        validate_id(id)?;
        Ok(self.directory.join(format!("{id}.{JOURNAL_EXTENSION}")))
    }

    fn create_temporary_file(&self, id: &str) -> Result<(File, PathBuf), RecoveryError> {
        let mut last_collision = None;
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!(".{id}.tmp-{:08x}-{sequence:016x}", process::id());
            let path = self.directory.join(name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);

            match options.open(&path) {
                Ok(file) => return Ok((file, path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                Err(source) => {
                    return Err(RecoveryError::io("create temporary journal", path, source));
                }
            }
        }

        Err(RecoveryError::io(
            "create temporary journal",
            &self.directory,
            last_collision.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "temporary journal name collision",
                )
            }),
        ))
    }
}

/// Resolve `$XDG_STATE_HOME/wscrpt/recovery`, falling back to
/// `$HOME/.local/state/wscrpt/recovery` as required by the XDG Base Directory
/// specification.
pub fn recovery_dir() -> Result<PathBuf, RecoveryError> {
    recovery_dir_from(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))
}

/// Write a journal using the standard XDG recovery directory.
pub fn write_record(record: &RecoveryRecord) -> Result<PathBuf, RecoveryError> {
    RecoveryStore::from_env()?.write(record)
}

/// List journals from the standard XDG recovery directory.
pub fn list_records() -> Result<Vec<RecoveryRecord>, RecoveryError> {
    RecoveryStore::from_env()?.list()
}

/// List journals and non-fatal skipped-entry warnings from the standard XDG
/// recovery directory.
pub fn list_records_with_warnings() -> Result<RecoveryListing, RecoveryError> {
    RecoveryStore::from_env()?.list_with_warnings()
}

/// Load a journal from the standard XDG recovery directory.
pub fn load_record(id: &str) -> Result<RecoveryRecord, RecoveryError> {
    RecoveryStore::from_env()?.load(id)
}

/// Remove a journal from the standard XDG recovery directory.
pub fn remove_record(id: &str) -> Result<bool, RecoveryError> {
    RecoveryStore::from_env()?.remove(id)
}

/// Create an opaque filename-safe journal ID without external dependencies.
pub fn generate_id() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let sequence = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "r-{:016x}-{:08x}-{:08x}-{sequence:016x}",
        duration.as_secs(),
        duration.subsec_nanos(),
        process::id()
    )
}

fn recovery_dir_from(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, RecoveryError> {
    if let Some(value) = xdg_state_home.filter(|value| !value.is_empty()) {
        let root = PathBuf::from(value);
        if root.is_absolute() {
            return Ok(root.join(APPLICATION_DIRECTORY).join(RECOVERY_DIRECTORY));
        }
        // The XDG specification says relative values must be ignored.
    }

    let home = home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(RecoveryError::StateDirectoryUnavailable)?;
    Ok(home
        .join(".local/state")
        .join(APPLICATION_DIRECTORY)
        .join(RECOVERY_DIRECTORY))
}

fn journal_id_from_filename(filename: &OsStr) -> Option<String> {
    let filename = filename.to_str()?;
    let suffix = format!(".{JOURNAL_EXTENSION}");
    let id = filename.strip_suffix(&suffix)?;
    validate_id(id).ok()?;
    Some(id.to_owned())
}

fn filename_has_journal_extension(filename: &OsStr) -> bool {
    filename
        .to_str()
        .is_some_and(|name| name.ends_with(&format!(".{JOURNAL_EXTENSION}")))
}

fn validate_id(id: &str) -> Result<(), RecoveryError> {
    if id.is_empty()
        || id.len() > MAX_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(RecoveryError::InvalidId(id.to_owned()));
    }
    Ok(())
}

fn unix_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn write_all(file: &mut File, bytes: &[u8], path: &Path) -> Result<(), RecoveryError> {
    file.write_all(bytes)
        .map_err(|source| RecoveryError::io("write journal", path, source))
}

fn sync_directory_best_effort(directory: &Path) {
    // Directory fsync is supported on the Unix targets where wscrpt runs, but
    // not on every filesystem. The file itself has already been synced and the
    // rename completed, so an unsupported directory sync must not turn a
    // successful atomic publication into a reported failure.
    if let Ok(file) = File::open(directory) {
        let _ = file.sync_all();
    }
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

/// Errors produced by recovery path resolution, serialization, and storage.
#[derive(Debug)]
pub enum RecoveryError {
    /// Neither a valid absolute XDG state root nor an absolute home was found.
    StateDirectoryUnavailable,
    /// An explicit store path exists but is not a real directory (for example,
    /// it is a symlink or regular file).
    InvalidStateDirectory(PathBuf),
    /// The ID cannot safely be used as a single filename component.
    InvalidId(String),
    /// A journal path exists but is not a regular file.
    InvalidJournalFile(PathBuf),
    /// A journal exceeds the bounded encoded-size limit.
    JournalTooLarge { path: PathBuf, limit: u64 },
    /// A journal is not valid UTF-8.
    InvalidUtf8(PathBuf),
    /// The record is structurally valid TOML but violates recovery invariants.
    InvalidRecord(String),
    /// The record belongs to an on-disk schema this build does not understand.
    UnsupportedVersion { found: u32, supported: u32 },
    /// A filesystem operation failed.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// TOML serialization failed.
    Encode(toml::ser::Error),
    /// TOML deserialization failed.
    Decode {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl RecoveryError {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateDirectoryUnavailable => formatter.write_str(
                "could not resolve recovery state directory: XDG_STATE_HOME and HOME are unavailable or invalid",
            ),
            Self::InvalidStateDirectory(path) => write!(
                formatter,
                "recovery path is not a real directory: {}",
                path.display()
            ),
            Self::InvalidId(id) => write!(formatter, "invalid recovery journal ID {id:?}"),
            Self::InvalidJournalFile(path) => write!(
                formatter,
                "recovery journal is not a regular file: {}",
                path.display()
            ),
            Self::JournalTooLarge { path, limit } => write!(
                formatter,
                "recovery journal {} exceeds the {limit}-byte limit",
                path.display()
            ),
            Self::InvalidUtf8(path) => write!(
                formatter,
                "recovery journal is not valid UTF-8: {}",
                path.display()
            ),
            Self::InvalidRecord(reason) => write!(formatter, "invalid recovery record: {reason}"),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported recovery format version {found}; this build supports version {supported}"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "could not {operation} {}: {source}", path.display()),
            Self::Encode(source) => write!(formatter, "could not encode recovery journal: {source}"),
            Self::Decode { path, source } => write!(
                formatter,
                "could not decode recovery journal {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for RecoveryError {
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

    fn dirty_record(root: &Path) -> RecoveryRecord {
        RecoveryRecord {
            version: RECOVERY_FORMAT_VERSION,
            id: "r-test-buffer-01".to_owned(),
            workspace_root: root.to_path_buf(),
            original_path: Some(root.join("notes/ideas.rs")),
            text: "fn café() {\n    println!(\"🪶\");\n}\n".to_owned(),
            cursor: 8,
            anchor: Some(3),
            revision: 9,
            saved_revision: Some(4),
            recorded_at_unix_millis: 123_456,
        }
    }

    #[test]
    fn xdg_resolution_is_pure_and_uses_the_documented_fallback() {
        let xdg = PathBuf::from("/tmp/wscrpt-test-xdg");
        let home = PathBuf::from("/tmp/wscrpt-test-home");
        assert_eq!(
            recovery_dir_from(
                Some(xdg.clone().into_os_string()),
                Some(home.clone().into_os_string())
            )
            .unwrap(),
            xdg.join("wscrpt/recovery")
        );
        assert_eq!(
            recovery_dir_from(None, Some(home.clone().into_os_string())).unwrap(),
            home.join(".local/state/wscrpt/recovery")
        );
        assert_eq!(
            recovery_dir_from(
                Some(OsString::from("relative/state")),
                Some(home.clone().into_os_string())
            )
            .unwrap(),
            home.join(".local/state/wscrpt/recovery")
        );
        assert!(matches!(
            recovery_dir_from(Some(OsString::new()), None),
            Err(RecoveryError::StateDirectoryUnavailable)
        ));
    }

    #[test]
    fn generated_ids_are_unique_safe_filename_components() {
        let first = generate_id();
        let second = generate_id();
        assert_ne!(first, second);
        assert!(first.len() <= MAX_ID_BYTES);
        assert!(validate_id(&first).is_ok());
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
    }

    #[test]
    fn round_trip_list_load_and_remove_stays_inside_tempdir() {
        let temp = tempfile::tempdir().unwrap();
        let journal_dir = temp.path().join("state/wscrpt/recovery");
        let store = RecoveryStore::new(&journal_dir);
        let record = dirty_record(temp.path());

        let path = store.write(&record).unwrap();
        assert_eq!(path, journal_dir.join("r-test-buffer-01.toml"));
        assert!(path.starts_with(temp.path()));
        assert_eq!(store.load(&record.id).unwrap(), record);
        assert_eq!(store.list().unwrap(), vec![record.clone()]);

        assert!(store.remove(&record.id).unwrap());
        assert!(!store.remove(&record.id).unwrap());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn corrupt_journal_does_not_hide_valid_recovery_records() {
        let temp = tempfile::tempdir().unwrap();
        let journal_dir = temp.path().join("journals");
        let store = RecoveryStore::new(&journal_dir);
        let record = dirty_record(temp.path());
        store.write(&record).unwrap();
        let corrupt_path = journal_dir.join("r-corrupt.toml");
        fs::write(&corrupt_path, "this is not = valid = toml").unwrap();

        let listing = store.list_with_warnings().unwrap();
        assert_eq!(listing.records, vec![record.clone()]);
        assert_eq!(listing.warnings.len(), 1);
        assert_eq!(listing.warnings[0].path, corrupt_path);
        assert!(listing.warnings[0].message.contains("decode"));

        // The compatibility API also preserves valid records, even though
        // callers that need diagnostics should use `list_with_warnings`.
        assert_eq!(store.list().unwrap(), vec![record]);
    }

    #[test]
    fn oversized_and_non_utf8_journals_are_skipped_with_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let journal_dir = temp.path().join("journals");
        let store = RecoveryStore::new(&journal_dir);
        let record = dirty_record(temp.path());
        store.write(&record).unwrap();

        let oversized_path = journal_dir.join("r-oversized.toml");
        let oversized = File::create(&oversized_path).unwrap();
        oversized.set_len(MAX_RECOVERY_JOURNAL_BYTES + 1).unwrap();
        drop(oversized);
        let non_utf8_path = journal_dir.join("r-non-utf8.toml");
        fs::write(&non_utf8_path, [0xff, 0xfe]).unwrap();

        assert!(matches!(
            store.load("r-oversized"),
            Err(RecoveryError::JournalTooLarge { .. })
        ));
        assert!(matches!(
            store.load("r-non-utf8"),
            Err(RecoveryError::InvalidUtf8(_))
        ));

        let listing = store.list_with_warnings().unwrap();
        assert_eq!(listing.records, vec![record]);
        assert_eq!(listing.warnings.len(), 2);
        assert!(listing.warnings.iter().any(|warning| {
            warning.path == oversized_path && warning.message.contains("byte limit")
        }));
        assert!(
            listing.warnings.iter().any(|warning| {
                warning.path == non_utf8_path && warning.message.contains("UTF-8")
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn published_journal_is_mode_0600_and_directory_is_private() {
        let temp = tempfile::tempdir().unwrap();
        let journal_dir = temp.path().join("journals");
        let store = RecoveryStore::new(&journal_dir);
        let path = store.write(&dirty_record(temp.path())).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(journal_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn replacing_a_snapshot_is_atomic_at_the_directory_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::new(temp.path().join("journals"));
        let mut record = dirty_record(temp.path());
        store.write(&record).unwrap();

        record.text = "new complete snapshot\n".to_owned();
        record.cursor = record.text.chars().count();
        record.anchor = None;
        record.revision = 10;
        record.recorded_at_unix_millis += 1;
        store.write(&record).unwrap();

        assert_eq!(store.load(&record.id).unwrap(), record);
        let names: Vec<_> = fs::read_dir(store.directory())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("r-test-buffer-01.toml")]);
    }

    #[test]
    fn invalid_ids_and_clean_records_never_create_state() {
        let temp = tempfile::tempdir().unwrap();
        let journal_dir = temp.path().join("journals");
        let store = RecoveryStore::new(&journal_dir);
        let mut record = dirty_record(temp.path());

        record.id = "../../escape".to_owned();
        assert!(matches!(
            store.write(&record),
            Err(RecoveryError::InvalidId(_))
        ));

        record.id = "safe-id".to_owned();
        record.saved_revision = Some(record.revision);
        assert!(matches!(
            store.write(&record),
            Err(RecoveryError::InvalidRecord(_))
        ));
        assert!(!journal_dir.exists());
    }

    #[test]
    fn character_offsets_are_checked_against_utf8_text() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::new(temp.path().join("journals"));
        let mut record = dirty_record(temp.path());
        record.text = "🪶".to_owned();
        record.cursor = 2;

        assert!(matches!(
            store.write(&record),
            Err(RecoveryError::InvalidRecord(_))
        ));
        assert!(!store.directory().exists());
    }

    #[test]
    fn missing_directory_lists_empty_without_creating_it() {
        let temp = tempfile::tempdir().unwrap();
        let journal_dir = temp.path().join("not-created");
        let store = RecoveryStore::new(&journal_dir);
        assert!(store.list().unwrap().is_empty());
        assert!(!store.remove("safe-id").unwrap());
        assert!(!journal_dir.exists());
    }
}
