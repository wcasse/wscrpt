//! Literal, project-wide text search.
//!
//! Search is deliberately independent of the terminal UI. It consumes the
//! stable, root-relative paths in [`ProjectIndex`], skips files that are no
//! longer readable text, and returns positions that can be handed directly to
//! the editor. A small generation token is shared by synchronous search and
//! [`SearchWorker`] so submitting a newer query promptly cancels older work.

use std::fmt;
#[cfg(test)]
use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvError, RecvTimeoutError, SendError, Sender, TryRecvError,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::project::ProjectIndex;

/// Default cap for matches retained from one file.
pub const DEFAULT_MATCHES_PER_FILE: usize = 100;
/// Default cap for matches retained across the whole project.
pub const DEFAULT_MATCHES_TOTAL: usize = 1_000;
/// Absolute cap for matches retained from one file, even if a query asks for more.
pub const MAX_MATCHES_PER_FILE: usize = 500;
/// Absolute cap for matches retained across the whole project.
pub const MAX_MATCHES_TOTAL: usize = 5_000;
/// Largest individual file read by project search.
pub const MAX_SEARCH_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Largest aggregate payload read for one project-search generation.
pub const MAX_SEARCH_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

const MAX_PREVIEW_CHARS: usize = 240;
const CANCELLATION_CHECK_INTERVAL: usize = 1_024;

/// A typed search request.
///
/// Limits are public so UI configuration can adjust them, but every search
/// clamps them to [`MAX_MATCHES_PER_FILE`] and [`MAX_MATCHES_TOTAL`]. The text
/// is not trimmed: leading and trailing whitespace are valid literal search
/// terms. Prefix the text with `re:` or `re:i:` for regular expressions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    pub max_matches_per_file: usize,
    pub max_matches_total: usize,
}

impl SearchQuery {
    /// Creates a case-insensitive query with the default result bounds.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            case_sensitive: false,
            max_matches_per_file: DEFAULT_MATCHES_PER_FILE,
            max_matches_total: DEFAULT_MATCHES_TOTAL,
        }
    }

    pub fn with_case_sensitive(mut self, case_sensitive: bool) -> Self {
        self.case_sensitive = case_sensitive;
        self
    }

    pub fn with_limits(mut self, per_file: usize, total: usize) -> Self {
        self.max_matches_per_file = per_file.min(MAX_MATCHES_PER_FILE);
        self.max_matches_total = total.min(MAX_MATCHES_TOTAL);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn limits(&self) -> (usize, usize) {
        (
            self.max_matches_per_file.min(MAX_MATCHES_PER_FILE),
            self.max_matches_total.min(MAX_MATCHES_TOTAL),
        )
    }

    fn compile_pattern(&self) -> Result<crate::pattern::Pattern, crate::pattern::PatternError> {
        crate::pattern::Pattern::parse(&self.text, self.case_sensitive)
    }
}

impl From<&str> for SearchQuery {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for SearchQuery {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

/// One literal occurrence in an indexed project file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMatch {
    /// Path relative to [`ProjectIndex::root`].
    pub path: PathBuf,
    /// Zero-based logical line containing the beginning of the match.
    pub line: usize,
    /// Zero-based Unicode-scalar column on `line` (not a UTF-8 byte offset).
    pub char_column: usize,
    /// Absolute UTF-8 byte range in the file's original contents.
    pub byte_range: Range<usize>,
    /// The containing line, bounded and centered near the match when necessary.
    pub preview: String,
}

/// The result of a cancellable synchronous search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchOutcome {
    pub matches: Vec<SearchMatch>,
    pub cancelled: bool,
    /// True whenever the retained matches are not a complete answer for the
    /// indexed snapshot: the index or occurrence set was capped, a scan budget
    /// was reached, or an indexed source became unreadable, unsafe, binary, or
    /// invalid UTF-8. Retained matches are still individually valid.
    pub truncated: bool,
}

#[derive(Clone, Copy)]
struct SearchScanLimits {
    file_bytes: u64,
    total_bytes: u64,
}

/// Shared monotonically increasing search generation.
///
/// Calling [`SearchGeneration::next`] invalidates every token previously made
/// from this value. Clones share the same atomic generation, making the token
/// cheap to check from a search thread and cheap to invalidate from the UI
/// thread.
#[derive(Clone, Debug, Default)]
pub struct SearchGeneration {
    current: Arc<AtomicU64>,
}

impl SearchGeneration {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts and returns a new current generation.
    pub fn next(&self) -> SearchToken {
        let generation = increment_nonzero(&self.current);
        SearchToken {
            current: Arc::clone(&self.current),
            generation,
        }
    }

    /// Returns a token for the current generation without advancing it.
    pub fn token(&self) -> SearchToken {
        SearchToken {
            current: Arc::clone(&self.current),
            generation: self.current.load(Ordering::Acquire),
        }
    }

    /// Invalidates all outstanding tokens.
    pub fn cancel(&self) {
        increment_nonzero(&self.current);
    }

    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }
}

/// An immutable view of one search generation.
#[derive(Clone, Debug)]
pub struct SearchToken {
    current: Arc<AtomicU64>,
    generation: u64,
}

impl SearchToken {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_cancelled(&self) -> bool {
        self.current.load(Ordering::Acquire) != self.generation
    }
}

/// Runs a bounded project search synchronously.
pub fn search_project(index: &ProjectIndex, query: &SearchQuery) -> Vec<SearchMatch> {
    let generation = SearchGeneration::new();
    let token = generation.token();
    search_project_cancellable(index, query, &token).matches
}

/// Short alias for [`search_project`].
pub fn search(index: &ProjectIndex, query: &SearchQuery) -> Vec<SearchMatch> {
    search_project(index, query)
}

/// Runs a bounded search which stops when `token` is superseded or cancelled.
///
/// Files that cannot be read, contain NUL/control-heavy binary data, or are not
/// valid UTF-8 are skipped and make [`SearchOutcome::truncated`] true. That
/// preserves an IDE's best-effort search while making every incomplete result
/// visible instead of silently claiming project-wide completeness.
pub fn search_project_cancellable(
    index: &ProjectIndex,
    query: &SearchQuery,
    token: &SearchToken,
) -> SearchOutcome {
    search_project_cancellable_with_limits(
        index,
        query,
        token,
        SearchScanLimits {
            file_bytes: MAX_SEARCH_FILE_BYTES,
            total_bytes: MAX_SEARCH_TOTAL_BYTES,
        },
    )
}

fn search_project_cancellable_with_limits(
    index: &ProjectIndex,
    query: &SearchQuery,
    token: &SearchToken,
    scan_limits: SearchScanLimits,
) -> SearchOutcome {
    let (per_file_limit, total_limit) = query.limits();
    let mut matches = Vec::new();
    let mut scanned_bytes = 0_u64;
    let mut truncated = index.is_truncated();

    if query.is_empty() {
        return SearchOutcome {
            matches,
            cancelled: token.is_cancelled(),
            truncated,
        };
    }
    if index.validate_root_identity().is_err() {
        return SearchOutcome {
            matches,
            cancelled: token.is_cancelled(),
            truncated: true,
        };
    }

    let regex_pattern = if query.text.starts_with("re:") || query.text.starts_with("re:i:") {
        match query.compile_pattern() {
            Ok(pattern) => Some(pattern),
            Err(_) => {
                return SearchOutcome {
                    matches: Vec::new(),
                    cancelled: false,
                    truncated: false,
                };
            }
        }
    } else {
        None
    };
    let folded_query =
        (regex_pattern.is_none() && !query.case_sensitive).then(|| fold_query(&query.text));

    for relative_path in index.files() {
        if token.is_cancelled() {
            return SearchOutcome {
                matches,
                cancelled: true,
                truncated,
            };
        }
        let remaining_scan_bytes = scan_limits.total_bytes.saturating_sub(scanned_bytes);
        let read_limit = scan_limits.file_bytes.min(remaining_scan_bytes);
        let bytes = match index.read_indexed_file_from_held_root(relative_path, read_limit) {
            Ok(bytes) => bytes,
            Err(error)
                if read_limit < scan_limits.file_bytes
                    && error.kind() == std::io::ErrorKind::InvalidData =>
            {
                truncated = true;
                // This source did not fit in the remaining aggregate budget.
                break;
            }
            Err(_) => {
                truncated = true;
                // A root replacement makes every remaining descriptor-relative
                // read fail. Stop promptly instead of retrying the whole index;
                // an isolated file removal remains a best-effort skip.
                if index.validate_root_identity().is_err() {
                    break;
                }
                continue;
            }
        };
        scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
        if looks_binary(&bytes) {
            truncated = true;
            continue;
        }
        let Ok(contents) = String::from_utf8(bytes) else {
            truncated = true;
            continue;
        };

        let remaining = total_limit.saturating_sub(matches.len());
        let retained_file_limit = per_file_limit.min(remaining);
        // Probe one occurrence past the retained boundary. This distinguishes
        // an exact-at-limit result from a genuinely partial result without
        // retaining unbounded match state.
        let probe_limit = retained_file_limit.saturating_add(1);
        let ranges = if let Some(pattern) = regex_pattern.as_ref() {
            pattern.find_all(&contents, probe_limit, || token.is_cancelled())
        } else if query.case_sensitive {
            find_case_sensitive(&contents, &query.text, probe_limit, token)
        } else {
            find_case_insensitive(
                &contents,
                folded_query.as_deref().unwrap_or_default(),
                probe_limit,
                token,
            )
        };

        let Some(ranges) = ranges else {
            return SearchOutcome {
                matches,
                cancelled: true,
                truncated,
            };
        };

        if ranges.is_empty() {
            continue;
        }

        let mut ranges = ranges;
        if ranges.len() > retained_file_limit {
            ranges.truncate(retained_file_limit);
            truncated = true;
        }
        if ranges.is_empty() {
            // The total result cap was already full and this file proved that
            // at least one additional match exists.
            break;
        }

        let lines = LineIndex::new(&contents);
        matches.extend(ranges.into_iter().map(|byte_range| {
            let (line, char_column, preview) = lines.describe(&contents, &byte_range);
            SearchMatch {
                path: relative_path.clone(),
                line,
                char_column,
                byte_range,
                preview,
            }
        }));
    }

    if index.validate_root_identity().is_err() {
        truncated = true;
    }
    SearchOutcome {
        matches,
        cancelled: token.is_cancelled(),
        truncated,
    }
}

fn find_case_sensitive(
    contents: &str,
    needle: &str,
    limit: usize,
    token: &SearchToken,
) -> Option<Vec<Range<usize>>> {
    let mut ranges = Vec::new();
    let mut offset = 0;

    while offset <= contents.len() && ranges.len() < limit {
        if token.is_cancelled() {
            return None;
        }
        let Some(relative_start) = contents[offset..].find(needle) else {
            break;
        };
        let start = offset + relative_start;
        let end = start + needle.len();
        ranges.push(start..end);
        offset = end;
    }

    Some(ranges)
}

fn find_case_insensitive(
    contents: &str,
    folded_needle: &str,
    limit: usize,
    token: &SearchToken,
) -> Option<Vec<Range<usize>>> {
    let folded = FoldedText::new(contents, token)?;
    let mut ranges = Vec::new();
    let mut folded_offset = 0;
    let mut previous_original: Option<Range<usize>> = None;

    while folded_offset <= folded.text.len() && ranges.len() < limit {
        if token.is_cancelled() {
            return None;
        }
        let Some(relative_start) = folded.text[folded_offset..].find(folded_needle) else {
            break;
        };
        let start = folded_offset + relative_start;
        let end = start + folded_needle.len();
        let original = folded.original_range(start..end);
        if previous_original.as_ref() != Some(&original) {
            ranges.push(original.clone());
            previous_original = Some(original);
        }
        folded_offset = end;
    }

    Some(ranges)
}

#[derive(Clone, Debug)]
struct FoldSpan {
    folded: Range<usize>,
    original: Range<usize>,
}

#[derive(Clone, Debug)]
struct FoldedText {
    text: String,
    spans: Vec<FoldSpan>,
}

impl FoldedText {
    fn new(original: &str, token: &SearchToken) -> Option<Self> {
        let mut text = String::with_capacity(original.len());
        let mut spans = Vec::with_capacity(original.chars().count());

        for (index, (original_start, character)) in original.char_indices().enumerate() {
            if index % CANCELLATION_CHECK_INTERVAL == 0 && token.is_cancelled() {
                return None;
            }
            let original_end = original_start + character.len_utf8();
            let folded_start = text.len();
            push_case_folded(character, &mut text);
            let folded_end = text.len();
            spans.push(FoldSpan {
                folded: folded_start..folded_end,
                original: original_start..original_end,
            });
        }

        Some(Self { text, spans })
    }

    fn original_range(&self, folded: Range<usize>) -> Range<usize> {
        debug_assert!(folded.start < folded.end);
        let first = self
            .spans
            .partition_point(|span| span.folded.end <= folded.start)
            .min(self.spans.len().saturating_sub(1));
        let after_last = self
            .spans
            .partition_point(|span| span.folded.start < folded.end);
        let last = after_last.saturating_sub(1).max(first);
        self.spans[first].original.start..self.spans[last].original.end
    }
}

fn fold_query(query: &str) -> String {
    let mut folded = String::with_capacity(query.len());
    for character in query.chars() {
        push_case_folded(character, &mut folded);
    }
    folded
}

/// `to_uppercase` followed by `to_lowercase` supplies the useful multi-scalar
/// folds exposed by `std` (for example, `ß`/`SS` and Greek final sigma) without
/// adding a Unicode-table dependency. It is locale independent.
fn push_case_folded(character: char, output: &mut String) {
    for uppercase in character.to_uppercase() {
        output.extend(uppercase.to_lowercase());
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    if bytes.is_empty() {
        return false;
    }

    let controls = bytes
        .iter()
        .filter(|byte| matches!(byte, 0..=8 | 11 | 12 | 14..=31 | 127))
        .count();
    controls.saturating_mul(100) > bytes.len()
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(contents: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            contents
                .bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        Self { starts }
    }

    fn describe(&self, contents: &str, range: &Range<usize>) -> (usize, usize, String) {
        let line = self
            .starts
            .partition_point(|start| *start <= range.start)
            .saturating_sub(1);
        let line_start = self.starts[line];
        let mut line_end = self
            .starts
            .get(line + 1)
            .map_or(contents.len(), |next_start| next_start.saturating_sub(1));
        if contents.as_bytes().get(line_end.wrapping_sub(1)) == Some(&b'\r') {
            line_end -= 1;
        }

        let char_column = contents[line_start..range.start].chars().count();
        let match_end = range.end.min(line_end);
        let match_chars = contents[range.start..match_end].chars().count().max(1);
        let preview = bounded_preview(&contents[line_start..line_end], char_column, match_chars);
        (line, char_column, preview)
    }
}

fn bounded_preview(line: &str, match_column: usize, match_chars: usize) -> String {
    let characters: Vec<char> = line.chars().collect();
    if characters.len() <= MAX_PREVIEW_CHARS {
        return line.to_owned();
    }

    let match_end = match_column
        .saturating_add(match_chars)
        .min(characters.len());
    let desired_before = MAX_PREVIEW_CHARS / 3;
    let mut start = match_column.saturating_sub(desired_before);
    let minimum_end = match_end.min(characters.len());
    if start + MAX_PREVIEW_CHARS < minimum_end {
        start = minimum_end.saturating_sub(MAX_PREVIEW_CHARS);
    }
    let mut end = (start + MAX_PREVIEW_CHARS).min(characters.len());
    if end == characters.len() {
        start = end.saturating_sub(MAX_PREVIEW_CHARS);
    } else if end < minimum_end {
        end = minimum_end;
    }

    let mut preview = String::new();
    if start > 0 {
        preview.push('…');
    }
    preview.extend(characters[start..end].iter());
    if end < characters.len() {
        preview.push('…');
    }
    preview
}

fn increment_nonzero(counter: &AtomicU64) -> u64 {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.wrapping_add(1).max(1);
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

/// Identifier copied into every background result so the UI can reject races.
pub type SearchQueryId = u64;

/// A queued worker request. Construct requests through [`SearchRequestSender`].
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query_id: SearchQueryId,
    pub query: SearchQuery,
    token: SearchToken,
}

/// A completed background result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub query_id: SearchQueryId,
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
}

impl SearchResult {
    pub fn is_stale(&self, latest_query_id: SearchQueryId) -> bool {
        self.query_id != latest_query_id
    }
}

enum WorkerMessage {
    Request(SearchRequest),
    Shutdown,
}

/// A clonable producer for a [`SearchWorker`]'s request channel.
#[derive(Clone)]
pub struct SearchRequestSender {
    sender: Sender<WorkerMessage>,
    generation: SearchGeneration,
    latest_query_id: Arc<AtomicU64>,
}

impl SearchRequestSender {
    /// Submits a query, immediately cancelling all older generations.
    pub fn send(
        &self,
        query: impl Into<SearchQuery>,
    ) -> Result<SearchQueryId, SearchWorkerDisconnected> {
        let token = self.generation.next();
        let query_id = token.generation();
        self.latest_query_id.fetch_max(query_id, Ordering::AcqRel);
        let request = SearchRequest {
            query_id,
            query: query.into(),
            token,
        };
        self.sender
            .send(WorkerMessage::Request(request))
            .map_err(|SendError(_)| SearchWorkerDisconnected)?;
        Ok(query_id)
    }

    pub fn cancel(&self) {
        self.generation.cancel();
        // Cancellation is itself a superseding generation. Advancing the
        // published ID ensures a result queued in the narrow race just before
        // cancel cannot still pass a later `try_recv_latest` call.
        self.latest_query_id
            .fetch_max(self.generation.current(), Ordering::AcqRel);
    }

    pub fn latest_query_id(&self) -> Option<SearchQueryId> {
        match self.latest_query_id.load(Ordering::Acquire) {
            0 => None,
            query_id => Some(query_id),
        }
    }
}

/// Error returned when a background search worker has stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchWorkerDisconnected;

impl fmt::Display for SearchWorkerDisconnected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("project search worker has stopped")
    }
}

impl std::error::Error for SearchWorkerDisconnected {}

/// A single background search thread with request and result channels.
///
/// New requests supersede older work through [`SearchGeneration`]. The worker
/// also coalesces queued requests to the newest generation. A result can still
/// race with a submission at the final channel send, so every result carries a
/// `query_id`; [`SearchWorker::try_recv_latest`] and
/// [`SearchWorker::recv_latest_timeout`] discard such stale results for callers
/// that do not want to perform the comparison themselves.
pub struct SearchWorker {
    requests: SearchRequestSender,
    results: Receiver<SearchResult>,
    thread: Option<JoinHandle<()>>,
}

impl SearchWorker {
    pub fn new(index: ProjectIndex) -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let generation = SearchGeneration::new();
        let latest_query_id = Arc::new(AtomicU64::new(0));
        let requests = SearchRequestSender {
            sender: request_tx,
            generation,
            latest_query_id,
        };
        let thread = thread::Builder::new()
            .name("wscrpt-project-search".to_owned())
            .spawn(move || worker_loop(index, request_rx, result_tx))
            .expect("failed to spawn project search worker");

        Self {
            requests,
            results: result_rx,
            thread: Some(thread),
        }
    }

    pub fn request_sender(&self) -> SearchRequestSender {
        self.requests.clone()
    }

    pub fn results(&self) -> &Receiver<SearchResult> {
        &self.results
    }

    pub fn request(
        &self,
        query: impl Into<SearchQuery>,
    ) -> Result<SearchQueryId, SearchWorkerDisconnected> {
        self.requests.send(query)
    }

    pub fn cancel(&self) {
        self.requests.cancel();
    }

    pub fn latest_query_id(&self) -> Option<SearchQueryId> {
        self.requests.latest_query_id()
    }

    /// Receives the next result, including one that may have become stale.
    pub fn recv(&self) -> Result<SearchResult, RecvError> {
        self.results.recv()
    }

    /// Tries to receive the next result, including one that may be stale.
    pub fn try_recv(&self) -> Result<SearchResult, TryRecvError> {
        self.results.try_recv()
    }

    /// Returns the next currently relevant result, discarding queued stale IDs.
    pub fn try_recv_latest(&self) -> Result<SearchResult, TryRecvError> {
        loop {
            let result = self.results.try_recv()?;
            if self
                .latest_query_id()
                .is_none_or(|latest| !result.is_stale(latest))
            {
                return Ok(result);
            }
        }
    }

    /// Waits up to `timeout` for a currently relevant result.
    pub fn recv_latest_timeout(&self, timeout: Duration) -> Result<SearchResult, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let result = self.results.recv_timeout(remaining)?;
            if self
                .latest_query_id()
                .is_none_or(|latest| !result.is_stale(latest))
            {
                return Ok(result);
            }
            if Instant::now() >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.requests.cancel();
        let _ = self.requests.sender.send(WorkerMessage::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn worker_loop(
    index: ProjectIndex,
    requests: Receiver<WorkerMessage>,
    results: Sender<SearchResult>,
) {
    while let Ok(message) = requests.recv() {
        let WorkerMessage::Request(mut request) = message else {
            break;
        };

        // Collapse bursts of keystrokes. Generation order, rather than channel
        // arrival order, remains correct even with cloned concurrent senders.
        loop {
            match requests.try_recv() {
                Ok(WorkerMessage::Request(candidate)) => {
                    if candidate.token.generation() > request.token.generation() {
                        request = candidate;
                    }
                }
                Ok(WorkerMessage::Shutdown) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if request.token.is_cancelled() {
            continue;
        }
        let outcome = search_project_cancellable(&index, &request.query, &request.token);
        if outcome.cancelled || request.token.is_cancelled() {
            continue;
        }
        if results
            .send(SearchResult {
                query_id: request.query_id,
                matches: outcome.matches,
                truncated: outcome.truncated,
            })
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let unique = NEXT_TEMP.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wscrpt-search-test-{}-{unique}",
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

    #[test]
    fn unicode_positions_use_scalar_columns_and_original_byte_ranges() {
        let project = TempProject::new();
        let contents = "heading\nnaïve 🦀 Café and café\n世界\n";
        project.write("notes/Unicode.txt", contents);
        let index = ProjectIndex::build(project.path()).unwrap();

        let matches = search_project(&index, &SearchQuery::new("CAFÉ"));
        assert_eq!(matches.len(), 2);
        let expected_start = contents.find("Café").unwrap();
        assert_eq!(matches[0].path, PathBuf::from("notes/Unicode.txt"));
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[0].char_column, 8);
        assert_eq!(
            matches[0].byte_range,
            expected_start..expected_start + "Café".len()
        );
        assert_eq!(matches[0].preview, "naïve 🦀 Café and café");
    }

    #[test]
    fn case_sensitive_option_is_literal() {
        let project = TempProject::new();
        project.write("case.txt", "Rust rust RUST\n");
        let index = ProjectIndex::build(project.path()).unwrap();

        let exact = SearchQuery::new("Rust").with_case_sensitive(true);
        assert_eq!(search_project(&index, &exact).len(), 1);
        assert_eq!(search_project(&index, &SearchQuery::new("rust")).len(), 3);
    }

    #[test]
    fn std_case_fold_handles_multi_scalar_unicode() {
        let project = TempProject::new();
        project.write("street.txt", "Straße STRASSE ΟΣ ος\n");
        let index = ProjectIndex::build(project.path()).unwrap();

        let streets = search_project(&index, &SearchQuery::new("strasse"));
        assert_eq!(streets.len(), 2);
        assert_eq!(&"Straße STRASSE"[streets[0].byte_range.clone()], "Straße");

        let sigma = search_project(&index, &SearchQuery::new("ΟΣ"));
        assert_eq!(sigma.len(), 2);
    }

    #[test]
    fn skips_binary_and_invalid_utf8_that_changed_after_indexing() {
        let project = TempProject::new();
        project.write("good.txt", "needle\n");
        project.write("binary.txt", "needle\n");
        project.write("invalid.txt", "needle\n");
        let index = ProjectIndex::build(project.path()).unwrap();

        project.write("binary.txt", b"needle\0needle\n");
        project.write("invalid.txt", [b'n', b'e', 0xff, b'd', b'l', b'e']);
        let matches = search_project(&index, &SearchQuery::new("needle"));

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, PathBuf::from("good.txt"));
    }

    #[test]
    fn per_file_total_and_hard_limits_are_enforced() {
        let project = TempProject::new();
        project.write("a.txt", "hit hit hit hit\n");
        project.write("b.txt", "hit hit hit hit\n");
        let index = ProjectIndex::build(project.path()).unwrap();

        let matches = search_project(&index, &SearchQuery::new("hit").with_limits(2, 3));
        assert_eq!(matches.len(), 3);
        assert_eq!(
            matches
                .iter()
                .filter(|found| found.path == Path::new("a.txt"))
                .count(),
            2
        );
        assert_eq!(
            matches
                .iter()
                .filter(|found| found.path == Path::new("b.txt"))
                .count(),
            1
        );

        let query = SearchQuery {
            max_matches_per_file: usize::MAX,
            max_matches_total: usize::MAX,
            ..SearchQuery::new("hit")
        };
        assert!(search_project(&index, &query).len() <= MAX_MATCHES_TOTAL);
    }

    #[test]
    fn occurrence_caps_report_only_genuinely_omitted_matches() {
        let exact = TempProject::new();
        exact.write("only.txt", "hit hit\n");
        let exact_index = ProjectIndex::build(exact.path()).unwrap();
        let generations = SearchGeneration::new();
        let exact_outcome = search_project_cancellable(
            &exact_index,
            &SearchQuery::new("hit")
                .with_case_sensitive(true)
                .with_limits(2, 2),
            &generations.token(),
        );
        assert_eq!(exact_outcome.matches.len(), 2);
        assert!(!exact_outcome.truncated);

        let per_file = TempProject::new();
        per_file.write("only.txt", "hit hit hit\n");
        let per_file_index = ProjectIndex::build(per_file.path()).unwrap();
        let per_file_outcome = search_project_cancellable(
            &per_file_index,
            &SearchQuery::new("hit")
                .with_case_sensitive(true)
                .with_limits(2, 10),
            &generations.token(),
        );
        assert_eq!(per_file_outcome.matches.len(), 2);
        assert!(per_file_outcome.truncated);

        let total = TempProject::new();
        total.write("a.txt", "hit hit\n");
        total.write("b.txt", "hit\n");
        let total_index = ProjectIndex::build(total.path()).unwrap();
        let total_outcome = search_project_cancellable(
            &total_index,
            &SearchQuery::new("hit")
                .with_case_sensitive(true)
                .with_limits(10, 2),
            &generations.token(),
        );
        assert_eq!(total_outcome.matches.len(), 2);
        assert!(total_outcome.truncated);
    }

    #[test]
    fn zero_occurrence_caps_probe_for_omitted_matches() {
        let matching = TempProject::new();
        matching.write("one.txt", "hit\n");
        let matching_index = ProjectIndex::build(matching.path()).unwrap();
        let generations = SearchGeneration::new();
        for query in [
            SearchQuery::new("hit")
                .with_case_sensitive(true)
                .with_limits(0, 10),
            SearchQuery::new("hit")
                .with_case_sensitive(true)
                .with_limits(10, 0),
        ] {
            let outcome = search_project_cancellable(&matching_index, &query, &generations.token());
            assert!(outcome.matches.is_empty());
            assert!(outcome.truncated);
        }

        let nonmatching = TempProject::new();
        nonmatching.write("one.txt", "miss\n");
        let nonmatching_index = ProjectIndex::build(nonmatching.path()).unwrap();
        let outcome = search_project_cancellable(
            &nonmatching_index,
            &SearchQuery::new("hit").with_limits(0, 0),
            &generations.token(),
        );
        assert!(outcome.matches.is_empty());
        assert!(!outcome.truncated);
    }

    #[test]
    fn indexed_file_read_failure_is_visible_as_partial_search() {
        let project = TempProject::new();
        project.write("gone.txt", "needle\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        fs::remove_file(project.path().join("gone.txt")).unwrap();
        let generations = SearchGeneration::new();

        let outcome =
            search_project_cancellable(&index, &SearchQuery::new("needle"), &generations.token());

        assert!(outcome.matches.is_empty());
        assert!(outcome.truncated);
        assert!(!outcome.cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn final_symlink_swap_cannot_redirect_project_search() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        project.write("inside.txt", "original needle\n");
        let outside = TempProject::new();
        outside.write("outside.txt", "redirected needle\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        fs::remove_file(project.path().join("inside.txt")).unwrap();
        symlink(
            outside.path().join("outside.txt"),
            project.path().join("inside.txt"),
        )
        .unwrap();
        let generations = SearchGeneration::new();

        let outcome = search_project_cancellable(
            &index,
            &SearchQuery::new("redirected"),
            &generations.token(),
        );

        assert!(outcome.matches.is_empty());
        assert!(outcome.truncated);
        assert!(!outcome.cancelled);
    }

    #[test]
    fn byte_scan_limits_return_valid_partial_results_and_report_truncation() {
        let project = TempProject::new();
        project.write("a.txt", "needle\n");
        project.write("b.txt", "needle\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        let generations = SearchGeneration::new();
        let token = generations.token();

        let skipped = search_project_cancellable_with_limits(
            &index,
            &SearchQuery::new("needle"),
            &token,
            SearchScanLimits {
                file_bytes: 6,
                total_bytes: 100,
            },
        );
        assert!(skipped.matches.is_empty());
        assert!(skipped.truncated);
        assert!(!skipped.cancelled);

        let partial = search_project_cancellable_with_limits(
            &index,
            &SearchQuery::new("needle"),
            &token,
            SearchScanLimits {
                file_bytes: 100,
                total_bytes: 7,
            },
        );
        assert_eq!(partial.matches.len(), 1);
        assert_eq!(partial.matches[0].path, PathBuf::from("a.txt"));
        assert!(partial.truncated);
        assert!(!partial.cancelled);

        let exact = TempProject::new();
        exact.write("a.txt", "needle\n");
        exact.write("z-empty.txt", "");
        let exact_index = ProjectIndex::build(exact.path()).unwrap();
        let exact_outcome = search_project_cancellable_with_limits(
            &exact_index,
            &SearchQuery::new("needle"),
            &token,
            SearchScanLimits {
                file_bytes: 100,
                total_bytes: 7,
            },
        );
        assert_eq!(exact_outcome.matches.len(), 1);
        assert!(!exact_outcome.truncated);

        let disappeared = TempProject::new();
        disappeared.write("a-gone.txt", "indexed then removed\n");
        disappeared.write("b-fit.txt", "x");
        let disappeared_index = ProjectIndex::build(disappeared.path()).unwrap();
        fs::remove_file(disappeared.path().join("a-gone.txt")).unwrap();
        let continued = search_project_cancellable_with_limits(
            &disappeared_index,
            &SearchQuery::new("x"),
            &token,
            SearchScanLimits {
                file_bytes: 100,
                total_bytes: 1,
            },
        );
        assert_eq!(continued.matches.len(), 1);
        assert_eq!(continued.matches[0].path, PathBuf::from("b-fit.txt"));
        assert!(continued.truncated);

        let aggregate = TempProject::new();
        aggregate.write("a-too-large.txt", "xx");
        aggregate.write("b-would-fit.txt", "x");
        let aggregate_index = ProjectIndex::build(aggregate.path()).unwrap();
        let stopped = search_project_cancellable_with_limits(
            &aggregate_index,
            &SearchQuery::new("x"),
            &token,
            SearchScanLimits {
                file_bytes: 100,
                total_bytes: 1,
            },
        );
        assert!(stopped.matches.is_empty());
        assert!(stopped.truncated);
    }

    #[test]
    fn a_superseded_generation_cancels_synchronous_search() {
        let project = TempProject::new();
        project.write("large.txt", "find me\n".repeat(10_000));
        let index = ProjectIndex::build(project.path()).unwrap();
        let generations = SearchGeneration::new();
        let old = generations.next();
        let _new = generations.next();

        let outcome = search_project_cancellable(&index, &SearchQuery::new("find"), &old);
        assert!(outcome.cancelled);
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn worker_results_have_ids_and_latest_receive_discards_stale_work() {
        let project = TempProject::new();
        project.write("many.txt", "old value\n".repeat(20_000));
        project.write("one.txt", "the newest needle\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        let worker = SearchWorker::new(index);

        let old_id = worker.request(SearchQuery::new("old value")).unwrap();
        let new_id = worker.request(SearchQuery::new("newest needle")).unwrap();
        assert!(new_id > old_id);

        let result = worker.recv_latest_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result.query_id, new_id);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, PathBuf::from("one.txt"));
        assert!(!result.is_stale(worker.latest_query_id().unwrap()));
    }

    #[test]
    fn cancelling_a_worker_supersedes_even_an_already_completed_result_id() {
        let project = TempProject::new();
        project.write("one.txt", "needle\n");
        let index = ProjectIndex::build(project.path()).unwrap();
        let worker = SearchWorker::new(index);
        let request_id = worker.request(SearchQuery::new("needle")).unwrap();
        let completed = worker.recv_latest_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(completed.query_id, request_id);

        worker.cancel();

        assert!(completed.is_stale(worker.latest_query_id().unwrap()));
    }

    #[test]
    fn long_previews_remain_bounded_and_contain_the_match() {
        let project = TempProject::new();
        let line = format!("{}needle{}\n", "前".repeat(400), "後".repeat(400));
        project.write("long.txt", &line);
        let index = ProjectIndex::build(project.path()).unwrap();

        let found = search_project(&index, &SearchQuery::new("needle"));
        assert_eq!(found.len(), 1);
        assert!(found[0].preview.contains("needle"));
        assert!(found[0].preview.chars().count() <= MAX_PREVIEW_CHARS + 2);
        assert_eq!(found[0].char_column, 400);
    }
}
