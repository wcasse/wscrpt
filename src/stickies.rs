//! Markdown Stickies: spatial working notes with personal/team storage.
//!
//! Content lives in ordinary Markdown files with a small TOML front matter
//! block. Team notes sit under `.wscrpt/stickies/` in the workspace. Personal
//! notes and any future layout files live only under the XDG state directory
//! so geometry never enters git.
//!
//! This module does not render the TUI picker; `App` lists and opens notes.

use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agent_contract::{
    MAX_LABEL_BYTES, MAX_STICKY_BODY_BYTES, StickyAnchor, StickyNote, StickyStore, validate_id,
};

/// On-disk sticky front-matter schema version.
pub const STICKY_FORMAT_VERSION: u32 = 1;

/// Maximum sticky files inspected in one team or personal directory.
pub const MAX_STICKY_DIRECTORY_ENTRIES: usize = 1_024;
/// Maximum aggregate sticky file bytes loaded during one list.
pub const MAX_STICKY_LIST_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum encoded size of one sticky file.
pub const MAX_STICKY_FILE_BYTES: u64 = 96 * 1024;

const APPLICATION_DIRECTORY: &str = "wscrpt";
const STICKIES_DIRECTORY: &str = "stickies";
const LAYOUT_DIRECTORY: &str = "stickies-layout";
const TEAM_RELATIVE: &str = ".wscrpt/stickies";
const FRONT_MATTER_FENCE: &str = "+++";
const TEMP_CREATE_ATTEMPTS: usize = 32;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Non-fatal problem while listing stickies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickyWarning {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for StickyWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Bounded listing of stickies for one workspace.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StickyListing {
    pub notes: Vec<StickyNote>,
    pub warnings: Vec<StickyWarning>,
    pub partial: bool,
}

/// Host-side sticky library bound to one workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickyLibrary {
    workspace_root: PathBuf,
    workspace_key: String,
    personal_dir: PathBuf,
    team_dir: PathBuf,
    layout_path: PathBuf,
}

impl StickyLibrary {
    /// Build a library for `workspace_root` using standard XDG state paths.
    pub fn for_workspace(workspace_root: impl AsRef<Path>) -> Result<Self, StickyError> {
        let workspace_root = absolute_path(workspace_root.as_ref())?;
        let workspace_key = workspace_key_for(&workspace_root);
        let state_root = state_dir()?;
        let personal_dir = state_root.join(STICKIES_DIRECTORY).join(&workspace_key);
        let layout_path = state_root
            .join(LAYOUT_DIRECTORY)
            .join(format!("{workspace_key}.toml"));
        let team_dir = workspace_root.join(TEAM_RELATIVE);
        Ok(Self {
            workspace_root,
            workspace_key,
            personal_dir,
            team_dir,
            layout_path,
        })
    }

    /// Test helper: pin personal/layout roots under a temporary directory.
    #[cfg(test)]
    pub fn for_workspace_with_state(
        workspace_root: impl AsRef<Path>,
        state_root: impl AsRef<Path>,
    ) -> Result<Self, StickyError> {
        let workspace_root = absolute_path(workspace_root.as_ref())?;
        let workspace_key = workspace_key_for(&workspace_root);
        let state_root = state_root.as_ref().to_path_buf();
        Ok(Self {
            personal_dir: state_root.join(STICKIES_DIRECTORY).join(&workspace_key),
            team_dir: workspace_root.join(TEAM_RELATIVE),
            layout_path: state_root
                .join(LAYOUT_DIRECTORY)
                .join(format!("{workspace_key}.toml")),
            workspace_root,
            workspace_key,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn workspace_key(&self) -> &str {
        &self.workspace_key
    }

    pub fn personal_dir(&self) -> &Path {
        &self.personal_dir
    }

    pub fn team_dir(&self) -> &Path {
        &self.team_dir
    }

    /// Layout file path (XDG only — never under the workspace).
    pub fn layout_path(&self) -> &Path {
        &self.layout_path
    }

    pub fn path_for(&self, store: StickyStore, id: &str) -> Result<PathBuf, StickyError> {
        validate_id(id, "sticky_id").map_err(StickyError::Contract)?;
        let dir = match store {
            StickyStore::Personal => &self.personal_dir,
            StickyStore::Team => &self.team_dir,
        };
        Ok(dir.join(format!("{id}.md")))
    }

    /// Create a new sticky, write it atomically, and return the note.
    pub fn create(
        &self,
        store: StickyStore,
        title: impl Into<String>,
        body_markdown: impl Into<String>,
        anchor: StickyAnchor,
    ) -> Result<StickyNote, StickyError> {
        let now = unix_now_ms();
        let title = title.into();
        let body_markdown = sanitize_markdown(&body_markdown.into());
        let note = StickyNote {
            id: generate_sticky_id(),
            store,
            title: sanitize_title(&title),
            body_markdown,
            anchor,
            archived: false,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        };
        note.validate().map_err(StickyError::Contract)?;
        self.save(&note)?;
        Ok(note)
    }

    /// Atomically write a sticky file for the note's store.
    pub fn save(&self, note: &StickyNote) -> Result<PathBuf, StickyError> {
        note.validate().map_err(StickyError::Contract)?;
        let path = self.path_for(note.store, &note.id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| StickyError::Io {
                action: "create sticky directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let encoded = encode_sticky_file(note)?;
        if encoded.len() as u64 > MAX_STICKY_FILE_BYTES {
            return Err(StickyError::Oversized {
                path: path.clone(),
                limit: MAX_STICKY_FILE_BYTES,
            });
        }
        atomic_write(&path, encoded.as_bytes())?;
        Ok(path)
    }

    /// Load one sticky by id, searching personal then team.
    pub fn load(&self, id: &str) -> Result<StickyNote, StickyError> {
        validate_id(id, "sticky_id").map_err(StickyError::Contract)?;
        for store in [StickyStore::Personal, StickyStore::Team] {
            let path = self.path_for(store, id)?;
            if path.is_file() {
                return self.load_path(&path, store);
            }
        }
        Err(StickyError::NotFound(id.to_owned()))
    }

    /// Load a sticky from an explicit path (used when opening from the picker).
    pub fn load_path(
        &self,
        path: &Path,
        expected_store: StickyStore,
    ) -> Result<StickyNote, StickyError> {
        let bytes = read_bounded(path, MAX_STICKY_FILE_BYTES)?;
        let source = String::from_utf8(bytes).map_err(|_| StickyError::InvalidUtf8 {
            path: path.to_path_buf(),
        })?;
        let mut note = decode_sticky_file(&source).map_err(|message| StickyError::Parse {
            path: path.to_path_buf(),
            message,
        })?;
        note.store = expected_store;
        note.body_markdown = sanitize_markdown(&note.body_markdown);
        note.title = sanitize_title(&note.title);
        note.validate().map_err(StickyError::Contract)?;
        Ok(note)
    }

    /// Mark a sticky archived (content retained).
    pub fn archive(&self, id: &str) -> Result<StickyNote, StickyError> {
        let mut note = self.load(id)?;
        if note.archived {
            return Ok(note);
        }
        note.archived = true;
        note.updated_at_unix_ms = unix_now_ms();
        self.save(&note)?;
        Ok(note)
    }

    /// Restore an archived sticky.
    pub fn unarchive(&self, id: &str) -> Result<StickyNote, StickyError> {
        let mut note = self.load(id)?;
        if !note.archived {
            return Ok(note);
        }
        note.archived = false;
        note.updated_at_unix_ms = unix_now_ms();
        self.save(&note)?;
        Ok(note)
    }

    /// List personal and team stickies with bounded scan and labeled partials.
    pub fn list(&self) -> StickyListing {
        let mut listing = StickyListing::default();
        let mut total_bytes = 0u64;
        self.scan_dir(
            StickyStore::Personal,
            &self.personal_dir,
            &mut listing,
            &mut total_bytes,
        );
        self.scan_dir(
            StickyStore::Team,
            &self.team_dir,
            &mut listing,
            &mut total_bytes,
        );
        listing.notes.sort_by(|left, right| {
            left.archived
                .cmp(&right.archived)
                .then_with(|| right.updated_at_unix_ms.cmp(&left.updated_at_unix_ms))
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        });
        listing
    }

    fn scan_dir(
        &self,
        store: StickyStore,
        directory: &Path,
        listing: &mut StickyListing,
        total_bytes: &mut u64,
    ) {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                listing.warnings.push(StickyWarning {
                    path: directory.to_path_buf(),
                    message: format!("could not read sticky directory: {error}"),
                });
                listing.partial = true;
                return;
            }
        };

        for (seen, entry) in entries.enumerate() {
            if seen >= MAX_STICKY_DIRECTORY_ENTRIES {
                listing.partial = true;
                listing.warnings.push(StickyWarning {
                    path: directory.to_path_buf(),
                    message: format!(
                        "directory entry cap ({MAX_STICKY_DIRECTORY_ENTRIES}) reached; listing partial"
                    ),
                });
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing.warnings.push(StickyWarning {
                        path: directory.to_path_buf(),
                        message: format!("could not read entry: {error}"),
                    });
                    listing.partial = true;
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    listing.warnings.push(StickyWarning {
                        path: path.clone(),
                        message: format!("could not stat: {error}"),
                    });
                    listing.partial = true;
                    continue;
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let len = metadata.len();
            if *total_bytes + len > MAX_STICKY_LIST_BYTES {
                listing.partial = true;
                listing.warnings.push(StickyWarning {
                    path: directory.to_path_buf(),
                    message: format!(
                        "list byte cap ({MAX_STICKY_LIST_BYTES}) reached; listing partial"
                    ),
                });
                break;
            }
            match self.load_path(&path, store) {
                Ok(note) => {
                    *total_bytes = total_bytes.saturating_add(len);
                    listing.notes.push(note);
                }
                Err(error) => {
                    listing.warnings.push(StickyWarning {
                        path,
                        message: error.to_string(),
                    });
                    listing.partial = true;
                }
            }
        }
    }
}

/// Sticky I/O and validation errors.
#[derive(Debug)]
pub enum StickyError {
    Contract(crate::agent_contract::ContractError),
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    NotFound(String),
    InvalidUtf8 {
        path: PathBuf,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    Oversized {
        path: PathBuf,
        limit: u64,
    },
    StateHome,
}

impl fmt::Display for StickyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => write!(f, "{error}"),
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "could not {action} {}: {source}", path.display()),
            Self::NotFound(id) => write!(f, "sticky not found: {id}"),
            Self::InvalidUtf8 { path } => {
                write!(f, "sticky is not valid UTF-8: {}", path.display())
            }
            Self::Parse { path, message } => {
                write!(f, "invalid sticky {}: {message}", path.display())
            }
            Self::Oversized { path, limit } => {
                write!(
                    f,
                    "sticky {} exceeds limit of {limit} bytes",
                    path.display()
                )
            }
            Self::StateHome => write!(f, "HOME and XDG_STATE_HOME are unset"),
        }
    }
}

impl std::error::Error for StickyError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StickyFrontMatter {
    version: u32,
    id: String,
    title: String,
    #[serde(default)]
    archived: bool,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    anchor_kind: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    base_blob: Option<String>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    context_hash: Option<String>,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    preview_session_id: Option<String>,
}

/// Encode a sticky as Markdown with TOML front matter.
pub fn encode_sticky_file(note: &StickyNote) -> Result<String, StickyError> {
    note.validate().map_err(StickyError::Contract)?;
    let matter = StickyFrontMatter {
        version: STICKY_FORMAT_VERSION,
        id: note.id.clone(),
        title: note.title.clone(),
        archived: note.archived,
        created_at_unix_ms: note.created_at_unix_ms,
        updated_at_unix_ms: note.updated_at_unix_ms,
        anchor_kind: anchor_kind(&note.anchor).to_owned(),
        path: anchor_path(&note.anchor),
        base_blob: anchor_base_blob(&note.anchor),
        start_line: anchor_start_line(&note.anchor),
        end_line: anchor_end_line(&note.anchor),
        context_hash: anchor_context_hash(&note.anchor),
        object: anchor_object(&note.anchor),
        preview_session_id: anchor_preview_session(&note.anchor),
    };
    let toml = toml::to_string_pretty(&matter).map_err(|error| StickyError::Parse {
        path: PathBuf::from(format!("{}.md", note.id)),
        message: format!("could not encode front matter: {error}"),
    })?;
    let body = sanitize_markdown(&note.body_markdown);
    Ok(format!(
        "{FRONT_MATTER_FENCE}\n{toml}{FRONT_MATTER_FENCE}\n\n{body}"
    ))
}

/// Decode a sticky Markdown file into a note (store filled by caller).
pub fn decode_sticky_file(source: &str) -> Result<StickyNote, String> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Some(rest) = source.strip_prefix(FRONT_MATTER_FENCE) else {
        return Err("missing opening front matter fence (+++)".to_owned());
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let Some((matter_src, body)) = rest.split_once(FRONT_MATTER_FENCE) else {
        return Err("missing closing front matter fence (+++)".to_owned());
    };
    let matter: StickyFrontMatter =
        toml::from_str(matter_src.trim()).map_err(|error| format!("front matter TOML: {error}"))?;
    if matter.version != STICKY_FORMAT_VERSION {
        return Err(format!(
            "unsupported sticky version {} (expected {STICKY_FORMAT_VERSION})",
            matter.version
        ));
    }
    let body = body.strip_prefix('\n').unwrap_or(body);
    let body = body.strip_prefix('\n').unwrap_or(body);
    let anchor = decode_anchor(&matter)?;
    Ok(StickyNote {
        id: matter.id,
        store: StickyStore::Personal, // caller overwrites from path location
        title: matter.title,
        body_markdown: body.to_owned(),
        anchor,
        archived: matter.archived,
        created_at_unix_ms: matter.created_at_unix_ms,
        updated_at_unix_ms: matter.updated_at_unix_ms,
    })
}

/// One-line label for pickers.
pub fn sticky_label(note: &StickyNote) -> String {
    let store = match note.store {
        StickyStore::Personal => "personal",
        StickyStore::Team => "team",
    };
    let archived = if note.archived { " [archived]" } else { "" };
    let anchor = match &note.anchor {
        StickyAnchor::Workspace => "workspace".to_owned(),
        StickyAnchor::File { path } => path.display().to_string(),
        StickyAnchor::Selection {
            path,
            start_line,
            end_line,
            ..
        } => {
            format!("{}:{start_line}-{end_line}", path.display())
        }
        StickyAnchor::Commit { object } => format!("commit {object}"),
        StickyAnchor::PreviewSession { session_id } => format!("preview {session_id}"),
    };
    format!(
        "{}{}  —  {store}  —  {anchor}  —  {}",
        note.title, archived, note.id
    )
}

/// Strip terminal escapes, NULs, and other control bytes (keep tab/newline).
pub fn sanitize_markdown(input: &str) -> String {
    let without_escapes = strip_terminal_escapes(input);
    let mut out = String::with_capacity(without_escapes.len().min(MAX_STICKY_BODY_BYTES));
    for ch in without_escapes.chars() {
        if out.len() >= MAX_STICKY_BODY_BYTES {
            break;
        }
        match ch {
            '\0' => {}
            c if c == '\n' || c == '\t' || c == '\r' || !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}

fn sanitize_title(input: &str) -> String {
    let cleaned = sanitize_markdown(input);
    let line = cleaned.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "Untitled".to_owned()
    } else if line.len() > MAX_LABEL_BYTES {
        let mut truncated = line.chars().take(MAX_LABEL_BYTES).collect::<String>();
        while truncated.len() > MAX_LABEL_BYTES {
            truncated.pop();
        }
        truncated
    } else {
        line.to_owned()
    }
}

fn strip_terminal_escapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
                continue;
            }
            if i < bytes.len() && bytes[i] == b']' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            continue;
        }
        let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_anchor(matter: &StickyFrontMatter) -> Result<StickyAnchor, String> {
    match matter.anchor_kind.as_str() {
        "workspace" => Ok(StickyAnchor::Workspace),
        "file" => {
            let path = matter
                .path
                .as_deref()
                .ok_or_else(|| "file anchor missing path".to_owned())?;
            Ok(StickyAnchor::File {
                path: PathBuf::from(path),
            })
        }
        "selection" => {
            let path = matter
                .path
                .as_deref()
                .ok_or_else(|| "selection anchor missing path".to_owned())?;
            let base_blob = matter
                .base_blob
                .clone()
                .ok_or_else(|| "selection anchor missing base_blob".to_owned())?;
            let start_line = matter
                .start_line
                .ok_or_else(|| "selection anchor missing start_line".to_owned())?;
            let end_line = matter
                .end_line
                .ok_or_else(|| "selection anchor missing end_line".to_owned())?;
            let context_hash = matter
                .context_hash
                .clone()
                .ok_or_else(|| "selection anchor missing context_hash".to_owned())?;
            Ok(StickyAnchor::Selection {
                path: PathBuf::from(path),
                base_blob,
                start_line,
                end_line,
                context_hash,
            })
        }
        "commit" => {
            let object = matter
                .object
                .clone()
                .ok_or_else(|| "commit anchor missing object".to_owned())?;
            Ok(StickyAnchor::Commit { object })
        }
        "preview" => {
            let session_id = matter
                .preview_session_id
                .clone()
                .ok_or_else(|| "preview anchor missing preview_session_id".to_owned())?;
            Ok(StickyAnchor::PreviewSession { session_id })
        }
        other => Err(format!("unknown anchor_kind `{other}`")),
    }
}

fn anchor_kind(anchor: &StickyAnchor) -> &'static str {
    match anchor {
        StickyAnchor::Workspace => "workspace",
        StickyAnchor::File { .. } => "file",
        StickyAnchor::Selection { .. } => "selection",
        StickyAnchor::Commit { .. } => "commit",
        StickyAnchor::PreviewSession { .. } => "preview",
    }
}

fn anchor_path(anchor: &StickyAnchor) -> Option<String> {
    match anchor {
        StickyAnchor::File { path } | StickyAnchor::Selection { path, .. } => {
            Some(path.display().to_string())
        }
        _ => None,
    }
}

fn anchor_base_blob(anchor: &StickyAnchor) -> Option<String> {
    match anchor {
        StickyAnchor::Selection { base_blob, .. } => Some(base_blob.clone()),
        _ => None,
    }
}

fn anchor_start_line(anchor: &StickyAnchor) -> Option<u32> {
    match anchor {
        StickyAnchor::Selection { start_line, .. } => Some(*start_line),
        _ => None,
    }
}

fn anchor_end_line(anchor: &StickyAnchor) -> Option<u32> {
    match anchor {
        StickyAnchor::Selection { end_line, .. } => Some(*end_line),
        _ => None,
    }
}

fn anchor_context_hash(anchor: &StickyAnchor) -> Option<String> {
    match anchor {
        StickyAnchor::Selection { context_hash, .. } => Some(context_hash.clone()),
        _ => None,
    }
}

fn anchor_object(anchor: &StickyAnchor) -> Option<String> {
    match anchor {
        StickyAnchor::Commit { object } => Some(object.clone()),
        _ => None,
    }
}

fn anchor_preview_session(anchor: &StickyAnchor) -> Option<String> {
    match anchor {
        StickyAnchor::PreviewSession { session_id } => Some(session_id.clone()),
        _ => None,
    }
}

/// Opaque filename-safe sticky id.
pub fn generate_sticky_id() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let sequence = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "s-{:016x}-{:08x}-{:08x}-{sequence:016x}",
        duration.as_secs(),
        duration.subsec_nanos(),
        process::id()
    )
}

fn workspace_key_for(root: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    format!("ws-{:016x}", hasher.finish())
}

fn state_dir() -> Result<PathBuf, StickyError> {
    if let Some(root) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return Ok(root.join(APPLICATION_DIRECTORY));
        }
    }
    let home = env::var_os("HOME").ok_or(StickyError::StateHome)?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join(APPLICATION_DIRECTORY))
}

fn absolute_path(path: &Path) -> Result<PathBuf, StickyError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = env::current_dir().map_err(|source| StickyError::Io {
        action: "resolve workspace",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(cwd.join(path))
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, StickyError> {
    let metadata = fs::metadata(path).map_err(|source| StickyError::Io {
        action: "stat sticky",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > limit {
        return Err(StickyError::Oversized {
            path: path.to_path_buf(),
            limit,
        });
    }
    fs::read(path).map_err(|source| StickyError::Io {
        action: "read sticky",
        path: path.to_path_buf(),
        source,
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StickyError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| StickyError::Io {
        action: "create sticky directory",
        path: parent.to_path_buf(),
        source,
    })?;

    let mut last_collision = None;
    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".{}.tmp-{:08x}-{sequence:016x}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("sticky"),
            process::id()
        );
        let temporary = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(mut file) => {
                file.write_all(bytes).map_err(|source| StickyError::Io {
                    action: "write sticky temporary",
                    path: temporary.clone(),
                    source,
                })?;
                file.sync_all().map_err(|source| StickyError::Io {
                    action: "sync sticky temporary",
                    path: temporary.clone(),
                    source,
                })?;
                drop(file);
                fs::rename(&temporary, path).map_err(|source| StickyError::Io {
                    action: "publish sticky",
                    path: path.to_path_buf(),
                    source,
                })?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
            }
            Err(source) => {
                return Err(StickyError::Io {
                    action: "create sticky temporary",
                    path: temporary,
                    source,
                });
            }
        }
    }
    Err(StickyError::Io {
        action: "create sticky temporary",
        path: parent.to_path_buf(),
        source: last_collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary sticky name collision",
            )
        }),
    })
}

/// Default anchor when creating a sticky from the current buffer path.
///
/// Paths outside the workspace become a workspace anchor so personal notes
/// never store absolute host paths in shareable team content by accident.
pub fn anchor_for_open_file(workspace_root: &Path, path: Option<&Path>) -> StickyAnchor {
    let Some(path) = path else {
        return StickyAnchor::Workspace;
    };
    if let Ok(relative) = path.strip_prefix(workspace_root)
        && !relative.as_os_str().is_empty()
        && crate::agent_contract::normalize_relative(relative).is_ok()
    {
        return StickyAnchor::File {
            path: relative.to_path_buf(),
        };
    }
    StickyAnchor::Workspace
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn library() -> (TempDir, TempDir, StickyLibrary) {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let library =
            StickyLibrary::for_workspace_with_state(workspace.path(), state.path()).unwrap();
        (workspace, state, library)
    }

    #[test]
    fn create_save_load_round_trip_personal_and_team() {
        let (_workspace, state, library) = library();
        let personal = library
            .create(
                StickyStore::Personal,
                "Personal note",
                "hello **world**\n",
                StickyAnchor::Workspace,
            )
            .unwrap();
        let team = library
            .create(
                StickyStore::Team,
                "Team note",
                "- [ ] ship W1\n",
                StickyAnchor::File {
                    path: PathBuf::from("src/stickies.rs"),
                },
            )
            .unwrap();

        let loaded_personal = library.load(&personal.id).unwrap();
        assert_eq!(loaded_personal.title, "Personal note");
        assert_eq!(loaded_personal.body_markdown, "hello **world**\n");
        assert_eq!(loaded_personal.store, StickyStore::Personal);

        let loaded_team = library.load(&team.id).unwrap();
        assert_eq!(loaded_team.store, StickyStore::Team);
        assert!(
            library
                .path_for(StickyStore::Team, &team.id)
                .unwrap()
                .is_file()
        );
        assert!(
            library
                .path_for(StickyStore::Team, &team.id)
                .unwrap()
                .starts_with(library.team_dir())
        );
        assert!(
            library
                .path_for(StickyStore::Personal, &personal.id)
                .unwrap()
                .starts_with(state.path())
        );
        // Layout path is under state, never the workspace.
        assert!(library.layout_path().starts_with(state.path()));
        assert!(!library.layout_path().starts_with(library.workspace_root()));
    }

    #[test]
    fn list_orders_active_before_archived_and_recovers_from_garbage() {
        let (_workspace, _state, library) = library();
        let active = library
            .create(
                StickyStore::Personal,
                "Active",
                "body\n",
                StickyAnchor::Workspace,
            )
            .unwrap();
        let mut archived = library
            .create(
                StickyStore::Personal,
                "Old",
                "body\n",
                StickyAnchor::Workspace,
            )
            .unwrap();
        archived.archived = true;
        archived.updated_at_unix_ms = active.updated_at_unix_ms.saturating_add(10);
        library.save(&archived).unwrap();

        fs::create_dir_all(library.personal_dir()).unwrap();
        fs::write(library.personal_dir().join("broken.md"), "not a sticky").unwrap();

        let listing = library.list();
        assert!(listing.partial);
        assert!(!listing.warnings.is_empty());
        assert_eq!(listing.notes.len(), 2);
        assert_eq!(listing.notes[0].id, active.id);
        assert!(listing.notes[1].archived);
    }

    #[test]
    fn archive_and_unarchive() {
        let (_workspace, _state, library) = library();
        let note = library
            .create(
                StickyStore::Personal,
                "Toggle",
                "x\n",
                StickyAnchor::Workspace,
            )
            .unwrap();
        let archived = library.archive(&note.id).unwrap();
        assert!(archived.archived);
        let restored = library.unarchive(&note.id).unwrap();
        assert!(!restored.archived);
    }

    #[test]
    fn sanitizes_escapes_and_control_characters() {
        let dirty = "ok\u{1b}[31mRED\u{1b}[0m\0\u{07} done";
        let clean = sanitize_markdown(dirty);
        assert_eq!(clean, "okRED done");
    }

    #[test]
    fn encode_decode_preserves_selection_anchor() {
        let note = StickyNote {
            id: "s-test-1".to_owned(),
            store: StickyStore::Team,
            title: "Selection".to_owned(),
            body_markdown: "context\n".to_owned(),
            anchor: StickyAnchor::Selection {
                path: PathBuf::from("src/app.rs"),
                base_blob: "deadbeef".to_owned(),
                start_line: 10,
                end_line: 12,
                context_hash: "ctx".to_owned(),
            },
            archived: false,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
        };
        let encoded = encode_sticky_file(&note).unwrap();
        let decoded = decode_sticky_file(&encoded).unwrap();
        assert_eq!(decoded.anchor, note.anchor);
        assert_eq!(decoded.body_markdown, "context\n");
    }

    #[test]
    fn truncated_front_matter_is_a_parse_error() {
        let err = decode_sticky_file("+++\nid = \"x\"\n").unwrap_err();
        assert!(err.contains("closing front matter"));
    }
}
