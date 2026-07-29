//! Project-file indexing and fuzzy matching for quick open.
//!
//! The index deliberately stays independent of the editor UI. Paths are stored
//! relative to the project root so callers can display them directly and join
//! them to [`ProjectIndex::root`] when opening a file.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::{CStr, CString, OsString};
#[cfg(any(not(unix), test))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read};
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(all(unix, test))]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
use libc::___errno as errno_location;
#[cfg(any(
    target_os = "android",
    target_os = "cygwin",
    target_os = "netbsd",
    target_os = "openbsd"
))]
use libc::__errno as errno_location;
#[cfg(any(
    target_os = "dragonfly",
    target_os = "emscripten",
    target_os = "hurd",
    target_os = "linux",
    target_os = "redox"
))]
use libc::__errno_location as errno_location;
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
use libc::__error as errno_location;
#[cfg(target_os = "nto")]
use libc::__get_errno_ptr as errno_location;
#[cfg(target_os = "aix")]
use libc::_Errno as errno_location;
#[cfg(target_os = "haiku")]
use libc::_errnop as errno_location;

/// The largest number of matches a single quick-open query may return.
pub const MAX_RESULTS: usize = 100;

/// The result limit used by [`ProjectIndex::quick_open`].
pub const DEFAULT_RESULT_LIMIT: usize = 40;

/// Maximum number of text files retained in one workspace index.
pub const MAX_INDEXED_FILES: usize = 50_000;
/// Maximum aggregate encoded bytes retained by the text-file index paths.
pub const MAX_INDEXED_PATH_BYTES: usize = 32 * 1024 * 1024;
/// Maximum number of directories and regular files retained for the explorer.
pub const MAX_TREE_ENTRIES: usize = 100_000;
/// Maximum aggregate encoded bytes retained by explorer paths.
pub const MAX_TREE_PATH_BYTES: usize = 64 * 1024 * 1024;
/// Maximum bytes returned by one bounded explorer-file read.
pub const MAX_TREE_FILE_READ_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum number of directory entries inspected while building an index.
pub const MAX_SCANNED_ENTRIES: usize = 200_000;
/// Maximum directory nesting traversed below the workspace root.
pub const MAX_DIRECTORY_DEPTH: usize = 64;

const TEXT_SAMPLE_BYTES: u64 = 8 * 1024;
const MAX_RELATIVE_PATH_BYTES: usize = 4 * 1024;
const SCORE_FLOOR: i64 = i64::MIN / 4;

const IGNORED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".gradle",
    ".idea",
    ".mypy_cache",
    ".next",
    ".nuxt",
    ".parcel-cache",
    ".pytest_cache",
    ".ruff_cache",
    ".turbo",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "target",
    "tmp",
    "vendor",
    "venv",
];

const IGNORED_FILES: &[&str] = &[".DS_Store", ".git"];

/// A deterministic, bounded workspace snapshot below a project root.
///
/// Text-like regular files are retained separately for search. The tree view
/// also contains non-ignored directories and binary regular files, but never
/// symlinks or special files.
#[derive(Clone, Debug)]
pub struct ProjectIndex {
    root: PathBuf,
    #[cfg(unix)]
    root_directory: Arc<File>,
    files: Arc<[PathBuf]>,
    tree_entries: Arc<[ProjectTreeEntry]>,
    truncated: bool,
    tree_truncated: bool,
}

impl PartialEq for ProjectIndex {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
            && self.files == other.files
            && self.tree_entries == other.tree_entries
            && self.truncated == other.truncated
            && self.tree_truncated == other.tree_truncated
    }
}

impl Eq for ProjectIndex {}

/// One root-relative entry in the immutable workspace explorer snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTreeEntry {
    /// Raw root-relative path. It may not be valid UTF-8.
    pub path: PathBuf,
    /// `true` for a directory and `false` for a regular file.
    pub is_directory: bool,
}

/// One ranked quick-open result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickOpenMatch {
    /// Path relative to the [`ProjectIndex`] root.
    pub path: PathBuf,
    /// Higher scores are better. Scores should only be compared within a query.
    pub score: i64,
}

/// A bounded ranked search together with whether additional matches existed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedSearchResults {
    /// The retained top-ranked paths, never longer than [`MAX_RESULTS`].
    pub matches: Vec<QuickOpenMatch>,
    /// `true` when at least one matching path was omitted by the result limit.
    pub has_more: bool,
}

/// Reverses result quality so [`BinaryHeap::peek`] returns the worst retained
/// quick-open match. This lets search keep at most the requested top-k paths.
struct RankedQuickOpenMatch(QuickOpenMatch);

impl PartialEq for RankedQuickOpenMatch {
    fn eq(&self, other: &Self) -> bool {
        self.0.score == other.0.score
            && compare_paths(&self.0.path, &other.0.path) == Ordering::Equal
    }
}

impl Eq for RankedQuickOpenMatch {}

impl PartialOrd for RankedQuickOpenMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedQuickOpenMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .score
            .cmp(&self.0.score)
            .then_with(|| compare_paths(&self.0.path, &other.0.path))
    }
}

impl ProjectIndex {
    /// Recursively snapshots non-ignored directories and regular files under
    /// `root`, while separately indexing text-like files for content search.
    ///
    /// Directory symlinks are not followed. Failure to read the root is
    /// returned, while unreadable nested directories and files are skipped so a
    /// single protected cache directory does not disable quick open.
    pub fn build(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("project root is not a directory: {}", root.display()),
            ));
        }

        let collected = collect_files(&root)?;
        Ok(Self {
            root,
            #[cfg(unix)]
            root_directory: collected.root_directory,
            files: collected.files.into(),
            tree_entries: collected.tree_entries.into(),
            truncated: collected.truncated,
            tree_truncated: collected.tree_truncated,
        })
    }

    /// Rebuilds the file list while preserving the same canonical root.
    pub fn refresh(&mut self) -> io::Result<()> {
        let collected = collect_files(&self.root)?;
        #[cfg(unix)]
        {
            self.root_directory = collected.root_directory;
        }
        self.files = collected.files.into();
        self.tree_entries = collected.tree_entries.into();
        self.truncated = collected.truncated;
        self.tree_truncated = collected.tree_truncated;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns relative paths in stable lexical order.
    pub fn files(&self) -> &[PathBuf] {
        self.files.as_ref()
    }

    /// Returns directories and regular files in stable lexical order.
    ///
    /// This is a separately bounded snapshot for workspace navigation. Unlike
    /// [`Self::files`], it includes binary regular files and empty directories.
    pub fn tree_entries(&self) -> &[ProjectTreeEntry] {
        self.tree_entries.as_ref()
    }

    /// Checks the immutable explorer snapshot using its raw-path ordering.
    pub fn contains_tree_path(&self, relative: impl AsRef<Path>) -> bool {
        let relative = relative.as_ref();
        self.tree_entries
            .binary_search_by(|entry| compare_paths(&entry.path, relative))
            .is_ok()
    }

    /// Checks whether the explorer snapshot contains a regular file at `relative`.
    pub fn contains_tree_file(&self, relative: impl AsRef<Path>) -> bool {
        let relative = relative.as_ref();
        self.tree_entries
            .binary_search_by(|entry| compare_paths(&entry.path, relative))
            .ok()
            .is_some_and(|entry| !self.tree_entries[entry].is_directory)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Whether the text-file index is partial because a safety bound was hit.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Whether the workspace explorer snapshot is partial because a safety
    /// bound was hit.
    pub fn is_tree_truncated(&self) -> bool {
        self.tree_truncated
    }

    /// Returns a root-relative path as an absolute project path.
    pub fn absolute_path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    /// Searches the index, clamping `limit` to [`MAX_RESULTS`].
    ///
    /// An empty query returns the first paths in deterministic index order.
    pub fn search(&self, query: &str, limit: usize) -> Vec<QuickOpenMatch> {
        bounded_path_search(self.files.iter(), query, limit)
    }

    /// Searches with the UI-oriented default result bound.
    pub fn quick_open(&self, query: &str) -> Vec<QuickOpenMatch> {
        self.search(query, DEFAULT_RESULT_LIMIT)
    }

    /// Searches all regular explorer entries, including binary files.
    ///
    /// Results and retained ranking state are clamped to [`MAX_RESULTS`]. Empty
    /// queries return the first regular files in deterministic tree order.
    pub fn search_tree_files(&self, query: &str, limit: usize) -> Vec<QuickOpenMatch> {
        self.search_tree_files_with_metadata(query, limit).matches
    }

    /// Searches regular explorer entries and reports result-limit truncation.
    pub fn search_tree_files_with_metadata(
        &self,
        query: &str,
        limit: usize,
    ) -> BoundedSearchResults {
        bounded_path_search_with_metadata(
            self.tree_entries
                .iter()
                .filter(|entry| !entry.is_directory)
                .map(|entry| &entry.path),
            query,
            limit,
        )
    }

    /// Revalidates and reads one regular file from the explorer snapshot.
    ///
    /// The result is capped by both `max_bytes` and
    /// [`MAX_TREE_FILE_READ_BYTES`]. On Unix every path component is opened
    /// relative to the held workspace-root descriptor with symlink following
    /// disabled, so a later ancestor swap cannot escape the indexed workspace.
    pub fn read_tree_file(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: impl TryInto<u64>,
    ) -> io::Result<Vec<u8>> {
        let relative = relative.as_ref();
        let entry_index = self
            .tree_entries
            .binary_search_by(|entry| compare_paths(&entry.path, relative))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "path is not in the workspace-tree snapshot: {}",
                        relative.display()
                    ),
                )
            })?;
        if self.tree_entries[entry_index].is_directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("workspace-tree path is a directory: {}", relative.display()),
            ));
        }

        let max_bytes = max_bytes.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace-tree read limit cannot be represented as u64",
            )
        })?;
        let limit = max_bytes.min(MAX_TREE_FILE_READ_BYTES);
        #[cfg(unix)]
        let file = open_unix_tree_file(&self.root_directory, relative)?;
        #[cfg(not(unix))]
        let file = {
            let canonical = fs::canonicalize(self.root.join(relative))?;
            if !canonical.starts_with(&self.root) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "workspace-tree path escapes the project root: {}",
                        relative.display()
                    ),
                ));
            }
            open_regular_file_for_index(&canonical)?
        };
        read_bounded_tree_file(file, relative, limit)
    }

    /// Verifies that the canonical workspace path still names the directory
    /// held open by this immutable index snapshot.
    ///
    /// On Unix this compares the device and inode of a freshly, component-wise
    /// `O_NOFOLLOW`-opened root with the held descriptor. This prevents a root
    /// rename/replacement from associating bytes read through the old descriptor
    /// with paths below a new directory (or symlink) at the same spelling.
    pub fn validate_root_identity(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let current = open_unix_directory_path(&self.root)?;
            let held = self.root_directory.metadata()?;
            let current = current.metadata()?;
            if held.dev() != current.dev() || held.ino() != current.ino() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "workspace root identity changed since indexing: {}",
                        self.root.display()
                    ),
                ));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let current = fs::canonicalize(&self.root)?;
            if current != self.root || !current.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "workspace root identity changed since indexing: {}",
                        self.root.display()
                    ),
                ));
            }
            Ok(())
        }
    }

    /// Revalidates and reads one regular file from the immutable text index.
    ///
    /// This is intentionally separate from [`Self::read_tree_file`]: explorer
    /// retention has independent limits, so a complete text index can contain a
    /// file omitted from a partial tree snapshot. On Unix, root identity is
    /// checked first and every path component is then opened relative to the
    /// held root descriptor with symlink following disabled.
    pub fn read_indexed_file(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: impl TryInto<u64>,
    ) -> io::Result<Vec<u8>> {
        self.validate_root_identity()?;
        self.read_indexed_file_from_held_root(relative, max_bytes)
    }

    /// Reads through the already-held root after a caller has validated its
    /// path identity for a bounded batch. Batch callers must validate again
    /// after their final read before publishing any path-associated result.
    pub(crate) fn read_indexed_file_from_held_root(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: impl TryInto<u64>,
    ) -> io::Result<Vec<u8>> {
        let relative = relative.as_ref();
        if self
            .files
            .binary_search_by(|path| compare_paths(path, relative))
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "path is not in the workspace text-index snapshot: {}",
                    relative.display()
                ),
            ));
        }

        let max_bytes = max_bytes.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace text-index read limit cannot be represented as u64",
            )
        })?;
        let limit = max_bytes.min(MAX_TREE_FILE_READ_BYTES);
        #[cfg(unix)]
        let file = open_unix_tree_file(&self.root_directory, relative)?;
        #[cfg(not(unix))]
        let file = {
            let canonical = fs::canonicalize(self.root.join(relative))?;
            if !canonical.starts_with(&self.root) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "workspace text-index path escapes the project root: {}",
                        relative.display()
                    ),
                ));
            }
            open_regular_file_for_index(&canonical)?
        };
        read_bounded_tree_file(file, relative, limit)
    }
}

fn bounded_path_search<'a>(
    paths: impl Iterator<Item = &'a PathBuf>,
    query: &str,
    limit: usize,
) -> Vec<QuickOpenMatch> {
    bounded_path_search_with_metadata(paths, query, limit).matches
}

fn bounded_path_search_with_metadata<'a>(
    mut paths: impl Iterator<Item = &'a PathBuf>,
    query: &str,
    limit: usize,
) -> BoundedSearchResults {
    let limit = limit.min(MAX_RESULTS);
    let query = query.trim();
    if query.is_empty() {
        let matches = paths
            .by_ref()
            .take(limit)
            .cloned()
            .map(|path| QuickOpenMatch { path, score: 0 })
            .collect();
        return BoundedSearchResults {
            matches,
            has_more: paths.next().is_some(),
        };
    }

    let query = normalize_for_match(query);
    let mut matches = BinaryHeap::with_capacity(limit);
    let mut matching_paths = 0_usize;
    for path in paths {
        let display = display_path(path);
        let Some(score) = fuzzy_path_score_normalized(&query, &display) else {
            continue;
        };
        matching_paths = matching_paths.saturating_add(1);
        if limit == 0 {
            continue;
        }
        let matched = RankedQuickOpenMatch(QuickOpenMatch {
            path: path.clone(),
            score,
        });
        if matches.len() < limit {
            matches.push(matched);
        } else if matches.peek().is_some_and(|worst| matched < *worst) {
            matches.pop();
            matches.push(matched);
        }
    }

    let mut matches: Vec<_> = matches.into_iter().map(|ranked| ranked.0).collect();
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| compare_paths(&left.path, &right.path))
    });
    BoundedSearchResults {
        matches,
        has_more: matching_paths > limit,
    }
}

/// Scores `candidate` as a fuzzy, case-insensitive path match for `query`.
///
/// Matching is subsequence based. Consecutive characters, path/word
/// boundaries, basenames, filename stems, prefixes, and suffixes receive
/// bonuses; gaps, long paths, and late matches receive small penalties.
/// Returns `None` when every query character cannot be found in order.
pub fn fuzzy_path_score(query: &str, candidate: &str) -> Option<i64> {
    let query = normalize_for_match(query.trim());
    fuzzy_path_score_normalized(&query, candidate)
}

fn fuzzy_path_score_normalized(query: &str, candidate: &str) -> Option<i64> {
    let candidate_display = normalize_path(candidate);
    let candidate_folded = normalize_for_match(&candidate_display);

    if query.is_empty() {
        return Some(0);
    }
    if candidate_folded.is_empty() {
        return None;
    }

    let query_chars: Vec<char> = query.chars().collect();
    let candidate_chars: Vec<char> = candidate_folded.chars().collect();
    let original_chars: Vec<char> = candidate_display.chars().collect();
    if query_chars.len() > candidate_chars.len() {
        return None;
    }

    let basename_start = candidate_chars
        .iter()
        .rposition(|character| *character == '/')
        .map_or(0, |index| index + 1);

    let mut previous = vec![SCORE_FLOOR; candidate_chars.len()];
    for (candidate_index, candidate_char) in candidate_chars.iter().enumerate() {
        if *candidate_char == query_chars[0] {
            previous[candidate_index] = character_score(
                candidate_index,
                basename_start,
                &candidate_chars,
                &original_chars,
            ) - leading_penalty(candidate_index);
        }
    }

    for query_char in query_chars.iter().skip(1) {
        let mut current = vec![SCORE_FLOOR; candidate_chars.len()];
        let mut best_gapped = SCORE_FLOOR;

        for candidate_index in 1..candidate_chars.len() {
            let predecessor = previous[candidate_index - 1];
            if predecessor > SCORE_FLOOR {
                best_gapped = best_gapped.max(predecessor + candidate_index as i64 - 1);
            }
            if candidate_chars[candidate_index] != *query_char {
                continue;
            }

            let consecutive = if predecessor > SCORE_FLOOR {
                predecessor + 18
            } else {
                SCORE_FLOOR
            };
            let gapped = if best_gapped > SCORE_FLOOR {
                // One point per skipped character.
                best_gapped - candidate_index as i64 + 1
            } else {
                SCORE_FLOOR
            };
            let prior = consecutive.max(gapped);
            if prior > SCORE_FLOOR {
                current[candidate_index] = prior
                    + character_score(
                        candidate_index,
                        basename_start,
                        &candidate_chars,
                        &original_chars,
                    );
            }
        }

        previous = current;
    }

    let mut best = SCORE_FLOOR;
    for (last_match, score) in previous.into_iter().enumerate() {
        if score <= SCORE_FLOOR {
            continue;
        }
        let trailing = candidate_chars.len().saturating_sub(last_match + 1) as i64;
        best = best.max(score - trailing / 4);
    }
    if best <= SCORE_FLOOR {
        return None;
    }

    let basename = candidate_folded
        .rsplit_once('/')
        .map_or(candidate_folded.as_str(), |(_, name)| name);
    let stem = filename_stem(basename);

    if candidate_folded == query {
        best += 400;
    } else if basename == query {
        best += 300;
    } else if stem == query {
        best += 260;
    } else {
        if candidate_folded.ends_with(query) {
            best += 150;
        }
        if basename.starts_with(query) {
            best += 130;
        } else if stem.starts_with(query) {
            best += 110;
        }
        if candidate_folded.starts_with(query) {
            best += 90;
        }
        if candidate_folded.contains(query) {
            best += 70;
        }
    }

    let depth = candidate_chars
        .iter()
        .filter(|character| **character == '/')
        .count() as i64;
    best -= depth * 2;
    best -= candidate_chars.len() as i64 / 20;
    Some(best)
}

#[derive(Clone, Copy)]
struct WalkLimits {
    files: usize,
    indexed_path_bytes: usize,
    tree_entries: usize,
    tree_path_bytes: usize,
    entries: usize,
    depth: usize,
}

struct CollectedFiles {
    #[cfg(unix)]
    root_directory: Arc<File>,
    files: Vec<PathBuf>,
    tree_entries: Vec<ProjectTreeEntry>,
    truncated: bool,
    tree_truncated: bool,
}

fn collect_files(root: &Path) -> io::Result<CollectedFiles> {
    let limits = WalkLimits {
        files: MAX_INDEXED_FILES,
        indexed_path_bytes: MAX_INDEXED_PATH_BYTES,
        tree_entries: MAX_TREE_ENTRIES,
        tree_path_bytes: MAX_TREE_PATH_BYTES,
        entries: MAX_SCANNED_ENTRIES,
        depth: MAX_DIRECTORY_DEPTH,
    };
    #[cfg(unix)]
    {
        collect_files_with_limits_unix(root, limits, None)
    }
    #[cfg(not(unix))]
    {
        collect_files_with_limits_portable(root, limits)
    }
}

#[cfg(test)]
fn collect_files_with_limits(root: &Path, limits: WalkLimits) -> io::Result<CollectedFiles> {
    #[cfg(unix)]
    {
        let canonical = fs::canonicalize(root)?;
        collect_files_with_limits_unix(&canonical, limits, None)
    }
    #[cfg(not(unix))]
    {
        collect_files_with_limits_portable(root, limits)
    }
}

#[cfg(unix)]
enum UnixPendingDirectory {
    Open {
        directory: Arc<File>,
        relative: PathBuf,
        depth: usize,
    },
    Child {
        parent: Arc<File>,
        name: OsString,
        relative: PathBuf,
        depth: usize,
    },
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnixEntryKind {
    Directory,
    RegularFile,
    Other,
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn collect_files_with_limits_unix(
    root: &Path,
    limits: WalkLimits,
    mut before_child_open: Option<&mut dyn FnMut(&Path)>,
) -> io::Result<CollectedFiles> {
    let root_directory = Arc::new(open_unix_directory_path(root)?);
    let mut files = Vec::new();
    let mut tree_entries = Vec::new();
    let mut directories = vec![UnixPendingDirectory::Open {
        directory: Arc::clone(&root_directory),
        relative: PathBuf::new(),
        depth: 0,
    }];
    let mut scanned_entries = 0_usize;
    let mut truncated = false;
    let mut tree_truncated = false;
    let mut indexed_path_bytes = 0_usize;
    let mut tree_path_bytes = 0_usize;
    let file_limit = limits.files.min(MAX_INDEXED_FILES);
    let indexed_path_byte_limit = limits.indexed_path_bytes.min(MAX_INDEXED_PATH_BYTES);
    let tree_entry_limit = limits.tree_entries.min(MAX_TREE_ENTRIES);
    let tree_path_byte_limit = limits.tree_path_bytes.min(MAX_TREE_PATH_BYTES);
    let scanned_entry_limit = limits.entries.min(MAX_SCANNED_ENTRIES);
    let depth_limit = limits.depth.min(MAX_DIRECTORY_DEPTH);

    'walk: while let Some(pending) = directories.pop() {
        let (directory, relative_directory, depth) = match pending {
            UnixPendingDirectory::Open {
                directory,
                relative,
                depth,
            } => (directory, relative, depth),
            UnixPendingDirectory::Child {
                parent,
                name,
                relative,
                depth,
            } => {
                if let Some(hook) = before_child_open.as_mut() {
                    (**hook)(&relative);
                }
                match open_unix_directory_at(&parent, &name) {
                    Ok(directory) => (Arc::new(directory), relative, depth),
                    Err(_) => {
                        truncated = true;
                        tree_truncated = true;
                        continue;
                    }
                }
            }
        };

        let remaining_entries = scanned_entry_limit.saturating_sub(scanned_entries);
        let (mut names, directory_complete) =
            match read_unix_directory_names(&directory, remaining_entries) {
                Ok(result) => result,
                Err(error) if depth == 0 => return Err(error),
                Err(_) => {
                    truncated = true;
                    tree_truncated = true;
                    continue;
                }
            };
        scanned_entries += names.len();
        if !directory_complete {
            truncated = true;
            tree_truncated = true;
        }
        names.sort_by(|left, right| compare_os_str(left, right));

        let mut child_directories = Vec::new();
        for name in names {
            let kind = match classify_unix_entry(&directory, &name) {
                Ok(kind) => kind,
                Err(_) => {
                    truncated = true;
                    tree_truncated = true;
                    continue;
                }
            };
            let relative = relative_directory.join(&name);
            match kind {
                UnixEntryKind::Directory if !is_ignored_directory(&name) => {
                    retain_tree_entry(
                        &mut tree_entries,
                        &mut tree_path_bytes,
                        &mut tree_truncated,
                        &relative,
                        true,
                        tree_entry_limit,
                        tree_path_byte_limit,
                    );
                    if depth >= depth_limit {
                        truncated = true;
                        tree_truncated = true;
                    } else {
                        child_directories.push(UnixPendingDirectory::Child {
                            parent: Arc::clone(&directory),
                            name,
                            relative,
                            depth: depth + 1,
                        });
                    }
                }
                UnixEntryKind::RegularFile if !is_ignored_file(&name) => {
                    let file = match open_unix_regular_file_at(&directory, &name) {
                        Ok(file) => file,
                        Err(_) => {
                            truncated = true;
                            tree_truncated = true;
                            continue;
                        }
                    };
                    retain_tree_entry(
                        &mut tree_entries,
                        &mut tree_path_bytes,
                        &mut tree_truncated,
                        &relative,
                        false,
                        tree_entry_limit,
                        tree_path_byte_limit,
                    );
                    match is_probably_text_file(file) {
                        Ok(true) => retain_indexed_file(
                            &mut files,
                            &mut indexed_path_bytes,
                            &mut truncated,
                            &relative,
                            file_limit,
                            indexed_path_byte_limit,
                        ),
                        Ok(false) => {}
                        Err(_) => truncated = true,
                    }
                }
                UnixEntryKind::Directory | UnixEntryKind::RegularFile | UnixEntryKind::Other => {}
            }
        }

        if !directory_complete {
            break 'walk;
        }
        directories.extend(child_directories.into_iter().rev());
    }

    files.sort_by(|left, right| compare_paths(left, right));
    files.dedup();
    if files.len() > file_limit {
        files.truncate(file_limit);
        truncated = true;
    }

    tree_entries.sort_by(|left, right| {
        compare_paths(&left.path, &right.path)
            .then_with(|| right.is_directory.cmp(&left.is_directory))
    });
    tree_entries.dedup();
    if tree_entries.len() > tree_entry_limit {
        tree_entries.truncate(tree_entry_limit);
        tree_truncated = true;
    }

    Ok(CollectedFiles {
        root_directory,
        files,
        tree_entries,
        truncated,
        tree_truncated,
    })
}

#[cfg(unix)]
struct UnixDirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for UnixDirectoryStream {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a unique non-null pointer returned by `fdopendir`.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(unix)]
fn read_unix_directory_names(directory: &File, limit: usize) -> io::Result<(Vec<OsString>, bool)> {
    if limit == 0 {
        return Ok((Vec::new(), false));
    }

    // SAFETY: `fcntl` duplicates a live descriptor; ownership transfers to
    // `fdopendir` below or is closed on its error path.
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duplicate` is an owned directory descriptor.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so ownership did not transfer.
        unsafe {
            libc::close(duplicate);
        }
        return Err(error);
    }
    let stream = UnixDirectoryStream(stream);
    let mut names = Vec::new();

    loop {
        if names.len() >= limit {
            return Ok((names, false));
        }
        // POSIX requires callers to clear errno to distinguish EOF from error.
        // SAFETY: errno storage is thread-local and valid for this thread.
        unsafe {
            *errno_location() = 0;
        }
        // SAFETY: the stream remains owned and open for this call.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: errno storage is thread-local and was cleared above.
            let errno = unsafe { *errno_location() };
            return if errno == 0 {
                Ok((names, true))
            } else {
                Err(io::Error::from_raw_os_error(errno))
            };
        }

        // SAFETY: POSIX guarantees a NUL-terminated `d_name` for each returned
        // directory entry, valid until the next `readdir` call.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsString::from_vec(bytes.to_vec()));
    }
}

#[cfg(unix)]
fn classify_unix_entry(directory: &File, name: &OsStr) -> io::Result<UnixEntryKind> {
    let name = unix_c_string(name)?;
    let mut metadata = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: pointers are valid, the output is initialized on success, and
    // AT_SYMLINK_NOFOLLOW classifies the entry without following it.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `fstatat` initialized the structure.
    let metadata = unsafe { metadata.assume_init() };
    let mode = metadata.st_mode as libc::mode_t & libc::S_IFMT as libc::mode_t;
    Ok(if mode == libc::S_IFDIR as libc::mode_t {
        UnixEntryKind::Directory
    } else if mode == libc::S_IFREG as libc::mode_t {
        UnixEntryKind::RegularFile
    } else {
        UnixEntryKind::Other
    })
}

#[cfg(unix)]
fn unix_c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix path component contains NUL",
        )
    })
}

#[cfg(unix)]
fn open_unix_directory_path(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace root is not absolute: {}", path.display()),
        ));
    }
    let root_c = CString::new("/").expect("root path has no NUL");
    let flags =
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    // SAFETY: `root_c` is NUL terminated and the flags do not require a mode.
    let descriptor = unsafe { libc::open(root_c.as_ptr(), flags) };
    let mut directory = unix_file_from_descriptor(descriptor, Path::new("/"), true)?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                directory = open_unix_directory_at(&directory, name)?;
            }
            std::path::Component::CurDir
            | std::path::Component::ParentDir
            | std::path::Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("workspace root is not normalized: {}", path.display()),
                ));
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_unix_directory_at(directory: &File, name: &OsStr) -> io::Result<File> {
    let name_c = unix_c_string(name)?;
    let flags =
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    // SAFETY: `directory` is live, `name_c` is one NUL-terminated component,
    // and the flags do not require a mode.
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name_c.as_ptr(), flags) };
    unix_file_from_descriptor(descriptor, Path::new(name), true)
}

#[cfg(unix)]
fn open_unix_regular_file_at(directory: &File, name: &OsStr) -> io::Result<File> {
    let name_c = unix_c_string(name)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    // SAFETY: `directory` is live, `name_c` is one NUL-terminated component,
    // and the flags do not require a mode.
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name_c.as_ptr(), flags) };
    unix_file_from_descriptor(descriptor, Path::new(name), false)
}

#[cfg(unix)]
fn unix_file_from_descriptor(
    descriptor: libc::c_int,
    display_path: &Path,
    expect_directory: bool,
) -> io::Result<File> {
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `open`/`openat` returned one newly owned descriptor.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    let valid = if expect_directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "not a {}: {}",
                if expect_directory {
                    "directory"
                } else {
                    "regular file"
                },
                display_path.display()
            ),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_unix_tree_file(root: &File, relative: &Path) -> io::Result<File> {
    let mut components = relative.components().peekable();
    let mut directory = root.try_clone()?;
    let mut saw_component = false;
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "workspace-tree path is not relative: {}",
                    relative.display()
                ),
            ));
        };
        saw_component = true;
        if components.peek().is_none() {
            return open_unix_regular_file_at(&directory, name);
        }
        directory = open_unix_directory_at(&directory, name)?;
    }
    debug_assert!(!saw_component);
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "workspace-tree file path is empty",
    ))
}

fn read_bounded_tree_file(mut file: File, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if metadata.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workspace-tree file {} is {} bytes; read limit is {} bytes",
                path.display(),
                metadata.len(),
                limit
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workspace-tree file {} grew beyond the {} byte read limit",
                path.display(),
                limit
            ),
        ));
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn collect_files_with_limits_portable(
    root: &Path,
    limits: WalkLimits,
) -> io::Result<CollectedFiles> {
    let mut files = Vec::new();
    let mut tree_entries = Vec::new();
    let mut directories = vec![(root.to_path_buf(), 0_usize)];
    let mut scanned_entries = 0_usize;
    let mut truncated = false;
    let mut tree_truncated = false;
    let mut indexed_path_bytes = 0_usize;
    let mut tree_path_bytes = 0_usize;
    let file_limit = limits.files.min(MAX_INDEXED_FILES);
    let indexed_path_byte_limit = limits.indexed_path_bytes.min(MAX_INDEXED_PATH_BYTES);
    let tree_entry_limit = limits.tree_entries.min(MAX_TREE_ENTRIES);
    let tree_path_byte_limit = limits.tree_path_bytes.min(MAX_TREE_PATH_BYTES);
    let scanned_entry_limit = limits.entries.min(MAX_SCANNED_ENTRIES);
    let depth_limit = limits.depth.min(MAX_DIRECTORY_DEPTH);

    'walk: while let Some((directory, depth)) = directories.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_error) if depth > 0 => {
                truncated = true;
                tree_truncated = true;
                continue;
            }
            Err(error) => return Err(error),
        };

        let mut entries = entries;
        let mut entries_sorted = Vec::new();
        let mut scan_limit_reached = false;
        loop {
            if scanned_entries >= scanned_entry_limit {
                truncated = true;
                tree_truncated = true;
                scan_limit_reached = true;
                break;
            }
            let Some(entry) = entries.next() else {
                break;
            };
            scanned_entries += 1;
            match entry {
                Ok(entry) => entries_sorted.push(entry),
                Err(_) => {
                    truncated = true;
                    tree_truncated = true;
                }
            }
        }
        entries_sorted.sort_by(|left, right| {
            let left_name = left.file_name();
            let right_name = right.file_name();
            compare_os_str(&left_name, &right_name)
        });

        let mut child_directories = Vec::new();
        for entry in entries_sorted {
            let name = entry.file_name();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    truncated = true;
                    tree_truncated = true;
                    continue;
                }
            };
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                truncated = true;
                tree_truncated = true;
                continue;
            };

            if file_type.is_dir() {
                if !is_ignored_directory(&name) {
                    retain_tree_entry(
                        &mut tree_entries,
                        &mut tree_path_bytes,
                        &mut tree_truncated,
                        relative,
                        true,
                        tree_entry_limit,
                        tree_path_byte_limit,
                    );
                    if depth >= depth_limit {
                        truncated = true;
                        tree_truncated = true;
                    } else {
                        child_directories.push((path, depth + 1));
                    }
                }
            } else if file_type.is_file() && !is_ignored_file(&name) {
                let file = match open_regular_file_for_index(&path) {
                    Ok(file) => file,
                    Err(_) => {
                        truncated = true;
                        tree_truncated = true;
                        continue;
                    }
                };
                retain_tree_entry(
                    &mut tree_entries,
                    &mut tree_path_bytes,
                    &mut tree_truncated,
                    relative,
                    false,
                    tree_entry_limit,
                    tree_path_byte_limit,
                );
                match is_probably_text_file(file) {
                    Ok(true) => retain_indexed_file(
                        &mut files,
                        &mut indexed_path_bytes,
                        &mut truncated,
                        relative,
                        file_limit,
                        indexed_path_byte_limit,
                    ),
                    Ok(false) => {}
                    Err(_) => truncated = true,
                }
            }
        }

        if scan_limit_reached {
            break 'walk;
        }
        // The stack is LIFO, so reverse insertion preserves lexical traversal.
        directories.extend(child_directories.into_iter().rev());
    }

    files.sort_by(|left, right| compare_paths(left, right));
    files.dedup();
    if files.len() > file_limit {
        files.truncate(file_limit);
        truncated = true;
    }

    tree_entries.sort_by(|left, right| {
        compare_paths(&left.path, &right.path)
            .then_with(|| right.is_directory.cmp(&left.is_directory))
    });
    tree_entries.dedup();
    if tree_entries.len() > tree_entry_limit {
        tree_entries.truncate(tree_entry_limit);
        tree_truncated = true;
    }

    Ok(CollectedFiles {
        files,
        tree_entries,
        truncated,
        tree_truncated,
    })
}

#[allow(clippy::too_many_arguments)]
fn retain_tree_entry(
    entries: &mut Vec<ProjectTreeEntry>,
    retained_path_bytes: &mut usize,
    truncated: &mut bool,
    relative: &Path,
    is_directory: bool,
    entry_limit: usize,
    path_byte_limit: usize,
) {
    let entry_limit = entry_limit.min(MAX_TREE_ENTRIES);
    let path_byte_limit = path_byte_limit.min(MAX_TREE_PATH_BYTES);
    let path_bytes = encoded_path_bytes(relative);
    if path_bytes > MAX_RELATIVE_PATH_BYTES
        || entries.len() >= entry_limit
        || path_bytes > path_byte_limit.saturating_sub(*retained_path_bytes)
    {
        *truncated = true;
        return;
    }

    entries.push(ProjectTreeEntry {
        path: relative.to_path_buf(),
        is_directory,
    });
    *retained_path_bytes += path_bytes;
}

fn retain_indexed_file(
    files: &mut Vec<PathBuf>,
    retained_path_bytes: &mut usize,
    truncated: &mut bool,
    relative: &Path,
    file_limit: usize,
    path_byte_limit: usize,
) {
    let file_limit = file_limit.min(MAX_INDEXED_FILES);
    let path_byte_limit = path_byte_limit.min(MAX_INDEXED_PATH_BYTES);
    let path_bytes = encoded_path_bytes(relative);
    if path_bytes > MAX_RELATIVE_PATH_BYTES
        || files.len() >= file_limit
        || path_bytes > path_byte_limit.saturating_sub(*retained_path_bytes)
    {
        *truncated = true;
        return;
    }

    files.push(relative.to_path_buf());
    *retained_path_bytes += path_bytes;
}

fn compare_paths(left: &Path, right: &Path) -> Ordering {
    let left_display = left.as_os_str().to_string_lossy();
    let right_display = right.as_os_str().to_string_lossy();
    left_display
        .chars()
        .map(normalize_path_character)
        .flat_map(char::to_lowercase)
        .cmp(
            right_display
                .chars()
                .map(normalize_path_character)
                .flat_map(char::to_lowercase),
        )
        .then_with(|| {
            left_display
                .chars()
                .map(normalize_path_character)
                .cmp(right_display.chars().map(normalize_path_character))
        })
        .then_with(|| {
            left.as_os_str()
                .as_encoded_bytes()
                .cmp(right.as_os_str().as_encoded_bytes())
        })
}

fn normalize_path_character(character: char) -> char {
    if character == '\\' { '/' } else { character }
}

fn compare_os_str(left: &OsStr, right: &OsStr) -> Ordering {
    let left_display = left.to_string_lossy();
    let right_display = right.to_string_lossy();
    left_display
        .cmp(&right_display)
        .then_with(|| left.as_encoded_bytes().cmp(right.as_encoded_bytes()))
}

fn encoded_path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn display_path(path: &Path) -> String {
    normalize_path(&path.to_string_lossy())
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn normalize_for_match(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
}

fn filename_stem(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or(filename, |(stem, _)| stem)
}

fn character_score(
    index: usize,
    basename_start: usize,
    candidate: &[char],
    original: &[char],
) -> i64 {
    let mut score = 10;
    if index >= basename_start {
        score += 5;
    }
    if index == 0 {
        score += 24;
        return score;
    }

    let previous = candidate[index - 1];
    score += match previous {
        '/' => 24,
        '_' | '-' | ' ' => 18,
        '.' => 10,
        _ => 0,
    };
    if original
        .get(index)
        .is_some_and(|character| character.is_uppercase())
        && original
            .get(index - 1)
            .is_some_and(|character| character.is_lowercase())
    {
        score += 12;
    }
    score
}

fn leading_penalty(index: usize) -> i64 {
    (index as i64).min(30)
}

fn is_ignored_directory(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        IGNORED_DIRECTORIES
            .iter()
            .any(|ignored| name.eq_ignore_ascii_case(ignored))
    })
}

fn is_ignored_file(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        IGNORED_FILES
            .iter()
            .any(|ignored| name.eq_ignore_ascii_case(ignored))
    })
}

#[cfg(any(not(unix), test))]
fn open_regular_file_for_index(path: &Path) -> io::Result<File> {
    // The directory entry's type can become stale before open. On Unix these
    // flags make a FIFO/device swap nonblocking and reject a final symlink;
    // descriptor metadata then validates the object that will actually be read.
    #[cfg(unix)]
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    #[cfg(not(unix))]
    let file = OpenOptions::new().read(true).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

fn is_probably_text_file(file: File) -> io::Result<bool> {
    let mut sample = Vec::new();
    file.take(TEXT_SAMPLE_BYTES).read_to_end(&mut sample)?;

    if sample.is_empty() {
        return Ok(true);
    }
    if sample.contains(&0) {
        return Ok(false);
    }

    let control_bytes = sample
        .iter()
        .filter(|byte| matches!(byte, 0..=8 | 11 | 12 | 14..=31 | 127))
        .count();
    if control_bytes.saturating_mul(100) > sample.len() {
        return Ok(false);
    }

    if std::str::from_utf8(&sample).is_ok() {
        return Ok(true);
    }

    // A sample can end midway through a UTF-8 scalar. Tolerating at most one
    // percent invalid bytes keeps such files indexable without admitting most
    // compressed/image formats.
    let invalid = invalid_utf8_bytes(&sample);
    Ok(invalid.saturating_mul(100) <= sample.len())
}

fn invalid_utf8_bytes(bytes: &[u8]) -> usize {
    let mut offset = 0;
    let mut invalid = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => break,
            Err(error) => {
                offset += error.valid_up_to();
                match error.error_len() {
                    Some(length) => {
                        invalid += length;
                        offset += length;
                    }
                    None => {
                        invalid += bytes.len() - offset;
                        break;
                    }
                }
            }
        }
    }
    invalid
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let unique = NEXT_TEMP.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wscrpt-project-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn score(query: &str, candidate: &str) -> i64 {
        fuzzy_path_score(query, candidate).unwrap()
    }

    fn walk_limits(files: usize, entries: usize, depth: usize) -> WalkLimits {
        WalkLimits {
            files,
            indexed_path_bytes: MAX_INDEXED_PATH_BYTES,
            tree_entries: MAX_TREE_ENTRIES,
            tree_path_bytes: MAX_TREE_PATH_BYTES,
            entries,
            depth,
        }
    }

    fn exhaustive_path_search(paths: &[PathBuf], query: &str, limit: usize) -> Vec<QuickOpenMatch> {
        let limit = limit.min(MAX_RESULTS);
        if limit == 0 {
            return Vec::new();
        }
        let query = query.trim();
        if query.is_empty() {
            return paths
                .iter()
                .take(limit)
                .cloned()
                .map(|path| QuickOpenMatch { path, score: 0 })
                .collect();
        }

        let query = normalize_for_match(query);
        let mut matches: Vec<_> = paths
            .iter()
            .filter_map(|path| {
                fuzzy_path_score_normalized(&query, &display_path(path)).map(|score| {
                    QuickOpenMatch {
                        path: path.clone(),
                        score,
                    }
                })
            })
            .collect();
        matches.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| compare_paths(&left.path, &right.path))
        });
        matches.truncate(limit);
        matches
    }

    #[test]
    fn indexes_text_files_in_deterministic_order_and_ignores_dependencies() {
        let project = TempProject::new();
        project.write("README.md", "hello\n");
        project.write("src/zeta.rs", "fn zeta() {}\n");
        project.write("src/alpha.rs", "fn alpha() {}\n");
        project.write(".github/workflows/check.yml", "name: check\n");
        project.write(".git/config", "[core]\n");
        project.write("target/debug/generated.rs", "ignored\n");
        project.write("node_modules/pkg/index.js", "ignored\n");
        project.write("image.png", [0x89, b'P', b'N', b'G', 0, 1, 2]);

        let index = ProjectIndex::build(project.path()).unwrap();
        assert_eq!(
            index.files(),
            &[
                PathBuf::from(".github/workflows/check.yml"),
                PathBuf::from("README.md"),
                PathBuf::from("src/alpha.rs"),
                PathBuf::from("src/zeta.rs"),
            ]
        );
    }

    #[test]
    fn explorer_snapshot_includes_binary_files_and_empty_directories() {
        let project = TempProject::new();
        project.write("README.md", "hello\n");
        project.write("assets/image.png", [0x89, b'P', b'N', b'G', 0, 1]);
        fs::create_dir_all(project.path().join("empty/nested")).unwrap();
        project.write("target/hidden.bin", [0, 1]);

        let index = ProjectIndex::build(project.path()).unwrap();
        let paths: Vec<_> = index
            .tree_entries()
            .iter()
            .map(|entry| (entry.path.clone(), entry.is_directory))
            .collect();

        assert_eq!(
            paths,
            [
                (PathBuf::from("assets"), true),
                (PathBuf::from("assets/image.png"), false),
                (PathBuf::from("empty"), true),
                (PathBuf::from("empty/nested"), true),
                (PathBuf::from("README.md"), false),
            ]
        );
        assert!(index.contains_tree_path("assets/image.png"));
        assert!(index.contains_tree_path("empty/nested"));
        assert!(!index.contains_tree_path("target"));
        assert!(!index.files().contains(&PathBuf::from("assets/image.png")));
        assert!(!index.is_tree_truncated());

        let matches = index.search_tree_files("image", usize::MAX);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, PathBuf::from("assets/image.png"));
        assert!(
            index
                .search_tree_files("", usize::MAX)
                .iter()
                .all(|matched| !index
                    .tree_entries()
                    .iter()
                    .find(|entry| entry.path == matched.path)
                    .unwrap()
                    .is_directory)
        );
    }

    #[test]
    fn refresh_finds_new_files() {
        let project = TempProject::new();
        project.write("one.rs", "one\n");
        let mut index = ProjectIndex::build(project.path()).unwrap();
        project.write("two.rs", "two\n");
        index.refresh().unwrap();
        assert_eq!(index.len(), 2);
        assert!(!index.is_truncated());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_rejects_a_replacement_root_symlink_and_retains_snapshot() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("trusted.rs"), "trusted\n").unwrap();
        fs::write(outside.join("secret.rs"), "outside\n").unwrap();
        let mut index = ProjectIndex::build(&root).unwrap();
        let previous_files = index.files().to_vec();
        let previous_tree = index.tree_entries().to_vec();

        fs::rename(&root, parent.path().join("workspace-held")).unwrap();
        symlink(&outside, &root).unwrap();

        assert!(index.refresh().is_err());
        assert_eq!(index.files(), previous_files);
        assert_eq!(index.tree_entries(), previous_tree);
        assert!(!index.contains_tree_path("secret.rs"));
    }

    #[test]
    fn traversal_limits_are_deterministic_and_report_partial_indexes() {
        let project = TempProject::new();
        project.write("c.rs", "c\n");
        project.write("a.rs", "a\n");
        project.write("b.rs", "b\n");

        let collected = collect_files_with_limits(project.path(), walk_limits(2, 10, 10)).unwrap();
        assert_eq!(
            collected.files,
            [PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
        assert!(collected.truncated);
        assert!(collected.files.len() <= 2);
        assert!(collected.files.len() <= MAX_INDEXED_FILES);
        assert_eq!(collected.tree_entries.len(), 3);
        assert!(!collected.tree_truncated);

        project.write("deep/inside/file.rs", "deep\n");
        let depth_limited =
            collect_files_with_limits(project.path(), walk_limits(10, 20, 1)).unwrap();
        assert!(depth_limited.truncated);
        assert!(
            !depth_limited
                .files
                .contains(&PathBuf::from("deep/inside/file.rs"))
        );
        assert!(depth_limited.tree_truncated);
    }

    #[test]
    fn text_and_tree_retention_caps_are_independent_and_byte_bounded() {
        let project = TempProject::new();
        project.write("a.rs", "a\n");
        project.write("b.rs", "b\n");
        project.write("c.rs", "c\n");

        let mut tree_count_limits = walk_limits(10, 10, 10);
        tree_count_limits.tree_entries = 2;
        let tree_count_limited =
            collect_files_with_limits(project.path(), tree_count_limits).unwrap();
        assert_eq!(tree_count_limited.files.len(), 3);
        assert!(!tree_count_limited.truncated);
        assert_eq!(tree_count_limited.tree_entries.len(), 2);
        assert!(tree_count_limited.tree_truncated);

        let one_path_budget = encoded_path_bytes(Path::new("a.rs"));
        let mut tree_byte_limits = walk_limits(10, 10, 10);
        tree_byte_limits.tree_path_bytes = one_path_budget;
        let tree_byte_limited =
            collect_files_with_limits(project.path(), tree_byte_limits).unwrap();
        assert_eq!(tree_byte_limited.files.len(), 3);
        assert_eq!(tree_byte_limited.tree_entries.len(), 1);
        assert_eq!(
            tree_byte_limited.tree_entries[0].path,
            PathBuf::from("a.rs")
        );
        assert!(tree_byte_limited.tree_truncated);

        let mut index_byte_limits = walk_limits(10, 10, 10);
        index_byte_limits.indexed_path_bytes = one_path_budget;
        let index_byte_limited =
            collect_files_with_limits(project.path(), index_byte_limits).unwrap();
        assert_eq!(index_byte_limited.files, [PathBuf::from("a.rs")]);
        assert!(index_byte_limited.truncated);
        assert_eq!(index_byte_limited.tree_entries.len(), 3);
        assert!(!index_byte_limited.tree_truncated);
    }

    #[test]
    fn indexed_file_retention_never_exceeds_the_global_hard_cap() {
        let mut files = Vec::new();
        let mut path_bytes = 0;
        let mut truncated = false;

        for index in 0..=MAX_INDEXED_FILES {
            let path = PathBuf::from(format!("file-{index:05}.rs"));
            retain_indexed_file(
                &mut files,
                &mut path_bytes,
                &mut truncated,
                &path,
                usize::MAX,
                usize::MAX,
            );
        }

        assert_eq!(files.len(), MAX_INDEXED_FILES);
        assert!(truncated);
        assert!(path_bytes <= MAX_INDEXED_PATH_BYTES);
    }

    #[test]
    fn accepts_empty_and_utf8_files_but_rejects_binary_controls() {
        let project = TempProject::new();
        project.write("empty.txt", []);
        project.write("unicode.txt", "naïve 世界\n");
        project.write("binary.bin", [1, 2, 3, 4, 5, 6]);

        let index = ProjectIndex::build(project.path()).unwrap();
        assert_eq!(
            index.files(),
            &[PathBuf::from("empty.txt"), PathBuf::from("unicode.txt")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_file_or_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        project.write("real.txt", "real\n");
        project.write("real-dir/nested.txt", "nested\n");
        symlink(
            project.path().join("real.txt"),
            project.path().join("linked.txt"),
        )
        .unwrap();
        symlink(
            project.path().join("real-dir"),
            project.path().join("linked-dir"),
        )
        .unwrap();

        let index = ProjectIndex::build(project.path()).unwrap();
        assert_eq!(
            index.files(),
            &[
                PathBuf::from("real-dir/nested.txt"),
                PathBuf::from("real.txt")
            ]
        );
        assert_eq!(
            index.tree_entries(),
            &[
                ProjectTreeEntry {
                    path: PathBuf::from("real-dir"),
                    is_directory: true,
                },
                ProjectTreeEntry {
                    path: PathBuf::from("real-dir/nested.txt"),
                    is_directory: false,
                },
                ProjectTreeEntry {
                    path: PathBuf::from("real.txt"),
                    is_directory: false,
                },
            ]
        );
        assert!(open_regular_file_for_index(&project.path().join("linked.txt")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_validation_rejects_a_fifo_without_blocking() {
        let project = TempProject::new();
        let path = project.path().join("swapped.rs");
        fs::write(&path, "regular\n").unwrap();
        fs::remove_file(&path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );

        let started = std::time::Instant::now();
        let error = open_regular_file_for_index(&path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let index = ProjectIndex::build(project.path()).unwrap();
        assert!(index.tree_entries().is_empty());
        assert!(index.files().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn queued_directory_symlink_swap_is_rejected_without_traversing_outside() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        let outside = TempProject::new();
        project.write("inside/kept.txt", "inside\n");
        outside.write("secret.txt", "outside\n");
        let mut swapped = false;
        let mut hook = |relative: &Path| {
            if !swapped && relative == Path::new("inside") {
                fs::rename(project.path().join("inside"), project.path().join("held")).unwrap();
                symlink(outside.path(), project.path().join("inside")).unwrap();
                swapped = true;
            }
        };

        let canonical_project = fs::canonicalize(project.path()).unwrap();
        let collected = collect_files_with_limits_unix(
            &canonical_project,
            walk_limits(10, 20, 10),
            Some(&mut hook),
        )
        .unwrap();

        assert!(swapped);
        assert!(collected.truncated);
        assert!(collected.tree_truncated);
        assert!(collected.files.is_empty());
        assert_eq!(
            collected.tree_entries,
            [ProjectTreeEntry {
                path: PathBuf::from("inside"),
                is_directory: true,
            }]
        );
        assert!(
            !collected
                .tree_entries
                .iter()
                .any(|entry| entry.path.ends_with("secret.txt"))
        );
    }

    #[test]
    fn tree_file_reads_are_snapshot_gated_and_byte_bounded() {
        let project = TempProject::new();
        project.write("assets/data.bin", [0, 1, 2, 3, 4, 5]);
        let index = ProjectIndex::build(project.path()).unwrap();

        assert_eq!(
            index.read_tree_file("assets/data.bin", 6).unwrap(),
            [0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            index
                .read_tree_file("assets/data.bin", 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            index.read_tree_file("assets", 6).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            index.read_tree_file("missing.bin", 6).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn indexed_file_reads_use_text_membership_not_the_independent_tree_snapshot() {
        let project = TempProject::new();
        project.write("src/value.txt", "trusted text\n");
        let mut index = ProjectIndex::build(project.path()).unwrap();
        index.tree_entries = Arc::from([]);
        index.tree_truncated = true;

        assert_eq!(
            index.read_indexed_file("src/value.txt", 1024).unwrap(),
            b"trusted text\n"
        );
        assert!(index.read_tree_file("src/value.txt", 1024).is_err());
        assert_eq!(
            index
                .read_indexed_file("src/not-indexed.txt", 1024)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            index
                .read_indexed_file("src/value.txt", 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn indexed_file_read_rejects_root_replacement_and_retains_old_descriptor() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("value.txt"), "trusted\n").unwrap();
        fs::write(outside.join("value.txt"), "outside\n").unwrap();
        let index = ProjectIndex::build(&root).unwrap();
        fs::rename(&root, parent.path().join("held")).unwrap();
        symlink(&outside, &root).unwrap();

        assert!(index.validate_root_identity().is_err());
        assert!(index.read_indexed_file("value.txt", 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn root_identity_rejects_a_new_real_directory_at_the_same_path() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("value.txt"), "trusted\n").unwrap();
        let index = ProjectIndex::build(&root).unwrap();
        fs::rename(&root, parent.path().join("held")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("value.txt"), "replacement\n").unwrap();

        let error = index.validate_root_identity().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(index.read_indexed_file("value.txt", 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn indexed_file_read_rejects_ancestor_symlink_and_fifo_swaps_without_blocking() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        let outside = TempProject::new();
        project.write("inside/value.txt", "trusted\n");
        outside.write("value.txt", "outside\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        fs::rename(project.path().join("inside"), project.path().join("held")).unwrap();
        symlink(outside.path(), project.path().join("inside")).unwrap();
        assert!(index.read_indexed_file("inside/value.txt", 1024).is_err());

        fs::remove_file(project.path().join("inside")).unwrap();
        fs::rename(project.path().join("held"), project.path().join("inside")).unwrap();
        let path = project.path().join("inside/value.txt");
        fs::remove_file(&path).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let started = std::time::Instant::now();
        assert!(index.read_indexed_file("inside/value.txt", 1024).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn tree_file_read_rejects_an_ancestor_symlink_swap() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        let outside = TempProject::new();
        project.write("inside/value.txt", "trusted\n");
        outside.write("value.txt", "outside secret\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        fs::rename(project.path().join("inside"), project.path().join("held")).unwrap();
        symlink(outside.path(), project.path().join("inside")).unwrap();

        assert!(index.read_tree_file("inside/value.txt", 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tree_file_read_rejects_post_snapshot_final_file_type_swaps() {
        let project = TempProject::new();
        project.write("inside/value.txt", "trusted\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        let path = project.path().join("inside/value.txt");

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(index.read_tree_file("inside/value.txt", 1024).is_err());

        fs::remove_dir(&path).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let started = std::time::Instant::now();
        assert!(index.read_tree_file("inside/value.txt", 1024).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn tree_file_read_rejects_post_snapshot_ancestor_directory_to_file_swap() {
        let project = TempProject::new();
        project.write("inside/value.txt", "trusted\n");
        let index = ProjectIndex::build(project.path()).unwrap();

        fs::remove_file(project.path().join("inside/value.txt")).unwrap();
        fs::remove_dir(project.path().join("inside")).unwrap();
        project.write("inside", "no longer a directory\n");

        assert!(index.read_tree_file("inside/value.txt", 1024).is_err());
    }

    #[test]
    fn zero_entry_scan_limit_inspects_and_retains_no_workspace_entries() {
        let project = TempProject::new();
        project.write("a.rs", "a\n");
        let collected = collect_files_with_limits(project.path(), walk_limits(10, 0, 10)).unwrap();

        assert!(collected.files.is_empty());
        assert!(collected.tree_entries.is_empty());
        assert!(collected.truncated);
        assert!(collected.tree_truncated);
    }

    #[cfg(unix)]
    #[test]
    fn lossy_colliding_non_utf8_paths_have_a_raw_deterministic_order() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first_name = OsString::from_vec(vec![b'a', 0x80, b'.', b'r', b's']);
        let second_name = OsString::from_vec(vec![b'a', 0x81, b'.', b'r', b's']);
        let first = PathBuf::from(&first_name);
        let second = PathBuf::from(&second_name);

        assert_eq!(display_path(&first), display_path(&second));
        assert_eq!(compare_paths(&first, &second), Ordering::Less);
        assert_eq!(compare_os_str(&first_name, &second_name), Ordering::Less);

        let mut files = vec![second.clone(), first.clone()];
        files.sort_by(|left, right| compare_paths(left, right));
        let index = ProjectIndex {
            root: PathBuf::from("/"),
            root_directory: Arc::new(open_unix_directory_path(Path::new("/")).unwrap()),
            files: files.into(),
            tree_entries: vec![
                ProjectTreeEntry {
                    path: first.clone(),
                    is_directory: false,
                },
                ProjectTreeEntry {
                    path: second.clone(),
                    is_directory: false,
                },
            ]
            .into(),
            truncated: false,
            tree_truncated: false,
        };
        assert_eq!(index.files(), &[first.clone(), second.clone()]);
        assert_eq!(
            index
                .tree_entries()
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [first.clone(), second.clone()]
        );
        assert_eq!(
            index
                .search("a", MAX_RESULTS)
                .into_iter()
                .map(|matched| matched.path)
                .collect::<Vec<_>>(),
            [first, second]
        );
    }

    #[test]
    fn fuzzy_matching_is_case_insensitive_and_requires_order() {
        assert!(fuzzy_path_score("PRJ", "src/project.rs").is_some());
        assert!(fuzzy_path_score("rés", "docs/RÉsumÉ.md").is_some());
        assert_eq!(fuzzy_path_score("zxy", "src/xyz.rs"), None);
    }

    #[test]
    fn fuzzy_matching_rewards_consecutive_and_boundary_matches() {
        assert!(score("pro", "src/project.rs") > score("pro", "src/p_r_o.rs"));
        assert!(score("project", "src/project.rs") > score("project", "project/archive.rs"));
        assert!(score("main", "src/main.rs") > score("main", "src/domain_mainland.rs"));
    }

    #[test]
    fn exact_path_filename_and_stem_receive_strong_bonuses() {
        assert!(score("src/main.rs", "src/main.rs") > score("src/main.rs", "old/src/main.rs"));
        assert!(score("main.rs", "src/main.rs") > score("main.rs", "src/main.rs.bak"));
        assert!(score("main", "src/main.rs") > score("main", "src/mainland.rs"));
    }

    #[test]
    fn search_is_ranked_deterministic_and_bounded() {
        let project = TempProject::new();
        for index in 0..(MAX_RESULTS + 25) {
            project.write(&format!("src/item_{index:03}.rs"), "text\n");
        }
        project.write("item.rs", "best\n");

        let index = ProjectIndex::build(project.path()).unwrap();
        let results = index.search("item", usize::MAX);
        assert_eq!(results.len(), MAX_RESULTS);
        assert_eq!(results[0].path, PathBuf::from("item.rs"));

        let tied = index.search("", 3);
        assert_eq!(tied.len(), 3);
        assert!(tied.windows(2).all(|pair| pair[0].path < pair[1].path));
        assert!(index.search("item", 0).is_empty());
    }

    #[test]
    fn explorer_search_reports_omitted_matches_without_retaining_them() {
        let project = TempProject::new();
        for index in 0..(MAX_RESULTS + 25) {
            project.write(&format!("src/match_{index:03}.bin"), [0, 1, 2]);
        }
        let index = ProjectIndex::build(project.path()).unwrap();

        let results = index.search_tree_files_with_metadata("match", MAX_RESULTS);
        assert_eq!(results.matches.len(), MAX_RESULTS);
        assert!(results.has_more);

        let all = index.search_tree_files_with_metadata("does-not-exist", MAX_RESULTS);
        assert!(all.matches.is_empty());
        assert!(!all.has_more);
    }

    #[test]
    fn bounded_top_k_search_matches_exhaustive_ranking() {
        let paths: Vec<_> = (0..500)
            .map(|index| {
                PathBuf::from(format!(
                    "src/{}/item_{index:03}_{}.rs",
                    if index % 3 == 0 { "deep" } else { "flat" },
                    if index % 7 == 0 { "Main" } else { "helper" }
                ))
            })
            .collect();

        for query in ["", "item", "MAIN", "src/deep", "helper", "not-present"] {
            for limit in [0, 1, 7, MAX_RESULTS, usize::MAX] {
                assert_eq!(
                    bounded_path_search(paths.iter(), query, limit),
                    exhaustive_path_search(&paths, query, limit),
                    "query={query:?}, limit={limit}"
                );
            }
        }
    }

    #[test]
    fn path_separators_are_normalized_for_matching() {
        assert_eq!(
            fuzzy_path_score("src/main", "src\\main.rs"),
            fuzzy_path_score("src/main", "src/main.rs")
        );
    }

    #[test]
    fn build_rejects_a_file_as_project_root() {
        let project = TempProject::new();
        project.write("file.txt", "text\n");
        let error = ProjectIndex::build(project.path().join("file.txt")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
