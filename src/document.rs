use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ropey::Rope;
use thiserror::Error;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Largest file loaded into an editable in-memory rope.
pub const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not UTF-8 text")]
    NotUtf8 { path: PathBuf },
    #[error("{path} contains NUL bytes and appears to be binary")]
    Binary { path: PathBuf },
    #[error("{path} is {size} bytes; the editable-file limit is {limit} bytes")]
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    #[error("could not save {path}: {source}")]
    Save { path: PathBuf, source: io::Error },
    #[error("this document has no filename; use Save As")]
    MissingPath,
    #[error("{name} is a read-only IDE view")]
    ReadOnly { name: String },
    #[error(
        "{path} changed on disk since it was opened or saved; reload, diff, or Save As instead"
    )]
    ExternalChange { path: PathBuf },
    #[error("Save As refused to replace existing file {path}")]
    TargetExists { path: PathBuf },
    #[error("atomic save would break hard links for {path}; use an explicit in-place workflow")]
    HardLinked { path: PathBuf },
    #[error("invalid character range {start}..{end} for a {len}-character document")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditKind {
    Insert,
    Backspace,
    Delete,
    Paste,
    Replace,
}

#[derive(Clone, Debug)]
struct Edit {
    start: usize,
    deleted: String,
    inserted: String,
    cursor_before: usize,
    cursor_after: usize,
    state_before: u64,
    state_after: u64,
    kind: EditKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DiskFingerprint {
    len: u64,
    hash: u64,
}

#[derive(Clone, Debug)]
pub struct Document {
    rope: Rope,
    path: Option<PathBuf>,
    display_name: String,
    line_ending: LineEnding,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    state_id: u64,
    saved_state_id: Option<u64>,
    save_generation: u64,
    next_state_id: u64,
    undo_group_open: bool,
    read_only: bool,
    utf8_bom: bool,
    disk_fingerprint: Option<DiskFingerprint>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            display_name: "Untitled".to_owned(),
            line_ending: LineEnding::Lf,
            undo: Vec::new(),
            redo: Vec::new(),
            state_id: 0,
            saved_state_id: Some(0),
            save_generation: 0,
            next_state_id: 1,
            undo_group_open: false,
            read_only: false,
            utf8_bom: false,
            disk_fingerprint: None,
        }
    }

    pub fn from_text(text: &str) -> Self {
        let (normalized, line_ending) = normalize_newlines(text);
        Self {
            rope: Rope::from_str(&normalized),
            line_ending,
            ..Self::new()
        }
    }

    pub fn virtual_view(name: impl Into<String>, text: &str) -> Self {
        let (normalized, line_ending) = normalize_newlines(text);
        Self {
            rope: Rope::from_str(&normalized),
            display_name: name.into(),
            line_ending,
            read_only: true,
            ..Self::new()
        }
    }

    pub fn recovered(path: Option<PathBuf>, text: &str) -> Self {
        let (normalized, line_ending) = normalize_newlines(text);
        let display_name = path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Recovered Untitled".to_owned());
        Self {
            rope: Rope::from_str(&normalized),
            path,
            display_name,
            line_ending,
            state_id: 1,
            saved_state_id: Some(0),
            next_state_id: 2,
            ..Self::new()
        }
    }

    pub fn new_at(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let display_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path: Some(path),
            display_name,
            ..Self::new()
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, DocumentError> {
        let path = path.as_ref().to_path_buf();
        let bytes = read_document_bytes(&path)?;
        Self::from_disk_snapshot(path, &bytes)
    }

    /// Build a file-backed document from bytes already read through a trusted
    /// descriptor. Workspace-edit commit uses this to avoid reopening a path
    /// after validating it, which could otherwise race with a special-file
    /// replacement and block the editor.
    pub(crate) fn from_disk_snapshot(
        path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<Self, DocumentError> {
        let path = path.as_ref().to_path_buf();
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::TooLarge {
                path,
                size: bytes.len() as u64,
                limit: MAX_DOCUMENT_BYTES,
            });
        }
        if bytes.contains(&0) {
            return Err(DocumentError::Binary { path });
        }
        let utf8_bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
        let text_bytes = if utf8_bom { &bytes[3..] } else { bytes };
        let text = String::from_utf8(text_bytes.to_vec())
            .map_err(|_| DocumentError::NotUtf8 { path: path.clone() })?;
        let (normalized, line_ending) = normalize_newlines(&text);
        let display_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        Ok(Self {
            rope: Rope::from_str(&normalized),
            path: Some(path),
            display_name,
            line_ending,
            undo: Vec::new(),
            redo: Vec::new(),
            state_id: 0,
            saved_state_id: Some(0),
            save_generation: 0,
            next_state_id: 1,
            undo_group_open: false,
            read_only: false,
            utf8_bom,
            disk_fingerprint: Some(fingerprint(bytes)),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn set_line_ending(&mut self, ending: LineEnding) {
        if self.line_ending != ending {
            self.line_ending = ending;
            self.saved_state_id = None;
        }
    }

    pub fn is_modified(&self) -> bool {
        self.saved_state_id != Some(self.state_id)
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn has_utf8_bom(&self) -> bool {
        self.utf8_bom
    }

    pub fn state_id(&self) -> u64 {
        self.state_id
    }

    pub fn saved_state_id(&self) -> Option<u64> {
        self.saved_state_id
    }

    /// Monotonic-in-practice identity of successful disk publications during
    /// this document's lifetime. Unlike `saved_state_id`, this advances even
    /// when an already-clean snapshot is explicitly saved again.
    pub fn save_generation(&self) -> u64 {
        self.save_generation
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Cheap immutable access for in-crate services that can clone Ropey's
    /// shared tree instead of flattening a large buffer into a `String`.
    pub(crate) fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.slice(range).to_string()
    }

    pub fn line(&self, line: usize) -> String {
        self.rope.line(line).to_string()
    }

    pub fn char(&self, char_idx: usize) -> Option<char> {
        (char_idx < self.len_chars()).then(|| self.rope.char(char_idx))
    }

    pub fn char_to_line(&self, char_idx: usize) -> usize {
        self.rope.char_to_line(char_idx.min(self.len_chars()))
    }

    pub fn line_start_char(&self, line: usize) -> usize {
        self.rope
            .line_to_char(line.min(self.line_count().saturating_sub(1)))
    }

    pub fn line_end_char(&self, line: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let start = self.rope.line_to_char(line);
        let mut end = start + self.rope.line(line).len_chars();
        while end > start && matches!(self.rope.char(end - 1), '\n' | '\r') {
            end -= 1;
        }
        end
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn break_undo_group(&mut self) {
        self.undo_group_open = false;
    }

    pub fn edit(
        &mut self,
        range: Range<usize>,
        inserted: &str,
        cursor_before: usize,
        cursor_after: usize,
        kind: EditKind,
    ) -> Result<(), DocumentError> {
        if self.read_only {
            return Err(DocumentError::ReadOnly {
                name: self.display_name.clone(),
            });
        }
        let len = self.len_chars();
        if range.start > range.end || range.end > len {
            return Err(DocumentError::InvalidRange {
                start: range.start,
                end: range.end,
                len,
            });
        }
        if range.is_empty() && inserted.is_empty() {
            return Ok(());
        }

        let deleted = self.rope.slice(range.clone()).to_string();
        self.rope.remove(range.clone());
        self.rope.insert(range.start, inserted);

        let state_before = self.state_id;
        let state_after = self.fresh_state_id();
        self.state_id = state_after;
        let edit = Edit {
            start: range.start,
            deleted,
            inserted: inserted.to_owned(),
            cursor_before,
            cursor_after,
            state_before,
            state_after,
            kind,
        };

        let coalesced = if self.undo_group_open {
            self.undo
                .last_mut()
                .is_some_and(|previous| coalesce(previous, &edit))
        } else {
            false
        };
        if !coalesced {
            self.undo.push(edit);
        }
        self.undo_group_open = matches!(
            kind,
            EditKind::Insert | EditKind::Backspace | EditKind::Delete
        );
        self.redo.clear();
        Ok(())
    }

    pub fn undo(&mut self) -> Option<usize> {
        self.break_undo_group();
        let edit = self.undo.pop()?;
        let inserted_len = edit.inserted.chars().count();
        self.rope.remove(edit.start..edit.start + inserted_len);
        self.rope.insert(edit.start, &edit.deleted);
        self.state_id = edit.state_before;
        let cursor = edit.cursor_before;
        self.redo.push(edit);
        Some(cursor)
    }

    pub fn redo(&mut self) -> Option<usize> {
        self.break_undo_group();
        let edit = self.redo.pop()?;
        let deleted_len = edit.deleted.chars().count();
        self.rope.remove(edit.start..edit.start + deleted_len);
        self.rope.insert(edit.start, &edit.inserted);
        self.state_id = edit.state_after;
        let cursor = edit.cursor_after;
        self.undo.push(edit);
        Some(cursor)
    }

    pub fn save(&mut self) -> Result<(), DocumentError> {
        let path = self.path.clone().ok_or(DocumentError::MissingPath)?;
        self.save_to(&path, true)
    }

    /// Explicitly accept the current on-disk version as the overwrite base,
    /// then perform the normal race-checked atomic save. This does not bypass
    /// hard-link protection and still detects another change between the
    /// acknowledgement read and publication.
    pub fn save_over_external_change(&mut self) -> Result<(), DocumentError> {
        let path = self.path.clone().ok_or(DocumentError::MissingPath)?;
        self.disk_fingerprint = fingerprint_file(&path).ok();
        self.save_to(&path, true)
    }

    /// Replace this buffer with a fresh disk snapshot, clearing its undo
    /// history. Callers own the dirty-buffer confirmation and cursor clamping.
    pub fn reload_from_disk(&mut self) -> Result<(), DocumentError> {
        let path = self.path.clone().ok_or(DocumentError::MissingPath)?;
        let mut replacement = Self::open(path)?;
        // State IDs are the in-process identity of a text snapshot, not a
        // property of bytes on disk. Reusing `open()`'s initial zero here can
        // make an LSP/session observer mistake externally reloaded text for the
        // pre-reload snapshot. Advance this document's existing state clock and
        // mark the fresh disk image saved under that new identity.
        let reloaded_state_id = self.next_state_id;
        replacement.state_id = reloaded_state_id;
        replacement.saved_state_id = Some(reloaded_state_id);
        // Reload observes a disk write; it is not an explicit editor save and
        // must not synthesize an LSP didSave notification.
        replacement.save_generation = self.save_generation;
        replacement.next_state_id = reloaded_state_id.wrapping_add(1).max(1);
        *self = replacement;
        Ok(())
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<(), DocumentError> {
        let path = path.as_ref().to_path_buf();
        if self
            .path
            .as_deref()
            .is_some_and(|current| same_path(current, &path))
        {
            return self.save();
        }
        self.save_to(&path, false)?;
        self.display_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.path = Some(path);
        Ok(())
    }

    pub fn save_copy_as(&self, path: impl AsRef<Path>) -> Result<(), DocumentError> {
        if self.read_only {
            return Err(DocumentError::ReadOnly {
                name: self.display_name.clone(),
            });
        }
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            return Err(DocumentError::TargetExists { path });
        }
        let bytes = self.encoded_bytes();
        atomic_create_new(&path, &bytes).map_err(|source| DocumentError::Save { path, source })
    }

    /// Rebind a clean file-backed document after its on-disk file was renamed
    /// by the caller. This is intentionally not a save: text state, undo
    /// history, and save-generation remain unchanged, while the disk
    /// fingerprint is refreshed at the new path so the next normal save still
    /// detects external changes.
    pub fn retarget_after_rename(&mut self, path: impl AsRef<Path>) -> Result<(), DocumentError> {
        let path = path.as_ref().to_path_buf();
        if self.read_only {
            return Err(DocumentError::ReadOnly {
                name: self.display_name.clone(),
            });
        }
        if self.is_modified() {
            return Err(DocumentError::ExternalChange {
                path: self.path.clone().unwrap_or(path),
            });
        }
        self.disk_fingerprint =
            Some(
                fingerprint_file(&path).map_err(|source| DocumentError::Read {
                    path: path.clone(),
                    source,
                })?,
            );
        self.display_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.path = Some(path);
        Ok(())
    }

    fn save_to(
        &mut self,
        requested_path: &Path,
        existing_document: bool,
    ) -> Result<(), DocumentError> {
        self.break_undo_group();
        let on_disk = fingerprint_file(requested_path);
        match (existing_document, self.disk_fingerprint, on_disk) {
            (false, _, Ok(_)) => {
                return Err(DocumentError::TargetExists {
                    path: requested_path.to_path_buf(),
                });
            }
            (true, Some(expected), Ok(actual)) if actual != expected => {
                return Err(DocumentError::ExternalChange {
                    path: requested_path.to_path_buf(),
                });
            }
            (true, None, Ok(_)) | (true, Some(_), Err(_)) => {
                return Err(DocumentError::ExternalChange {
                    path: requested_path.to_path_buf(),
                });
            }
            (true, Some(_), Ok(_)) | (true, None, Err(_)) | (false, _, Err(_)) => {}
        }

        #[cfg(unix)]
        if let Ok(metadata) = fs::metadata(requested_path) {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(DocumentError::HardLinked {
                    path: requested_path.to_path_buf(),
                });
            }
        }

        let target = resolve_save_target(requested_path).map_err(|source| DocumentError::Save {
            path: requested_path.to_path_buf(),
            source,
        })?;
        let bytes = self.encoded_bytes();
        atomic_write(&target, &bytes).map_err(|source| DocumentError::Save {
            path: requested_path.to_path_buf(),
            source,
        })?;
        self.disk_fingerprint = Some(fingerprint(&bytes));
        self.saved_state_id = Some(self.state_id);
        self.save_generation = self.save_generation.wrapping_add(1);
        Ok(())
    }

    fn encoded_bytes(&self) -> Vec<u8> {
        let mut text = self.rope.to_string();
        if self.line_ending == LineEnding::CrLf {
            text = text.replace('\n', "\r\n");
        }
        let mut bytes = Vec::with_capacity(text.len() + usize::from(self.utf8_bom) * 3);
        if self.utf8_bom {
            bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    fn fresh_state_id(&mut self) -> u64 {
        let id = self.next_state_id;
        self.next_state_id = self.next_state_id.wrapping_add(1).max(1);
        id
    }
}

fn normalize_newlines(text: &str) -> (String, LineEnding) {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    let ending = if crlf > 0 && crlf * 2 >= lf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    (normalized, ending)
}

fn coalesce(previous: &mut Edit, next: &Edit) -> bool {
    if previous.kind != next.kind || previous.cursor_after != next.cursor_before {
        return false;
    }

    match next.kind {
        EditKind::Insert
            if previous.deleted.is_empty()
                && next.deleted.is_empty()
                && next.start == previous.start + previous.inserted.chars().count() =>
        {
            previous.inserted.push_str(&next.inserted);
        }
        EditKind::Backspace
            if previous.inserted.is_empty()
                && next.inserted.is_empty()
                && next.start + next.deleted.chars().count() == previous.start =>
        {
            previous.start = next.start;
            previous.deleted.insert_str(0, &next.deleted);
        }
        EditKind::Delete
            if previous.inserted.is_empty()
                && next.inserted.is_empty()
                && next.start == previous.start =>
        {
            previous.deleted.push_str(&next.deleted);
        }
        _ => return false,
    }

    previous.cursor_after = next.cursor_after;
    previous.state_after = next.state_after;
    true
}

fn resolve_save_target(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error),
    }
}

fn fingerprint(bytes: &[u8]) -> DiskFingerprint {
    let mut hash = FNV_OFFSET_BASIS;
    update_fingerprint(&mut hash, bytes);
    DiskFingerprint {
        len: bytes.len() as u64,
        hash,
    }
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fingerprint_file(path: &Path) -> io::Result<DiskFingerprint> {
    let (mut file, _) = open_regular_file_for_read(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut len = 0_u64;
    let mut hash = FNV_OFFSET_BASIS;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        len = len.saturating_add(read as u64);
        update_fingerprint(&mut hash, &buffer[..read]);
    }
    Ok(DiskFingerprint { len, hash })
}

fn update_fingerprint(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn read_document_bytes(path: &Path) -> Result<Vec<u8>, DocumentError> {
    let (file, metadata) =
        open_regular_file_for_read(path).map_err(|source| DocumentError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit: MAX_DOCUMENT_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_DOCUMENT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| DocumentError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            path: path.to_path_buf(),
            size: bytes.len() as u64,
            limit: MAX_DOCUMENT_BYTES,
        });
    }
    Ok(bytes)
}

fn open_regular_file_for_read(path: &Path) -> io::Result<(File, fs::Metadata)> {
    // Resolve symlinks first so user-opened symlink paths keep their save
    // spelling while O_NOFOLLOW still protects the final read from a swap.
    // O_NONBLOCK makes a path replaced by a FIFO or device fail validation
    // without hanging the interactive terminal.
    #[cfg(unix)]
    let file = {
        let resolved = fs::canonicalize(path)?;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(resolved)?
    };
    #[cfg(not(unix))]
    let file = OpenOptions::new().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    Ok((file, metadata))
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (parent, temp_path) = temporary_sibling_path(path);

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut temp = options.open(&temp_path)?;

        if let Ok(metadata) = fs::metadata(path) {
            temp.set_permissions(metadata.permissions())?;
        }
        temp.write_all(bytes)?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn atomic_create_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("target already exists: {}", path.display()),
        ));
    }
    let (parent, temp_path) = temporary_sibling_path(path);

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut temp = options.open(&temp_path)?;
        temp.write_all(bytes)?;
        temp.sync_all()?;
        drop(temp);
        fs::hard_link(&temp_path, path)?;
        fs::remove_file(&temp_path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn temporary_sibling_path(path: &Path) -> (&Path, PathBuf) {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path.file_name().unwrap_or_default();
    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(
        ".wscrpt-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temp_path = parent.join(temp_name);
    (parent, temp_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_round_trips_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        fs::write(&path, b"one\r\ntwo\r\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.text(), "one\ntwo\n");
        assert_eq!(doc.line_ending(), LineEnding::CrLf);
        doc.save().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"one\r\ntwo\r\n");
    }

    #[test]
    fn groups_typing_into_one_undo_step() {
        let mut doc = Document::new();
        doc.edit(0..0, "a", 0, 1, EditKind::Insert).unwrap();
        doc.edit(1..1, "🦀", 1, 2, EditKind::Insert).unwrap();
        doc.edit(2..2, "b", 2, 3, EditKind::Insert).unwrap();
        assert_eq!(doc.text(), "a🦀b");
        assert_eq!(doc.undo(), Some(0));
        assert_eq!(doc.text(), "");
        assert_eq!(doc.redo(), Some(3));
        assert_eq!(doc.text(), "a🦀b");
    }

    #[test]
    fn groups_backspaces_in_reading_order() {
        let mut doc = Document::from_text("abc");
        doc.edit(2..3, "", 3, 2, EditKind::Backspace).unwrap();
        doc.edit(1..2, "", 2, 1, EditKind::Backspace).unwrap();
        assert_eq!(doc.text(), "a");
        assert_eq!(doc.undo(), Some(3));
        assert_eq!(doc.text(), "abc");
    }

    #[test]
    fn undo_tracks_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        let mut doc = Document::new();
        doc.edit(0..0, "hello", 0, 5, EditKind::Paste).unwrap();
        assert!(doc.is_modified());
        doc.save_as(&path).unwrap();
        assert!(!doc.is_modified());
        doc.edit(5..5, "!", 5, 6, EditKind::Insert).unwrap();
        assert!(doc.is_modified());
        doc.undo();
        assert!(!doc.is_modified());
    }

    #[cfg(unix)]
    #[test]
    fn saving_via_symlink_preserves_the_link() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "old").unwrap();
        symlink(&target, &link).unwrap();
        let mut doc = Document::open(&link).unwrap();
        doc.edit(0..3, "new", 0, 3, EditKind::Replace).unwrap();
        doc.save().unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn refuses_to_overwrite_an_external_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "original").unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.edit(8..8, " local", 8, 14, EditKind::Insert).unwrap();
        fs::write(&path, "external").unwrap();

        assert!(matches!(
            doc.save(),
            Err(DocumentError::ExternalChange { .. })
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "external");
        assert!(doc.is_modified());
    }

    #[test]
    fn explicit_external_overwrite_rechecks_and_saves_current_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "original\n").unwrap();
        let mut document = Document::open(&path).unwrap();
        document
            .edit(0..8, "editor", 0, 6, EditKind::Replace)
            .unwrap();
        fs::write(&path, "external\n").unwrap();

        document.save_over_external_change().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "editor\n");
        assert!(!document.is_modified());
    }

    #[test]
    fn every_successful_explicit_save_advances_the_save_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "stable\n").unwrap();
        let mut document = Document::open(&path).unwrap();
        assert_eq!(document.save_generation(), 0);

        document.save().unwrap();
        assert_eq!(document.save_generation(), 1);
        document.save().unwrap();
        assert_eq!(document.save_generation(), 2);

        fs::write(&path, "external\n").unwrap();
        assert!(matches!(
            document.save(),
            Err(DocumentError::ExternalChange { .. })
        ));
        assert_eq!(document.save_generation(), 2);
        document.save_over_external_change().unwrap();
        assert_eq!(document.save_generation(), 3);
    }

    #[test]
    fn reload_from_disk_discards_buffer_history_and_refreshes_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "first\n").unwrap();
        let mut document = Document::open(&path).unwrap();
        document
            .edit(0..5, "dirty", 0, 5, EditKind::Replace)
            .unwrap();
        let state_before_reload = document.state_id();
        let save_generation_before_reload = document.save_generation();
        fs::write(&path, "second\n").unwrap();

        document.reload_from_disk().unwrap();
        assert_eq!(document.text(), "second\n");
        assert_ne!(document.state_id(), state_before_reload);
        assert_eq!(document.saved_state_id(), Some(document.state_id()));
        assert_eq!(document.save_generation(), save_generation_before_reload);
        assert!(!document.is_modified());
        assert!(!document.can_undo());
        document.save().unwrap();
    }

    #[test]
    fn clean_initial_reload_gets_a_distinct_saved_state_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "first\n").unwrap();
        let mut document = Document::open(&path).unwrap();
        let initial_state = document.state_id();
        assert_eq!(initial_state, 0);

        fs::write(&path, "externally replaced\n").unwrap();
        document.reload_from_disk().unwrap();

        assert_eq!(document.text(), "externally replaced\n");
        assert_ne!(document.state_id(), initial_state);
        assert_eq!(document.saved_state_id(), Some(document.state_id()));
        assert!(!document.is_modified());
    }

    #[test]
    fn utf8_bom_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        fs::write(&path, b"\xef\xbb\xbfhello\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert!(doc.has_utf8_bom());
        assert_eq!(doc.text(), "hello\n");
        doc.edit(5..5, "!", 5, 6, EditKind::Insert).unwrap();
        doc.save().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"\xef\xbb\xbfhello!\n");
    }

    #[test]
    fn binary_nul_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.dat");
        fs::write(&path, b"text\0more").unwrap();
        assert!(matches!(
            Document::open(path),
            Err(DocumentError::Binary { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_swapped_to_fifo_is_rejected_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server-target.rs");
        fs::write(&path, "regular\n").unwrap();
        assert!(fs::metadata(&path).unwrap().is_file());
        fs::remove_file(&path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );

        let started = std::time::Instant::now();
        let error = Document::open(&path).unwrap_err();

        assert!(matches!(error, DocumentError::Read { .. }));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn oversized_sparse_files_are_refused_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giant.log");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_DOCUMENT_BYTES + 1).unwrap();

        let error = Document::open(&path).unwrap_err();
        assert!(matches!(
            error,
            DocumentError::TooLarge {
                size,
                limit: MAX_DOCUMENT_BYTES,
                ..
            } if size == MAX_DOCUMENT_BYTES + 1
        ));
    }

    #[test]
    fn save_as_does_not_replace_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        fs::write(&path, "keep").unwrap();
        let mut doc = Document::from_text("new");
        assert!(matches!(
            doc.save_as(&path),
            Err(DocumentError::TargetExists { .. })
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_refuses_to_break_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.txt");
        let linked = dir.path().join("two.txt");
        fs::write(&path, "old").unwrap();
        fs::hard_link(&path, &linked).unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.edit(0..3, "new", 0, 3, EditKind::Replace).unwrap();
        assert!(matches!(doc.save(), Err(DocumentError::HardLinked { .. })));
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fs::read_to_string(&linked).unwrap(), "old");
    }
}
