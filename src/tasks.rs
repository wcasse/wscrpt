//! Workspace task definitions and direct, bounded subprocess execution.
//!
//! Tasks live in `.wscrpt/tasks.toml`.  Version 1 uses named tables and an
//! argument vector rather than a command string:
//!
//! ```toml
//! version = 1
//!
//! [tasks.check]
//! argv = ["cargo", "check", "--all-targets"]
//! cwd = "."
//! env = { CARGO_TERM_COLOR = "never" }
//! ```
//!
//! [`TaskRunner::start`] always requires an explicit [`WorkspaceTrust`] value.
//! An untrusted call is rejected before the executable is looked up.  The
//! runner passes `argv` directly to [`std::process::Command`]; it never inserts
//! a shell or reparses arguments.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;

pub const TASK_FILE_VERSION: u32 = 1;
pub const TASK_FILE_RELATIVE_PATH: &str = ".wscrpt/tasks.toml";

const CANCEL_GRACE: Duration = Duration::from_millis(750);
const EXIT_PIPE_DRAIN_GRACE: Duration = Duration::from_millis(250);
const MONITOR_INTERVAL: Duration = Duration::from_millis(10);

/// Parsed contents of a workspace task file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskConfig {
    version: u32,
    tasks: BTreeMap<String, TaskDefinition>,
}

/// One direct executable invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskDefinition {
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskConfig {
    version: u32,
    #[serde(default)]
    tasks: BTreeMap<String, TaskDefinition>,
}

impl TaskConfig {
    /// Parse and validate an in-memory versioned task file.
    pub fn parse(source: &str) -> Result<Self, TaskConfigError> {
        Self::parse_at(source, None)
    }

    /// Load `<workspace_root>/.wscrpt/tasks.toml`.
    pub fn load(workspace_root: impl AsRef<Path>) -> Result<Self, TaskConfigError> {
        let path = Self::path(workspace_root);
        let source = fs::read_to_string(&path).map_err(|source| TaskConfigError::Read {
            path: path.clone(),
            source,
        })?;
        Self::parse_at(&source, Some(path))
    }

    /// Load a task file when present, treating a missing file as no task
    /// configuration.  Other read failures are still reported.
    pub fn load_if_present(
        workspace_root: impl AsRef<Path>,
    ) -> Result<Option<Self>, TaskConfigError> {
        let path = Self::path(workspace_root);
        match fs::read_to_string(&path) {
            Ok(source) => Self::parse_at(&source, Some(path)).map(Some),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(TaskConfigError::Read { path, source }),
        }
    }

    pub fn path(workspace_root: impl AsRef<Path>) -> PathBuf {
        workspace_root.as_ref().join(TASK_FILE_RELATIVE_PATH)
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn tasks(&self) -> &BTreeMap<String, TaskDefinition> {
        &self.tasks
    }

    pub fn get(&self, name: &str) -> Option<&TaskDefinition> {
        self.tasks.get(name)
    }

    fn parse_at(source: &str, path: Option<PathBuf>) -> Result<Self, TaskConfigError> {
        let raw: RawTaskConfig =
            toml::from_str(source).map_err(|source| TaskConfigError::Parse {
                path: path.clone(),
                source,
            })?;
        if raw.version != TASK_FILE_VERSION {
            return Err(TaskConfigError::UnsupportedVersion {
                path,
                found: raw.version,
                supported: TASK_FILE_VERSION,
            });
        }
        for (name, task) in &raw.tasks {
            validate_task(name, task).map_err(|message| TaskConfigError::InvalidTask {
                path: path.clone(),
                task: name.clone(),
                message,
            })?;
        }
        Ok(Self {
            version: raw.version,
            tasks: raw.tasks,
        })
    }
}

impl TaskDefinition {
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }
}

/// A task-file read, syntax, version, or semantic validation failure.
#[derive(Debug)]
pub enum TaskConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: Option<PathBuf>,
        source: toml::de::Error,
    },
    UnsupportedVersion {
        path: Option<PathBuf>,
        found: u32,
        supported: u32,
    },
    InvalidTask {
        path: Option<PathBuf>,
        task: String,
        message: String,
    },
}

impl fmt::Display for TaskConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write_config_location(formatter, path.as_deref())?;
                write!(formatter, "invalid task TOML: {source}")
            }
            Self::UnsupportedVersion {
                path,
                found,
                supported,
            } => {
                write_config_location(formatter, path.as_deref())?;
                write!(
                    formatter,
                    "task file version {found} is unsupported (expected {supported})"
                )
            }
            Self::InvalidTask {
                path,
                task,
                message,
            } => {
                write_config_location(formatter, path.as_deref())?;
                write!(formatter, "invalid task {task:?}: {message}")
            }
        }
    }
}

impl Error for TaskConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::UnsupportedVersion { .. } | Self::InvalidTask { .. } => None,
        }
    }
}

fn write_config_location(formatter: &mut fmt::Formatter<'_>, path: Option<&Path>) -> fmt::Result {
    if let Some(path) = path {
        write!(formatter, "{}: ", path.display())?;
    }
    Ok(())
}

fn validate_task(name: &str, task: &TaskDefinition) -> Result<(), String> {
    if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
        return Err("name must be non-empty, trimmed, and contain no control characters".into());
    }
    if task.argv.is_empty() {
        return Err("argv must contain an executable".into());
    }
    if task.argv[0].is_empty() {
        return Err("argv[0] must not be empty".into());
    }
    for (index, argument) in task.argv.iter().enumerate() {
        if argument.contains('\0') {
            return Err(format!("argv[{index}] contains a NUL byte"));
        }
    }
    if task
        .cwd
        .as_ref()
        .is_some_and(|cwd| cwd.as_os_str().is_empty())
    {
        return Err("cwd must not be empty".into());
    }
    for (key, value) in &task.env {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(format!(
                "environment key {key:?} must be non-empty and contain neither '=' nor NUL"
            ));
        }
        if value.contains('\0') {
            return Err(format!("environment value for {key:?} contains a NUL byte"));
        }
    }
    Ok(())
}

/// Trust must be supplied at every launch site; it intentionally has no
/// `Default` implementation and no conversion from `bool`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceTrust {
    Untrusted,
    Trusted,
}

/// Memory limits for unread task output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskOutputLimits {
    /// Combined stdout/stderr payload retained between calls to
    /// [`TaskHandle::drain_events`].
    pub max_queued_bytes: usize,
    /// Maximum number of unread events. Terminal events take precedence over
    /// older output when this limit is reached.
    pub max_queued_events: usize,
    /// Maximum payload emitted by one reader event.
    pub read_chunk_bytes: usize,
}

impl Default for TaskOutputLimits {
    fn default() -> Self {
        Self {
            max_queued_bytes: 256 * 1024,
            max_queued_events: 512,
            read_chunk_bytes: 8 * 1024,
        }
    }
}

impl TaskOutputLimits {
    fn normalized(self) -> Self {
        Self {
            max_queued_bytes: self.max_queued_bytes.max(1),
            max_queued_events: self.max_queued_events.max(1),
            read_chunk_bytes: self.read_chunk_bytes.clamp(1, 64 * 1024),
        }
    }
}

/// A reusable launcher bound to one workspace and one parsed task file.
#[derive(Clone, Debug)]
pub struct TaskRunner {
    workspace_root: PathBuf,
    config: TaskConfig,
    output_limits: TaskOutputLimits,
}

impl TaskRunner {
    pub fn new(workspace_root: impl Into<PathBuf>, config: TaskConfig) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            config,
            output_limits: TaskOutputLimits::default(),
        }
    }

    pub fn with_output_limits(mut self, limits: TaskOutputLimits) -> Self {
        self.output_limits = limits.normalized();
        self
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn config(&self) -> &TaskConfig {
        &self.config
    }

    /// Launch a named task. The trust check happens before task lookup,
    /// working-directory inspection, or executable lookup.
    pub fn start(&self, name: &str, trust: WorkspaceTrust) -> Result<TaskHandle, TaskRunError> {
        if trust != WorkspaceTrust::Trusted {
            return Err(TaskRunError::UntrustedWorkspace);
        }
        let task = self
            .config
            .get(name)
            .ok_or_else(|| TaskRunError::TaskNotFound(name.to_owned()))?;
        // Revalidate here too so construction inside this module can never
        // become a bypass if another config source is added later.
        validate_task(name, task).map_err(|message| TaskRunError::InvalidTask {
            task: name.to_owned(),
            message,
        })?;

        let resolved_cwd = match task.cwd() {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => self.workspace_root.join(path),
            None => self.workspace_root.clone(),
        };
        let cwd =
            fs::canonicalize(&resolved_cwd).map_err(|_| TaskRunError::InvalidWorkingDirectory {
                task: name.to_owned(),
                path: resolved_cwd.clone(),
            })?;
        if !cwd.is_dir() {
            return Err(TaskRunError::InvalidWorkingDirectory {
                task: name.to_owned(),
                path: cwd,
            });
        }

        let mut command = Command::new(&task.argv[0]);
        command
            .args(&task.argv[1..])
            .current_dir(&cwd)
            .envs(&task.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);

        let mut child = command.spawn().map_err(|source| TaskRunError::Spawn {
            task: name.to_owned(),
            executable: task.argv[0].clone(),
            source,
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let reader_count = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
        let shared = Arc::new(RunSignals {
            inner: Mutex::new(RunShared {
                state: TaskState::Running,
                events: VecDeque::new(),
                queued_output_bytes: 0,
                dropped_output_bytes: 0,
                dropped_output_sequence: None,
                next_sequence: 0,
                accept_output: true,
                active_readers: reader_count,
                cancel_requested_at: None,
                force_kill_sent: false,
            }),
            changed: Condvar::new(),
            limits: self.output_limits.normalized(),
        });
        let child = Arc::new(Mutex::new(child));
        let mut reader_threads = Vec::with_capacity(reader_count);

        if let Some(stdout) = stdout {
            reader_threads.push(spawn_pipe_reader(
                stdout,
                OutputStream::Stdout,
                Arc::clone(&shared),
            ));
        }
        if let Some(stderr) = stderr {
            reader_threads.push(spawn_pipe_reader(
                stderr,
                OutputStream::Stderr,
                Arc::clone(&shared),
            ));
        }
        let monitor_thread = spawn_monitor(Arc::clone(&child), Arc::clone(&shared));

        Ok(TaskHandle {
            task_name: name.to_owned(),
            cwd,
            pid,
            child,
            shared,
            reader_threads,
            monitor_thread: Some(monitor_thread),
        })
    }
}

/// A launch refusal or operating-system spawn failure.
#[derive(Debug)]
pub enum TaskRunError {
    UntrustedWorkspace,
    TaskNotFound(String),
    InvalidTask {
        task: String,
        message: String,
    },
    InvalidWorkingDirectory {
        task: String,
        path: PathBuf,
    },
    Spawn {
        task: String,
        executable: String,
        source: io::Error,
    },
}

impl fmt::Display for TaskRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedWorkspace => write!(formatter, "workspace tasks are not trusted"),
            Self::TaskNotFound(task) => write!(formatter, "task {task:?} does not exist"),
            Self::InvalidTask { task, message } => {
                write!(formatter, "task {task:?} is invalid: {message}")
            }
            Self::InvalidWorkingDirectory { task, path } => write!(
                formatter,
                "task {task:?} working directory is not a directory: {}",
                path.display()
            ),
            Self::Spawn {
                task,
                executable,
                source,
            } => write!(
                formatter,
                "could not start task {task:?} executable {executable:?}: {source}"
            ),
        }
    }
}

impl Error for TaskRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::UntrustedWorkspace
            | Self::TaskNotFound(_)
            | Self::InvalidTask { .. }
            | Self::InvalidWorkingDirectory { .. } => None,
        }
    }
}

/// Which child pipe produced an output event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// One item in the ordered, bounded output queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEvent {
    /// A monotonic ordering key. An [`TaskEventKind::OutputDropped`] event uses
    /// the sequence position at which the first represented byte was lost, so
    /// a stable sort keeps the marker before the retained output suffix.
    pub sequence: u64,
    pub kind: TaskEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEventKind {
    Output {
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    /// Output discarded because the unread queue reached its byte or event
    /// limit. This synthetic event is produced by `drain_events`, ordered at
    /// the first represented loss boundary, and is not itself stored in the
    /// bounded queue.
    OutputDropped {
        bytes: usize,
    },
    PipeReadFailed {
        stream: OutputStream,
        message: String,
    },
    Exited(TaskExit),
    MonitorFailed {
        message: String,
        cancel_requested: bool,
    },
}

impl TaskEvent {
    pub fn output(&self) -> Option<(OutputStream, &[u8])> {
        match &self.kind {
            TaskEventKind::Output { stream, bytes } => Some((*stream, bytes)),
            _ => None,
        }
    }

    fn output_len(&self) -> usize {
        match &self.kind {
            TaskEventKind::Output { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }
}

/// Observable task lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Running,
    Cancelling,
    Exited(TaskExit),
    MonitorFailed {
        message: String,
        cancel_requested: bool,
    },
}

impl TaskState {
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Exited(_) | Self::MonitorFailed { .. })
    }
}

/// Process status paired with whether the editor requested cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskExit {
    pub status: ExitStatus,
    pub cancel_requested: bool,
}

impl TaskExit {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelResult {
    Requested,
    AlreadyRequested,
    AlreadyFinished,
}

/// A live or completed task. Dropping the handle requests cancellation so
/// editor shutdown does not intentionally orphan a running process tree.
#[derive(Debug)]
pub struct TaskHandle {
    task_name: String,
    cwd: PathBuf,
    pid: u32,
    child: Arc<Mutex<Child>>,
    shared: Arc<RunSignals>,
    reader_threads: Vec<JoinHandle<()>>,
    monitor_thread: Option<JoinHandle<()>>,
}

impl TaskHandle {
    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    /// Canonical working directory passed to the spawned task process.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn state(&self) -> TaskState {
        lock_unpoisoned(&self.shared.inner).state.clone()
    }

    /// Remove all currently queued events. Pipe readers continue in the
    /// background. If output overflowed since the previous drain, one
    /// `OutputDropped` event is inserted at the first represented loss
    /// boundary. The returned batch is already in sequence order; stable
    /// sorting by [`TaskEvent::sequence`] preserves that boundary.
    pub fn drain_events(&self) -> Vec<TaskEvent> {
        let mut shared = lock_unpoisoned(&self.shared.inner);
        drain_events_locked(&mut shared)
    }

    /// Wait until the process has a terminal state.
    pub fn wait(&self) -> TaskState {
        let mut shared = lock_unpoisoned(&self.shared.inner);
        while !shared.state.is_finished() {
            shared = wait_unpoisoned(&self.shared.changed, shared);
        }
        shared.state.clone()
    }

    /// Wait up to `timeout`, returning the most recent state even on timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> TaskState {
        let deadline = Instant::now().checked_add(timeout);
        let mut shared = lock_unpoisoned(&self.shared.inner);
        while !shared.state.is_finished() {
            let Some(deadline) = deadline else {
                shared = wait_unpoisoned(&self.shared.changed, shared);
                continue;
            };
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if remaining.is_zero() {
                break;
            }
            let (next, timed_out) =
                wait_timeout_unpoisoned(&self.shared.changed, shared, remaining);
            shared = next;
            if timed_out {
                break;
            }
        }
        shared.state.clone()
    }

    /// Request graceful cancellation. On Unix, each task starts in a fresh
    /// process group, so SIGTERM and the later SIGKILL fallback target its
    /// descendants as well as the direct child. Other platforms use
    /// `Child::kill` for the direct process.
    pub fn cancel(&self) -> io::Result<CancelResult> {
        let mut child = lock_unpoisoned(&self.child);
        let child_exited = child.try_wait()?.is_some();
        let shared = lock_unpoisoned(&self.shared.inner);
        if shared.state.is_finished() {
            return Ok(CancelResult::AlreadyFinished);
        }
        if shared.cancel_requested_at.is_some() {
            return Ok(CancelResult::AlreadyRequested);
        }
        drop(shared);

        let signal_result = if child_exited {
            terminate_process_group(self.pid)
        } else {
            terminate_process(&mut child)
        };
        if let Err(signal_error) = signal_result {
            if process_is_missing(&signal_error) {
                // The leader may have exited just before the signal while the
                // monitor is publishing its terminal state.
            } else if child.try_wait()?.is_some() {
                return Ok(CancelResult::AlreadyFinished);
            } else {
                return Err(signal_error);
            }
        }

        let mut shared = lock_unpoisoned(&self.shared.inner);
        shared.cancel_requested_at = Some(Instant::now());
        shared.state = TaskState::Cancelling;
        self.shared.changed.notify_all();
        Ok(CancelResult::Requested)
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        let _ = self.cancel();
        let _ = self.wait_timeout(CANCEL_GRACE + EXIT_PIPE_DRAIN_GRACE * 2);

        if let Some(thread) = self.monitor_thread.take()
            && thread.is_finished()
        {
            let _ = thread.join();
        }
        for thread in self.reader_threads.drain(..) {
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

#[derive(Debug)]
struct RunSignals {
    inner: Mutex<RunShared>,
    changed: Condvar,
    limits: TaskOutputLimits,
}

#[derive(Debug)]
struct RunShared {
    state: TaskState,
    events: VecDeque<TaskEvent>,
    queued_output_bytes: usize,
    dropped_output_bytes: usize,
    dropped_output_sequence: Option<u64>,
    next_sequence: u64,
    accept_output: bool,
    active_readers: usize,
    cancel_requested_at: Option<Instant>,
    force_kill_sent: bool,
}

fn drain_events_locked(shared: &mut RunShared) -> Vec<TaskEvent> {
    let mut events: Vec<_> = shared.events.drain(..).collect();
    shared.queued_output_bytes = 0;
    let dropped = std::mem::take(&mut shared.dropped_output_bytes);
    let dropped_sequence = shared.dropped_output_sequence.take();
    if dropped != 0 {
        let sequence =
            dropped_sequence.expect("dropped task output must retain its sequence boundary");
        let insertion = events.partition_point(|event| event.sequence < sequence);
        events.insert(
            insertion,
            TaskEvent {
                sequence,
                kind: TaskEventKind::OutputDropped { bytes: dropped },
            },
        );
    }
    events
}

fn spawn_pipe_reader<R>(
    mut pipe: R,
    stream: OutputStream,
    shared: Arc<RunSignals>,
) -> JoinHandle<()>
where
    R: Read + Send + 'static,
{
    let chunk_bytes = shared.limits.read_chunk_bytes;
    thread::spawn(move || {
        let mut buffer = vec![0_u8; chunk_bytes];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => push_output(&shared, stream, &buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    push_control_event(
                        &shared,
                        TaskEventKind::PipeReadFailed {
                            stream,
                            message: error.to_string(),
                        },
                    );
                    break;
                }
            }
        }
        let mut state = lock_unpoisoned(&shared.inner);
        state.active_readers = state.active_readers.saturating_sub(1);
        shared.changed.notify_all();
    })
}

fn push_output(shared: &RunSignals, stream: OutputStream, bytes: &[u8]) {
    let mut state = lock_unpoisoned(&shared.inner);
    if !state.accept_output {
        return;
    }

    let mut kept = bytes;
    if kept.len() > shared.limits.max_queued_bytes {
        let discard = kept.len() - shared.limits.max_queued_bytes;
        let sequence = next_sequence(&mut state);
        record_dropped_output(&mut state, discard, sequence);
        kept = &kept[discard..];
    }
    while state.events.len() >= shared.limits.max_queued_events
        || state.queued_output_bytes.saturating_add(kept.len()) > shared.limits.max_queued_bytes
    {
        let Some(removed) = state.events.pop_front() else {
            break;
        };
        let removed_bytes = removed.output_len();
        state.queued_output_bytes = state.queued_output_bytes.saturating_sub(removed_bytes);
        record_dropped_output(&mut state, removed_bytes, removed.sequence);
    }

    let sequence = next_sequence(&mut state);
    state.queued_output_bytes += kept.len();
    state.events.push_back(TaskEvent {
        sequence,
        kind: TaskEventKind::Output {
            stream,
            bytes: kept.to_vec(),
        },
    });
    shared.changed.notify_all();
}

fn push_control_event(shared: &RunSignals, kind: TaskEventKind) {
    let mut state = lock_unpoisoned(&shared.inner);
    while state.events.len() >= shared.limits.max_queued_events {
        let Some(removed) = state.events.pop_front() else {
            break;
        };
        let removed_bytes = removed.output_len();
        state.queued_output_bytes = state.queued_output_bytes.saturating_sub(removed_bytes);
        record_dropped_output(&mut state, removed_bytes, removed.sequence);
    }
    let sequence = next_sequence(&mut state);
    state.events.push_back(TaskEvent { sequence, kind });
    shared.changed.notify_all();
}

fn next_sequence(state: &mut RunShared) -> u64 {
    let sequence = state.next_sequence;
    state.next_sequence = state.next_sequence.saturating_add(1);
    sequence
}

fn record_dropped_output(state: &mut RunShared, bytes: usize, sequence: u64) {
    if bytes == 0 {
        return;
    }
    state.dropped_output_bytes = state.dropped_output_bytes.saturating_add(bytes);
    state.dropped_output_sequence = Some(
        state
            .dropped_output_sequence
            .map_or(sequence, |existing| existing.min(sequence)),
    );
}

fn spawn_monitor(child: Arc<Mutex<Child>>, shared: Arc<RunSignals>) -> JoinHandle<()> {
    thread::spawn(move || monitor(child, shared))
}

fn monitor(child: Arc<Mutex<Child>>, shared: Arc<RunSignals>) {
    let pid = lock_unpoisoned(&child).id();
    loop {
        let status = {
            let mut child = lock_unpoisoned(&child);
            child.try_wait()
        };
        match status {
            Ok(Some(status)) => {
                finish_after_pipe_drain(pid, &shared, status);
                return;
            }
            Ok(None) => maybe_force_cancel(&child, &shared),
            Err(error) => {
                // A monitor failure must not deliberately leave a process tree
                // behind. Best-effort force termination precedes publication.
                {
                    let mut child = lock_unpoisoned(&child);
                    let _ = force_terminate_process(&mut child);
                }
                finish_monitor_failure(&shared, error);
                return;
            }
        }
        thread::sleep(MONITOR_INTERVAL);
    }
}

fn maybe_force_cancel(child: &Mutex<Child>, shared: &RunSignals) {
    let should_force = {
        let state = lock_unpoisoned(&shared.inner);
        !state.force_kill_sent
            && state
                .cancel_requested_at
                .is_some_and(|started| started.elapsed() >= CANCEL_GRACE)
    };
    if !should_force {
        return;
    }

    let result = {
        let mut child = lock_unpoisoned(child);
        match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => force_terminate_process(&mut child),
            Err(error) => Err(error),
        }
    };
    let mut state = lock_unpoisoned(&shared.inner);
    state.force_kill_sent = true;
    if let Err(error) = result {
        let sequence = next_sequence(&mut state);
        while state.events.len() >= shared.limits.max_queued_events {
            if let Some(removed) = state.events.pop_front() {
                let removed_bytes = removed.output_len();
                state.queued_output_bytes = state.queued_output_bytes.saturating_sub(removed_bytes);
                record_dropped_output(&mut state, removed_bytes, removed.sequence);
            }
        }
        state.events.push_back(TaskEvent {
            sequence,
            kind: TaskEventKind::MonitorFailed {
                message: format!("could not force-cancel task: {error}"),
                cancel_requested: true,
            },
        });
    }
    shared.changed.notify_all();
}

fn finish_after_pipe_drain(pid: u32, shared: &RunSignals, status: ExitStatus) {
    let mut state = wait_for_pipe_readers(shared, EXIT_PIPE_DRAIN_GRACE);
    if state.active_readers != 0 {
        drop(state);
        let cleanup_error = force_terminate_process_group(pid)
            .err()
            .filter(|error| !process_is_missing(error));
        state = wait_for_pipe_readers(shared, EXIT_PIPE_DRAIN_GRACE);
        if state.active_readers != 0 || cleanup_error.is_some() {
            state.accept_output = false;
            let cancel_requested = state.cancel_requested_at.is_some();
            let message = cleanup_error.map_or_else(
                || "task process-group pipes remained open after forced cleanup".to_owned(),
                |error| format!("could not clean up task process group: {error}"),
            );
            state.state = TaskState::MonitorFailed {
                message: message.clone(),
                cancel_requested,
            };
            enqueue_control_locked(
                shared,
                &mut state,
                TaskEventKind::MonitorFailed {
                    message,
                    cancel_requested,
                },
            );
            shared.changed.notify_all();
            return;
        }
    }
    state.accept_output = false;
    let exit = TaskExit {
        status,
        cancel_requested: state.cancel_requested_at.is_some(),
    };
    state.state = TaskState::Exited(exit.clone());
    enqueue_control_locked(shared, &mut state, TaskEventKind::Exited(exit));
    shared.changed.notify_all();
}

fn wait_for_pipe_readers(shared: &RunSignals, timeout: Duration) -> MutexGuard<'_, RunShared> {
    let deadline = Instant::now().checked_add(timeout);
    let mut state = lock_unpoisoned(&shared.inner);
    while state.active_readers != 0 {
        let Some(deadline) = deadline else {
            state = wait_unpoisoned(&shared.changed, state);
            continue;
        };
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        let (next, timed_out) = wait_timeout_unpoisoned(&shared.changed, state, remaining);
        state = next;
        if timed_out {
            break;
        }
    }
    state
}

fn finish_monitor_failure(shared: &RunSignals, error: io::Error) {
    let mut state = lock_unpoisoned(&shared.inner);
    state.accept_output = false;
    let cancel_requested = state.cancel_requested_at.is_some();
    let message = error.to_string();
    state.state = TaskState::MonitorFailed {
        message: message.clone(),
        cancel_requested,
    };
    enqueue_control_locked(
        shared,
        &mut state,
        TaskEventKind::MonitorFailed {
            message,
            cancel_requested,
        },
    );
    shared.changed.notify_all();
}

fn enqueue_control_locked(shared: &RunSignals, state: &mut RunShared, kind: TaskEventKind) {
    while state.events.len() >= shared.limits.max_queued_events {
        let Some(removed) = state.events.pop_front() else {
            break;
        };
        let removed_bytes = removed.output_len();
        state.queued_output_bytes = state.queued_output_bytes.saturating_sub(removed_bytes);
        record_dropped_output(state, removed_bytes, removed.sequence);
    }
    let sequence = next_sequence(state);
    state.events.push_back(TaskEvent { sequence, kind });
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_unpoisoned<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
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

    // A group id of zero asks the child to become the leader of a new process
    // group before exec. This lets cancellation include descendants without
    // accidentally signalling the editor's own process group.
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process(child: &mut Child) -> io::Result<()> {
    terminate_process_group(child.id())
}

#[cfg(not(unix))]
fn terminate_process(child: &mut Child) -> io::Result<()> {
    child.kill()
}

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
const SIGTERM: std::ffi::c_int = 15;

#[cfg(unix)]
fn terminate_process_group(pid: u32) -> io::Result<()> {
    signal_process_group(pid, SIGTERM)
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn force_terminate_process_group(pid: u32) -> io::Result<()> {
    signal_process_group(pid, SIGKILL)
}

#[cfg(not(unix))]
fn force_terminate_process_group(_pid: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process-group cleanup is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: std::ffi::c_int) -> io::Result<()> {
    let pid = std::ffi::c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child PID exceeds c_int"))?;
    // SAFETY: `kill` is called with a valid integer signal and the negation of
    // the freshly spawned child's process-group leader PID. No pointer or
    // borrowed memory crosses the FFI boundary.
    let result = unsafe { unix_kill(-pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn process_is_missing(error: &io::Error) -> bool {
    // POSIX ESRCH is 3 on the Unix targets supported by Rust, including the
    // macOS and Linux hosts this editor targets.
    error.raw_os_error() == Some(3)
}

#[cfg(not(unix))]
fn process_is_missing(_error: &io::Error) -> bool {
    false
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn unix_kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_problem::parse_task_problems;
    use std::env;
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);
    const OVERFLOW_DISCARDED_PREFIX: &str = "noise";
    const OVERFLOW_SEVERED_TAIL: &str = "victim.rs:1:1: error: severed location\n";
    const OVERFLOW_FULL_PROBLEM: &str = "full.rs:2:3: error: complete location\n";
    const OVERFLOW_PHASE_ONE_READY: &str = "overflow-phase-one.ready";
    const OVERFLOW_CONTINUE: &str = "overflow-continue";
    const OVERFLOW_PHASE_TWO_READY: &str = "overflow-phase-two.ready";

    fn quoted_toml(value: &OsStr) -> String {
        let value = value.to_string_lossy();
        let mut quoted = String::from("\"");
        for character in value.chars() {
            match character {
                '\\' => quoted.push_str("\\\\"),
                '"' => quoted.push_str("\\\""),
                '\n' => quoted.push_str("\\n"),
                '\r' => quoted.push_str("\\r"),
                '\t' => quoted.push_str("\\t"),
                character => quoted.push(character),
            }
        }
        quoted.push('"');
        quoted
    }

    fn write_config(root: &Path, source: &str) {
        let directory = root.join(".wscrpt");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("tasks.toml"), source).unwrap();
    }

    fn task_config(_root: &Path, mode: &str, cwd: &str) -> String {
        let executable = quoted_toml(env::current_exe().unwrap().as_os_str());
        format!(
            r#"version = 1

[tasks.probe]
argv = [{executable}, "--ignored", "task_process_helper", "--nocapture"]
cwd = {cwd:?}
env = {{ WSCRPT_TASK_TEST_MODE = {mode:?}, WSCRPT_TASK_TEST_ID = "{}" }}
"#,
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn output_text(events: &[TaskEvent], stream: OutputStream) -> String {
        let mut bytes = Vec::new();
        for event in events {
            if let Some((event_stream, output)) = event.output()
                && event_stream == stream
            {
                bytes.extend_from_slice(output);
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn assembled_task_output(mut events: Vec<TaskEvent>) -> String {
        events.sort_by_key(|event| event.sequence);
        let mut output = String::new();
        for event in events {
            match event.kind {
                TaskEventKind::Output { bytes, .. } => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                }
                TaskEventKind::OutputDropped { bytes } => {
                    output.push_str(&format!("\n[… {bytes} output bytes dropped …]\n"));
                }
                _ => {}
            }
        }
        output
    }

    #[test]
    fn parses_versioned_argv_cwd_and_environment() {
        let config = TaskConfig::parse(
            r#"version = 1

[tasks.check]
argv = ["cargo", "check", "--all-targets"]
cwd = "backend"
env = { CARGO_TERM_COLOR = "never" }
"#,
        )
        .unwrap();

        assert_eq!(config.version(), 1);
        let check = config.get("check").unwrap();
        assert_eq!(check.argv(), ["cargo", "check", "--all-targets"]);
        assert_eq!(check.cwd(), Some(Path::new("backend")));
        assert_eq!(check.env().get("CARGO_TERM_COLOR"), Some(&"never".into()));
    }

    #[test]
    fn rejects_unknown_versions_empty_argv_and_command_strings() {
        assert!(matches!(
            TaskConfig::parse("version = 2"),
            Err(TaskConfigError::UnsupportedVersion { found: 2, .. })
        ));
        assert!(matches!(
            TaskConfig::parse("version = 1\n[tasks.bad]\nargv = []"),
            Err(TaskConfigError::InvalidTask { .. })
        ));
        assert!(matches!(
            TaskConfig::parse("version = 1\n[tasks.bad]\ncommand = \"echo unsafe\""),
            Err(TaskConfigError::Parse { .. })
        ));
    }

    #[test]
    fn load_if_present_distinguishes_missing_and_malformed_files() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            TaskConfig::load_if_present(directory.path())
                .unwrap()
                .is_none()
        );

        write_config(directory.path(), "version = 1\n[tasks.bad]\nargv = []");
        let error = TaskConfig::load(directory.path()).unwrap_err();
        match error {
            TaskConfigError::InvalidTask { path, task, .. } => {
                assert_eq!(path.unwrap(), TaskConfig::path(directory.path()));
                assert_eq!(task, "bad");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn untrusted_workspace_is_rejected_before_any_spawn_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let config = TaskConfig::parse(
            "version = 1\n[tasks.probe]\nargv = [\"definitely-not-an-executable\"]",
        )
        .unwrap();
        let runner = TaskRunner::new(directory.path(), config);
        assert!(matches!(
            runner.start("probe", WorkspaceTrust::Untrusted),
            Err(TaskRunError::UntrustedWorkspace)
        ));
        // Trust is also checked before revealing whether a requested task is
        // present, keeping every untrusted call on the same non-executing path.
        assert!(matches!(
            runner.start("missing", WorkspaceTrust::Untrusted),
            Err(TaskRunError::UntrustedWorkspace)
        ));
    }

    #[test]
    fn missing_and_non_directory_cwds_use_the_working_directory_error() {
        let directory = tempfile::tempdir().unwrap();
        let regular_file = directory.path().join("not-a-directory");
        fs::write(&regular_file, "file").unwrap();
        let config = TaskConfig::parse(
            r#"version = 1

[tasks.missing]
argv = ["definitely-not-an-executable"]
cwd = "missing"

[tasks.file]
argv = ["definitely-not-an-executable"]
cwd = "not-a-directory"
"#,
        )
        .unwrap();
        let runner = TaskRunner::new(directory.path(), config);

        assert!(matches!(
            runner.start("missing", WorkspaceTrust::Trusted),
            Err(TaskRunError::InvalidWorkingDirectory { path, .. })
                if path == directory.path().join("missing")
        ));
        let canonical_file = fs::canonicalize(regular_file).unwrap();
        assert!(matches!(
            runner.start("file", WorkspaceTrust::Trusted),
            Err(TaskRunError::InvalidWorkingDirectory { path, .. })
                if path == canonical_file
        ));
    }

    #[test]
    fn real_process_uses_configured_cwd_env_and_captures_both_pipes() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("work")).unwrap();
        write_config(
            directory.path(),
            &task_config(directory.path(), "probe", "work"),
        );
        let runner = TaskRunner::new(
            directory.path(),
            TaskConfig::load(directory.path()).unwrap(),
        );
        let task = runner.start("probe", WorkspaceTrust::Trusted).unwrap();
        let expected_cwd = fs::canonicalize(directory.path().join("work")).unwrap();
        assert_eq!(task.cwd(), expected_cwd);

        let state = task.wait_timeout(Duration::from_secs(10));
        let TaskState::Exited(exit) = state else {
            panic!("task did not exit: {state:?}");
        };
        assert!(exit.success());
        assert!(!exit.cancel_requested);

        let events = task.drain_events();
        let stdout = output_text(&events, OutputStream::Stdout);
        let stderr = output_text(&events, OutputStream::Stderr);
        assert!(stdout.contains("WSCRPT_HELPER_OUT=probe"), "{stdout:?}");
        assert!(
            stdout.contains(&format!("CWD={}", expected_cwd.display())),
            "{stdout:?}"
        );
        assert!(stderr.contains("WSCRPT_HELPER_ERR=probe"), "{stderr:?}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, TaskEventKind::Exited(_)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn absolute_symlink_cwd_is_canonical_and_stable_after_retarget() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let replacement = directory.path().join("replacement");
        let alias = directory.path().join("task-cwd");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();
        symlink(&original, &alias).unwrap();

        let executable = quoted_toml(env::current_exe().unwrap().as_os_str());
        let cwd = quoted_toml(alias.as_os_str());
        write_config(
            directory.path(),
            &format!(
                r#"version = 1

[tasks.probe]
argv = [{executable}, "--ignored", "task_process_helper", "--nocapture"]
cwd = {cwd}
env = {{ WSCRPT_TASK_TEST_MODE = "probe", WSCRPT_TASK_TEST_ID = "{}" }}
"#,
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ),
        );
        let runner = TaskRunner::new(
            directory.path(),
            TaskConfig::load(directory.path()).unwrap(),
        );
        let expected_cwd = fs::canonicalize(&original).unwrap();
        let task = runner.start("probe", WorkspaceTrust::Trusted).unwrap();

        fs::remove_file(&alias).unwrap();
        symlink(&replacement, &alias).unwrap();
        assert_eq!(task.cwd(), expected_cwd);
        assert_eq!(
            fs::canonicalize(&alias).unwrap(),
            fs::canonicalize(replacement).unwrap()
        );

        assert!(task.wait_timeout(Duration::from_secs(10)).is_finished());
        let stdout = output_text(&task.drain_events(), OutputStream::Stdout);
        assert!(
            stdout.contains(&format!("CWD={}", expected_cwd.display())),
            "{stdout:?}"
        );
    }

    #[test]
    fn unread_output_is_bounded_and_reports_drops() {
        let directory = tempfile::tempdir().unwrap();
        write_config(
            directory.path(),
            &task_config(directory.path(), "flood", "."),
        );
        let limits = TaskOutputLimits {
            max_queued_bytes: 128,
            max_queued_events: 6,
            read_chunk_bytes: 32,
        };
        let runner = TaskRunner::new(
            directory.path(),
            TaskConfig::load(directory.path()).unwrap(),
        )
        .with_output_limits(limits);
        let task = runner.start("probe", WorkspaceTrust::Trusted).unwrap();
        assert!(task.wait_timeout(Duration::from_secs(10)).is_finished());
        let events = task.drain_events();
        let retained: usize = events.iter().map(TaskEvent::output_len).sum();
        assert!(retained <= limits.max_queued_bytes, "retained {retained}");
        let dropped_index = events
            .iter()
            .position(
                |event| matches!(event.kind, TaskEventKind::OutputDropped { bytes } if bytes > 0),
            )
            .expect("overflow should emit a dropped-output marker");
        let retained_index = events
            .iter()
            .position(|event| event.output().is_some())
            .expect("overflow should retain an output suffix");
        assert!(dropped_index < retained_index, "{events:?}");
        let mut stably_sorted = events.clone();
        stably_sorted.sort_by_key(|event| event.sequence);
        let sorted_dropped_index = stably_sorted
            .iter()
            .position(|event| matches!(event.kind, TaskEventKind::OutputDropped { .. }))
            .unwrap();
        let sorted_retained_index = stably_sorted
            .iter()
            .position(|event| event.output().is_some())
            .unwrap();
        assert!(
            sorted_dropped_index < sorted_retained_index,
            "{stably_sorted:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, TaskEventKind::Exited(_)))
        );
    }

    #[test]
    fn dropped_prefix_reserves_a_sequence_before_the_retained_chunk() {
        let shared = RunSignals {
            inner: Mutex::new(RunShared {
                state: TaskState::Running,
                events: VecDeque::new(),
                queued_output_bytes: 0,
                dropped_output_bytes: 0,
                dropped_output_sequence: None,
                next_sequence: 0,
                accept_output: true,
                active_readers: 0,
                cancel_requested_at: None,
                force_kill_sent: false,
            }),
            changed: Condvar::new(),
            limits: TaskOutputLimits {
                max_queued_bytes: 5,
                max_queued_events: 4,
                read_chunk_bytes: 8,
            },
        };
        push_control_event(
            &shared,
            TaskEventKind::PipeReadFailed {
                stream: OutputStream::Stderr,
                message: "earlier control".into(),
            },
        );
        push_output(&shared, OutputStream::Stdout, b"abcdef");

        let mut state = lock_unpoisoned(&shared.inner);
        let events = drain_events_locked(&mut state);
        assert!(matches!(
            events.as_slice(),
            [
                TaskEvent {
                    sequence: 0,
                    kind: TaskEventKind::PipeReadFailed { .. }
                },
                TaskEvent {
                    sequence: 1,
                    kind: TaskEventKind::OutputDropped { bytes: 1 }
                },
                TaskEvent {
                    sequence: 2,
                    kind: TaskEventKind::Output { bytes, .. }
                }
            ] if bytes == b"bcdef"
        ));
        let mut stably_sorted = events.clone();
        stably_sorted.sort_by_key(|event| event.sequence);
        assert_eq!(stably_sorted, events);
    }

    #[test]
    fn real_runner_overflow_skips_severed_problem_and_parses_following_problem() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("victim.rs"), "victim\n").unwrap();
        fs::write(directory.path().join("full.rs"), "one\ntwo\n").unwrap();
        write_config(
            directory.path(),
            &task_config(directory.path(), "problem_overflow", "."),
        );
        let limits = TaskOutputLimits {
            max_queued_bytes: OVERFLOW_SEVERED_TAIL.len() + OVERFLOW_FULL_PROBLEM.len(),
            max_queued_events: 64,
            read_chunk_bytes: 64 * 1024,
        };
        let runner = TaskRunner::new(
            directory.path(),
            TaskConfig::load(directory.path()).unwrap(),
        )
        .with_output_limits(limits);
        let task = runner.start("probe", WorkspaceTrust::Trusted).unwrap();

        let phase_one = directory.path().join(OVERFLOW_PHASE_ONE_READY);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !phase_one.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            phase_one.exists(),
            "overflow helper did not finish phase one"
        );
        loop {
            let state = lock_unpoisoned(&task.shared.inner);
            if state.dropped_output_bytes != 0 && state.queued_output_bytes != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "phase-one output was not captured"
            );
            drop(state);
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(directory.path().join(OVERFLOW_CONTINUE), "continue").unwrap();

        let phase_two = directory.path().join(OVERFLOW_PHASE_TWO_READY);
        while !phase_two.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            phase_two.exists(),
            "overflow helper did not finish phase two"
        );
        loop {
            let state = lock_unpoisoned(&task.shared.inner);
            let retained: Vec<u8> = state
                .events
                .iter()
                .filter_map(|event| event.output().map(|(_, bytes)| bytes))
                .flatten()
                .copied()
                .collect();
            if retained.ends_with(OVERFLOW_FULL_PROBLEM.as_bytes()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "phase-two output was not captured"
            );
            drop(state);
            thread::sleep(Duration::from_millis(10));
        }

        let events = task.drain_events();
        let dropped_index = events
            .iter()
            .position(|event| matches!(event.kind, TaskEventKind::OutputDropped { .. }))
            .unwrap();
        let retained_index = events
            .iter()
            .position(|event| event.output().is_some())
            .unwrap();
        assert!(dropped_index < retained_index, "{events:?}");
        let assembled = assembled_task_output(events);
        assert!(assembled.contains(OVERFLOW_SEVERED_TAIL), "{assembled:?}");
        assert!(assembled.contains(OVERFLOW_FULL_PROBLEM), "{assembled:?}");

        let report = parse_task_problems(&assembled, task.cwd(), directory.path()).unwrap();
        assert!(report.truncated);
        assert_eq!(report.problems.len(), 1, "{report:?}\n{assembled:?}");
        assert_eq!(
            report.problems[0].path,
            fs::canonicalize(directory.path().join("full.rs")).unwrap()
        );
        assert_eq!(report.problems[0].line, 1);
        assert_eq!(report.problems[0].column, 2);

        let _ = task.cancel();
        assert!(task.wait_timeout(Duration::from_secs(5)).is_finished());
    }

    #[test]
    fn cancellation_reaches_a_real_process_and_is_visible_in_state() {
        let directory = tempfile::tempdir().unwrap();
        write_config(
            directory.path(),
            &task_config(directory.path(), "sleep", "."),
        );
        let runner = TaskRunner::new(
            directory.path(),
            TaskConfig::load(directory.path()).unwrap(),
        );
        let task = runner.start("probe", WorkspaceTrust::Trusted).unwrap();
        assert_eq!(task.cancel().unwrap(), CancelResult::Requested);
        assert!(matches!(
            task.state(),
            TaskState::Cancelling | TaskState::Exited(_)
        ));
        let state = task.wait_timeout(Duration::from_secs(5));
        let TaskState::Exited(exit) = state else {
            panic!("cancelled task did not exit: {state:?}");
        };
        assert!(exit.cancel_requested);
        assert!(!exit.success());
    }

    #[cfg(unix)]
    #[test]
    fn leader_exit_cleans_descendant_that_keeps_task_pipes_open() {
        let directory = tempfile::tempdir().unwrap();
        write_config(
            directory.path(),
            &task_config(directory.path(), "orphan", "."),
        );
        let runner = TaskRunner::new(
            directory.path(),
            TaskConfig::load(directory.path()).unwrap(),
        );
        let task = runner.start("probe", WorkspaceTrust::Trusted).unwrap();
        let pid_path = directory.path().join("descendant.pid");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let descendant_pid: u32 = fs::read_to_string(&pid_path)
            .expect("helper should publish descendant PID")
            .parse()
            .unwrap();
        assert!(unix_process_exists(descendant_pid));

        let state = task.wait_timeout(Duration::from_secs(5));
        let TaskState::Exited(exit) = state else {
            let _ = unsafe { unix_kill(descendant_pid as std::ffi::c_int, SIGKILL) };
            panic!("task leader exit did not finish cleanup: {state:?}");
        };
        assert!(exit.success());

        let deadline = Instant::now() + Duration::from_secs(2);
        while unix_process_exists(descendant_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let survived = unix_process_exists(descendant_pid);
        if survived {
            let _ = unsafe { unix_kill(descendant_pid as std::ffi::c_int, SIGKILL) };
        }
        assert!(
            !survived,
            "task descendant {descendant_pid} survived leader exit"
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

    /// Re-entered through the current test executable so task tests exercise a
    /// real child without relying on a platform shell or an external utility.
    // The `orphan` branch intentionally drops its child handle: the production
    // runner must prove it cleans that inherited process group after the test
    // helper leader exits.
    #[allow(clippy::zombie_processes)]
    #[test]
    #[ignore = "subprocess helper; invoked explicitly by task tests"]
    fn task_process_helper() {
        let Ok(mode) = env::var("WSCRPT_TASK_TEST_MODE") else {
            return;
        };
        match mode.as_str() {
            "probe" => {
                println!("WSCRPT_HELPER_OUT={mode}");
                println!("CWD={}", env::current_dir().unwrap().display());
                eprintln!("WSCRPT_HELPER_ERR={mode}");
            }
            "flood" => {
                let output = "o".repeat(4096);
                let error = "e".repeat(4096);
                println!("{output}");
                eprintln!("{error}");
            }
            "problem_overflow" => {
                let mut stdout = io::stdout().lock();
                stdout.write_all("x".repeat(4096).as_bytes()).unwrap();
                stdout
                    .write_all(OVERFLOW_DISCARDED_PREFIX.as_bytes())
                    .unwrap();
                stdout.flush().unwrap();
                fs::write(OVERFLOW_PHASE_ONE_READY, "ready").unwrap();

                let deadline = Instant::now() + Duration::from_secs(10);
                while !Path::new(OVERFLOW_CONTINUE).exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    Path::new(OVERFLOW_CONTINUE).exists(),
                    "overflow parent did not release phase two"
                );
                stdout.write_all(OVERFLOW_SEVERED_TAIL.as_bytes()).unwrap();
                stdout.write_all(OVERFLOW_FULL_PROBLEM.as_bytes()).unwrap();
                stdout.flush().unwrap();
                fs::write(OVERFLOW_PHASE_TWO_READY, "ready").unwrap();
                thread::sleep(Duration::from_secs(30));
            }
            "sleep" => thread::sleep(Duration::from_secs(30)),
            "orphan" => {
                let child = Command::new(env::current_exe().unwrap())
                    .args(["--ignored", "task_process_helper", "--nocapture"])
                    .env("WSCRPT_TASK_TEST_MODE", "descendant")
                    .spawn()
                    .unwrap();
                fs::write("descendant.pid", child.id().to_string()).unwrap();
            }
            "descendant" => thread::sleep(Duration::from_secs(30)),
            other => panic!("unknown helper mode {other}"),
        }
    }
}
