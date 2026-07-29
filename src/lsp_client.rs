//! Bounded live Language Server Protocol process and client service.
//!
//! The service launches an explicitly configured argument vector directly
//! (never through an implicit shell), owns the server's stdio, and speaks
//! JSON-RPC 2.0 using LSP `Content-Length` frames. Incoming frames, queued
//! commands, queued UI events, document payloads, JSON nesting, and retained
//! stderr are all bounded. Document versions are checked when work is queued
//! and again when asynchronous results arrive.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvError, RecvTimeoutError, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::lsp::{DocumentVersion, FrameDecoder, LspPosition, write_frame};
use crate::workspace::normalized_file_path;

pub const DEFAULT_MAX_HEADER_BYTES: usize = 8 * 1024;
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_OPEN_DOCUMENTS: usize = 64;
pub const DEFAULT_MAX_DOCUMENT_URI_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_QUEUED_COMMANDS: usize = 128;
pub const DEFAULT_MAX_QUEUED_EVENTS: usize = 256;
pub const DEFAULT_MAX_QUEUED_INPUT_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_QUEUED_EVENT_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_READ_CHUNK_BYTES: usize = 8 * 1024;
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(750);
pub const MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_PENDING_REQUESTS: usize = 128;

const MAX_JSON_DEPTH: usize = 128;
const MAX_QUEUE_BYTES: usize = 512 * 1024 * 1024;
const INVALID_SERVER_RESPONSE_CODE: i64 = -32098;
const MAX_OPEN_DOCUMENTS: usize = 4_096;
const MAX_DOCUMENT_URI_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTICS_ALIAS_URI_BYTES: usize = DEFAULT_MAX_DOCUMENT_URI_BYTES;
const MAX_DIAGNOSTICS_ALIAS_PATH_COMPONENTS: usize = 256;
const INPUT_CONTROL_RESERVE: usize = 4;
const EVENT_CONTROL_RESERVE: usize = 1;
/// The JSON estimate counts enum storage, collection capacity, string/number
/// capacity, and approximate `BTreeMap` entry storage. Doubling that estimate
/// covers allocator and tree-node slack without serializing the value merely
/// to weigh a queued event.
const JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER: usize = 2;
const FORCE_KILL_GRACE: Duration = Duration::from_millis(200);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Resource bounds for one language-server process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LspClientLimits {
    pub max_header_bytes: usize,
    pub max_message_bytes: usize,
    pub max_document_bytes: usize,
    pub max_open_documents: usize,
    pub max_document_uri_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_queued_commands: usize,
    pub max_queued_events: usize,
    pub max_queued_input_bytes: usize,
    pub max_queued_event_bytes: usize,
    pub read_chunk_bytes: usize,
}

impl Default for LspClientLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_document_bytes: DEFAULT_MAX_DOCUMENT_BYTES,
            max_open_documents: DEFAULT_MAX_OPEN_DOCUMENTS,
            max_document_uri_bytes: DEFAULT_MAX_DOCUMENT_URI_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_queued_commands: DEFAULT_MAX_QUEUED_COMMANDS,
            max_queued_events: DEFAULT_MAX_QUEUED_EVENTS,
            max_queued_input_bytes: DEFAULT_MAX_QUEUED_INPUT_BYTES,
            max_queued_event_bytes: DEFAULT_MAX_QUEUED_EVENT_BYTES,
            read_chunk_bytes: DEFAULT_READ_CHUNK_BYTES,
        }
    }
}

impl LspClientLimits {
    fn normalized(self) -> Self {
        Self {
            max_header_bytes: self.max_header_bytes.clamp(64, 64 * 1024),
            max_message_bytes: self.max_message_bytes.clamp(1_024, 64 * 1024 * 1024),
            max_document_bytes: self.max_document_bytes.clamp(1_024, 128 * 1024 * 1024),
            max_open_documents: self.max_open_documents.clamp(1, MAX_OPEN_DOCUMENTS),
            max_document_uri_bytes: self
                .max_document_uri_bytes
                .clamp(64, MAX_DOCUMENT_URI_BYTES),
            max_stderr_bytes: self.max_stderr_bytes.min(16 * 1024 * 1024),
            max_queued_commands: self.max_queued_commands.clamp(1, 4_096),
            max_queued_events: self.max_queued_events.clamp(1, 4_096),
            max_queued_input_bytes: self.max_queued_input_bytes.clamp(1_024, MAX_QUEUE_BYTES),
            max_queued_event_bytes: self.max_queued_event_bytes.clamp(1_024, MAX_QUEUE_BYTES),
            read_chunk_bytes: self.read_chunk_bytes.clamp(256, 64 * 1024),
        }
    }
}

/// Direct process configuration for one language server.
#[derive(Clone, Debug)]
pub struct LspServerConfig {
    pub argv: Vec<OsString>,
    pub workspace_root: PathBuf,
    pub root_uri: Option<String>,
    pub initialization_options: Option<JsonValue>,
    pub limits: LspClientLimits,
    pub shutdown_timeout: Duration,
}

impl LspServerConfig {
    pub fn new<I, S>(argv: I, workspace_root: impl Into<PathBuf>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let workspace_root = workspace_root.into();
        let mut config = Self {
            root_uri: Some(file_uri_identity(&workspace_root)),
            argv: argv.into_iter().map(Into::into).collect(),
            workspace_root,
            initialization_options: None,
            limits: LspClientLimits::default(),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        };
        config.normalize_workspace_root();
        config
    }

    pub fn with_initialization_options(mut self, options: JsonValue) -> Self {
        self.initialization_options = Some(options);
        self
    }

    pub fn with_limits(mut self, limits: LspClientLimits) -> Self {
        self.limits = limits.normalized();
        self
    }

    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = normalized_shutdown_timeout(timeout);
        self
    }

    fn validate(&self) -> Result<(), LspClientError> {
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return Err(LspClientError::InvalidConfig(
                "argv must contain a non-empty executable".to_owned(),
            ));
        }
        if self.argv.iter().any(|argument| os_contains_nul(argument)) {
            return Err(LspClientError::InvalidConfig(
                "argv must not contain NUL bytes".to_owned(),
            ));
        }
        if !self.workspace_root.is_dir() {
            return Err(LspClientError::InvalidConfig(format!(
                "workspace root is not a directory: {}",
                self.workspace_root.display()
            )));
        }
        Ok(())
    }

    fn normalize_workspace_root(&mut self) {
        let original_root_uri = file_uri_identity(&self.workspace_root);
        let absolute = if self.workspace_root.is_absolute() {
            self.workspace_root.clone()
        } else {
            let Ok(current_dir) = std::env::current_dir() else {
                return;
            };
            current_dir.join(&self.workspace_root)
        };
        self.workspace_root = normalized_file_path(&absolute);
        if self.root_uri.as_deref() == Some(original_root_uri.as_str()) {
            self.root_uri = Some(file_uri_identity(&self.workspace_root));
        }
    }
}

/// A zero-based UTF-16 LSP range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

impl LspRange {
    pub const fn new(start: LspPosition, end: LspPosition) -> Self {
        Self { start, end }
    }
}

pub type RequestId = u64;

/// Monotonic identity for one successful `didOpen` within an LSP client
/// session. A URI may reuse its document version after close/reopen, but it
/// never reuses this identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentIncarnation(u64);

impl DocumentIncarnation {
    const fn new(value: u64) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenDocumentState {
    version: DocumentVersion,
    incarnation: DocumentIncarnation,
}

impl OpenDocumentState {
    const fn new(version: DocumentVersion, incarnation: DocumentIncarnation) -> Self {
        Self {
            version,
            incarnation,
        }
    }
}

/// The operation associated with a correlated JSON-RPC request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspOperation {
    Initialize,
    Completion,
    Hover,
    Definition,
    References,
    DocumentSymbols,
    WorkspaceSymbols,
    Formatting,
    Shutdown,
}

/// JSON-RPC error returned by the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<JsonValue>,
}

/// Normalized static text-document synchronization advertised by the server.
///
/// `full` and `incremental` describe distinct wire shapes. Callers must use
/// [`LspClient::did_change`] for `full` servers and
/// [`LspClient::did_change_full_document_replacement`] for `incremental`
/// servers; a whole-document replacement sent to an incremental server still
/// requires an explicit range.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextDocumentSyncCapability {
    pub open_close: bool,
    pub full: bool,
    pub incremental: bool,
    pub save: bool,
    pub save_include_text: bool,
}

/// Events emitted to the editor. Large feature payloads remain lossless JSON
/// because LSP response shapes vary by server and negotiated capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LspEvent {
    Ready {
        capabilities: JsonValue,
        /// Whether the server advertised `workspaceSymbolProvider` as `true`
        /// or as an options object for this client session.
        workspace_symbols: bool,
        text_document_sync: TextDocumentSyncCapability,
    },
    Diagnostics {
        uri: String,
        /// Optional version supplied by the language server publication.
        version: Option<DocumentVersion>,
        /// Client version observed in protocol-input order immediately before
        /// this server frame was processed. Unlike the caller-side optimistic
        /// version map, this cannot advance past an older already-queued
        /// publication merely because `didChange` was accepted by the API.
        observed_version: Option<DocumentVersion>,
        /// Open-document identity observed at the same protocol-order point as
        /// `observed_version`. Consumers must compare both fields: versions may
        /// legally restart after a URI is closed and reopened.
        observed_incarnation: Option<DocumentIncarnation>,
        diagnostics: Vec<JsonValue>,
    },
    /// A `publishDiagnostics` notification was structurally invalid and must
    /// not leave an older cache bucket looking current.
    DiagnosticsRejected {
        /// Present when the notification supplied a valid string URI before a
        /// later field failed validation.
        uri: Option<String>,
        reason: String,
    },
    Completion {
        request_id: RequestId,
        uri: String,
        version: DocumentVersion,
        result: JsonValue,
    },
    Hover {
        request_id: RequestId,
        uri: String,
        version: DocumentVersion,
        result: JsonValue,
    },
    Definition {
        request_id: RequestId,
        uri: String,
        version: DocumentVersion,
        result: JsonValue,
    },
    References {
        request_id: RequestId,
        uri: String,
        version: DocumentVersion,
        result: JsonValue,
    },
    DocumentSymbols {
        request_id: RequestId,
        uri: String,
        version: DocumentVersion,
        result: JsonValue,
    },
    WorkspaceSymbols {
        request_id: RequestId,
        result: JsonValue,
    },
    Formatting {
        request_id: RequestId,
        uri: String,
        version: DocumentVersion,
        result: JsonValue,
    },
    RequestFailed {
        request_id: RequestId,
        operation: LspOperation,
        error: JsonRpcError,
    },
    StaleResponse {
        request_id: RequestId,
        operation: LspOperation,
        uri: String,
        requested_version: DocumentVersion,
        current_version: Option<DocumentVersion>,
    },
    StaleDiagnostics {
        uri: String,
        received_version: DocumentVersion,
        current_version: Option<DocumentVersion>,
    },
    ServerNotification {
        method: String,
        params: Option<JsonValue>,
    },
    ServerRequestRejected {
        method: String,
    },
    Stderr(Vec<u8>),
    StderrTruncated {
        limit: usize,
    },
    ProtocolError(String),
    TransportError(String),
    EventsDropped {
        count: usize,
    },
    ShutdownComplete,
    ServerClosed,
}

impl LspEvent {
    /// Conservatively estimates heap bytes retained by this event using the
    /// same allocation-aware accounting as the bounded event queue.
    ///
    /// The result is capped at `MAX_QUEUE_BYTES + 1`; that final value is an
    /// overflow sentinel meaning the estimate exceeded the queue's hard byte
    /// ceiling. This method does not serialize, clone, or otherwise consume
    /// the event.
    pub fn estimated_retained_bytes(&self) -> usize {
        self.queue_weight(MAX_QUEUE_BYTES)
    }
}

/// Public API failure before a request reaches the language server.
#[derive(Debug)]
pub enum LspClientError {
    InvalidConfig(String),
    Spawn {
        executable: OsString,
        source: io::Error,
    },
    NotReady,
    Stopped,
    ShuttingDown,
    QueueFull,
    RequestIdExhausted,
    DocumentIncarnationExhausted,
    DocumentAlreadyOpen(String),
    DocumentNotOpen(String),
    OpenDocumentLimitReached {
        max: usize,
    },
    DocumentUriTooLarge {
        bytes: usize,
        max: usize,
    },
    StaleDocumentVersion {
        uri: String,
        attempted: DocumentVersion,
        current: DocumentVersion,
    },
    VersionOutOfRange(DocumentVersion),
    PositionOutOfRange(LspPosition),
    DocumentTooLarge {
        bytes: usize,
        max: usize,
    },
    MessageTooLarge {
        max: usize,
    },
}

impl fmt::Display for LspClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "invalid LSP config: {message}"),
            Self::Spawn { executable, source } => write!(
                formatter,
                "could not spawn language server {executable:?}: {source}"
            ),
            Self::NotReady => formatter.write_str("language server has not initialized"),
            Self::Stopped => formatter.write_str("language server has stopped"),
            Self::ShuttingDown => formatter.write_str("language server is shutting down"),
            Self::QueueFull => formatter.write_str("language-server command queue is full"),
            Self::RequestIdExhausted => formatter.write_str("LSP request ID counter exhausted"),
            Self::DocumentIncarnationExhausted => {
                formatter.write_str("LSP document-incarnation counter exhausted")
            }
            Self::DocumentAlreadyOpen(uri) => write!(formatter, "document is already open: {uri}"),
            Self::DocumentNotOpen(uri) => write!(formatter, "document is not open: {uri}"),
            Self::OpenDocumentLimitReached { max } => {
                write!(
                    formatter,
                    "language-server open-document limit reached ({max})"
                )
            }
            Self::DocumentUriTooLarge { bytes, max } => {
                write!(formatter, "document URI is {bytes} bytes; limit is {max}")
            }
            Self::StaleDocumentVersion {
                uri,
                attempted,
                current,
            } => write!(
                formatter,
                "stale version {attempted} for {uri}; current version is {current}"
            ),
            Self::VersionOutOfRange(version) => write!(
                formatter,
                "document version {version} exceeds LSP's signed 32-bit range"
            ),
            Self::PositionOutOfRange(position) => write!(
                formatter,
                "LSP position {}:{} exceeds the protocol's signed 32-bit coordinate range",
                position.line, position.character
            ),
            Self::DocumentTooLarge { bytes, max } => {
                write!(
                    formatter,
                    "document payload is {bytes} bytes; limit is {max}"
                )
            }
            Self::MessageTooLarge { max } => {
                write!(
                    formatter,
                    "outbound LSP message exceeds the {max}-byte limit"
                )
            }
        }
    }
}

impl Error for LspClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

const STATE_STARTING: u8 = 0;
const STATE_READY: u8 = 1;
const STATE_SHUTTING_DOWN: u8 = 2;
const STATE_STOPPED: u8 = 3;

trait QueueWeight {
    fn queue_weight(&self, limit: usize) -> usize;
}

struct QueueAccounting {
    bytes: AtomicUsize,
    items: AtomicUsize,
}

struct BudgetedSender<T> {
    sender: SyncSender<QueueEnvelope<T>>,
    accounting: Arc<QueueAccounting>,
    max_bytes: usize,
    max_items: usize,
}

impl<T> Clone for BudgetedSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            accounting: Arc::clone(&self.accounting),
            max_bytes: self.max_bytes,
            max_items: self.max_items,
        }
    }
}

struct BudgetedReceiver<T> {
    receiver: Receiver<QueueEnvelope<T>>,
}

struct QueueEnvelope<T> {
    item: Option<T>,
    weight: usize,
    counted: bool,
    accounting: Arc<QueueAccounting>,
}

impl<T> QueueEnvelope<T> {
    fn queued(item: T, weight: usize, accounting: Arc<QueueAccounting>) -> Self {
        Self {
            item: Some(item),
            weight,
            counted: true,
            accounting,
        }
    }

    fn control(item: T, accounting: Arc<QueueAccounting>) -> Self {
        Self {
            item: Some(item),
            weight: 0,
            counted: false,
            accounting,
        }
    }

    fn into_item(mut self) -> T {
        self.release();
        self.item.take().expect("queue envelope contains an item")
    }

    fn take_item(&mut self) -> T {
        self.release();
        self.item.take().expect("queue envelope contains an item")
    }

    fn release(&mut self) {
        if self.counted {
            self.accounting
                .bytes
                .fetch_sub(self.weight, Ordering::AcqRel);
            self.accounting.items.fetch_sub(1, Ordering::AcqRel);
            self.counted = false;
        }
    }
}

impl<T> Drop for QueueEnvelope<T> {
    fn drop(&mut self) {
        self.release();
    }
}

impl<T: QueueWeight> BudgetedSender<T> {
    fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let weight = item.queue_weight(self.max_bytes);
        if !self.reserve(weight) {
            return Err(TrySendError::Full(item));
        }
        let envelope = QueueEnvelope::queued(item, weight, Arc::clone(&self.accounting));
        match self.sender.try_send(envelope) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(mut envelope)) => Err(TrySendError::Full(envelope.take_item())),
            Err(TrySendError::Disconnected(mut envelope)) => {
                Err(TrySendError::Disconnected(envelope.take_item()))
            }
        }
    }

    fn reserve(&self, weight: usize) -> bool {
        if weight > self.max_bytes
            || self
                .accounting
                .items
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |items| {
                    (items < self.max_items).then_some(items + 1)
                })
                .is_err()
        {
            return false;
        }

        let mut used = self.accounting.bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(weight) else {
                self.accounting.items.fetch_sub(1, Ordering::AcqRel);
                return false;
            };
            if next > self.max_bytes {
                self.accounting.items.fetch_sub(1, Ordering::AcqRel);
                return false;
            }
            match self.accounting.bytes.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }
}

impl<T> BudgetedSender<T> {
    /// Sends a tiny lifecycle/terminal item through capacity reserved outside
    /// the ordinary item and byte limits.
    fn try_send_control(&self, item: T) -> Result<(), TrySendError<T>> {
        let envelope = QueueEnvelope::control(item, Arc::clone(&self.accounting));
        match self.sender.try_send(envelope) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(mut envelope)) => Err(TrySendError::Full(envelope.take_item())),
            Err(TrySendError::Disconnected(mut envelope)) => {
                Err(TrySendError::Disconnected(envelope.take_item()))
            }
        }
    }
}

impl<T> BudgetedReceiver<T> {
    fn recv(&self) -> Result<T, RecvError> {
        self.receiver.recv().map(QueueEnvelope::into_item)
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.receiver
            .recv_timeout(timeout)
            .map(QueueEnvelope::into_item)
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        self.receiver.try_recv().map(QueueEnvelope::into_item)
    }
}

fn budgeted_channel<T>(
    max_items: usize,
    max_bytes: usize,
    control_reserve: usize,
) -> (BudgetedSender<T>, BudgetedReceiver<T>) {
    let accounting = Arc::new(QueueAccounting {
        bytes: AtomicUsize::new(0),
        items: AtomicUsize::new(0),
    });
    let (sender, receiver) = mpsc::sync_channel(max_items.saturating_add(control_reserve).max(1));
    (
        BudgetedSender {
            sender,
            accounting,
            max_bytes,
            max_items,
        },
        BudgetedReceiver { receiver },
    )
}

/// Live handle for one language-server process.
pub struct LspClient {
    input: BudgetedSender<ProtocolInput>,
    events: BudgetedReceiver<LspEvent>,
    child: Arc<Mutex<Child>>,
    lifecycle: Arc<Lifecycle>,
    protocol_thread: Option<JoinHandle<()>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    document_versions: Arc<Mutex<HashMap<String, OpenDocumentState>>>,
    next_document_incarnation: Option<DocumentIncarnation>,
    next_request_id: RequestId,
    limits: LspClientLimits,
    shutdown_timeout: Duration,
    pid: u32,
}

impl LspClient {
    /// Spawns `config.argv` directly and immediately sends `initialize`.
    pub fn spawn(mut config: LspServerConfig) -> Result<Self, LspClientError> {
        config.limits = config.limits.normalized();
        config.shutdown_timeout = normalized_shutdown_timeout(config.shutdown_timeout);
        config.normalize_workspace_root();
        config.validate()?;

        let executable = config.argv[0].clone();
        let mut command = Command::new(&config.argv[0]);
        command
            .args(&config.argv[1..])
            .current_dir(&config.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);

        let mut child = command.spawn().map_err(|source| LspClientError::Spawn {
            executable: executable.clone(),
            source,
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("piped child stdin");
        let stdout = child.stdout.take().expect("piped child stdout");
        let stderr = child.stderr.take().expect("piped child stderr");
        let child = Arc::new(Mutex::new(child));

        let (input_tx, input_rx) = budgeted_channel(
            config.limits.max_queued_commands,
            config.limits.max_queued_input_bytes,
            INPUT_CONTROL_RESERVE,
        );
        let (event_tx, event_rx) = budgeted_channel(
            config.limits.max_queued_events,
            config.limits.max_queued_event_bytes,
            EVENT_CONTROL_RESERVE,
        );
        let lifecycle = Arc::new(Lifecycle::new());
        let document_versions = Arc::new(Mutex::new(HashMap::new()));

        let stdout_thread =
            spawn_stdout_reader(stdout, input_tx.clone(), config.limits).map_err(|source| {
                abort_spawned_child(&child);
                LspClientError::Spawn {
                    executable: executable.clone(),
                    source,
                }
            })?;
        let stderr_thread = match spawn_stderr_reader(stderr, input_tx.clone(), config.limits) {
            Ok(thread) => thread,
            Err(source) => {
                abort_spawned_child(&child);
                let _ = wait_for_reader_threads(Some(&stdout_thread), None, FORCE_KILL_GRACE);
                join_if_finished(stdout_thread);
                return Err(LspClientError::Spawn {
                    executable: executable.clone(),
                    source,
                });
            }
        };

        let protocol_lifecycle = Arc::clone(&lifecycle);
        let protocol_versions = Arc::clone(&document_versions);
        let protocol_config = config.clone();
        let protocol_thread = match thread::Builder::new()
            .name("wscrpt-lsp-protocol".to_owned())
            .spawn(move || {
                protocol_loop(
                    stdin,
                    input_rx,
                    event_tx,
                    protocol_lifecycle,
                    protocol_versions,
                    protocol_config,
                )
            }) {
            Ok(thread) => thread,
            Err(source) => {
                abort_spawned_child(&child);
                let _ = wait_for_reader_threads(
                    Some(&stdout_thread),
                    Some(&stderr_thread),
                    FORCE_KILL_GRACE,
                );
                join_if_finished(stdout_thread);
                join_if_finished(stderr_thread);
                return Err(LspClientError::Spawn { executable, source });
            }
        };

        Ok(Self {
            input: input_tx,
            events: event_rx,
            child,
            lifecycle,
            protocol_thread: Some(protocol_thread),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            document_versions,
            next_document_incarnation: Some(DocumentIncarnation::new(1)),
            next_request_id: 2, // initialize is request 1
            limits: config.limits,
            shutdown_timeout: config.shutdown_timeout,
            pid,
        })
    }

    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_ready(&self) -> bool {
        self.lifecycle.state.load(Ordering::Acquire) == STATE_READY
    }

    pub fn is_stopped(&self) -> bool {
        self.lifecycle.state.load(Ordering::Acquire) == STATE_STOPPED
    }

    pub fn recv_event(&self) -> Result<LspEvent, RecvError> {
        self.events.recv()
    }

    pub fn recv_event_timeout(&self, timeout: Duration) -> Result<LspEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn try_recv_event(&self) -> Result<LspEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn did_open(
        &mut self,
        uri: impl Into<String>,
        language_id: impl Into<String>,
        version: DocumentVersion,
        text: impl Into<String>,
    ) -> Result<DocumentIncarnation, LspClientError> {
        let uri = uri.into();
        let language_id = language_id.into();
        let text = text.into();
        self.validate_did_open(&uri, &language_id, version, &text)?;
        let incarnation = self
            .next_document_incarnation
            .ok_or(LspClientError::DocumentIncarnationExhausted)?;
        let command = ClientCommand::DidOpen {
            uri: uri.clone(),
            language_id,
            version,
            incarnation,
            text,
        };
        let mut versions = lock_unpoisoned(&self.document_versions);
        if versions.contains_key(&uri) {
            return Err(LspClientError::DocumentAlreadyOpen(uri));
        }
        if versions.len() >= self.limits.max_open_documents {
            return Err(LspClientError::OpenDocumentLimitReached {
                max: self.limits.max_open_documents,
            });
        }
        versions.insert(uri.clone(), OpenDocumentState::new(version, incarnation));
        if let Err(error) = self.enqueue_command(command) {
            versions.remove(&uri);
            return Err(error);
        }
        drop(versions);
        self.next_document_incarnation = incarnation
            .get()
            .checked_add(1)
            .map(DocumentIncarnation::new);
        Ok(incarnation)
    }

    /// Performs every deterministic `didOpen` check without changing document
    /// state or reserving a queue slot. The JSON-size check uses the same
    /// serializer as the eventual command, including URI, language ID, text
    /// escaping, and the complete JSON-RPC payload shape.
    pub fn validate_did_open(
        &self,
        uri: &str,
        language_id: &str,
        version: DocumentVersion,
        text: &str,
    ) -> Result<(), LspClientError> {
        self.ensure_ready()?;
        validate_version_range(version)?;
        self.validate_document_uri_size(uri)?;
        self.validate_document_size(text)?;
        did_open_json_len(
            uri,
            language_id,
            version,
            text,
            self.limits.max_message_bytes,
        )
        .map(|_| ())
        .map_err(|()| LspClientError::MessageTooLarge {
            max: self.limits.max_message_bytes,
        })?;
        let versions = lock_unpoisoned(&self.document_versions);
        if versions.contains_key(uri) {
            Err(LspClientError::DocumentAlreadyOpen(uri.to_owned()))
        } else if versions.len() >= self.limits.max_open_documents {
            Err(LspClientError::OpenDocumentLimitReached {
                max: self.limits.max_open_documents,
            })
        } else if self.next_document_incarnation.is_none() {
            Err(LspClientError::DocumentIncarnationExhausted)
        } else {
            Ok(())
        }
    }

    /// Sends a full-content `didChange`; versions must increase monotonically.
    pub fn did_change(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        text: impl Into<String>,
    ) -> Result<(), LspClientError> {
        self.did_change_with_replacement_range(uri.into(), version, text.into(), None)
    }

    /// Sends one incremental `didChange` whose range replaces the complete
    /// previous document.
    ///
    /// Incremental-only servers do not accept the range-less shape emitted by
    /// [`Self::did_change`]. The caller must therefore provide the previous
    /// synchronized document's exact UTF-16 end position. The replacement
    /// range always starts at `0:0` and ends at `previous_end`; `text` is the
    /// complete new document. Versions, payload bytes, serialized message
    /// bytes, and queue admission use the same checks as the full-sync path.
    pub fn did_change_full_document_replacement(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        previous_end: LspPosition,
        text: impl Into<String>,
    ) -> Result<(), LspClientError> {
        self.did_change_with_replacement_range(uri.into(), version, text.into(), Some(previous_end))
    }

    fn did_change_with_replacement_range(
        &mut self,
        uri: String,
        version: DocumentVersion,
        text: String,
        previous_end: Option<LspPosition>,
    ) -> Result<(), LspClientError> {
        self.ensure_ready()?;
        validate_version_range(version)?;
        if let Some(previous_end) = previous_end {
            validate_position_range(previous_end)?;
        }
        let current = self.current_document_state(&uri)?;
        if version <= current.version {
            return Err(LspClientError::StaleDocumentVersion {
                uri,
                attempted: version,
                current: current.version,
            });
        }
        self.validate_document_size(&text)?;
        let command = ClientCommand::DidChange {
            uri: uri.clone(),
            version,
            previous_end,
            text,
        };
        self.validate_outbound_command(&command)?;
        let mut versions = lock_unpoisoned(&self.document_versions);
        versions.insert(
            uri.clone(),
            OpenDocumentState::new(version, current.incarnation),
        );
        let sent = self.enqueue_command(command);
        if sent.is_err() {
            versions.insert(uri, current);
        }
        sent
    }

    pub fn did_save(
        &self,
        uri: impl Into<String>,
        version: DocumentVersion,
        text: Option<String>,
    ) -> Result<(), LspClientError> {
        self.ensure_ready()?;
        let uri = uri.into();
        self.require_current_version(&uri, version)?;
        if let Some(text) = &text {
            self.validate_document_size(text)?;
        }
        self.send_command(ClientCommand::DidSave { uri, text })
    }

    pub fn did_close(&mut self, uri: impl Into<String>) -> Result<(), LspClientError> {
        self.ensure_ready()?;
        let uri = uri.into();
        self.current_document_state(&uri)?;
        let command = ClientCommand::DidClose { uri: uri.clone() };
        self.validate_outbound_command(&command)?;
        let mut versions = lock_unpoisoned(&self.document_versions);
        let previous = versions.remove(&uri).expect("open document was checked");
        let sent = self.enqueue_command(command);
        if sent.is_err() {
            versions.insert(uri, previous);
        }
        sent
    }

    pub fn request_completion(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        position: LspPosition,
    ) -> Result<RequestId, LspClientError> {
        self.request_feature(
            LspOperation::Completion,
            "textDocument/completion",
            uri.into(),
            version,
            text_position_params(position),
        )
    }

    pub fn request_hover(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        position: LspPosition,
    ) -> Result<RequestId, LspClientError> {
        self.request_feature(
            LspOperation::Hover,
            "textDocument/hover",
            uri.into(),
            version,
            text_position_params(position),
        )
    }

    pub fn request_definition(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        position: LspPosition,
    ) -> Result<RequestId, LspClientError> {
        self.request_feature(
            LspOperation::Definition,
            "textDocument/definition",
            uri.into(),
            version,
            text_position_params(position),
        )
    }

    pub fn request_references(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        position: LspPosition,
        include_declaration: bool,
    ) -> Result<RequestId, LspClientError> {
        let mut params = match text_position_params(position) {
            JsonValue::Object(params) => params,
            _ => unreachable!("text position params are an object"),
        };
        params.insert(
            "context".to_owned(),
            object([("includeDeclaration", JsonValue::Bool(include_declaration))]),
        );
        self.request_feature(
            LspOperation::References,
            "textDocument/references",
            uri.into(),
            version,
            JsonValue::Object(params),
        )
    }

    pub fn request_document_symbols(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
    ) -> Result<RequestId, LspClientError> {
        self.request_feature(
            LspOperation::DocumentSymbols,
            "textDocument/documentSymbol",
            uri.into(),
            version,
            object([]),
        )
    }

    /// Requests symbols across the initialized workspace. This request is not
    /// tied to an open text document and is therefore valid before `did_open`.
    pub fn request_workspace_symbols(
        &mut self,
        query: impl Into<String>,
    ) -> Result<RequestId, LspClientError> {
        self.ensure_ready()?;
        let request_id = self.allocate_request_id()?;
        self.send_command(ClientCommand::Request {
            request_id,
            operation: LspOperation::WorkspaceSymbols,
            method: "workspace/symbol",
            scope: RequestScope::Workspace,
            params: object([("query", JsonValue::String(query.into()))]),
        })?;
        Ok(request_id)
    }

    /// Asks the server to cancel one still-pending request.
    ///
    /// Cancellation is advisory in LSP: the server must still send one terminal
    /// response. The protocol thread therefore retains the original request
    /// correlation until that response arrives. Repeated cancellation requests
    /// for the same pending ID emit at most one `$/cancelRequest` notification;
    /// an ID whose response already arrived is a harmless no-op.
    pub fn cancel_request(&self, request_id: RequestId) -> Result<(), LspClientError> {
        self.ensure_ready()?;
        self.send_command(ClientCommand::CancelRequest { request_id })
    }

    pub fn request_formatting(
        &mut self,
        uri: impl Into<String>,
        version: DocumentVersion,
        tab_size: usize,
        insert_spaces: bool,
    ) -> Result<RequestId, LspClientError> {
        let tab_size = u64::try_from(tab_size).unwrap_or(u64::MAX);
        self.request_feature(
            LspOperation::Formatting,
            "textDocument/formatting",
            uri.into(),
            version,
            object([(
                "options",
                object([
                    ("tabSize", JsonValue::from(tab_size)),
                    ("insertSpaces", JsonValue::Bool(insert_spaces)),
                ]),
            )]),
        )
    }

    /// Starts the LSP `shutdown` request. `exit` is sent only after its response.
    pub fn shutdown(&mut self) -> Result<(), LspClientError> {
        let initial_state = self.lifecycle.state.load(Ordering::Acquire);
        match initial_state {
            STATE_STOPPED => return Err(LspClientError::Stopped),
            STATE_SHUTTING_DOWN => return Ok(()),
            _ => {}
        }
        let request_id = self.allocate_request_id()?;
        let previous_state = loop {
            let state = self.lifecycle.state.load(Ordering::Acquire);
            match state {
                STATE_STOPPED => return Err(LspClientError::Stopped),
                STATE_SHUTTING_DOWN => return Ok(()),
                STATE_STARTING | STATE_READY => {
                    if self
                        .lifecycle
                        .state
                        .compare_exchange(
                            state,
                            STATE_SHUTTING_DOWN,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break state;
                    }
                }
                _ => return Err(LspClientError::Stopped),
            }
        };

        match self.send_control_command(ClientCommand::Shutdown { request_id }) {
            Ok(()) => Ok(()),
            Err(error) => {
                // READY is safe to restore because initialization was already
                // published. STARTING is not: an initialize response may have
                // observed SHUTTING_DOWN and deliberately suppressed READY,
                // so rolling back could strand the client in a false STARTING
                // state after that response was consumed.
                if previous_state == STATE_READY {
                    let _ = self.lifecycle.state.compare_exchange(
                        STATE_SHUTTING_DOWN,
                        STATE_READY,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                Err(error)
            }
        }
    }

    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.lifecycle.wait_stopped(timeout)
    }

    fn request_feature(
        &mut self,
        operation: LspOperation,
        method: &'static str,
        uri: String,
        version: DocumentVersion,
        mut params: JsonValue,
    ) -> Result<RequestId, LspClientError> {
        self.ensure_ready()?;
        let current_document = self.require_current_version(&uri, version)?;
        if let JsonValue::Object(params) = &mut params {
            params.insert(
                "textDocument".to_owned(),
                object([("uri", JsonValue::String(uri.clone()))]),
            );
        }
        let request_id = self.allocate_request_id()?;
        self.send_command(ClientCommand::Request {
            request_id,
            operation,
            method,
            scope: RequestScope::Document {
                uri,
                version,
                incarnation: current_document.incarnation,
            },
            params,
        })?;
        Ok(request_id)
    }

    fn allocate_request_id(&mut self) -> Result<RequestId, LspClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(LspClientError::RequestIdExhausted)?;
        Ok(request_id)
    }

    fn ensure_ready(&self) -> Result<(), LspClientError> {
        match self.lifecycle.state.load(Ordering::Acquire) {
            STATE_READY => Ok(()),
            STATE_STARTING => Err(LspClientError::NotReady),
            STATE_SHUTTING_DOWN => Err(LspClientError::ShuttingDown),
            _ => Err(LspClientError::Stopped),
        }
    }

    fn current_document_state(&self, uri: &str) -> Result<OpenDocumentState, LspClientError> {
        lock_unpoisoned(&self.document_versions)
            .get(uri)
            .copied()
            .ok_or_else(|| LspClientError::DocumentNotOpen(uri.to_owned()))
    }

    fn require_current_version(
        &self,
        uri: &str,
        version: DocumentVersion,
    ) -> Result<OpenDocumentState, LspClientError> {
        let current = self.current_document_state(uri)?;
        if current.version == version {
            Ok(current)
        } else {
            Err(LspClientError::StaleDocumentVersion {
                uri: uri.to_owned(),
                attempted: version,
                current: current.version,
            })
        }
    }

    fn validate_document_size(&self, text: &str) -> Result<(), LspClientError> {
        if text.len() > self.limits.max_document_bytes {
            Err(LspClientError::DocumentTooLarge {
                bytes: text.len(),
                max: self.limits.max_document_bytes,
            })
        } else {
            Ok(())
        }
    }

    fn validate_document_uri_size(&self, uri: &str) -> Result<(), LspClientError> {
        if uri.len() > self.limits.max_document_uri_bytes {
            Err(LspClientError::DocumentUriTooLarge {
                bytes: uri.len(),
                max: self.limits.max_document_uri_bytes,
            })
        } else {
            Ok(())
        }
    }

    fn send_command(&self, command: ClientCommand) -> Result<(), LspClientError> {
        self.validate_outbound_command(&command)?;
        self.enqueue_command(command)
    }

    fn enqueue_command(&self, command: ClientCommand) -> Result<(), LspClientError> {
        match self.input.try_send(ProtocolInput::Command(command)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(LspClientError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(LspClientError::Stopped),
        }
    }

    fn send_control_command(&self, command: ClientCommand) -> Result<(), LspClientError> {
        self.validate_outbound_command(&command)?;
        match self.input.try_send_control(ProtocolInput::Command(command)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(LspClientError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(LspClientError::Stopped),
        }
    }

    fn validate_outbound_command(&self, command: &ClientCommand) -> Result<(), LspClientError> {
        client_command_json_len(command, self.limits.max_message_bytes)
            .map(|_| ())
            .map_err(|()| LspClientError::MessageTooLarge {
                max: self.limits.max_message_bytes,
            })
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if !self.is_stopped() {
            let _ = self.shutdown();
            self.lifecycle.wait_stopped(self.shutdown_timeout);
        }

        // The child has not been waited on yet, so its PID/process-group ID
        // cannot be reused here even if the server leader is already a zombie.
        // Kill the owned group before reaping the leader; this also catches a
        // quiet background descendant that closed all inherited pipes.
        let _ = force_terminate_process_group(self.pid);
        {
            let mut child = lock_unpoisoned(&self.child);
            if child.try_wait().ok().flatten().is_none() {
                let _ = force_terminate_process(&mut child);
                let _ = child.wait();
            }
        }

        let _ = self.input.try_send_control(ProtocolInput::ForceStop);
        if let Some(thread) = self.protocol_thread.take() {
            let _ = thread.join();
        }

        let _ = wait_for_reader_threads(
            self.stdout_thread.as_ref(),
            self.stderr_thread.as_ref(),
            FORCE_KILL_GRACE,
        );
        if let Some(thread) = self.stdout_thread.take() {
            join_if_finished(thread);
        }
        if let Some(thread) = self.stderr_thread.take() {
            join_if_finished(thread);
        }
    }
}

#[derive(Debug)]
struct Lifecycle {
    state: AtomicU8,
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl Lifecycle {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_STARTING),
            stopped: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn mark_stopped(&self) {
        self.state.store(STATE_STOPPED, Ordering::Release);
        *lock_unpoisoned(&self.stopped) = true;
        self.changed.notify_all();
    }

    fn wait_stopped(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        let deadline = started
            .checked_add(normalized_shutdown_timeout(timeout))
            .unwrap_or(started);
        let mut stopped = lock_unpoisoned(&self.stopped);
        while !*stopped {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, timed_out) = wait_timeout_unpoisoned(&self.changed, stopped, remaining);
            stopped = next;
            if timed_out {
                break;
            }
        }
        *stopped
    }
}

fn normalized_shutdown_timeout(timeout: Duration) -> Duration {
    timeout.min(MAX_SHUTDOWN_TIMEOUT)
}

enum ProtocolInput {
    Command(ClientCommand),
    ServerMessage(String),
    Stderr(Vec<u8>),
    StderrTruncated { limit: usize },
    TransportError(String),
    ServerClosed,
    ForceStop,
}

enum ClientCommand {
    DidOpen {
        uri: String,
        language_id: String,
        version: DocumentVersion,
        incarnation: DocumentIncarnation,
        text: String,
    },
    DidChange {
        uri: String,
        version: DocumentVersion,
        /// `None` is full sync. `Some(end)` is an incremental change that
        /// replaces the previous document's range from `0:0` through `end`.
        previous_end: Option<LspPosition>,
        text: String,
    },
    DidSave {
        uri: String,
        text: Option<String>,
    },
    DidClose {
        uri: String,
    },
    Request {
        request_id: RequestId,
        operation: LspOperation,
        method: &'static str,
        scope: RequestScope,
        params: JsonValue,
    },
    CancelRequest {
        request_id: RequestId,
    },
    Shutdown {
        request_id: RequestId,
    },
}

fn client_command_json_len(command: &ClientCommand, max: usize) -> Result<usize, ()> {
    let mut length = BoundedJsonLength::new(max);
    write_client_command_json_to(command, &mut length)?;
    Ok(length.bytes)
}

fn did_open_json_len(
    uri: &str,
    language_id: &str,
    version: DocumentVersion,
    text: &str,
    max: usize,
) -> Result<usize, ()> {
    let mut length = BoundedJsonLength::new(max);
    length.push_str("{\"jsonrpc\":\"2.0\",")?;
    write_did_open_command_to(&mut length, uri, language_id, version, text)?;
    Ok(length.bytes)
}

fn write_client_command_json(
    writer: &mut impl Write,
    command: &ClientCommand,
    max_message_bytes: usize,
) -> io::Result<()> {
    let length = client_command_json_len(command, max_message_bytes).map_err(|()| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("outbound LSP message exceeds the {max_message_bytes}-byte limit"),
        )
    })?;
    let mut json = String::with_capacity(length);
    write_client_command_json_to(command, &mut json)
        .expect("String JSON output is infallible after bounded length preflight");
    debug_assert_eq!(json.len(), length);
    write_frame(&mut *writer, &json)?;
    writer.flush()
}

fn write_client_command_json_to(
    command: &ClientCommand,
    output: &mut impl JsonOutput,
) -> Result<(), ()> {
    output.push_str("{\"jsonrpc\":\"2.0\",")?;
    match command {
        ClientCommand::DidOpen {
            uri,
            language_id,
            version,
            incarnation: _,
            text,
        } => write_did_open_command_to(output, uri, language_id, *version, text)?,
        ClientCommand::DidChange {
            uri,
            version,
            previous_end,
            text,
        } => {
            output.push_str(
                "\"method\":\"textDocument/didChange\",\"params\":{\"textDocument\":{\"uri\":",
            )?;
            write_json_string_to(output, uri)?;
            output.push_str(",\"version\":")?;
            write_json_u64_to(output, version.get())?;
            output.push_str("},\"contentChanges\":[{")?;
            if let Some(previous_end) = previous_end {
                output.push_str(
                    "\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":",
                )?;
                write_json_u64_to(output, previous_end.line.get() as u64)?;
                output.push_str(",\"character\":")?;
                write_json_u64_to(output, previous_end.character.get() as u64)?;
                output.push_str("}},")?;
            }
            output.push_str("\"text\":")?;
            write_json_string_to(output, text)?;
            output.push_str("}]}}")?;
        }
        ClientCommand::DidSave { uri, text } => {
            output.push_str(
                "\"method\":\"textDocument/didSave\",\"params\":{\"textDocument\":{\"uri\":",
            )?;
            write_json_string_to(output, uri)?;
            output.push_char('}')?;
            if let Some(text) = text {
                output.push_str(",\"text\":")?;
                write_json_string_to(output, text)?;
            }
            output.push_str("}}")?;
        }
        ClientCommand::DidClose { uri } => {
            output.push_str(
                "\"method\":\"textDocument/didClose\",\"params\":{\"textDocument\":{\"uri\":",
            )?;
            write_json_string_to(output, uri)?;
            output.push_str("}}}")?;
        }
        ClientCommand::Request {
            request_id,
            method,
            params,
            ..
        } => {
            output.push_str("\"id\":")?;
            write_json_u64_to(output, *request_id)?;
            output.push_str(",\"method\":")?;
            write_json_string_to(output, method)?;
            output.push_str(",\"params\":")?;
            write_json_value_to(output, params)?;
            output.push_char('}')?;
        }
        ClientCommand::CancelRequest { request_id } => {
            output.push_str("\"method\":\"$/cancelRequest\",\"params\":{\"id\":")?;
            write_json_u64_to(output, *request_id)?;
            output.push_str("}}")?;
        }
        ClientCommand::Shutdown { request_id } => {
            output.push_str("\"id\":")?;
            write_json_u64_to(output, *request_id)?;
            output.push_str(",\"method\":\"shutdown\",\"params\":null}")?;
        }
    }
    Ok(())
}

fn write_did_open_command_to(
    output: &mut impl JsonOutput,
    uri: &str,
    language_id: &str,
    version: DocumentVersion,
    text: &str,
) -> Result<(), ()> {
    output
        .push_str("\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":")?;
    write_json_string_to(output, uri)?;
    output.push_str(",\"languageId\":")?;
    write_json_string_to(output, language_id)?;
    output.push_str(",\"version\":")?;
    write_json_u64_to(output, version.get())?;
    output.push_str(",\"text\":")?;
    write_json_string_to(output, text)?;
    output.push_str("}}}")
}

fn write_json_u64_to(output: &mut impl JsonOutput, value: u64) -> Result<(), ()> {
    output.push_str(&value.to_string())
}

#[derive(Clone, Debug)]
enum RequestScope {
    Document {
        uri: String,
        version: DocumentVersion,
        incarnation: DocumentIncarnation,
    },
    Workspace,
}

#[derive(Clone, Debug)]
struct PendingRequest {
    operation: LspOperation,
    scope: Option<RequestScope>,
    cancel_requested: bool,
    /// False only for a shutdown accepted while initialize is still pending.
    /// Such a request is retained for correlation but must not reach the wire
    /// until the initialize response and `initialized` notification do.
    sent: bool,
}

struct ByteEstimate {
    bytes: usize,
    ceiling: usize,
}

impl ByteEstimate {
    fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            ceiling: limit.saturating_add(1),
        }
    }

    fn add(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes).min(self.ceiling);
    }

    fn add_json(&mut self, value: &JsonValue) {
        self.add_json_at_depth(value, 0);
    }

    fn add_json_at_depth(&mut self, value: &JsonValue, depth: usize) {
        if self.bytes >= self.ceiling {
            return;
        }
        if depth > MAX_JSON_DEPTH {
            self.bytes = self.ceiling;
            return;
        }
        self.add(
            std::mem::size_of::<JsonValue>().saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER),
        );
        match value {
            JsonValue::Null | JsonValue::Bool(_) => {}
            JsonValue::Number(number) => self.add(
                number
                    .0
                    .capacity()
                    .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER),
            ),
            JsonValue::String(value) => self.add(
                value
                    .capacity()
                    .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER),
            ),
            JsonValue::Array(values) => {
                self.add(
                    values
                        .capacity()
                        .saturating_mul(std::mem::size_of::<JsonValue>())
                        .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER),
                );
                for value in values {
                    self.add_json_at_depth(value, depth + 1);
                }
            }
            JsonValue::Object(values) => {
                self.add(
                    btree_queue_object_allocation_estimate(values.len())
                        .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER),
                );
                for (key, value) in values {
                    self.add(
                        key.capacity()
                            .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER),
                    );
                    self.add_json_at_depth(value, depth + 1);
                }
            }
        }
    }
}

/// `BTreeMap` does not expose node occupancy or spare capacity. Rust's current
/// implementation uses 11 key/value slots and 12 child edges per node, so
/// charge a full header/slot/edge node for every live entry (and one node for
/// an empty object). The maximally sparse assumption intentionally overcounts
/// dense maps while staying safe for chains of one-entry objects.
fn btree_queue_object_allocation_estimate(entries: usize) -> usize {
    const WORST_CASE_NODE_SLOTS: usize = 11;
    const WORST_CASE_CHILD_EDGES: usize = WORST_CASE_NODE_SLOTS + 1;
    const NODE_HEADER_WORDS: usize = 8;

    let slot_bytes = std::mem::size_of::<String>().saturating_add(std::mem::size_of::<JsonValue>());
    let node_bytes = NODE_HEADER_WORDS
        .saturating_mul(std::mem::size_of::<usize>())
        .saturating_add(WORST_CASE_NODE_SLOTS.saturating_mul(slot_bytes))
        .saturating_add(WORST_CASE_CHILD_EDGES.saturating_mul(std::mem::size_of::<usize>()));
    entries.max(1).saturating_mul(node_bytes)
}

impl QueueWeight for ProtocolInput {
    fn queue_weight(&self, limit: usize) -> usize {
        let mut estimate = ByteEstimate::new(limit);
        estimate.add(std::mem::size_of::<Self>());
        match self {
            Self::Command(command) => add_command_weight(&mut estimate, command),
            Self::ServerMessage(message) | Self::TransportError(message) => {
                estimate.add(message.capacity());
            }
            Self::Stderr(bytes) => estimate.add(bytes.capacity()),
            Self::StderrTruncated { .. } | Self::ServerClosed | Self::ForceStop => {}
        }
        estimate.bytes
    }
}

fn add_command_weight(estimate: &mut ByteEstimate, command: &ClientCommand) {
    estimate.add(std::mem::size_of::<ClientCommand>());
    match command {
        ClientCommand::DidOpen {
            uri,
            language_id,
            text,
            ..
        } => {
            estimate.add(uri.capacity());
            estimate.add(language_id.capacity());
            estimate.add(text.capacity());
        }
        ClientCommand::DidChange { uri, text, .. } => {
            estimate.add(uri.capacity());
            estimate.add(text.capacity());
        }
        ClientCommand::DidSave { uri, text } => {
            estimate.add(uri.capacity());
            if let Some(text) = text {
                estimate.add(text.capacity());
            }
        }
        ClientCommand::DidClose { uri } => estimate.add(uri.capacity()),
        ClientCommand::Request { scope, params, .. } => {
            if let RequestScope::Document { uri, .. } = scope {
                estimate.add(uri.capacity());
            }
            estimate.add_json(params);
        }
        ClientCommand::CancelRequest { .. } | ClientCommand::Shutdown { .. } => {}
    }
}

impl QueueWeight for LspEvent {
    fn queue_weight(&self, limit: usize) -> usize {
        let mut estimate = ByteEstimate::new(limit);
        estimate.add(std::mem::size_of::<Self>());
        match self {
            Self::Ready { capabilities, .. } => estimate.add_json(capabilities),
            Self::Diagnostics {
                uri, diagnostics, ..
            } => {
                estimate.add(uri.capacity());
                estimate.add(
                    diagnostics
                        .capacity()
                        .saturating_mul(std::mem::size_of::<JsonValue>()),
                );
                for diagnostic in diagnostics {
                    estimate.add_json(diagnostic);
                }
            }
            Self::DiagnosticsRejected { uri, reason } => {
                if let Some(uri) = uri {
                    estimate.add(uri.capacity());
                }
                estimate.add(reason.capacity());
            }
            Self::Completion { uri, result, .. }
            | Self::Hover { uri, result, .. }
            | Self::Definition { uri, result, .. }
            | Self::References { uri, result, .. }
            | Self::DocumentSymbols { uri, result, .. }
            | Self::Formatting { uri, result, .. } => {
                estimate.add(uri.capacity());
                estimate.add_json(result);
            }
            Self::WorkspaceSymbols { result, .. } => estimate.add_json(result),
            Self::RequestFailed { error, .. } => {
                estimate.add(error.message.capacity());
                if let Some(data) = &error.data {
                    estimate.add_json(data);
                }
            }
            Self::StaleResponse { uri, .. } | Self::StaleDiagnostics { uri, .. } => {
                estimate.add(uri.capacity());
            }
            Self::ServerNotification { method, params } => {
                estimate.add(method.capacity());
                if let Some(params) = params {
                    estimate.add_json(params);
                }
            }
            Self::ServerRequestRejected { method } => estimate.add(method.capacity()),
            Self::Stderr(bytes) => estimate.add(bytes.capacity()),
            Self::ProtocolError(message) | Self::TransportError(message) => {
                estimate.add(message.capacity());
            }
            Self::StderrTruncated { .. }
            | Self::EventsDropped { .. }
            | Self::ShutdownComplete
            | Self::ServerClosed => {}
        }
        estimate.bytes
    }
}

struct EventSink {
    sender: BudgetedSender<LspEvent>,
    dropped: usize,
}

impl EventSink {
    fn new(sender: BudgetedSender<LspEvent>) -> Self {
        Self { sender, dropped: 0 }
    }

    fn emit(&mut self, event: LspEvent) {
        if !self.flush_dropped() {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped = self.dropped.saturating_add(1);
                // A lone event that exceeds the byte budget must still become
                // visible while the otherwise-empty queue has room for this
                // bounded marker.
                let _ = self.flush_dropped();
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn flush_dropped(&mut self) -> bool {
        if self.dropped == 0 {
            return true;
        }
        match self.sender.try_send_control(LspEvent::EventsDropped {
            count: self.dropped,
        }) {
            Ok(()) => {
                self.dropped = 0;
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn protocol_loop(
    mut stdin: ChildStdin,
    inputs: BudgetedReceiver<ProtocolInput>,
    events: BudgetedSender<LspEvent>,
    lifecycle: Arc<Lifecycle>,
    current_documents: Arc<Mutex<HashMap<String, OpenDocumentState>>>,
    config: LspServerConfig,
) {
    struct StopGuard(Arc<Lifecycle>);
    impl Drop for StopGuard {
        fn drop(&mut self) {
            self.0.mark_stopped();
        }
    }
    let _stop = StopGuard(Arc::clone(&lifecycle));
    let mut events = EventSink::new(events);
    let mut pending = HashMap::new();
    let mut documents = HashMap::new();

    let initialize = initialize_request(&config);
    if let Err(error) = write_json(&mut stdin, &initialize, config.limits.max_message_bytes) {
        events.emit(LspEvent::TransportError(error.to_string()));
        return;
    }
    pending.insert(
        1,
        PendingRequest {
            operation: LspOperation::Initialize,
            scope: None,
            cancel_requested: false,
            sent: true,
        },
    );

    while let Ok(input) = inputs.recv() {
        let keep_running = handle_protocol_input(
            input,
            &mut stdin,
            &mut pending,
            &mut documents,
            &current_documents,
            &mut events,
            &lifecycle,
            config.limits.max_message_bytes,
        );
        if !keep_running {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_protocol_input<W: Write>(
    input: ProtocolInput,
    stdin: &mut W,
    pending: &mut HashMap<RequestId, PendingRequest>,
    protocol_documents: &mut HashMap<String, OpenDocumentState>,
    current_documents: &Mutex<HashMap<String, OpenDocumentState>>,
    events: &mut EventSink,
    lifecycle: &Lifecycle,
    max_message_bytes: usize,
) -> bool {
    match input {
        ProtocolInput::Command(command) => handle_client_command(
            command,
            stdin,
            pending,
            protocol_documents,
            events,
            max_message_bytes,
        ),
        ProtocolInput::ServerMessage(message) => handle_server_message(
            &message,
            stdin,
            pending,
            DocumentStateViews {
                protocol: protocol_documents,
                optimistic: current_documents,
            },
            events,
            lifecycle,
            max_message_bytes,
        ),
        ProtocolInput::Stderr(bytes) => {
            events.emit(LspEvent::Stderr(bytes));
            true
        }
        ProtocolInput::StderrTruncated { limit } => {
            events.emit(LspEvent::StderrTruncated { limit });
            true
        }
        ProtocolInput::TransportError(message) => {
            events.emit(LspEvent::TransportError(message));
            false
        }
        ProtocolInput::ServerClosed => {
            events.emit(LspEvent::ServerClosed);
            false
        }
        ProtocolInput::ForceStop => false,
    }
}

fn handle_client_command<W: Write>(
    command: ClientCommand,
    stdin: &mut W,
    pending: &mut HashMap<RequestId, PendingRequest>,
    documents: &mut HashMap<String, OpenDocumentState>,
    events: &mut EventSink,
    max_message_bytes: usize,
) -> bool {
    match &command {
        ClientCommand::DidOpen {
            uri,
            version,
            incarnation,
            ..
        } => {
            documents.insert(uri.clone(), OpenDocumentState::new(*version, *incarnation));
        }
        ClientCommand::DidChange { uri, version, .. } => {
            let is_newer = documents
                .get(uri)
                .is_some_and(|current| version > &current.version);
            if !is_newer {
                return true;
            }
            documents
                .get_mut(uri)
                .expect("newer document version requires an open document")
                .version = *version;
        }
        ClientCommand::DidClose { uri } => {
            documents.remove(uri);
        }
        ClientCommand::Request {
            request_id,
            operation,
            scope,
            ..
        } => {
            if let RequestScope::Document {
                uri,
                version,
                incarnation,
            } = &scope
                && documents.get(uri) != Some(&OpenDocumentState::new(*version, *incarnation))
            {
                let current_version = documents.get(uri).map(|state| state.version);
                events.emit(LspEvent::StaleResponse {
                    request_id: *request_id,
                    operation: *operation,
                    uri: uri.clone(),
                    requested_version: *version,
                    current_version,
                });
                return true;
            }
            // Keep one of the hard-bounded slots available for a graceful
            // shutdown even when a server stops answering feature requests.
            if pending.len() >= MAX_PENDING_REQUESTS.saturating_sub(1) {
                emit_pending_limit(events, *request_id, *operation);
                return true;
            }
            pending.insert(
                *request_id,
                PendingRequest {
                    operation: *operation,
                    scope: Some(scope.clone()),
                    cancel_requested: false,
                    sent: true,
                },
            );
        }
        ClientCommand::CancelRequest { request_id } => {
            let Some(request) = pending.get_mut(request_id) else {
                return true;
            };
            if request.cancel_requested
                || matches!(
                    request.operation,
                    LspOperation::Initialize | LspOperation::Shutdown
                )
            {
                return true;
            }
            request.cancel_requested = true;
        }
        ClientCommand::Shutdown { request_id } => {
            if pending.len() >= MAX_PENDING_REQUESTS {
                emit_pending_limit(events, *request_id, LspOperation::Shutdown);
                return false;
            }
            let defer_until_initialized = pending
                .values()
                .any(|request| request.operation == LspOperation::Initialize);
            pending.insert(
                *request_id,
                PendingRequest {
                    operation: LspOperation::Shutdown,
                    scope: None,
                    cancel_requested: false,
                    sent: !defer_until_initialized,
                },
            );
            if defer_until_initialized {
                return true;
            }
        }
        ClientCommand::DidSave { .. } => {}
    }

    if let Err(error) = write_client_command_json(stdin, &command, max_message_bytes) {
        events.emit(LspEvent::TransportError(error.to_string()));
        false
    } else {
        true
    }
}

fn emit_pending_limit(events: &mut EventSink, request_id: RequestId, operation: LspOperation) {
    events.emit(LspEvent::RequestFailed {
        request_id,
        operation,
        error: JsonRpcError {
            code: -32099,
            message: format!("client pending-request limit ({MAX_PENDING_REQUESTS}) reached"),
            data: None,
        },
    });
}

/// A malformed response for a known request is still terminal for that
/// request. Emit the correlated failure that releases the editor's request
/// slot instead of degrading it to an uncorrelated protocol notice.
fn handle_correlated_protocol_failure<W: Write>(
    stdin: &mut W,
    events: &mut EventSink,
    request_id: RequestId,
    operation: LspOperation,
    reason: String,
    max_message_bytes: usize,
) -> bool {
    events.emit(LspEvent::RequestFailed {
        request_id,
        operation,
        error: JsonRpcError {
            code: INVALID_SERVER_RESPONSE_CODE,
            message: format!("invalid language-server response: {reason}"),
            data: None,
        },
    });
    match operation {
        LspOperation::Initialize => false,
        LspOperation::Shutdown => {
            if let Err(error) = write_json(
                stdin,
                &notification("exit", JsonValue::Null),
                max_message_bytes,
            ) {
                events.emit(LspEvent::TransportError(error.to_string()));
            }
            false
        }
        _ => true,
    }
}

#[derive(Clone, Copy)]
struct DocumentStateViews<'a> {
    /// State advanced only as commands are processed by the protocol loop.
    protocol: &'a HashMap<String, OpenDocumentState>,
    /// State advanced optimistically when the public API accepts a command.
    optimistic: &'a Mutex<HashMap<String, OpenDocumentState>>,
}

fn handle_server_message<W: Write>(
    source: &str,
    stdin: &mut W,
    pending: &mut HashMap<RequestId, PendingRequest>,
    document_states: DocumentStateViews<'_>,
    events: &mut EventSink,
    lifecycle: &Lifecycle,
    max_message_bytes: usize,
) -> bool {
    let mut message = match JsonValue::parse(source) {
        Ok(JsonValue::Object(object)) => object,
        Ok(_) => {
            events.emit(LspEvent::ProtocolError(
                "JSON-RPC message must be an object".to_owned(),
            ));
            return true;
        }
        Err(error) => {
            events.emit(LspEvent::ProtocolError(error.to_string()));
            return true;
        }
    };

    if message.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
        let reason = "JSON-RPC message is missing jsonrpc=\"2.0\"".to_owned();
        let response_shaped = message.contains_key("result") || message.contains_key("error");
        let has_valid_method = message.get("method").and_then(JsonValue::as_str).is_some();
        if response_shaped
            && !has_valid_method
            && let Some(request_id) = message.get("id").and_then(JsonValue::as_u64)
            && let Some(pending_request) = pending.remove(&request_id)
        {
            return handle_correlated_protocol_failure(
                stdin,
                events,
                request_id,
                pending_request.operation,
                reason,
                max_message_bytes,
            );
        }
        events.emit(LspEvent::ProtocolError(reason));
        return true;
    }

    if let Some(method) = message
        .get("method")
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
    {
        let params = message.remove("params");
        if let Some(id) = message.remove("id") {
            let error = object([
                ("jsonrpc", "2.0".into()),
                ("id", id),
                (
                    "error",
                    object([
                        ("code", JsonValue::from(-32601_i64)),
                        ("message", "method not supported by wscrpt".into()),
                    ]),
                ),
            ]);
            if let Err(write_error) = write_json(stdin, &error, max_message_bytes) {
                events.emit(LspEvent::TransportError(write_error.to_string()));
                return false;
            }
            events.emit(LspEvent::ServerRequestRejected { method });
            return true;
        }

        if method == "textDocument/publishDiagnostics" {
            handle_diagnostics(params, document_states.protocol, events);
        } else {
            events.emit(LspEvent::ServerNotification { method, params });
        }
        return true;
    }

    let Some(request_id) = message.get("id").and_then(JsonValue::as_u64) else {
        events.emit(LspEvent::ProtocolError(
            "JSON-RPC response has no numeric request ID".to_owned(),
        ));
        return true;
    };
    let Some(pending_request) = pending.remove(&request_id) else {
        events.emit(LspEvent::ProtocolError(format!(
            "response for unknown request ID {request_id}"
        )));
        return true;
    };
    if !pending_request.sent {
        return handle_correlated_protocol_failure(
            stdin,
            events,
            request_id,
            pending_request.operation,
            "response arrived before the client sent its request".to_owned(),
            max_message_bytes,
        );
    }
    let has_result = message.contains_key("result");
    let has_error = message.contains_key("error");
    if has_result == has_error {
        return handle_correlated_protocol_failure(
            stdin,
            events,
            request_id,
            pending_request.operation,
            "JSON-RPC response must contain exactly one of result or error".to_owned(),
            max_message_bytes,
        );
    }

    if let Some(error) = message.remove("error") {
        match parse_rpc_error(error) {
            Ok(error) => events.emit(LspEvent::RequestFailed {
                request_id,
                operation: pending_request.operation,
                error,
            }),
            Err(error) => {
                return handle_correlated_protocol_failure(
                    stdin,
                    events,
                    request_id,
                    pending_request.operation,
                    error,
                    max_message_bytes,
                );
            }
        }
        if pending_request.operation == LspOperation::Shutdown {
            let _ = write_json(
                stdin,
                &notification("exit", JsonValue::Null),
                max_message_bytes,
            );
            return false;
        }
        return pending_request.operation != LspOperation::Initialize;
    }

    let result = message.remove("result").unwrap_or(JsonValue::Null);
    match pending_request.operation {
        LspOperation::Initialize => {
            if let Err(error) = write_json(
                stdin,
                &notification("initialized", object([])),
                max_message_bytes,
            ) {
                events.emit(LspEvent::TransportError(error.to_string()));
                return false;
            }
            let deferred_shutdown = pending.iter().find_map(|(request_id, request)| {
                (request.operation == LspOperation::Shutdown && !request.sent)
                    .then_some(*request_id)
            });
            if let Some(request_id) = deferred_shutdown {
                if let Err(error) = write_client_command_json(
                    stdin,
                    &ClientCommand::Shutdown { request_id },
                    max_message_bytes,
                ) {
                    events.emit(LspEvent::TransportError(error.to_string()));
                    return false;
                }
                pending
                    .get_mut(&request_id)
                    .expect("deferred shutdown remains pending until its response")
                    .sent = true;
            }
            let capabilities = match result {
                JsonValue::Object(mut result) => {
                    result.remove("capabilities").unwrap_or(JsonValue::Null)
                }
                _ => JsonValue::Null,
            };
            let workspace_symbols = workspace_symbol_provider_supported(&capabilities);
            let text_document_sync = parse_text_document_sync_capability(&capabilities);
            if lifecycle
                .state
                .compare_exchange(
                    STATE_STARTING,
                    STATE_READY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                events.emit(LspEvent::Ready {
                    capabilities,
                    workspace_symbols,
                    text_document_sync,
                });
            }
        }
        LspOperation::Shutdown => {
            if let Err(error) = write_json(
                stdin,
                &notification("exit", JsonValue::Null),
                max_message_bytes,
            ) {
                events.emit(LspEvent::TransportError(error.to_string()));
            } else {
                events.emit(LspEvent::ShutdownComplete);
            }
            return false;
        }
        LspOperation::WorkspaceSymbols => {
            if !matches!(pending_request.scope, Some(RequestScope::Workspace)) {
                return handle_correlated_protocol_failure(
                    stdin,
                    events,
                    request_id,
                    pending_request.operation,
                    format!(
                        "workspace-symbol response {request_id} had a non-workspace request scope"
                    ),
                    max_message_bytes,
                );
            }
            events.emit(LspEvent::WorkspaceSymbols { request_id, result });
        }
        operation => {
            let Some(RequestScope::Document {
                uri,
                version,
                incarnation,
            }) = pending_request.scope
            else {
                return handle_correlated_protocol_failure(
                    stdin,
                    events,
                    request_id,
                    pending_request.operation,
                    format!("document response {request_id} had a non-document request scope"),
                    max_message_bytes,
                );
            };
            let current_document = lock_unpoisoned(document_states.optimistic)
                .get(&uri)
                .copied();
            if current_document != Some(OpenDocumentState::new(version, incarnation)) {
                events.emit(LspEvent::StaleResponse {
                    request_id,
                    operation,
                    uri,
                    requested_version: version,
                    current_version: current_document.map(|state| state.version),
                });
                return true;
            }
            let event = match operation {
                LspOperation::Completion => LspEvent::Completion {
                    request_id,
                    uri,
                    version,
                    result,
                },
                LspOperation::Hover => LspEvent::Hover {
                    request_id,
                    uri,
                    version,
                    result,
                },
                LspOperation::Definition => LspEvent::Definition {
                    request_id,
                    uri,
                    version,
                    result,
                },
                LspOperation::References => LspEvent::References {
                    request_id,
                    uri,
                    version,
                    result,
                },
                LspOperation::DocumentSymbols => LspEvent::DocumentSymbols {
                    request_id,
                    uri,
                    version,
                    result,
                },
                LspOperation::Formatting => LspEvent::Formatting {
                    request_id,
                    uri,
                    version,
                    result,
                },
                LspOperation::Initialize
                | LspOperation::WorkspaceSymbols
                | LspOperation::Shutdown => unreachable!(),
            };
            events.emit(event);
        }
    }
    true
}

fn handle_diagnostics(
    params: Option<JsonValue>,
    protocol_documents: &HashMap<String, OpenDocumentState>,
    events: &mut EventSink,
) {
    let Some(JsonValue::Object(mut params)) = params else {
        reject_diagnostics(events, None, "publishDiagnostics params must be an object");
        return;
    };
    let Some(JsonValue::String(uri)) = params.remove("uri") else {
        reject_diagnostics(events, None, "publishDiagnostics is missing uri");
        return;
    };
    let uri = match normalize_diagnostics_uri(uri, protocol_documents) {
        Ok(uri) => uri,
        Err((uri, reason)) => {
            reject_diagnostics(events, uri, reason);
            return;
        }
    };
    let Some(JsonValue::Array(diagnostics)) = params.remove("diagnostics") else {
        reject_diagnostics(
            events,
            Some(uri),
            "publishDiagnostics is missing diagnostics array",
        );
        return;
    };
    let version = match params.remove("version") {
        None => None,
        Some(JsonValue::Number(version)) => {
            let Some(version) = version
                .as_str()
                .parse::<u64>()
                .ok()
                .filter(|version| *version <= i32::MAX as u64)
            else {
                reject_diagnostics(
                    events,
                    Some(uri),
                    "publishDiagnostics version must be an integer from 0 through 2147483647",
                );
                return;
            };
            Some(DocumentVersion::new(version))
        }
        Some(_) => {
            reject_diagnostics(
                events,
                Some(uri),
                "publishDiagnostics version must be an integer from 0 through 2147483647",
            );
            return;
        }
    };
    let current_document = protocol_documents.get(&uri).copied();
    let current_version = current_document.map(|document| document.version);
    if let Some(received_version) = version
        && current_version.is_some()
        && current_version != Some(received_version)
    {
        events.emit(LspEvent::StaleDiagnostics {
            uri,
            received_version,
            current_version,
        });
        return;
    }
    events.emit(LspEvent::Diagnostics {
        uri,
        version,
        observed_version: current_version,
        observed_incarnation: current_document.map(|document| document.incarnation),
        diagnostics,
    });
}

/// Normalize one bounded local diagnostics URI to the same identity emitted
/// for synchronized documents. Keeping the original allocation when it is
/// already canonical avoids copying the common publication path.
fn normalize_diagnostics_uri(
    uri: String,
    protocol_documents: &HashMap<String, OpenDocumentState>,
) -> Result<String, (Option<String>, String)> {
    if protocol_documents.contains_key(&uri) {
        return Ok(uri);
    }
    if uri.is_empty() {
        return Err((Some(uri), "publishDiagnostics uri is empty".to_owned()));
    }
    if uri.len() > MAX_DIAGNOSTICS_ALIAS_URI_BYTES {
        return Err((
            None,
            format!(
                "publishDiagnostics alias uri exceeds the {MAX_DIAGNOSTICS_ALIAS_URI_BYTES}-byte safety limit"
            ),
        ));
    }
    let path = crate::lsp_ui::file_uri_to_path(&uri).map_err(|error| {
        (
            Some(uri.clone()),
            format!("publishDiagnostics uri is not a valid local file URI: {error}"),
        )
    })?;
    if path
        .components()
        .take(MAX_DIAGNOSTICS_ALIAS_PATH_COMPONENTS + 1)
        .count()
        > MAX_DIAGNOSTICS_ALIAS_PATH_COMPONENTS
    {
        return Err((
            Some(uri),
            format!(
                "publishDiagnostics alias path exceeds the {MAX_DIAGNOSTICS_ALIAS_PATH_COMPONENTS}-component safety limit"
            ),
        ));
    }
    let canonical = file_uri(lexically_normalized_diagnostics_path(&path));
    if canonical.len() > MAX_DOCUMENT_URI_BYTES {
        return Err((
            None,
            format!(
                "normalized publishDiagnostics uri exceeds the {MAX_DOCUMENT_URI_BYTES}-byte safety limit"
            ),
        ));
    }
    if let Some((canonical_uri, _)) = protocol_documents.get_key_value(&canonical) {
        Ok(canonical_uri.clone())
    } else if canonical == uri {
        Ok(uri)
    } else {
        Ok(canonical)
    }
}

/// Normalize only URI spelling aliases. This deliberately performs no
/// filesystem access: the protocol thread must remain stoppable even if a
/// server publishes a path beneath a stalled mount.
fn lexically_normalized_diagnostics_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn reject_diagnostics(events: &mut EventSink, uri: Option<String>, reason: impl Into<String>) {
    events.emit(LspEvent::DiagnosticsRejected {
        uri,
        reason: reason.into(),
    });
}

fn parse_rpc_error(value: JsonValue) -> Result<JsonRpcError, String> {
    let JsonValue::Object(mut error) = value else {
        return Err("JSON-RPC error must be an object".to_owned());
    };
    let code = error
        .remove("code")
        .and_then(|code| code.as_i64())
        .ok_or_else(|| "JSON-RPC error is missing an integer code".to_owned())?;
    let message = error
        .remove("message")
        .and_then(|message| match message {
            JsonValue::String(message) => Some(message),
            _ => None,
        })
        .ok_or_else(|| "JSON-RPC error is missing a message".to_owned())?;
    Ok(JsonRpcError {
        code,
        message,
        data: error.remove("data"),
    })
}

fn workspace_symbol_provider_supported(capabilities: &JsonValue) -> bool {
    match capabilities.get("workspaceSymbolProvider") {
        Some(JsonValue::Bool(supported)) => *supported,
        Some(JsonValue::Object(_)) => true,
        _ => false,
    }
}

fn parse_text_document_sync_capability(capabilities: &JsonValue) -> TextDocumentSyncCapability {
    let Some(advertised) = capabilities.get("textDocumentSync") else {
        return TextDocumentSyncCapability::default();
    };

    match advertised {
        // The legacy numeric form only communicates the change kind. Full and
        // incremental imply the matching open/change/close synchronization
        // lifecycle; it predates save negotiation, so save remains disabled.
        JsonValue::Number(_) => match advertised.as_u64() {
            Some(1) => TextDocumentSyncCapability {
                open_close: true,
                full: true,
                ..TextDocumentSyncCapability::default()
            },
            Some(2) => TextDocumentSyncCapability {
                open_close: true,
                incremental: true,
                ..TextDocumentSyncCapability::default()
            },
            _ => TextDocumentSyncCapability::default(),
        },
        JsonValue::Object(options) => {
            let open_close = matches!(options.get("openClose"), Some(JsonValue::Bool(true)));
            let full = options.get("change").and_then(JsonValue::as_u64) == Some(1);
            let incremental = options.get("change").and_then(JsonValue::as_u64) == Some(2);
            let (save, save_include_text) = match options.get("save") {
                Some(JsonValue::Bool(true)) => (true, false),
                Some(JsonValue::Object(save_options)) => (
                    true,
                    matches!(save_options.get("includeText"), Some(JsonValue::Bool(true))),
                ),
                _ => (false, false),
            };
            TextDocumentSyncCapability {
                open_close,
                full,
                incremental,
                save,
                save_include_text,
            }
        }
        _ => TextDocumentSyncCapability::default(),
    }
}

fn initialize_request(config: &LspServerConfig) -> JsonValue {
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        "textDocument".to_owned(),
        object([
            (
                "synchronization",
                object([
                    ("dynamicRegistration", false.into()),
                    ("willSave", false.into()),
                    ("willSaveWaitUntil", false.into()),
                    ("didSave", true.into()),
                ]),
            ),
            (
                "publishDiagnostics",
                object([("versionSupport", true.into())]),
            ),
            ("completion", object([])),
            ("hover", object([])),
            ("definition", object([])),
            ("references", object([])),
            (
                "documentSymbol",
                object([("hierarchicalDocumentSymbolSupport", true.into())]),
            ),
            ("formatting", object([])),
        ]),
    );
    capabilities.insert(
        "workspace".to_owned(),
        object([(
            "symbol",
            object([
                ("dynamicRegistration", false.into()),
                (
                    "symbolKind",
                    object([(
                        "valueSet",
                        JsonValue::Array((1_u64..=26).map(JsonValue::from).collect()),
                    )]),
                ),
            ]),
        )]),
    );
    let mut params = BTreeMap::new();
    params.insert(
        "processId".to_owned(),
        JsonValue::from(u64::from(std::process::id())),
    );
    params.insert(
        "clientInfo".to_owned(),
        object([
            ("name", "wscrpt".into()),
            ("version", env!("CARGO_PKG_VERSION").into()),
        ]),
    );
    params.insert(
        "rootUri".to_owned(),
        config
            .root_uri
            .clone()
            .map(JsonValue::String)
            .unwrap_or(JsonValue::Null),
    );
    params.insert("capabilities".to_owned(), JsonValue::Object(capabilities));
    params.insert("trace".to_owned(), "off".into());
    if let Some(options) = &config.initialization_options {
        params.insert("initializationOptions".to_owned(), options.clone());
    }
    request(1, "initialize", JsonValue::Object(params))
}

fn request(id: RequestId, method: &str, params: JsonValue) -> JsonValue {
    object([
        ("jsonrpc", "2.0".into()),
        ("id", id.into()),
        ("method", method.into()),
        ("params", params),
    ])
}

fn notification(method: &str, params: JsonValue) -> JsonValue {
    object([
        ("jsonrpc", "2.0".into()),
        ("method", method.into()),
        ("params", params),
    ])
}

fn text_position_params(position: LspPosition) -> JsonValue {
    object([("position", position_json(position))])
}

fn position_json(position: LspPosition) -> JsonValue {
    object([
        ("line", (position.line.get() as u64).into()),
        ("character", (position.character.get() as u64).into()),
    ])
}

fn validate_version_range(version: DocumentVersion) -> Result<(), LspClientError> {
    if version.get() > i32::MAX as u64 {
        Err(LspClientError::VersionOutOfRange(version))
    } else {
        Ok(())
    }
}

fn validate_position_range(position: LspPosition) -> Result<(), LspClientError> {
    if position.line.get() > i32::MAX as usize || position.character.get() > i32::MAX as usize {
        Err(LspClientError::PositionOutOfRange(position))
    } else {
        Ok(())
    }
}

fn write_json(
    writer: &mut impl Write,
    message: &JsonValue,
    max_message_bytes: usize,
) -> io::Result<()> {
    let mut length = BoundedJsonLength::new(max_message_bytes);
    write_json_value_to(&mut length, message).map_err(|()| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "outbound LSP message exceeds the {max_message_bytes}-byte or JSON-depth limit"
            ),
        )
    })?;
    let mut json = String::with_capacity(length.bytes);
    write_json_value_to(&mut json, message)
        .expect("String JSON output is infallible after bounded length preflight");
    debug_assert_eq!(json.len(), length.bytes);
    write_frame(&mut *writer, &json)?;
    writer.flush()
}

fn spawn_stdout_reader(
    mut stdout: impl Read + Send + 'static,
    input: BudgetedSender<ProtocolInput>,
    limits: LspClientLimits,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("wscrpt-lsp-stdout".to_owned())
        .spawn(move || {
            let mut decoder =
                FrameDecoder::with_limits(limits.max_header_bytes, limits.max_message_bytes);
            let mut buffer = vec![0_u8; limits.read_chunk_bytes];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        let event = if decoder.pending_bytes() == 0 {
                            ProtocolInput::ServerClosed
                        } else {
                            ProtocolInput::TransportError(format!(
                                "language-server stdout ended with {} bytes of a partial frame",
                                decoder.pending_bytes()
                            ))
                        };
                        let _ = input.try_send_control(event);
                        break;
                    }
                    Ok(read) => match decoder.push(&buffer[..read]) {
                        Ok(messages) => {
                            for message in messages {
                                if !try_send_reader_input(
                                    &input,
                                    ProtocolInput::ServerMessage(message),
                                    "stdout",
                                ) {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = input
                                .try_send_control(ProtocolInput::TransportError(error.to_string()));
                            break;
                        }
                    },
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = input.try_send_control(ProtocolInput::TransportError(format!(
                            "language-server stdout read failed: {error}"
                        )));
                        break;
                    }
                }
            }
        })
}

fn spawn_stderr_reader(
    mut stderr: impl Read + Send + 'static,
    input: BudgetedSender<ProtocolInput>,
    limits: LspClientLimits,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("wscrpt-lsp-stderr".to_owned())
        .spawn(move || {
            let mut retained = 0_usize;
            let mut truncation_sent = false;
            let mut buffer = vec![0_u8; limits.read_chunk_bytes];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let available = limits.max_stderr_bytes.saturating_sub(retained);
                        let keep = available.min(read);
                        if keep != 0 {
                            retained += keep;
                            if !try_send_reader_input(
                                &input,
                                ProtocolInput::Stderr(buffer[..keep].to_vec()),
                                "stderr",
                            ) {
                                break;
                            }
                        }
                        if keep < read && !truncation_sent {
                            truncation_sent = true;
                            if input
                                .try_send_control(ProtocolInput::StderrTruncated {
                                    limit: limits.max_stderr_bytes,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = input.try_send_control(ProtocolInput::TransportError(format!(
                            "language-server stderr read failed: {error}"
                        )));
                        break;
                    }
                }
            }
        })
}

fn try_send_reader_input(
    input: &BudgetedSender<ProtocolInput>,
    item: ProtocolInput,
    stream: &str,
) -> bool {
    match input.try_send(item) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            // Do not block forever on one frame/chunk that cannot fit the
            // shared count/byte budget. A reserved, bounded terminal signal
            // makes the loss visible and stops the protocol loop safely.
            let _ = input.try_send_control(ProtocolInput::TransportError(format!(
                "language-server {stream} exceeded the shared input queue limit"
            )));
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn abort_spawned_child(child: &Mutex<Child>) {
    let mut child = lock_unpoisoned(child);
    let _ = force_terminate_process(&mut child);
    let _ = child.wait();
}

fn wait_for_reader_threads(
    stdout: Option<&JoinHandle<()>>,
    stderr: Option<&JoinHandle<()>>,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        let finished = stdout.is_none_or(JoinHandle::is_finished)
            && stderr.is_none_or(JoinHandle::is_finished);
        if finished {
            return true;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return false;
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    }
}

fn join_if_finished(thread: JoinHandle<()>) {
    if thread.is_finished() {
        let _ = thread.join();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_timeout_unpoisoned<'a, T>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> (MutexGuard<'a, T>, bool) {
    match condvar.wait_timeout(guard, timeout) {
        Ok((guard, result)) => (guard, result.timed_out()),
        Err(poisoned) => {
            let (guard, result) = poisoned.into_inner();
            (guard, result.timed_out())
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn force_terminate_process(child: &mut Child) -> io::Result<()> {
    force_terminate_process_group(child.id())
}

#[cfg(not(unix))]
fn force_terminate_process(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
const SIGKILL: std::ffi::c_int = 9;
#[cfg(unix)]
fn force_terminate_process_group(pid: u32) -> io::Result<()> {
    signal_process_group(pid, SIGKILL)
}

#[cfg(not(unix))]
fn force_terminate_process_group(_pid: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: std::ffi::c_int) -> io::Result<()> {
    let pid = std::ffi::c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child PID exceeds c_int"))?;
    // SAFETY: `kill` receives a valid signal and the negative PID of the fresh
    // process-group leader created immediately before exec.
    let result = unsafe { unix_kill(-pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn unix_kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
}

fn os_contains_nul(value: &OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

/// Convert a local path to an absolute-style `file://` URI without a URL crate.
pub fn file_uri(path: impl AsRef<Path>) -> String {
    let normalized = file_uri_path_bytes(path.as_ref());
    let mut uri = String::from("file://");
    for byte in normalized {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            uri.push(char::from(byte));
        } else {
            use fmt::Write as _;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

/// Convert a local file identity to its stable LSP URI. Existing aliases and
/// dot segments are canonicalized; for a not-yet-created file, the nearest
/// existing ancestor is canonicalized before the missing suffix is appended.
pub fn file_uri_identity(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    file_uri(normalized_file_path(&absolute))
}

#[cfg(unix)]
fn file_uri_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = path.as_os_str().as_bytes();
    let mut normalized = Vec::with_capacity(path.len().saturating_add(1));
    if !path.starts_with(b"/") {
        normalized.push(b'/');
    }
    normalized.extend_from_slice(path);
    normalized
}

#[cfg(not(unix))]
fn file_uri_path_bytes(path: &Path) -> Vec<u8> {
    let mut normalized = path.to_string_lossy().replace('\\', "/");
    if !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    normalized.into_bytes()
}

/// A validated JSON number retained in its lossless source spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonNumber(String);

impl JsonNumber {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Minimal lossless JSON value used at the LSP boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

trait JsonOutput {
    fn push_str(&mut self, value: &str) -> Result<(), ()>;
    fn push_char(&mut self, value: char) -> Result<(), ()>;
}

impl JsonOutput for String {
    fn push_str(&mut self, value: &str) -> Result<(), ()> {
        String::push_str(self, value);
        Ok(())
    }

    fn push_char(&mut self, value: char) -> Result<(), ()> {
        String::push(self, value);
        Ok(())
    }
}

struct BoundedJsonLength {
    bytes: usize,
    max: usize,
}

impl BoundedJsonLength {
    const fn new(max: usize) -> Self {
        Self { bytes: 0, max }
    }

    fn add(&mut self, bytes: usize) -> Result<(), ()> {
        let next = self.bytes.checked_add(bytes).ok_or(())?;
        if next > self.max {
            return Err(());
        }
        self.bytes = next;
        Ok(())
    }
}

impl JsonOutput for BoundedJsonLength {
    fn push_str(&mut self, value: &str) -> Result<(), ()> {
        self.add(value.len())
    }

    fn push_char(&mut self, value: char) -> Result<(), ()> {
        self.add(value.len_utf8())
    }
}

impl JsonValue {
    pub fn parse(source: &str) -> Result<Self, JsonError> {
        JsonParser::new(source).parse()
    }

    pub fn to_json_string(&self) -> String {
        let mut output = String::new();
        self.write_json(&mut output);
        output
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => value.0.parse().ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(value) => value.0.parse().ok(),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(object) => object.get(key),
            _ => None,
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        matches!(self, Self::Object(object) if object.contains_key(key))
    }

    fn write_json(&self, output: &mut String) {
        write_json_value_to(output, self).expect("String JSON output is infallible");
    }
}

fn write_json_value_to(output: &mut impl JsonOutput, value: &JsonValue) -> Result<(), ()> {
    write_json_value_to_depth(output, value, 0)
}

fn write_json_value_to_depth(
    output: &mut impl JsonOutput,
    value: &JsonValue,
    depth: usize,
) -> Result<(), ()> {
    if depth > MAX_JSON_DEPTH {
        return Err(());
    }
    match value {
        JsonValue::Null => output.push_str("null")?,
        JsonValue::Bool(true) => output.push_str("true")?,
        JsonValue::Bool(false) => output.push_str("false")?,
        JsonValue::Number(number) => output.push_str(&number.0)?,
        JsonValue::String(value) => write_json_string_to(output, value)?,
        JsonValue::Array(values) => {
            output.push_char('[')?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push_char(',')?;
                }
                write_json_value_to_depth(output, value, depth + 1)?;
            }
            output.push_char(']')?;
        }
        JsonValue::Object(object) => {
            output.push_char('{')?;
            for (index, (key, value)) in object.iter().enumerate() {
                if index != 0 {
                    output.push_char(',')?;
                }
                write_json_string_to(output, key)?;
                output.push_char(':')?;
                write_json_value_to_depth(output, value, depth + 1)?;
            }
            output.push_char('}')?;
        }
    }
    Ok(())
}

impl From<&str> for JsonValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<String> for JsonValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<bool> for JsonValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<u64> for JsonValue {
    fn from(value: u64) -> Self {
        Self::Number(JsonNumber(value.to_string()))
    }
}

impl From<i64> for JsonValue {
    fn from(value: i64) -> Self {
        Self::Number(JsonNumber(value.to_string()))
    }
}

fn object<const N: usize>(entries: [(&str, JsonValue); N]) -> JsonValue {
    JsonValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn write_json_string_to(output: &mut impl JsonOutput, value: &str) -> Result<(), ()> {
    output.push_char('"')?;
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\"")?,
            '\\' => output.push_str("\\\\")?,
            '\u{08}' => output.push_str("\\b")?,
            '\u{0c}' => output.push_str("\\f")?,
            '\n' => output.push_str("\\n")?,
            '\r' => output.push_str("\\r")?,
            '\t' => output.push_str("\\t")?,
            character if character <= '\u{1f}' => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let value = u32::from(character) as usize;
                output.push_str("\\u00")?;
                output.push_char(char::from(HEX[value >> 4]))?;
                output.push_char(char::from(HEX[value & 0x0f]))?;
            }
            character => output.push_char(character)?,
        }
    }
    output.push_char('"')?;
    Ok(())
}

/// Strict JSON syntax failure with a UTF-8 byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid JSON at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl Error for JsonError {}

struct JsonParser<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> JsonParser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    fn parse(mut self) -> Result<JsonValue, JsonError> {
        self.skip_whitespace();
        let value = self.parse_value(0)?;
        self.skip_whitespace();
        if self.offset != self.source.len() {
            return self.error("trailing characters after JSON value");
        }
        Ok(value)
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth > MAX_JSON_DEPTH {
            return self.error("JSON nesting limit exceeded");
        }
        match self.peek_byte() {
            Some(b'n') => {
                self.expect_literal("null")?;
                Ok(JsonValue::Null)
            }
            Some(b't') => {
                self.expect_literal("true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.expect_literal("false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(JsonValue::Number),
            Some(_) => self.error("expected a JSON value"),
            None => self.error("unexpected end of input"),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.offset += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume_byte(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value(depth)?);
            self.skip_whitespace();
            if self.consume_byte(b']') {
                break;
            }
            self.expect_byte(b',', "expected ',' or ']' in array")?;
            self.skip_whitespace();
        }
        Ok(JsonValue::Array(values))
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.offset += 1;
        self.skip_whitespace();
        let mut object = BTreeMap::new();
        if self.consume_byte(b'}') {
            return Ok(JsonValue::Object(object));
        }
        loop {
            if self.peek_byte() != Some(b'"') {
                return self.error("object keys must be strings");
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect_byte(b':', "expected ':' after object key")?;
            self.skip_whitespace();
            let value = self.parse_value(depth)?;
            if object.insert(key.clone(), value).is_some() {
                return self.error(&format!("duplicate object key {key:?}"));
            }
            self.skip_whitespace();
            if self.consume_byte(b'}') {
                break;
            }
            self.expect_byte(b',', "expected ',' or '}' in object")?;
            self.skip_whitespace();
        }
        Ok(JsonValue::Object(object))
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.offset += 1;
        let mut output = String::new();
        loop {
            let Some(byte) = self.peek_byte() else {
                return self.error("unterminated string");
            };
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.offset += 1;
                    self.parse_escape(&mut output)?;
                }
                0..=31 => return self.error("unescaped control character in string"),
                32..=127 => {
                    output.push(char::from(byte));
                    self.offset += 1;
                }
                _ => {
                    let character = self.source[self.offset..]
                        .chars()
                        .next()
                        .expect("offset remains on UTF-8 boundary");
                    output.push(character);
                    self.offset += character.len_utf8();
                }
            }
        }
    }

    fn parse_escape(&mut self, output: &mut String) -> Result<(), JsonError> {
        let Some(escape) = self.peek_byte() else {
            return self.error("unterminated string escape");
        };
        self.offset += 1;
        match escape {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{08}'),
            b'f' => output.push('\u{0c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let first = self.parse_hex_quad()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if !self.source[self.offset..].starts_with("\\u") {
                        return self.error("high surrogate is not followed by a low surrogate");
                    }
                    self.offset += 2;
                    let second = self.parse_hex_quad()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return self.error("invalid low surrogate");
                    }
                    0x1_0000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return self.error("unpaired low surrogate");
                } else {
                    first
                };
                output.push(char::from_u32(scalar).expect("validated JSON Unicode scalar"));
            }
            _ => return self.error("invalid string escape"),
        }
        Ok(())
    }

    fn parse_hex_quad(&mut self) -> Result<u32, JsonError> {
        let end = self.offset.saturating_add(4);
        let Some(hex) = self.source.get(self.offset..end) else {
            return self.error("short Unicode escape");
        };
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return self.error("invalid Unicode escape");
        }
        self.offset = end;
        Ok(u32::from_str_radix(hex, 16).expect("four validated hex digits"))
    }

    fn parse_number(&mut self) -> Result<JsonNumber, JsonError> {
        let start = self.offset;
        self.consume_byte(b'-');
        match self.peek_byte() {
            Some(b'0') => {
                self.offset += 1;
                if self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                    return self.error("leading zero in number");
                }
            }
            Some(b'1'..=b'9') => {
                while self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.offset += 1;
                }
            }
            _ => return self.error("invalid number"),
        }
        if self.consume_byte(b'.') {
            let fraction_start = self.offset;
            while self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.offset += 1;
            }
            if self.offset == fraction_start {
                return self.error("number fraction has no digits");
            }
        }
        if self.consume_byte(b'e') || self.consume_byte(b'E') {
            let _ = self.consume_byte(b'+') || self.consume_byte(b'-');
            let exponent_start = self.offset;
            while self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.offset += 1;
            }
            if self.offset == exponent_start {
                return self.error("number exponent has no digits");
            }
        }
        Ok(JsonNumber(self.source[start..self.offset].to_owned()))
    }

    fn expect_literal(&mut self, literal: &str) -> Result<(), JsonError> {
        if self.source[self.offset..].starts_with(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            self.error(&format!("expected {literal}"))
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek_byte(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.offset += 1;
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.source.as_bytes().get(self.offset).copied()
    }

    fn consume_byte(&mut self, wanted: u8) -> bool {
        if self.peek_byte() == Some(wanted) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn expect_byte(&mut self, wanted: u8, message: &str) -> Result<(), JsonError> {
        if self.consume_byte(wanted) {
            Ok(())
        } else {
            self.error(message)
        }
    }

    fn error<T>(&self, message: &str) -> Result<T, JsonError> {
        Err(JsonError {
            offset: self.offset,
            message: message.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{Line, Utf16Offset};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn position(line: usize, character: usize) -> LspPosition {
        LspPosition::new(Line::new(line), Utf16Offset::new(character))
    }

    #[test]
    fn queue_byte_limits_have_bounded_normalized_defaults() {
        let defaults = LspClientLimits::default();
        assert_eq!(
            defaults.max_queued_input_bytes,
            DEFAULT_MAX_QUEUED_INPUT_BYTES
        );
        assert_eq!(
            defaults.max_queued_event_bytes,
            DEFAULT_MAX_QUEUED_EVENT_BYTES
        );
        assert_eq!(defaults.max_open_documents, DEFAULT_MAX_OPEN_DOCUMENTS);
        assert_eq!(
            defaults.max_document_uri_bytes,
            DEFAULT_MAX_DOCUMENT_URI_BYTES
        );

        let low = LspClientLimits {
            max_queued_input_bytes: 0,
            max_queued_event_bytes: 1,
            max_open_documents: 0,
            max_document_uri_bytes: 0,
            ..defaults
        }
        .normalized();
        assert_eq!(low.max_queued_input_bytes, 1_024);
        assert_eq!(low.max_queued_event_bytes, 1_024);
        assert_eq!(low.max_open_documents, 1);
        assert_eq!(low.max_document_uri_bytes, 64);

        let high = LspClientLimits {
            max_queued_input_bytes: usize::MAX,
            max_queued_event_bytes: usize::MAX,
            max_open_documents: usize::MAX,
            max_document_uri_bytes: usize::MAX,
            ..defaults
        }
        .normalized();
        assert_eq!(high.max_queued_input_bytes, MAX_QUEUE_BYTES);
        assert_eq!(high.max_queued_event_bytes, MAX_QUEUE_BYTES);
        assert_eq!(high.max_open_documents, MAX_OPEN_DOCUMENTS);
        assert_eq!(high.max_document_uri_bytes, MAX_DOCUMENT_URI_BYTES);
    }

    #[test]
    fn input_byte_budget_fills_before_count_and_releases_on_receive() {
        let make_message = || ProtocolInput::ServerMessage("x".repeat(512));
        let weight = make_message().queue_weight(MAX_QUEUE_BYTES);
        let (sender, receiver) = budgeted_channel(8, weight * 2, 0);

        sender.try_send(make_message()).unwrap();
        sender.try_send(make_message()).unwrap();
        assert!(matches!(
            sender.try_send(make_message()),
            Err(TrySendError::Full(_))
        ));

        assert!(matches!(
            receiver.recv().unwrap(),
            ProtocolInput::ServerMessage(message) if message.len() == 512
        ));
        sender.try_send(make_message()).unwrap();
    }

    #[test]
    fn event_byte_budget_fills_before_count_and_releases_on_receive() {
        let make_event = || LspEvent::ServerNotification {
            method: "window/logMessage".to_owned(),
            params: Some(object([("message", "x".repeat(512).into())])),
        };
        let weight = make_event().queue_weight(MAX_QUEUE_BYTES);
        let (sender, receiver) = budgeted_channel(8, weight * 2, 0);

        sender.try_send(make_event()).unwrap();
        sender.try_send(make_event()).unwrap();
        assert!(matches!(
            sender.try_send(make_event()),
            Err(TrySendError::Full(_))
        ));

        assert!(matches!(
            receiver.recv().unwrap(),
            LspEvent::ServerNotification { method, .. } if method == "window/logMessage"
        ));
        sender.try_send(make_event()).unwrap();
    }

    #[test]
    fn full_event_queue_reports_first_loss_without_a_later_event() {
        let filler = LspEvent::TransportError("ordinary queue filler".to_owned());
        let filler_weight = filler.queue_weight(MAX_QUEUE_BYTES);
        let (sender, receiver) = budgeted_channel(1, filler_weight, EVENT_CONTROL_RESERVE);
        sender.try_send(filler).unwrap();

        let mut events = EventSink::new(sender);
        events.emit(LspEvent::ServerClosed);

        assert!(matches!(
            receiver.recv().unwrap(),
            LspEvent::TransportError(message) if message == "ordinary queue filler"
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            LspEvent::EventsDropped { count: 1 }
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn nested_singleton_objects_charge_sparse_btree_nodes_and_hit_byte_budget() {
        let depth = 64_usize;
        let make_event = || LspEvent::WorkspaceSymbols {
            request_id: 9,
            result: (0..depth).fold(JsonValue::Null, |value, _| object([("only", value)])),
        };
        let weight = make_event().queue_weight(MAX_QUEUE_BYTES);
        let sparse_node_floor = depth
            .saturating_mul(btree_queue_object_allocation_estimate(1))
            .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER);
        assert!(weight >= sparse_node_floor);

        let eight_node_budget = 8_usize
            .saturating_mul(btree_queue_object_allocation_estimate(1))
            .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER);
        assert_eq!(
            make_event().queue_weight(eight_node_budget),
            eight_node_budget + 1
        );

        let (sender, _receiver) = budgeted_channel(8, weight * 2, 0);
        sender.try_send(make_event()).unwrap();
        sender.try_send(make_event()).unwrap();
        assert!(matches!(
            sender.try_send(make_event()),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn queue_weights_cover_command_payloads_stderr_and_json_expansion() {
        let baseline = ProtocolInput::Command(ClientCommand::CancelRequest { request_id: 1 })
            .queue_weight(MAX_QUEUE_BYTES);
        let text_bytes = 8 * 1024;
        let document = ProtocolInput::Command(ClientCommand::DidOpen {
            uri: "file:///large.rs".to_owned(),
            language_id: "rust".to_owned(),
            version: DocumentVersion::new(1),
            incarnation: DocumentIncarnation::new(1),
            text: "x".repeat(text_bytes),
        });
        assert!(document.queue_weight(MAX_QUEUE_BYTES) >= baseline + text_bytes);

        let incremental_change = ProtocolInput::Command(ClientCommand::DidChange {
            uri: "file:///large.rs".to_owned(),
            version: DocumentVersion::new(2),
            previous_end: Some(position(0, text_bytes)),
            text: "y".repeat(text_bytes),
        });
        assert!(incremental_change.queue_weight(MAX_QUEUE_BYTES) >= baseline + text_bytes);

        let request = ProtocolInput::Command(ClientCommand::Request {
            request_id: 2,
            operation: LspOperation::WorkspaceSymbols,
            method: "workspace/symbol",
            scope: RequestScope::Workspace,
            params: object([("query", "q".repeat(text_bytes).into())]),
        });
        assert!(request.queue_weight(MAX_QUEUE_BYTES) >= baseline + text_bytes);

        let stderr = ProtocolInput::Stderr(vec![0; text_bytes]);
        assert!(stderr.queue_weight(MAX_QUEUE_BYTES) >= text_bytes);

        let payload = "z".repeat(1_024);
        let payload_capacity = payload.capacity();
        let event = LspEvent::WorkspaceSymbols {
            request_id: 3,
            result: JsonValue::String(payload),
        };
        let expected_json_floor = std::mem::size_of::<JsonValue>()
            .saturating_add(payload_capacity)
            .saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER);
        assert!(
            event.queue_weight(MAX_QUEUE_BYTES)
                >= std::mem::size_of::<LspEvent>() + expected_json_floor
        );
    }

    #[test]
    fn public_event_retained_byte_estimate_matches_queue_accounting_without_consuming() {
        let payload = "z".repeat(4 * 1024);
        let payload_capacity = payload.capacity();
        let event = LspEvent::WorkspaceSymbols {
            request_id: 9,
            result: JsonValue::String(payload),
        };

        let estimated = event.estimated_retained_bytes();
        assert_eq!(estimated, event.queue_weight(MAX_QUEUE_BYTES));
        assert!(estimated <= MAX_QUEUE_BYTES.saturating_add(1));
        assert!(
            estimated
                >= std::mem::size_of::<LspEvent>()
                    + payload_capacity.saturating_mul(JSON_QUEUE_MEMORY_SAFETY_MULTIPLIER)
        );
        assert!(matches!(
            &event,
            LspEvent::WorkspaceSymbols {
                request_id: 9,
                result: JsonValue::String(payload),
            } if payload.len() == 4 * 1024
        ));
    }

    #[test]
    fn outbound_command_preflight_is_exact_at_the_byte_boundary_for_every_shape() {
        let commands = vec![
            ClientCommand::DidOpen {
                uri: "file:///a\"b.rs".to_owned(),
                language_id: "rust".to_owned(),
                version: DocumentVersion::new(1),
                incarnation: DocumentIncarnation::new(1),
                text: "line\n\u{1}\"\\🦀".to_owned(),
            },
            ClientCommand::DidChange {
                uri: "file:///a.rs".to_owned(),
                version: DocumentVersion::new(2),
                previous_end: None,
                text: "changed\r\n".to_owned(),
            },
            ClientCommand::DidChange {
                uri: "file:///incremental.rs".to_owned(),
                version: DocumentVersion::new(3),
                previous_end: Some(position(17, 29)),
                text: "replacement\n\u{1}\"\\🦀".to_owned(),
            },
            ClientCommand::DidSave {
                uri: "file:///a.rs".to_owned(),
                text: Some("saved\ttext".to_owned()),
            },
            ClientCommand::DidSave {
                uri: "file:///a.rs".to_owned(),
                text: None,
            },
            ClientCommand::DidClose {
                uri: "file:///a.rs".to_owned(),
            },
            ClientCommand::Request {
                request_id: u64::MAX,
                operation: LspOperation::WorkspaceSymbols,
                method: "workspace/symbol",
                scope: RequestScope::Workspace,
                params: object([("query", "q\n\"\\🦀".into())]),
            },
            ClientCommand::CancelRequest {
                request_id: u64::MAX,
            },
            ClientCommand::Shutdown {
                request_id: u64::MAX,
            },
        ];

        for command in commands {
            let exact = client_command_json_len(&command, MAX_QUEUE_BYTES).unwrap();
            if let ClientCommand::DidOpen {
                uri,
                language_id,
                version,
                text,
                ..
            } = &command
            {
                assert_eq!(
                    did_open_json_len(uri, language_id, *version, text, MAX_QUEUE_BYTES),
                    Ok(exact)
                );
                assert_eq!(
                    did_open_json_len(uri, language_id, *version, text, exact),
                    Ok(exact)
                );
                assert_eq!(
                    did_open_json_len(uri, language_id, *version, text, exact - 1),
                    Err(())
                );
            }
            assert_eq!(client_command_json_len(&command, exact), Ok(exact));
            assert_eq!(client_command_json_len(&command, exact - 1), Err(()));

            let mut frame = Vec::new();
            write_client_command_json(&mut frame, &command, exact).unwrap();
            let frame = String::from_utf8(frame).unwrap();
            let (header, payload) = frame.split_once("\r\n\r\n").unwrap();
            assert_eq!(header, format!("Content-Length: {exact}"));
            assert_eq!(payload.len(), exact);
            JsonValue::parse(payload).unwrap();

            let mut refused = Vec::new();
            assert_eq!(
                write_client_command_json(&mut refused, &command, exact - 1)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert!(refused.is_empty());
        }
    }

    #[test]
    fn incremental_full_document_replacement_has_exact_range_and_size_preflight() {
        let command = ClientCommand::DidChange {
            uri: "file:///incremental.rs".to_owned(),
            version: DocumentVersion::new(2),
            previous_end: Some(position(7, 11)),
            text: "next\n\u{1}\"\\🦀".to_owned(),
        };
        let expected = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",",
            "\"params\":{\"textDocument\":{\"uri\":\"file:///incremental.rs\",",
            "\"version\":2},\"contentChanges\":[{\"range\":{\"start\":{\"line\":0,",
            "\"character\":0},\"end\":{\"line\":7,\"character\":11}},",
            "\"text\":\"next\\n\\u0001\\\"\\\\🦀\"}]}}"
        );

        let exact = client_command_json_len(&command, MAX_QUEUE_BYTES).unwrap();
        assert_eq!(exact, expected.len());
        assert!(client_command_json_len(&command, exact).is_ok());
        assert!(client_command_json_len(&command, exact - 1).is_err());

        let mut serialized = String::new();
        write_client_command_json_to(&command, &mut serialized).unwrap();
        assert_eq!(serialized, expected);
    }

    #[test]
    fn outbound_preflight_rejects_eight_to_sixteen_mib_and_escaped_expansion() {
        for text_bytes in [8, 12, 16].map(|mib| mib * 1024 * 1024) {
            let command = ClientCommand::DidOpen {
                uri: "file:///large.rs".to_owned(),
                language_id: "rust".to_owned(),
                version: DocumentVersion::new(1),
                incarnation: DocumentIncarnation::new(1),
                text: "x".repeat(text_bytes),
            };
            assert_eq!(
                client_command_json_len(&command, DEFAULT_MAX_MESSAGE_BYTES),
                Err(())
            );
            let exact = client_command_json_len(&command, MAX_QUEUE_BYTES).unwrap();
            assert!(exact > DEFAULT_MAX_MESSAGE_BYTES);
            assert!(exact > text_bytes);
        }

        let source_bytes = 2 * 1024 * 1024;
        let command = ClientCommand::DidSave {
            uri: "file:///escape-heavy.rs".to_owned(),
            text: Some("\u{1}".repeat(source_bytes)),
        };
        assert_eq!(
            client_command_json_len(&command, DEFAULT_MAX_MESSAGE_BYTES),
            Err(())
        );
        let exact = client_command_json_len(&command, MAX_QUEUE_BYTES).unwrap();
        assert!(source_bytes < DEFAULT_MAX_MESSAGE_BYTES);
        assert!(exact > DEFAULT_MAX_MESSAGE_BYTES);

        let too_deep =
            (0..=MAX_JSON_DEPTH).fold(JsonValue::Null, |value, _| JsonValue::Array(vec![value]));
        let command = ClientCommand::Request {
            request_id: 1,
            operation: LspOperation::Formatting,
            method: "textDocument/formatting",
            scope: RequestScope::Workspace,
            params: too_deep,
        };
        assert_eq!(client_command_json_len(&command, MAX_QUEUE_BYTES), Err(()));
    }

    #[test]
    fn diagnostics_payload_is_moved_and_oversize_event_is_reported_dropped() {
        let uri = "file:///large.rs".to_owned();
        let uri_pointer = uri.as_ptr();
        let documents = HashMap::from([(
            uri.clone(),
            OpenDocumentState::new(DocumentVersion::new(1), DocumentIncarnation::new(1)),
        )]);
        let message = "diagnostic".repeat(1_024);
        let message_pointer = message.as_ptr();
        let params = object([
            ("uri", JsonValue::String(uri)),
            ("version", 1_u64.into()),
            (
                "diagnostics",
                JsonValue::Array(vec![object([("message", JsonValue::String(message))])]),
            ),
        ]);
        let (event_tx, event_rx) = budgeted_channel(4, MAX_QUEUE_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        handle_diagnostics(Some(params), &documents, &mut events);
        let moved = event_rx.recv().unwrap();
        let LspEvent::Diagnostics {
            uri, diagnostics, ..
        } = moved
        else {
            panic!("expected diagnostics event");
        };
        assert_eq!(uri.as_ptr(), uri_pointer);
        assert_eq!(
            diagnostics[0]
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap()
                .as_ptr(),
            message_pointer
        );

        let marker_weight = LspEvent::EventsDropped { count: 1 }.queue_weight(MAX_QUEUE_BYTES);
        let (event_tx, event_rx) = budgeted_channel(4, marker_weight, 0);
        let mut events = EventSink::new(event_tx);
        events.emit(LspEvent::Diagnostics {
            uri: "file:///large.rs".to_owned(),
            version: Some(DocumentVersion::new(1)),
            observed_version: Some(DocumentVersion::new(1)),
            observed_incarnation: Some(DocumentIncarnation::new(1)),
            diagnostics: vec![object([("message", "x".repeat(8 * 1024).into())])],
        });
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::EventsDropped { count: 1 }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn diagnostics_normalize_local_uri_aliases_before_document_correlation() {
        let root = TempDir::new();
        let path = root.path().join("diagnostic-target.rs");
        fs::write(&path, "fn target() {}\n").unwrap();
        let canonical_uri = file_uri_identity(&path);
        let canonical_path = canonical_uri.strip_prefix("file://").unwrap();
        let alias_path = canonical_path.strip_suffix("diagnostic-target.rs").unwrap();
        let alias_uri = format!("FiLe://LOCALHOST{alias_path}./diagnostic%2Dtarget.rs");
        let version = DocumentVersion::new(9);
        let incarnation = DocumentIncarnation::new(3);
        let documents = HashMap::from([(
            canonical_uri.clone(),
            OpenDocumentState::new(version, incarnation),
        )]);
        let (event_tx, event_rx) = budgeted_channel(8, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);

        handle_diagnostics(
            Some(object([
                ("uri", alias_uri.clone().into()),
                ("version", version.get().into()),
                ("diagnostics", JsonValue::Array(Vec::new())),
            ])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                uri,
                version: Some(received_version),
                observed_version: Some(observed_version),
                observed_incarnation: Some(observed_incarnation),
                ..
            } if uri == canonical_uri
                && received_version == version
                && observed_version == version
                && observed_incarnation == incarnation
        ));

        handle_diagnostics(
            Some(object([
                ("uri", alias_uri.into()),
                ("diagnostics", JsonValue::Null),
            ])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::DiagnosticsRejected {
                uri: Some(uri),
                reason,
            } if uri == canonical_uri && reason.contains("diagnostics array")
        ));

        for invalid_uri in [
            "file://remote.invalid/tmp/diagnostic-target.rs",
            "file:///tmp/diagnostic-target.rs?query",
            "file:///tmp/diagnostic-target.rs#fragment",
            "file:///tmp/diagnostic%QQtarget.rs",
        ] {
            handle_diagnostics(
                Some(object([
                    ("uri", invalid_uri.into()),
                    ("diagnostics", JsonValue::Array(Vec::new())),
                ])),
                &documents,
                &mut events,
            );
            assert!(matches!(
                event_rx.recv().unwrap(),
                LspEvent::DiagnosticsRejected {
                    uri: Some(uri),
                    reason,
                } if uri == invalid_uri && reason.contains("valid local file URI")
            ));
        }
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn diagnostics_exact_deleted_open_uri_correlates_and_component_bomb_rejects() {
        let root = TempDir::new();
        let path = root.path().join("deleted-but-open.rs");
        fs::write(&path, "fn deleted() {}\n").unwrap();
        let canonical_uri = file_uri_identity(&path);
        fs::remove_file(&path).unwrap();
        let version = DocumentVersion::new(4);
        let incarnation = DocumentIncarnation::new(11);
        let documents = HashMap::from([(
            canonical_uri.clone(),
            OpenDocumentState::new(version, incarnation),
        )]);
        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);

        handle_diagnostics(
            Some(object([
                ("uri", canonical_uri.clone().into()),
                ("version", version.get().into()),
                ("diagnostics", JsonValue::Array(Vec::new())),
            ])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                uri,
                observed_version: Some(observed_version),
                observed_incarnation: Some(observed_incarnation),
                ..
            } if uri == canonical_uri
                && observed_version == version
                && observed_incarnation == incarnation
        ));

        let component_bomb = format!(
            "file:///{}missing.rs",
            "segment/".repeat(MAX_DIAGNOSTICS_ALIAS_PATH_COMPONENTS + 1)
        );
        handle_diagnostics(
            Some(object([
                ("uri", component_bomb.clone().into()),
                ("diagnostics", JsonValue::Array(Vec::new())),
            ])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::DiagnosticsRejected {
                uri: Some(uri),
                reason,
            } if uri == component_bomb && reason.contains("component safety limit")
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn reader_overflow_is_visible_and_control_slots_preserve_shutdown_progress() {
        let (sender, receiver) = budgeted_channel(1, 128, INPUT_CONTROL_RESERVE);
        assert!(!try_send_reader_input(
            &sender,
            ProtocolInput::ServerMessage("x".repeat(8 * 1024)),
            "stdout"
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            ProtocolInput::TransportError(message)
                if message.contains("shared input queue limit")
        ));

        let normal = ProtocolInput::ServerMessage("ok".to_owned());
        let normal_weight = normal.queue_weight(MAX_QUEUE_BYTES);
        let (sender, receiver) = budgeted_channel(1, normal_weight, 2);
        sender.try_send(normal).unwrap();
        sender
            .try_send_control(ProtocolInput::Command(ClientCommand::Shutdown {
                request_id: 7,
            }))
            .unwrap();
        sender.try_send_control(ProtocolInput::ForceStop).unwrap();
        assert!(matches!(
            receiver.recv().unwrap(),
            ProtocolInput::ServerMessage(_)
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            ProtocolInput::Command(ClientCommand::Shutdown { request_id: 7 })
        ));
        assert!(matches!(receiver.recv().unwrap(), ProtocolInput::ForceStop));
    }

    #[cfg(unix)]
    #[test]
    fn file_uri_keeps_distinct_non_utf8_unix_path_identity() {
        use std::os::unix::ffi::OsStringExt as _;

        let first = PathBuf::from(OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', b'n', b'a', b'm', b'e', 0x80,
        ]));
        let second = PathBuf::from(OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', b'n', b'a', b'm', b'e', 0x81,
        ]));
        let first_uri = file_uri(first);
        let second_uri = file_uri(second);
        assert_ne!(first_uri, second_uri);
        assert!(first_uri.ends_with("name%80"));
        assert!(second_uri.ends_with("name%81"));
        assert!(!first_uri.contains('\u{fffd}'));
        assert!(!second_uri.contains('\u{fffd}'));
    }

    #[test]
    fn strict_json_round_trips_unicode_surrogates_and_lossless_numbers() {
        let source = r#"{"emoji":"\ud83d\udc69‍💻","escaped":"line\n\t","number":-12.50e+3,"array":[true,null]}"#;
        let parsed = JsonValue::parse(source).unwrap();
        assert_eq!(parsed.get("emoji").and_then(JsonValue::as_str), Some("👩‍💻"));
        assert_eq!(
            parsed.get("number"),
            Some(&JsonValue::Number(JsonNumber("-12.50e+3".to_owned())))
        );
        assert_eq!(JsonValue::parse(&parsed.to_json_string()).unwrap(), parsed);
    }

    #[test]
    fn strict_json_rejects_duplicates_bad_numbers_surrogates_and_trailing_data() {
        for invalid in [
            r#"{"a":1,"a":2}"#,
            "01",
            "1.",
            r#""\ud800""#,
            r#""\udc00""#,
            "true false",
            "[1,]",
        ] {
            assert!(JsonValue::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn initialize_and_document_messages_are_strict_json_rpc() {
        let root = std::env::temp_dir();
        let config = LspServerConfig::new(["server"], &root);
        let initialize = initialize_request(&config);
        assert_eq!(
            initialize.get("jsonrpc").and_then(JsonValue::as_str),
            Some("2.0")
        );
        assert_eq!(initialize.get("id").and_then(JsonValue::as_u64), Some(1));
        assert_eq!(
            initialize.get("method").and_then(JsonValue::as_str),
            Some("initialize")
        );

        let unicode = notification("textDocument/didChange", object([("text", "a🦀👩‍💻".into())]));
        let encoded = unicode.to_json_string();
        assert_eq!(JsonValue::parse(&encoded).unwrap(), unicode);
        assert!(encoded.len() > encoded.chars().count());
    }

    #[test]
    fn initialize_advertises_bounded_workspace_symbol_support_without_resolve() {
        let config = LspServerConfig::new(["server"], std::env::temp_dir());
        let initialize = initialize_request(&config);
        let symbol = initialize
            .get("params")
            .and_then(|params| params.get("capabilities"))
            .and_then(|capabilities| capabilities.get("workspace"))
            .and_then(|workspace| workspace.get("symbol"))
            .expect("workspace symbol client capability");
        assert_eq!(
            symbol.get("dynamicRegistration"),
            Some(&JsonValue::Bool(false))
        );
        let Some(JsonValue::Array(kinds)) = symbol
            .get("symbolKind")
            .and_then(|symbol_kind| symbol_kind.get("valueSet"))
        else {
            panic!("workspace symbol kinds must be advertised")
        };
        assert_eq!(kinds.len(), 26);
        assert_eq!(kinds.first().and_then(JsonValue::as_u64), Some(1));
        assert_eq!(kinds.last().and_then(JsonValue::as_u64), Some(26));
        assert!(!symbol.contains_key("resolveSupport"));
    }

    #[test]
    fn initialize_does_not_advertise_removed_cross_file_mutation_workflows() {
        let config = LspServerConfig::new(["server"], std::env::temp_dir());
        let initialize = initialize_request(&config);
        let capabilities = initialize
            .get("params")
            .and_then(|params| params.get("capabilities"))
            .expect("client capabilities");
        let text_document = capabilities
            .get("textDocument")
            .expect("text document capabilities");
        let workspace = capabilities
            .get("workspace")
            .expect("workspace capabilities");
        assert!(!text_document.contains_key("rename"));
        assert!(!text_document.contains_key("codeAction"));
        assert!(!workspace.contains_key("workspaceEdit"));
    }

    #[test]
    fn initialize_advertises_static_did_save_support() {
        let config = LspServerConfig::new(["server"], std::env::temp_dir());
        let synchronization = initialize_request(&config)
            .get("params")
            .and_then(|params| params.get("capabilities"))
            .and_then(|capabilities| capabilities.get("textDocument"))
            .and_then(|text_document| text_document.get("synchronization"))
            .cloned()
            .expect("text document synchronization client capability");
        assert_eq!(
            synchronization,
            object([
                ("dynamicRegistration", false.into()),
                ("willSave", false.into()),
                ("willSaveWaitUntil", false.into()),
                ("didSave", true.into()),
            ])
        );
    }

    #[test]
    fn workspace_symbol_capability_accepts_true_or_options_object_only() {
        assert!(workspace_symbol_provider_supported(&object([(
            "workspaceSymbolProvider",
            JsonValue::Bool(true),
        )])));
        assert!(workspace_symbol_provider_supported(&object([(
            "workspaceSymbolProvider",
            object([("workDoneProgress", JsonValue::Bool(true))]),
        )])));
        assert!(!workspace_symbol_provider_supported(&object([(
            "workspaceSymbolProvider",
            JsonValue::Bool(false),
        )])));
        assert!(!workspace_symbol_provider_supported(&object([])));
        assert!(!workspace_symbol_provider_supported(&object([(
            "workspaceSymbolProvider",
            JsonValue::Null,
        )])));
    }

    #[test]
    fn text_document_sync_capability_normalizes_numeric_and_object_forms() {
        let normalized = |advertised| {
            parse_text_document_sync_capability(&object([("textDocumentSync", advertised)]))
        };
        let unsupported = TextDocumentSyncCapability::default();

        assert_eq!(normalized(0_u64.into()), unsupported);
        assert_eq!(
            normalized(1_u64.into()),
            TextDocumentSyncCapability {
                open_close: true,
                full: true,
                incremental: false,
                save: false,
                save_include_text: false,
            }
        );
        assert_eq!(
            normalized(2_u64.into()),
            TextDocumentSyncCapability {
                open_close: true,
                full: false,
                incremental: true,
                save: false,
                save_include_text: false,
            }
        );
        assert_eq!(normalized(3_u64.into()), unsupported);
        assert_eq!(normalized(JsonValue::from(-1_i64)), unsupported);
        assert_eq!(
            normalized(object([
                ("openClose", false.into()),
                ("change", 1_u64.into()),
                ("save", false.into()),
            ])),
            TextDocumentSyncCapability {
                open_close: false,
                full: true,
                incremental: false,
                save: false,
                save_include_text: false,
            }
        );
        assert_eq!(
            normalized(object([
                ("openClose", true.into()),
                ("change", 1_u64.into()),
                ("save", object([("includeText", true.into())])),
            ])),
            TextDocumentSyncCapability {
                open_close: true,
                full: true,
                incremental: false,
                save: true,
                save_include_text: true,
            }
        );
        assert_eq!(
            normalized(object([
                ("openClose", true.into()),
                ("change", 2_u64.into()),
                ("save", true.into()),
            ])),
            TextDocumentSyncCapability {
                open_close: true,
                full: false,
                incremental: true,
                save: true,
                save_include_text: false,
            }
        );
        assert_eq!(
            parse_text_document_sync_capability(&object([])),
            unsupported
        );
        assert_eq!(
            normalized(object([
                ("openClose", "yes".into()),
                ("change", "full".into()),
                ("save", JsonValue::Null),
            ])),
            unsupported
        );
    }

    #[test]
    fn ready_event_stores_workspace_symbol_capability() {
        for (provider, expected) in [
            (Some(JsonValue::Bool(true)), true),
            (Some(object([])), true),
            (Some(JsonValue::Bool(false)), false),
            (None, false),
        ] {
            let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
            let mut events = EventSink::new(event_tx);
            let mut capabilities = BTreeMap::new();
            if let Some(provider) = provider {
                capabilities.insert("workspaceSymbolProvider".to_owned(), provider);
            }
            let response = object([
                ("jsonrpc", "2.0".into()),
                ("id", 1_u64.into()),
                (
                    "result",
                    object([("capabilities", JsonValue::Object(capabilities))]),
                ),
            ])
            .to_json_string();
            let mut pending = HashMap::from([(
                1,
                PendingRequest {
                    operation: LspOperation::Initialize,
                    scope: None,
                    cancel_requested: false,
                    sent: true,
                },
            )]);
            let mut output = Vec::new();
            let protocol_documents = HashMap::new();
            let documents = Mutex::new(HashMap::new());
            assert!(handle_server_message(
                &response,
                &mut output,
                &mut pending,
                DocumentStateViews {
                    protocol: &protocol_documents,
                    optimistic: &documents,
                },
                &mut events,
                &Lifecycle::new(),
                4096,
            ));
            assert!(matches!(
                event_rx.recv().unwrap(),
                LspEvent::Ready {
                    workspace_symbols,
                    ..
                } if workspace_symbols == expected
            ));
        }
    }

    #[test]
    fn feature_request_params_use_utf16_positions() {
        let mut params = text_position_params(position(3, 7));
        if let JsonValue::Object(fields) = &mut params {
            fields.insert(
                "textDocument".to_owned(),
                object([("uri", "file:///a".into())]),
            );
        }
        assert_eq!(
            params
                .get("position")
                .and_then(|position| position.get("line"))
                .and_then(JsonValue::as_u64),
            Some(3)
        );
        assert_eq!(
            params
                .get("position")
                .and_then(|position| position.get("character"))
                .and_then(JsonValue::as_u64),
            Some(7)
        );
    }

    #[test]
    fn workspace_symbol_request_serializes_without_a_text_document() {
        let (event_tx, _event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut pending = HashMap::new();
        let mut documents = HashMap::new();
        let mut output = Vec::new();
        assert!(handle_client_command(
            ClientCommand::Request {
                request_id: 7,
                operation: LspOperation::WorkspaceSymbols,
                method: "workspace/symbol",
                scope: RequestScope::Workspace,
                params: object([("query", "needle".into())]),
            },
            &mut output,
            &mut pending,
            &mut documents,
            &mut events,
            4096,
        ));
        let frame = String::from_utf8(output).unwrap();
        let payload = frame.split_once("\r\n\r\n").unwrap().1;
        let request = JsonValue::parse(payload).unwrap();
        assert_eq!(
            request.get("method").and_then(JsonValue::as_str),
            Some("workspace/symbol")
        );
        assert_eq!(
            request
                .get("params")
                .and_then(|params| params.get("query"))
                .and_then(JsonValue::as_str),
            Some("needle")
        );
        assert!(
            !request
                .get("params")
                .is_some_and(|params| params.contains_key("textDocument"))
        );
        assert!(matches!(
            pending.get(&7),
            Some(PendingRequest {
                operation: LspOperation::WorkspaceSymbols,
                scope: Some(RequestScope::Workspace),
                cancel_requested: false,
                sent: true,
            })
        ));
    }

    #[test]
    fn cancellation_serializes_once_and_keeps_success_correlation() {
        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut pending = HashMap::new();
        let mut documents = HashMap::new();
        let mut output = Vec::new();
        assert!(handle_client_command(
            ClientCommand::Request {
                request_id: 7,
                operation: LspOperation::WorkspaceSymbols,
                method: "workspace/symbol",
                scope: RequestScope::Workspace,
                params: object([("query", "needle".into())]),
            },
            &mut output,
            &mut pending,
            &mut documents,
            &mut events,
            4096,
        ));
        let request_bytes = output.len();

        for _ in 0..2 {
            assert!(handle_client_command(
                ClientCommand::CancelRequest { request_id: 7 },
                &mut output,
                &mut pending,
                &mut documents,
                &mut events,
                4096,
            ));
        }
        let cancel_bytes = &output[request_bytes..];
        let cancel_frame = std::str::from_utf8(cancel_bytes).unwrap();
        let cancel_payload = cancel_frame.split_once("\r\n\r\n").unwrap().1;
        let cancel = JsonValue::parse(cancel_payload).unwrap();
        assert_eq!(
            cancel.get("method").and_then(JsonValue::as_str),
            Some("$/cancelRequest")
        );
        assert_eq!(
            cancel
                .get("params")
                .and_then(|params| params.get("id"))
                .and_then(JsonValue::as_u64),
            Some(7)
        );
        assert!(matches!(
            pending.get(&7),
            Some(PendingRequest {
                operation: LspOperation::WorkspaceSymbols,
                scope: Some(RequestScope::Workspace),
                cancel_requested: true,
                sent: true,
            })
        ));

        let current_documents = Mutex::new(HashMap::new());
        let protocol_documents = HashMap::new();
        assert!(handle_server_message(
            r#"{"jsonrpc":"2.0","id":7,"result":[]}"#,
            &mut output,
            &mut pending,
            DocumentStateViews {
                protocol: &protocol_documents,
                optimistic: &current_documents,
            },
            &mut events,
            &Lifecycle::new(),
            4096,
        ));
        assert!(pending.is_empty());
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::WorkspaceSymbols { request_id: 7, .. }
        ));

        let bytes_after_response = output.len();
        assert!(handle_client_command(
            ClientCommand::CancelRequest { request_id: 7 },
            &mut output,
            &mut pending,
            &mut documents,
            &mut events,
            4096,
        ));
        assert_eq!(output.len(), bytes_after_response);
    }

    #[test]
    fn malformed_error_for_known_response_emits_correlated_request_failure() {
        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut pending = HashMap::from([(
            7,
            PendingRequest {
                operation: LspOperation::WorkspaceSymbols,
                scope: Some(RequestScope::Workspace),
                cancel_requested: false,
                sent: true,
            },
        )]);
        let protocol_documents = HashMap::new();
        let current_documents = Mutex::new(HashMap::new());
        let mut output = Vec::new();

        assert!(handle_server_message(
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":"bad","message":false}}"#,
            &mut output,
            &mut pending,
            DocumentStateViews {
                protocol: &protocol_documents,
                optimistic: &current_documents,
            },
            &mut events,
            &Lifecycle::new(),
            4096,
        ));
        assert!(pending.is_empty());
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::RequestFailed {
                request_id: 7,
                operation: LspOperation::WorkspaceSymbols,
                error: JsonRpcError {
                    code: INVALID_SERVER_RESPONSE_CODE,
                    message,
                    data: None,
                },
            } if message.contains("missing an integer code")
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn missing_or_invalid_jsonrpc_for_known_response_emits_correlated_request_failure() {
        for response in [
            r#"{"id":7,"result":[]}"#,
            r#"{"jsonrpc":"1.0","id":7,"result":[]}"#,
            r#"{"id":7,"method":null,"result":[]}"#,
            r#"{"jsonrpc":"1.0","id":7,"method":7,"error":{}}"#,
        ] {
            let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
            let mut events = EventSink::new(event_tx);
            let mut pending = HashMap::from([(
                7,
                PendingRequest {
                    operation: LspOperation::WorkspaceSymbols,
                    scope: Some(RequestScope::Workspace),
                    cancel_requested: false,
                    sent: true,
                },
            )]);
            let protocol_documents = HashMap::new();
            let current_documents = Mutex::new(HashMap::new());
            let mut output = Vec::new();

            assert!(handle_server_message(
                response,
                &mut output,
                &mut pending,
                DocumentStateViews {
                    protocol: &protocol_documents,
                    optimistic: &current_documents,
                },
                &mut events,
                &Lifecycle::new(),
                4096,
            ));
            assert!(pending.is_empty());
            assert!(matches!(
                event_rx.recv().unwrap(),
                LspEvent::RequestFailed {
                    request_id: 7,
                    operation: LspOperation::WorkspaceSymbols,
                    error: JsonRpcError {
                        code: INVALID_SERVER_RESPONSE_CODE,
                        message,
                        data: None,
                    },
                } if message.contains("jsonrpc=\"2.0\"")
            ));
            assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[test]
    fn invalid_jsonrpc_for_unknown_response_remains_an_uncorrelated_protocol_error() {
        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut pending = HashMap::from([(
            8,
            PendingRequest {
                operation: LspOperation::Hover,
                scope: None,
                cancel_requested: false,
                sent: true,
            },
        )]);
        let protocol_documents = HashMap::new();
        let current_documents = Mutex::new(HashMap::new());
        let mut output = Vec::new();

        assert!(handle_server_message(
            r#"{"jsonrpc":"1.0","id":7,"result":null}"#,
            &mut output,
            &mut pending,
            DocumentStateViews {
                protocol: &protocol_documents,
                optimistic: &current_documents,
            },
            &mut events,
            &Lifecycle::new(),
            4096,
        ));
        assert!(pending.contains_key(&8));
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::ProtocolError(message) if message.contains("jsonrpc=\"2.0\"")
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn malformed_scope_for_known_response_emits_correlated_request_failure() {
        let malformed = [
            (
                8,
                PendingRequest {
                    operation: LspOperation::WorkspaceSymbols,
                    scope: Some(RequestScope::Document {
                        uri: "file:///wrong.rs".to_owned(),
                        version: DocumentVersion::new(1),
                        incarnation: DocumentIncarnation::new(1),
                    }),
                    cancel_requested: false,
                    sent: true,
                },
                LspOperation::WorkspaceSymbols,
                "non-workspace request scope",
            ),
            (
                9,
                PendingRequest {
                    operation: LspOperation::Hover,
                    scope: Some(RequestScope::Workspace),
                    cancel_requested: false,
                    sent: true,
                },
                LspOperation::Hover,
                "non-document request scope",
            ),
        ];

        for (request_id, request, expected_operation, expected_message) in malformed {
            let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
            let mut events = EventSink::new(event_tx);
            let mut pending = HashMap::from([(request_id, request)]);
            let protocol_documents = HashMap::new();
            let current_documents = Mutex::new(HashMap::new());
            let mut output = Vec::new();
            let response = format!("{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":null}}");

            assert!(handle_server_message(
                &response,
                &mut output,
                &mut pending,
                DocumentStateViews {
                    protocol: &protocol_documents,
                    optimistic: &current_documents,
                },
                &mut events,
                &Lifecycle::new(),
                4096,
            ));
            assert!(pending.is_empty());
            assert!(matches!(
                event_rx.recv().unwrap(),
                LspEvent::RequestFailed {
                    request_id: received_id,
                    operation,
                    error: JsonRpcError {
                        code: INVALID_SERVER_RESPONSE_CODE,
                        message,
                        data: None,
                    },
                } if received_id == request_id
                    && operation == expected_operation
                    && message.contains(expected_message)
            ));
            assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[test]
    fn pending_requests_are_hard_capped_with_a_reserved_shutdown_slot() {
        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut pending = HashMap::new();
        let mut documents = HashMap::new();
        let mut output = Vec::new();
        for request_id in 1..MAX_PENDING_REQUESTS as u64 {
            assert!(handle_client_command(
                ClientCommand::Request {
                    request_id,
                    operation: LspOperation::WorkspaceSymbols,
                    method: "workspace/symbol",
                    scope: RequestScope::Workspace,
                    params: object([("query", "".into())]),
                },
                &mut output,
                &mut pending,
                &mut documents,
                &mut events,
                4096,
            ));
        }
        assert_eq!(pending.len(), MAX_PENDING_REQUESTS - 1);
        let written_before_rejection = output.len();
        assert!(handle_client_command(
            ClientCommand::Request {
                request_id: 10_000,
                operation: LspOperation::WorkspaceSymbols,
                method: "workspace/symbol",
                scope: RequestScope::Workspace,
                params: object([("query", "rejected".into())]),
            },
            &mut output,
            &mut pending,
            &mut documents,
            &mut events,
            4096,
        ));
        assert_eq!(output.len(), written_before_rejection);
        assert_eq!(pending.len(), MAX_PENDING_REQUESTS - 1);
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::RequestFailed {
                request_id: 10_000,
                operation: LspOperation::WorkspaceSymbols,
                ..
            }
        ));

        assert!(handle_client_command(
            ClientCommand::Shutdown { request_id: 20_000 },
            &mut output,
            &mut pending,
            &mut documents,
            &mut events,
            4096,
        ));
        assert_eq!(pending.len(), MAX_PENDING_REQUESTS);
    }

    #[test]
    fn diagnostics_capture_observed_state_and_emit_typed_malformed_rejections() {
        let (event_tx, event_rx) = budgeted_channel(16, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let incarnation = DocumentIncarnation::new(7);
        let mut documents = HashMap::from([(
            "file:///a".to_owned(),
            OpenDocumentState::new(DocumentVersion::new(2), incarnation),
        )]);

        handle_diagnostics(
            Some(object([
                ("uri", "file:///a".into()),
                ("diagnostics", JsonValue::Array(Vec::new())),
            ])),
            &documents,
            &mut events,
        );
        documents.insert(
            "file:///a".to_owned(),
            OpenDocumentState::new(DocumentVersion::new(3), incarnation),
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                version: None,
                observed_version: Some(observed),
                observed_incarnation: Some(observed_incarnation),
                ..
            } if observed == DocumentVersion::new(2) && observed_incarnation == incarnation
        ));

        let malformed = [
            JsonValue::String("2".to_owned()),
            JsonValue::from(-1_i64),
            JsonValue::from(i32::MAX as u64 + 1),
            JsonValue::Number(JsonNumber("1.5".to_owned())),
            JsonValue::Null,
        ];
        for version in malformed {
            handle_diagnostics(
                Some(object([
                    ("uri", "file:///a".into()),
                    ("version", version),
                    ("diagnostics", JsonValue::Array(Vec::new())),
                ])),
                &documents,
                &mut events,
            );
            assert!(matches!(
                event_rx.recv().unwrap(),
                LspEvent::DiagnosticsRejected {
                    uri: Some(uri),
                    reason,
                } if uri == "file:///a" && reason.contains("publishDiagnostics version")
            ));
            assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
        }

        handle_diagnostics(
            Some(object([("uri", "file:///recoverable".into())])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::DiagnosticsRejected {
                uri: Some(uri),
                reason,
            } if uri == "file:///recoverable" && reason.contains("diagnostics array")
        ));

        handle_diagnostics(Some(JsonValue::Array(Vec::new())), &documents, &mut events);
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::DiagnosticsRejected { uri: None, reason }
                if reason.contains("params must be an object")
        ));

        documents.insert(
            "file:///a".to_owned(),
            OpenDocumentState::new(DocumentVersion::new(i32::MAX as u64), incarnation),
        );
        handle_diagnostics(
            Some(object([
                ("uri", "file:///a".into()),
                ("version", JsonValue::from(i32::MAX as u64)),
                ("diagnostics", JsonValue::Array(Vec::new())),
            ])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                version: Some(version),
                observed_version: Some(observed),
                observed_incarnation: Some(observed_incarnation),
                ..
            } if version == DocumentVersion::new(i32::MAX as u64)
                && observed == version
                && observed_incarnation == incarnation
        ));
    }

    #[test]
    fn versionless_diagnostics_observe_protocol_input_order_not_optimistic_state() {
        let uri = "file:///ordered.rs";
        let version_one = DocumentVersion::new(1);
        let version_two = DocumentVersion::new(2);
        let incarnation = DocumentIncarnation::new(1);
        let publication = |message: &str| {
            notification(
                "textDocument/publishDiagnostics",
                object([
                    ("uri", uri.into()),
                    (
                        "diagnostics",
                        JsonValue::Array(vec![object([("message", message.into())])]),
                    ),
                ]),
            )
            .to_json_string()
        };

        let (input_tx, input_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_INPUT_BYTES, 0);
        let server_tx = input_tx.clone();
        server_tx
            .try_send(ProtocolInput::ServerMessage(publication("old")))
            .unwrap();

        // `LspClient::did_change` updates this caller-visible map before its
        // command can be processed. Reproduce that state after the older
        // publication is already queued.
        let current_documents = Mutex::new(HashMap::from([(
            uri.to_owned(),
            OpenDocumentState::new(version_one, incarnation),
        )]));
        lock_unpoisoned(&current_documents).insert(
            uri.to_owned(),
            OpenDocumentState::new(version_two, incarnation),
        );
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::DidChange {
                uri: uri.to_owned(),
                version: version_two,
                previous_end: None,
                text: "fn changed() {}\n".to_owned(),
            }))
            .unwrap();
        server_tx
            .try_send(ProtocolInput::ServerMessage(publication("current")))
            .unwrap();

        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut output = Vec::new();
        let mut pending = HashMap::new();
        let mut protocol_documents = HashMap::from([(
            uri.to_owned(),
            OpenDocumentState::new(version_one, incarnation),
        )]);
        let lifecycle = Lifecycle::new();

        assert!(handle_protocol_input(
            input_rx.recv().unwrap(),
            &mut output,
            &mut pending,
            &mut protocol_documents,
            &current_documents,
            &mut events,
            &lifecycle,
            DEFAULT_MAX_MESSAGE_BYTES,
        ));
        let LspEvent::Diagnostics {
            version,
            observed_version,
            observed_incarnation,
            diagnostics,
            ..
        } = event_rx.recv().unwrap()
        else {
            panic!("expected the old diagnostics publication")
        };
        assert_eq!(version, None);
        assert_eq!(observed_version, Some(version_one));
        assert_ne!(observed_version, Some(version_two));
        assert_eq!(observed_incarnation, Some(incarnation));
        assert_eq!(
            diagnostics[0].get("message").and_then(JsonValue::as_str),
            Some("old")
        );

        assert!(handle_protocol_input(
            input_rx.recv().unwrap(),
            &mut output,
            &mut pending,
            &mut protocol_documents,
            &current_documents,
            &mut events,
            &lifecycle,
            DEFAULT_MAX_MESSAGE_BYTES,
        ));
        assert_eq!(
            protocol_documents.get(uri),
            Some(&OpenDocumentState::new(version_two, incarnation))
        );
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));

        assert!(handle_protocol_input(
            input_rx.recv().unwrap(),
            &mut output,
            &mut pending,
            &mut protocol_documents,
            &current_documents,
            &mut events,
            &lifecycle,
            DEFAULT_MAX_MESSAGE_BYTES,
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                version: None,
                observed_version: Some(observed),
                observed_incarnation: Some(observed_incarnation),
                diagnostics,
                ..
            } if observed == version_two
                && observed_incarnation == incarnation
                && diagnostics[0].get("message").and_then(JsonValue::as_str)
                    == Some("current")
        ));
    }

    #[test]
    fn queued_diagnostics_keep_the_pre_close_incarnation_after_uri_reopen() {
        let uri = "file:///reopened.rs";
        let version = DocumentVersion::new(1);
        let first_incarnation = DocumentIncarnation::new(1);
        let second_incarnation = DocumentIncarnation::new(2);
        let publication = |message: &str| {
            notification(
                "textDocument/publishDiagnostics",
                object([
                    ("uri", uri.into()),
                    (
                        "diagnostics",
                        JsonValue::Array(vec![object([("message", message.into())])]),
                    ),
                ]),
            )
            .to_json_string()
        };

        let (input_tx, input_rx) = budgeted_channel(8, DEFAULT_MAX_QUEUED_INPUT_BYTES, 0);
        let server_tx = input_tx.clone();
        let current_documents = Mutex::new(HashMap::from([(
            uri.to_owned(),
            OpenDocumentState::new(version, first_incarnation),
        )]));
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::DidOpen {
                uri: uri.to_owned(),
                language_id: "rust".to_owned(),
                version,
                incarnation: first_incarnation,
                text: "fn first() {}\n".to_owned(),
            }))
            .unwrap();

        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut output = Vec::new();
        let mut pending = HashMap::new();
        let mut protocol_documents = HashMap::new();
        let lifecycle = Lifecycle::new();
        assert!(handle_protocol_input(
            input_rx.recv().unwrap(),
            &mut output,
            &mut pending,
            &mut protocol_documents,
            &current_documents,
            &mut events,
            &lifecycle,
            DEFAULT_MAX_MESSAGE_BYTES,
        ));
        assert_eq!(
            protocol_documents.get(uri),
            Some(&OpenDocumentState::new(version, first_incarnation))
        );

        // The old frame reaches the bounded input queue first. Public
        // `didClose`/`didOpen` calls then install the new optimistic identity
        // before the protocol loop has a chance to process that frame.
        server_tx
            .try_send(ProtocolInput::ServerMessage(publication("old")))
            .unwrap();
        lock_unpoisoned(&current_documents).remove(uri);
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::DidClose {
                uri: uri.to_owned(),
            }))
            .unwrap();
        lock_unpoisoned(&current_documents).insert(
            uri.to_owned(),
            OpenDocumentState::new(version, second_incarnation),
        );
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::DidOpen {
                uri: uri.to_owned(),
                language_id: "rust".to_owned(),
                version,
                incarnation: second_incarnation,
                text: "fn second() {}\n".to_owned(),
            }))
            .unwrap();
        server_tx
            .try_send(ProtocolInput::ServerMessage(publication("current")))
            .unwrap();

        for _ in 0..4 {
            assert!(handle_protocol_input(
                input_rx.recv().unwrap(),
                &mut output,
                &mut pending,
                &mut protocol_documents,
                &current_documents,
                &mut events,
                &lifecycle,
                DEFAULT_MAX_MESSAGE_BYTES,
            ));
        }
        assert_eq!(
            protocol_documents.get(uri),
            Some(&OpenDocumentState::new(version, second_incarnation))
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                observed_version: Some(observed_version),
                observed_incarnation: Some(observed_incarnation),
                diagnostics,
                ..
            } if observed_version == version
                && observed_incarnation == first_incarnation
                && observed_incarnation != second_incarnation
                && diagnostics[0].get("message").and_then(JsonValue::as_str) == Some("old")
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::Diagnostics {
                observed_version: Some(observed_version),
                observed_incarnation: Some(observed_incarnation),
                diagnostics,
                ..
            } if observed_version == version
                && observed_incarnation == second_incarnation
                && diagnostics[0].get("message").and_then(JsonValue::as_str) == Some("current")
        ));
    }

    #[test]
    fn document_response_is_stale_after_same_uri_and_version_reopen() {
        let uri = "file:///request-reopen.rs";
        let version = DocumentVersion::new(1);
        let first_incarnation = DocumentIncarnation::new(11);
        let second_incarnation = DocumentIncarnation::new(12);
        let request_id = 77;
        let (input_tx, input_rx) = budgeted_channel(8, DEFAULT_MAX_QUEUED_INPUT_BYTES, 0);
        let server_tx = input_tx.clone();
        let current_documents = Mutex::new(HashMap::from([(
            uri.to_owned(),
            OpenDocumentState::new(version, first_incarnation),
        )]));
        let mut protocol_documents = HashMap::from([(
            uri.to_owned(),
            OpenDocumentState::new(version, first_incarnation),
        )]);
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::Request {
                request_id,
                operation: LspOperation::Hover,
                method: "textDocument/hover",
                scope: RequestScope::Document {
                    uri: uri.to_owned(),
                    version,
                    incarnation: first_incarnation,
                },
                params: object([
                    ("textDocument", object([("uri", uri.into())])),
                    ("position", position_json(position(0, 0))),
                ]),
            }))
            .unwrap();

        let (event_tx, event_rx) = budgeted_channel(4, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let mut output = Vec::new();
        let mut pending = HashMap::new();
        let lifecycle = Lifecycle::new();
        assert!(handle_protocol_input(
            input_rx.recv().unwrap(),
            &mut output,
            &mut pending,
            &mut protocol_documents,
            &current_documents,
            &mut events,
            &lifecycle,
            DEFAULT_MAX_MESSAGE_BYTES,
        ));
        assert!(pending.contains_key(&request_id));

        lock_unpoisoned(&current_documents).remove(uri);
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::DidClose {
                uri: uri.to_owned(),
            }))
            .unwrap();
        lock_unpoisoned(&current_documents).insert(
            uri.to_owned(),
            OpenDocumentState::new(version, second_incarnation),
        );
        input_tx
            .try_send(ProtocolInput::Command(ClientCommand::DidOpen {
                uri: uri.to_owned(),
                language_id: "rust".to_owned(),
                version,
                incarnation: second_incarnation,
                text: "fn reopened() {}\n".to_owned(),
            }))
            .unwrap();
        server_tx
            .try_send(ProtocolInput::ServerMessage(
                object([
                    ("jsonrpc", "2.0".into()),
                    ("id", request_id.into()),
                    ("result", object([("contents", "old hover".into())])),
                ])
                .to_json_string(),
            ))
            .unwrap();

        for _ in 0..3 {
            assert!(handle_protocol_input(
                input_rx.recv().unwrap(),
                &mut output,
                &mut pending,
                &mut protocol_documents,
                &current_documents,
                &mut events,
                &lifecycle,
                DEFAULT_MAX_MESSAGE_BYTES,
            ));
        }
        assert!(pending.is_empty());
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::StaleResponse {
                request_id: stale_id,
                operation: LspOperation::Hover,
                uri: stale_uri,
                requested_version,
                current_version: Some(current_version),
            } if stale_id == request_id
                && stale_uri == uri
                && requested_version == version
                && current_version == version
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn diagnostics_and_responses_are_rejected_after_document_version_advances() {
        let (event_tx, event_rx) = budgeted_channel(16, DEFAULT_MAX_QUEUED_EVENT_BYTES, 0);
        let mut events = EventSink::new(event_tx);
        let documents = HashMap::from([(
            "file:///a".to_owned(),
            OpenDocumentState::new(DocumentVersion::new(2), DocumentIncarnation::new(1)),
        )]);
        let current_documents = Mutex::new(documents.clone());

        handle_diagnostics(
            Some(object([
                ("uri", "file:///a".into()),
                ("version", 1_u64.into()),
                ("diagnostics", JsonValue::Array(Vec::new())),
            ])),
            &documents,
            &mut events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::StaleDiagnostics { .. }
        ));

        let pending = PendingRequest {
            operation: LspOperation::Hover,
            scope: Some(RequestScope::Document {
                uri: "file:///a".to_owned(),
                version: DocumentVersion::new(1),
                incarnation: DocumentIncarnation::new(1),
            }),
            cancel_requested: false,
            sent: true,
        };
        let mut pending_map = HashMap::from([(9, pending)]);
        let mut client_output = Vec::new();
        let response = r#"{"jsonrpc":"2.0","id":9,"result":{"contents":"old"}}"#;
        assert!(handle_server_message(
            response,
            &mut client_output,
            &mut pending_map,
            DocumentStateViews {
                protocol: &documents,
                optimistic: &current_documents,
            },
            &mut events,
            &Lifecycle::new(),
            4096,
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            LspEvent::StaleResponse { request_id: 9, .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn instant_shutdown_responses_never_regress_stopped_state() {
        let root = TempDir::new();
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;

        for _ in 0..16 {
            let config = LspServerConfig::new(["/bin/sh", "-c", script], root.path())
                .with_shutdown_timeout(Duration::from_millis(100));
            let mut client = LspClient::spawn(config).unwrap();
            assert!(matches!(
                next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
                LspEvent::Ready { .. }
            ));
            client.shutdown().unwrap();
            assert!(matches!(
                next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
                LspEvent::ShutdownComplete
            ));
            assert!(client.wait_stopped(Duration::from_secs(1)));
            assert!(client.is_stopped());
        }
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_during_initialize_waits_for_initialized_suppresses_ready_and_stops() {
        let root = TempDir::new();
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
initialized=0
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      sleep 0.05
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
      ;;
    *'"method":"initialized"'*)
      initialized=1
      ;;
    *'"method":"shutdown"'*)
      if [ "$initialized" -ne 1 ]; then
        exit 73
      fi
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let config = LspServerConfig::new(["/bin/sh", "-c", script], root.path())
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();
        client.shutdown().unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_ready = false;
        let mut saw_shutdown_complete = false;
        while Instant::now() < deadline && !saw_shutdown_complete {
            match client.recv_event_timeout(Duration::from_millis(100)) {
                Ok(LspEvent::Ready { .. }) => saw_ready = true,
                Ok(LspEvent::ShutdownComplete) => saw_shutdown_complete = true,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(!saw_ready);
        assert!(saw_shutdown_complete);
        assert!(client.wait_stopped(Duration::from_secs(1)));
        assert!(client.is_stopped());
    }

    #[cfg(unix)]
    #[test]
    fn live_ready_event_normalizes_text_document_sync_options() {
        let root = TempDir::new();
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":{"openClose":false,"change":1,"save":{"includeText":true}}}}}'
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let config = LspServerConfig::new(["/bin/sh", "-c", script], root.path())
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();

        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready {
                text_document_sync: TextDocumentSyncCapability {
                    open_close: false,
                    full: true,
                    incremental: false,
                    save: true,
                    save_include_text: true,
                },
                ..
            }
        ));

        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(1)));
    }

    #[cfg(unix)]
    #[test]
    fn relative_workspace_root_uses_one_canonical_child_cwd_and_initialize_uri() {
        let artifacts = TempDir::new();
        let log_path = artifacts.path().join("relative-root-wire.log");
        let script = r#"
printf 'cwd:%s\n' "$(pwd -P)" > "$1"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
  printf '%s\n' "$body" >> "$1"
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message "$1"; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("wscrpt-relative-root-mock"),
            log_path.as_os_str().to_owned(),
        ];
        let canonical_root = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let expected_root_uri = file_uri(&canonical_root);
        let config = LspServerConfig::new(argv, PathBuf::from("."))
            .with_shutdown_timeout(Duration::from_millis(300));
        assert_eq!(config.workspace_root, canonical_root);
        assert_eq!(config.root_uri.as_deref(), Some(expected_root_uri.as_str()));

        let mut client = LspClient::spawn(config).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready { .. }
        ));
        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(1)));

        let wire = fs::read_to_string(log_path).unwrap();
        let mut lines = wire.lines();
        let child_cwd = lines
            .next()
            .and_then(|line| line.strip_prefix("cwd:"))
            .map(PathBuf::from)
            .expect("child physical cwd was logged");
        {
            use std::os::unix::fs::MetadataExt as _;

            let child_metadata = fs::metadata(child_cwd).unwrap();
            let expected_metadata = fs::metadata(&canonical_root).unwrap();
            assert_eq!(child_metadata.dev(), expected_metadata.dev());
            assert_eq!(child_metadata.ino(), expected_metadata.ino());
        }
        let initialize = lines
            .filter_map(|line| JsonValue::parse(line).ok())
            .find(|message| message.get("method").and_then(JsonValue::as_str) == Some("initialize"))
            .expect("initialize request was logged");
        assert_eq!(
            initialize
                .get("params")
                .and_then(|params| params.get("rootUri"))
                .and_then(JsonValue::as_str),
            Some(expected_root_uri.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_incremental_change_replaces_previous_document_range_and_preserves_state_on_rejection() {
        let root = TempDir::new();
        let log_path = root.path().join("wire.log");
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
  printf '%s\n' "$body" >> "$1"
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message "$1"; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":2}}}'
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("wscrpt-incremental-mock"),
            log_path.as_os_str().to_owned(),
        ];
        let config = LspServerConfig::new(argv, root.path())
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready {
                text_document_sync: TextDocumentSyncCapability {
                    open_close: true,
                    full: false,
                    incremental: true,
                    save: false,
                    save_include_text: false,
                },
                ..
            }
        ));

        let uri = "file:///incremental.rs";
        client
            .did_open(uri, "rust", DocumentVersion::new(1), "old\n🦀")
            .unwrap();
        let invalid_end = position(i32::MAX as usize + 1, 0);
        assert!(matches!(
            client.did_change_full_document_replacement(
                uri,
                DocumentVersion::new(2),
                invalid_end,
                "must not advance",
            ),
            Err(LspClientError::PositionOutOfRange(position)) if position == invalid_end
        ));
        client
            .did_change_full_document_replacement(
                uri,
                DocumentVersion::new(2),
                position(1, 2),
                "new\n🦀",
            )
            .unwrap();
        client.did_close(uri).unwrap();
        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(1)));

        let wire = fs::read_to_string(log_path).unwrap();
        let expected = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",",
            "\"params\":{\"textDocument\":{\"uri\":\"file:///incremental.rs\",",
            "\"version\":2},\"contentChanges\":[{\"range\":{\"start\":{\"line\":0,",
            "\"character\":0},\"end\":{\"line\":1,\"character\":2}},",
            "\"text\":\"new\\n🦀\"}]}}"
        );
        assert!(wire.lines().any(|line| line == expected), "wire: {wire}");
    }

    #[cfg(unix)]
    #[test]
    fn live_open_document_and_uri_bounds_reject_before_maps_and_reuse_closed_slot() {
        let root = TempDir::new();
        let log_path = root.path().join("document-wire.log");
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
  printf '%s\n' "$body" >> "$1"
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message "$1"; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":1}}}'
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("wscrpt-document-bound-mock"),
            log_path.as_os_str().to_owned(),
        ];
        let limits = LspClientLimits {
            max_open_documents: 2,
            max_document_uri_bytes: 64,
            ..LspClientLimits::default()
        };
        let config = LspServerConfig::new(argv, root.path())
            .with_limits(limits)
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready { .. }
        ));

        let oversized_uri = format!("file:///{}", "x".repeat(64));
        assert!(matches!(
            client.did_open(
                oversized_uri,
                "rust",
                DocumentVersion::new(1),
                "oversized",
            ),
            Err(LspClientError::DocumentUriTooLarge { bytes, max })
                if bytes == 72 && max == 64
        ));
        assert!(lock_unpoisoned(&client.document_versions).is_empty());

        let first_uri = "file:///first.rs";
        let second_uri = "file:///second.rs";
        let third_uri = "file:///third.rs";
        let first_incarnation = client
            .did_open(first_uri, "rust", DocumentVersion::new(1), "first")
            .unwrap();
        let second_incarnation = client
            .did_open(second_uri, "rust", DocumentVersion::new(1), "second")
            .unwrap();
        assert_eq!(first_incarnation.get(), 1);
        assert_eq!(second_incarnation.get(), 2);
        assert_eq!(lock_unpoisoned(&client.document_versions).len(), 2);
        assert!(matches!(
            client.validate_did_open(third_uri, "rust", DocumentVersion::new(1), "third"),
            Err(LspClientError::OpenDocumentLimitReached { max: 2 })
        ));
        assert!(matches!(
            client.did_open(third_uri, "rust", DocumentVersion::new(1), "third"),
            Err(LspClientError::OpenDocumentLimitReached { max: 2 })
        ));
        assert!(matches!(
            client.did_open(first_uri, "rust", DocumentVersion::new(1), "duplicate"),
            Err(LspClientError::DocumentAlreadyOpen(uri)) if uri == first_uri
        ));
        assert_eq!(lock_unpoisoned(&client.document_versions).len(), 2);

        client.did_close(first_uri).unwrap();
        assert_eq!(lock_unpoisoned(&client.document_versions).len(), 1);
        let third_incarnation = client
            .did_open(third_uri, "rust", DocumentVersion::new(1), "third")
            .unwrap();
        assert_eq!(third_incarnation.get(), 3);
        {
            let documents = lock_unpoisoned(&client.document_versions);
            assert_eq!(documents.len(), 2);
            assert!(!documents.contains_key(first_uri));
            assert!(documents.contains_key(second_uri));
            assert!(documents.contains_key(third_uri));
        }
        client.did_close(second_uri).unwrap();
        client.did_close(third_uri).unwrap();
        assert!(lock_unpoisoned(&client.document_versions).is_empty());
        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(1)));

        let wire = fs::read_to_string(log_path).unwrap();
        let document_messages = wire
            .lines()
            .filter(|line| {
                line.contains("\"method\":\"textDocument/didOpen\"")
                    || line.contains("\"method\":\"textDocument/didClose\"")
            })
            .collect::<Vec<_>>();
        assert_eq!(document_messages.len(), 6, "wire: {wire}");
        assert!(document_messages[0].contains(first_uri));
        assert!(document_messages[1].contains(second_uri));
        assert!(document_messages[2].contains(first_uri));
        assert!(document_messages[3].contains(third_uri));
        assert!(document_messages[4].contains(second_uri));
        assert!(document_messages[5].contains(third_uri));
    }

    #[cfg(unix)]
    #[test]
    fn live_workspace_symbols_need_no_open_document_and_correlate_errors_and_shutdown() {
        let root = TempDir::new();
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
cancelled_id=
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"workspaceSymbolProvider":{"workDoneProgress":true}}}}'
      ;;
    *'"method":"workspace/symbol"'*'"query":"fail"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32603,\"message\":\"symbol failure\"}}"
      ;;
    *'"method":"workspace/symbol"'*'"query":"cancel"'*)
      cancelled_id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      ;;
    *'"method":"$/cancelRequest"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      if [ "$id" = "$cancelled_id" ]; then
        send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32800,\"message\":\"request cancelled\"}}"
      fi
      ;;
    *'"method":"workspace/symbol"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"Needle\",\"kind\":12,\"location\":{\"uri\":\"file:///workspace/lib.rs\",\"range\":{\"start\":{\"line\":1,\"character\":2},\"end\":{\"line\":1,\"character\":8}}}}]}"
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let config = LspServerConfig::new(["/bin/sh", "-c", script], root.path())
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready {
                workspace_symbols: true,
                ..
            }
        ));

        let uri = "file:///preflight.rs";
        let oversized_message = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES);
        assert!(matches!(
            client.validate_did_open(
                uri,
                "rust",
                DocumentVersion::new(1),
                &oversized_message,
            ),
            Err(LspClientError::MessageTooLarge { max })
                if max == DEFAULT_MAX_MESSAGE_BYTES
        ));
        assert!(matches!(
            client.current_document_state(uri),
            Err(LspClientError::DocumentNotOpen(not_open)) if not_open == uri
        ));
        assert!(matches!(
            client.did_open(
                uri,
                "rust",
                DocumentVersion::new(1),
                oversized_message,
            ),
            Err(LspClientError::MessageTooLarge { max })
                if max == DEFAULT_MAX_MESSAGE_BYTES
        ));
        assert!(matches!(
            client.current_document_state(uri),
            Err(LspClientError::DocumentNotOpen(not_open)) if not_open == uri
        ));
        client
            .validate_did_open(uri, "rust", DocumentVersion::new(1), "fn live() {}\n")
            .unwrap();
        let first_incarnation = client
            .did_open(uri, "rust", DocumentVersion::new(1), "fn live() {}\n")
            .unwrap();
        assert_eq!(first_incarnation.get(), 1);
        assert!(matches!(
            client.validate_did_open(uri, "rust", DocumentVersion::new(1), "fn live() {}\n"),
            Err(LspClientError::DocumentAlreadyOpen(open)) if open == uri
        ));
        assert!(matches!(
            client.did_change(
                uri,
                DocumentVersion::new(2),
                "x".repeat(DEFAULT_MAX_MESSAGE_BYTES),
            ),
            Err(LspClientError::MessageTooLarge { max })
                if max == DEFAULT_MAX_MESSAGE_BYTES
        ));
        client
            .did_change(uri, DocumentVersion::new(2), "fn still_live() {}\n")
            .unwrap();
        assert!(matches!(
            client.did_save(
                uri,
                DocumentVersion::new(2),
                Some("\u{1}".repeat(2 * 1024 * 1024)),
            ),
            Err(LspClientError::MessageTooLarge { max })
                if max == DEFAULT_MAX_MESSAGE_BYTES
        ));
        client.did_save(uri, DocumentVersion::new(2), None).unwrap();
        client.did_close(uri).unwrap();
        let second_incarnation = client
            .did_open(uri, "rust", DocumentVersion::new(1), "fn reopened() {}\n")
            .unwrap();
        assert!(second_incarnation > first_incarnation);
        assert_eq!(second_incarnation.get(), 2);
        client.did_close(uri).unwrap();

        client.next_document_incarnation = Some(DocumentIncarnation::new(u64::MAX));
        let final_incarnation = client
            .did_open(
                "file:///final-incarnation.rs",
                "rust",
                DocumentVersion::new(1),
                "fn final_open() {}\n",
            )
            .unwrap();
        assert_eq!(final_incarnation.get(), u64::MAX);
        client.did_close("file:///final-incarnation.rs").unwrap();
        assert!(matches!(
            client.validate_did_open(
                "file:///exhausted.rs",
                "rust",
                DocumentVersion::new(1),
                "fn exhausted() {}\n",
            ),
            Err(LspClientError::DocumentIncarnationExhausted)
        ));
        assert!(matches!(
            client.request_workspace_symbols("x".repeat(DEFAULT_MAX_MESSAGE_BYTES)),
            Err(LspClientError::MessageTooLarge { max })
                if max == DEFAULT_MAX_MESSAGE_BYTES
        ));

        let success_id = client.request_workspace_symbols("needle").unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::WorkspaceSymbols { .. })),
            LspEvent::WorkspaceSymbols { request_id, result }
                if request_id == success_id
                    && matches!(&result, JsonValue::Array(symbols) if symbols.len() == 1)
        ));

        let failure_id = client.request_workspace_symbols("fail").unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::RequestFailed { .. })),
            LspEvent::RequestFailed {
                request_id,
                operation: LspOperation::WorkspaceSymbols,
                error: JsonRpcError { code: -32603, .. },
            } if request_id == failure_id
        ));

        let cancelled_id = client.request_workspace_symbols("cancel").unwrap();
        client.cancel_request(cancelled_id).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::RequestFailed { .. })),
            LspEvent::RequestFailed {
                request_id,
                operation: LspOperation::WorkspaceSymbols,
                error: JsonRpcError { code: -32800, .. },
            } if request_id == cancelled_id
        ));

        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(2)));
    }

    #[cfg(unix)]
    #[test]
    fn live_diagnostics_alias_correlates_with_canonical_open_document_uri() {
        let root = TempDir::new();
        let path = root.path().join("live-diagnostic-target.rs");
        fs::write(&path, "fn live_target() {}\n").unwrap();
        let canonical_uri = file_uri_identity(&path);
        let canonical_path = canonical_uri.strip_prefix("file://").unwrap();
        let alias_path = canonical_path
            .strip_suffix("live-diagnostic-target.rs")
            .unwrap();
        let alias_uri = format!("fIlE://LOCALHOST{alias_path}./live%2Ddiagnostic%2Dtarget.rs");
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":1}}}'
      ;;
    *'"method":"textDocument/didOpen"'*)
      send_message "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$1\",\"version\":1,\"diagnostics\":[{\"message\":\"alias\"}]}}"
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("wscrpt-diagnostics-alias-mock"),
            OsString::from(alias_uri),
        ];
        let config = LspServerConfig::new(argv, root.path())
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready { .. }
        ));

        let incarnation = client
            .did_open(
                canonical_uri.clone(),
                "rust",
                DocumentVersion::new(1),
                "fn live_target() {}\n",
            )
            .unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Diagnostics { .. })),
            LspEvent::Diagnostics {
                uri,
                version: Some(version),
                observed_version: Some(observed_version),
                observed_incarnation: Some(observed_incarnation),
                diagnostics,
            } if uri == canonical_uri
                && version == DocumentVersion::new(1)
                && observed_version == version
                && observed_incarnation == incarnation
                && diagnostics[0].get("message").and_then(JsonValue::as_str) == Some("alias")
        ));

        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(1)));
    }

    #[cfg(unix)]
    #[test]
    fn live_server_handshake_features_versions_and_shutdown() {
        let root = TempDir::new();
        let script = r#"
read_message() {
  IFS= read -r header || return 1
  len=$(printf '%s' "$header" | tr -cd '0-9')
  IFS= read -r blank || return 1
  body=$(dd bs=1 count="$len" 2>/dev/null) || return 1
}
send_message() {
  payload=$1
  printf 'Content-Length: %s\r\n\r\n%s' "${#payload}" "$payload"
}
while read_message; do
  case "$body" in
    *'"method":"initialize"'*)
      send_message '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"hoverProvider":true}}}'
      ;;
    *'"method":"textDocument/didOpen"'*)
      send_message '{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///demo.rs","version":1,"diagnostics":[{"message":"demo"}]}}'
      ;;
    *'"method":"textDocument/completion"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      sleep 0.08
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"label\":\"item\"}]}"
      ;;
    *'"method":"textDocument/hover"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"contents\":\"hover\"}}"
      ;;
    *'"method":"textDocument/definition"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}"
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      send_message "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) exit 0 ;;
  esac
done
"#;
        let config = LspServerConfig::new(["/bin/sh", "-c", script], root.path())
            .with_shutdown_timeout(Duration::from_millis(300));
        let mut client = LspClient::spawn(config).unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Ready { .. })),
            LspEvent::Ready { .. }
        ));

        let uri = "file:///demo.rs";
        client
            .did_open(uri, "rust", DocumentVersion::new(1), "fn main() {}\n")
            .unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::Diagnostics { .. })),
            LspEvent::Diagnostics { diagnostics, .. } if diagnostics.len() == 1
        ));

        let stale_id = client
            .request_completion(uri, DocumentVersion::new(1), position(0, 2))
            .unwrap();
        client
            .did_change(uri, DocumentVersion::new(2), "fn main() { let x = 1; }\n")
            .unwrap();
        assert!(matches!(
            client.did_change(uri, DocumentVersion::new(1), "old"),
            Err(LspClientError::StaleDocumentVersion { .. })
        ));
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::StaleResponse { .. })),
            LspEvent::StaleResponse { request_id, .. } if request_id == stale_id
        ));

        let completion = client
            .request_completion(uri, DocumentVersion::new(2), position(0, 4))
            .unwrap();
        let hover = client
            .request_hover(uri, DocumentVersion::new(2), position(0, 4))
            .unwrap();
        let definition = client
            .request_definition(uri, DocumentVersion::new(2), position(0, 4))
            .unwrap();
        let mut seen = Vec::new();
        while seen.len() < 3 {
            let event = client.recv_event_timeout(Duration::from_secs(3)).unwrap();
            match event {
                LspEvent::Completion { request_id, .. }
                | LspEvent::Hover { request_id, .. }
                | LspEvent::Definition { request_id, .. } => seen.push(request_id),
                _ => {}
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![completion, hover, definition]);

        client.did_save(uri, DocumentVersion::new(2), None).unwrap();
        client.shutdown().unwrap();
        assert!(matches!(
            next_matching(&client, |event| matches!(event, LspEvent::ShutdownComplete)),
            LspEvent::ShutdownComplete
        ));
        assert!(client.wait_stopped(Duration::from_secs(2)));
    }

    #[cfg(unix)]
    #[test]
    fn overflow_shutdown_timeout_is_finite_and_drop_force_terminates_server() {
        let root = TempDir::new();
        let script =
            "printf 'abcdefghijklmnopqrstuvwxyz' >&2; trap '' TERM; while :; do sleep 1; done";
        let limits = LspClientLimits {
            max_stderr_bytes: 8,
            ..LspClientLimits::default()
        };
        let mut config = LspServerConfig::new(["/bin/sh", "-c", script], root.path())
            .with_limits(limits)
            .with_shutdown_timeout(Duration::MAX);
        assert_eq!(config.shutdown_timeout, MAX_SHUTDOWN_TIMEOUT);
        // Direct field mutation cannot bypass spawn-time normalization.
        config.shutdown_timeout = Duration::MAX;
        let client = LspClient::spawn(config).unwrap();
        assert_eq!(client.shutdown_timeout, MAX_SHUTDOWN_TIMEOUT);
        let mut retained = 0;
        let mut truncated = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !truncated {
            match client.recv_event_timeout(Duration::from_millis(100)) {
                Ok(LspEvent::Stderr(bytes)) => retained += bytes.len(),
                Ok(LspEvent::StderrTruncated { limit }) => {
                    assert_eq!(limit, 8);
                    truncated = true;
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        assert_eq!(retained, 8);
        assert!(truncated);

        // Keep the regression finite even against the old unbounded branch:
        // that implementation wakes at the watchdog and then fails the
        // bounded-result assertions below instead of hanging the test runner.
        let watchdog_lifecycle = Arc::clone(&client.lifecycle);
        let watchdog = thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            watchdog_lifecycle.mark_stopped();
        });
        let wait_started = Instant::now();
        assert!(!client.wait_stopped(Duration::MAX));
        assert!(wait_started.elapsed() < Duration::from_secs(2));

        let started = Instant::now();
        drop(client);
        assert!(started.elapsed() < Duration::from_secs(2));
        watchdog.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn drop_cleans_descendant_after_language_server_leader_exits() {
        let root = TempDir::new();
        let pid_path = root.path().join("descendant.pid");
        let script = format!("sleep 30 & echo $! > '{}'; exit 0", pid_path.display());
        let config = LspServerConfig::new(["/bin/sh", "-c", &script], root.path())
            .with_shutdown_timeout(Duration::from_millis(50));
        let client = LspClient::spawn(config).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let descendant_pid: u32 = fs::read_to_string(&pid_path)
            .expect("language-server helper should publish descendant PID")
            .trim()
            .parse()
            .unwrap();
        assert!(unix_process_exists(descendant_pid));

        let started = Instant::now();
        drop(client);
        assert!(started.elapsed() < Duration::from_secs(2));

        let deadline = Instant::now() + Duration::from_secs(2);
        while unix_process_exists(descendant_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let survived = unix_process_exists(descendant_pid);
        if survived {
            // SAFETY: the PID came from the dedicated test descendant.
            let _ = unsafe { unix_kill(descendant_pid as std::ffi::c_int, SIGKILL) };
        }
        assert!(
            !survived,
            "language-server descendant {descendant_pid} survived client drop"
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_cleans_quiet_descendant_even_after_all_server_pipes_close() {
        let root = TempDir::new();
        let pid_path = root.path().join("quiet-descendant.pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            pid_path.display()
        );
        let config = LspServerConfig::new(["/bin/sh", "-c", &script], root.path())
            .with_shutdown_timeout(Duration::from_millis(50));
        let client = LspClient::spawn(config).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut descendant_pid = None;
        while descendant_pid.is_none() && Instant::now() < deadline {
            descendant_pid = fs::read_to_string(&pid_path)
                .ok()
                .and_then(|contents| contents.trim().parse::<u32>().ok());
            if descendant_pid.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let descendant_pid =
            descendant_pid.expect("language-server helper should publish quiet descendant PID");
        assert!(unix_process_exists(descendant_pid));

        drop(client);

        let deadline = Instant::now() + Duration::from_secs(2);
        while unix_process_exists(descendant_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let survived = unix_process_exists(descendant_pid);
        if survived {
            // SAFETY: the PID came from the dedicated test descendant.
            let _ = unsafe { unix_kill(descendant_pid as std::ffi::c_int, SIGKILL) };
        }
        assert!(
            !survived,
            "quiet language-server descendant {descendant_pid} survived client drop"
        );
    }

    #[cfg(unix)]
    fn unix_process_exists(pid: u32) -> bool {
        let Ok(pid) = std::ffi::c_int::try_from(pid) else {
            return false;
        };
        // SAFETY: signal zero performs only an existence/permission check.
        let result = unsafe { unix_kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() != Some(3)
    }

    fn next_matching(client: &LspClient, wanted: impl Fn(&LspEvent) -> bool) -> LspEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = client.recv_event_timeout(remaining).unwrap();
            if wanted(&event) {
                return event;
            }
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "wscrpt-lsp-client-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
