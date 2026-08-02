//! Host-local agent runtimes for W2.
//!
//! UX model follows Grok Build's agentic terminal loop: plan-first receipts,
//! explicit pause points (Needs You), cancellation that invalidates a generation,
//! and review handoff — without requiring a full ACP wire client on day one.
//!
//! - [`FakeAgentRuntime`] drives the deterministic W0 script on a worker thread.
//! - [`AgentConfig`] (in `config`) may name a future ACP argv (for example
//!   `grok agent stdio`); process ACP is gated behind config and not the default.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::agent::{AgentCoordinator, FakeAgent};
use crate::agent_contract::{
    AgentAuthority, AgentEvent, AgentEventKind, AgentRunState, PathScope, WorkPacket,
    WorktreeBinding, unix_now_ms,
};

pub(crate) const EVENT_CAPACITY: usize = 64;
const FAKE_STEP_PAUSE: Duration = Duration::from_millis(40);
/// Sleep while waiting for UI to drain the channel (hard events only).
const HARD_SEND_SPIN: Duration = Duration::from_millis(1);

/// Soft = drop if UI is behind; hard = wait for a slot (unless cancelled).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendPriority {
    Soft,
    Hard,
}

/// Classify job events for backpressure. Kill path must never block on soft noise.
pub(crate) fn job_event_priority(event: &AgentJobEvent) -> SendPriority {
    match event {
        AgentJobEvent::Finished { .. } | AgentJobEvent::PermissionNeeded(_) => SendPriority::Hard,
        AgentJobEvent::Notice(_) => SendPriority::Soft,
        AgentJobEvent::Event(event) => match event.kind {
            AgentEventKind::ReviewReady | AgentEventKind::Approval => SendPriority::Hard,
            _ if matches!(
                event.run_state,
                Some(AgentRunState::Review | AgentRunState::NeedsYou)
            ) =>
            {
                SendPriority::Hard
            }
            _ => SendPriority::Soft,
        },
    }
}

/// Non-blocking soft drop / hard wait send. Returns whether the event was queued.
///
/// Hard sends spin (not block indefinitely) and abort if `cancel` is set so
/// process kill never stalls on a full channel.
pub(crate) fn send_job_event(
    sender: &SyncSender<AgentJobEvent>,
    event: AgentJobEvent,
    priority: SendPriority,
    cancel: Option<&AtomicBool>,
) -> bool {
    match priority {
        SendPriority::Soft => match sender.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        },
        SendPriority::Hard => {
            let mut event = event;
            loop {
                match sender.try_send(event) {
                    Ok(()) => return true,
                    Err(TrySendError::Disconnected(_)) => return false,
                    Err(TrySendError::Full(returned)) => {
                        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                            return false;
                        }
                        thread::sleep(HARD_SEND_SPIN);
                        event = returned;
                    }
                }
            }
        }
    }
}

/// Convenience: priority from the event itself.
pub(crate) fn send_job_event_auto(
    sender: &SyncSender<AgentJobEvent>,
    event: AgentJobEvent,
    cancel: Option<&AtomicBool>,
) -> bool {
    let priority = job_event_priority(&event);
    send_job_event(sender, event, priority, cancel)
}

/// Events produced by a background agent job for App admission.
#[derive(Debug)]
pub enum AgentJobEvent {
    /// One protocol event to pass through [`AgentCoordinator::admit`].
    Event(AgentEvent),
    /// Non-fatal notice for the status line.
    Notice(String),
    /// ACP `session/request_permission` — user must choose (Needs You).
    PermissionNeeded(PendingPermission),
    /// Job ended (success, cancel, or failure after the last event).
    Finished {
        cancelled: bool,
        error: Option<String>,
    },
}

/// One option from an ACP permission prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    /// ACP kind: `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

impl PermissionOption {
    pub fn is_allow(&self) -> bool {
        self.kind.starts_with("allow")
    }

    pub fn is_reject(&self) -> bool {
        self.kind.starts_with("reject")
    }
}

/// Pending tool permission awaiting a human decision on the Agents dashboard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingPermission {
    pub request_id: u64,
    pub summary: String,
    pub options: Vec<PermissionOption>,
}

/// User decision for a pending ACP permission request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionDecision {
    /// Selected one of the offered option ids.
    Select { option_id: String },
    /// Prompt cancelled (maps to ACP `cancelled` outcome).
    Cancelled,
}

/// Handle for one in-flight agent job.
#[derive(Debug)]
pub struct AgentJob {
    cancel: Arc<AtomicBool>,
    /// Live ACP process group leader pid (0 / absent for fake jobs).
    pub(crate) child_pid: Option<Arc<AtomicU32>>,
    /// Reply path for ACP permission prompts (process jobs only).
    permission_tx: Option<SyncSender<PermissionDecision>>,
    _handle: JoinHandle<()>,
}

impl AgentJob {
    pub(crate) fn new(
        cancel: Arc<AtomicBool>,
        child_pid: Option<Arc<AtomicU32>>,
        permission_tx: Option<SyncSender<PermissionDecision>>,
        handle: JoinHandle<()>,
    ) -> Self {
        Self {
            cancel,
            child_pid,
            permission_tx,
            _handle: handle,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(tx) = &self.permission_tx {
            let _ = tx.try_send(PermissionDecision::Cancelled);
        }
        if let Some(pid_cell) = &self.child_pid {
            let pid = pid_cell.load(Ordering::Acquire);
            if pid != 0 {
                crate::agent_acp::kill_agent_process_group(pid);
            }
        }
    }

    /// Reply to a pending ACP permission prompt (Needs You).
    pub fn reply_permission(&self, decision: PermissionDecision) -> Result<(), String> {
        let Some(tx) = &self.permission_tx else {
            return Err("no live ACP permission channel".to_owned());
        };
        tx.send(decision)
            .map_err(|_| "ACP permission waiter gone".to_owned())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
}

/// Channel end used by App to drain agent traffic without blocking the TUI.
#[derive(Debug)]
pub struct AgentEventPort {
    receiver: Receiver<AgentJobEvent>,
}

impl AgentEventPort {
    pub(crate) fn new(receiver: Receiver<AgentJobEvent>) -> Self {
        Self { receiver }
    }

    pub fn try_recv(&self) -> Result<AgentJobEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

/// Spawn the deterministic fake agent (Grok Build–like plan → edit → check → review).
pub fn spawn_fake_agent(
    workspace_id: u64,
    session_id: impl Into<String>,
    generation: u64,
    agent: FakeAgent,
) -> (AgentJob, AgentEventPort) {
    let session_id = session_id.into();
    let (sender, receiver) = mpsc::sync_channel(EVENT_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    let handle = thread::Builder::new()
        .name("wscrpt-agent-fake".to_owned())
        .spawn(move || {
            run_fake(
                workspace_id,
                session_id,
                generation,
                agent,
                sender,
                cancel_flag,
            );
        })
        .expect("spawn fake agent thread");
    (
        AgentJob::new(cancel, None, None, handle),
        AgentEventPort::new(receiver),
    )
}

/// Spawn a host-configured ACP process agent (`agent.use_fake = false`).
pub fn spawn_process_agent(
    workspace_id: u64,
    session_id: impl Into<String>,
    generation: u64,
    cwd: impl Into<std::path::PathBuf>,
    argv: &[String],
    goal: impl Into<String>,
) -> Result<(AgentJob, AgentEventPort), String> {
    crate::agent_acp::spawn_acp_agent(workspace_id, session_id, generation, cwd, argv, goal)
}

fn run_fake(
    workspace_id: u64,
    session_id: String,
    generation: u64,
    agent: FakeAgent,
    sender: SyncSender<AgentJobEvent>,
    cancel: Arc<AtomicBool>,
) {
    let start = unix_now_ms();
    let events = agent.materialize(workspace_id, &session_id, generation, start);
    for event in events {
        if cancel.load(Ordering::Acquire) {
            let _ = send_job_event_auto(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: true,
                    error: None,
                },
                Some(&cancel),
            );
            return;
        }
        // Fake script uses fixed sequences — never soft-drop events.
        if !send_job_event(
            &sender,
            AgentJobEvent::Event(event),
            SendPriority::Hard,
            Some(&cancel),
        ) {
            return;
        }
        thread::sleep(FAKE_STEP_PAUSE);
    }
    if cancel.load(Ordering::Acquire) {
        let _ = send_job_event_auto(
            &sender,
            AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            },
            Some(&cancel),
        );
        return;
    }
    let _ = send_job_event_auto(
        &sender,
        AgentJobEvent::Finished {
            cancelled: false,
            error: None,
        },
        Some(&cancel),
    );
}

/// Build a review-oriented work packet for the active workspace goal.
pub fn work_packet_for_goal(
    workspace_id: u64,
    workspace_root: impl Into<std::path::PathBuf>,
    goal: impl Into<String>,
) -> Result<WorkPacket, String> {
    let goal = goal.into();
    let goal_trimmed = goal.trim();
    if goal_trimmed.is_empty() {
        return Err("agent goal must not be empty".to_owned());
    }
    let root = workspace_root.into();
    let packet = WorkPacket {
        id: format!("pkt-{}", short_id()),
        workspace_id,
        goal: goal_trimmed.to_owned(),
        base_commit: None,
        worktree: WorktreeBinding::CurrentTree { root },
        // Demo-friendly scope: entire tree writable; protect editor trust paths.
        writable_paths: vec![PathScope::new(".").map_err(|error| error.to_string())?],
        protected_paths: vec![
            PathScope::new(".wscrpt").map_err(|error| error.to_string())?,
            PathScope::new(".git").map_err(|error| error.to_string())?,
        ],
        required_checks: vec![vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--locked".to_owned(),
        ]],
        authority: AgentAuthority::review_oriented(),
        creator: "local-user".to_owned(),
        created_at_unix_ms: unix_now_ms(),
    };
    packet.validate().map_err(|error| error.to_string())?;
    Ok(packet)
}

/// Session id for a new run.
pub fn new_session_id() -> String {
    format!("run-{}", short_id())
}

fn short_id() -> String {
    let now = unix_now_ms();
    format!("{now:x}")
}

/// Validate a goal line for the prompt (UTF-8 length bound).
pub fn validate_goal_input(goal: &str) -> Result<(), String> {
    let trimmed = goal.trim();
    if trimmed.is_empty() {
        return Err("type a goal, then Enter".to_owned());
    }
    if trimmed.len() > 4 * 1024 {
        return Err("goal is limited to 4096 UTF-8 bytes".to_owned());
    }
    validate_id_safe_summary(trimmed)?;
    Ok(())
}

fn validate_id_safe_summary(goal: &str) -> Result<(), String> {
    // Goals are free text; only reject NULs / bare controls that break the TUI.
    if goal
        .chars()
        .any(|ch| ch == '\0' || (ch.is_control() && ch != '\n' && ch != '\t'))
    {
        return Err("goal contains control characters".to_owned());
    }
    Ok(())
}

/// One-line activity label for footers (Grok Build–style mode breadcrumb).
pub fn run_state_label(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Brief => "AGENT BRIEF",
        AgentRunState::Working => "AGENT WORK",
        AgentRunState::NeedsYou => "AGENT NEED YOU",
        AgentRunState::Review => "AGENT REVIEW",
        AgentRunState::Closed => "AGENT IDLE",
    }
}

/// Format session / goal / authority / receipt lines for the Agents dashboard.
pub fn format_receipt_lines(coordinator: &AgentCoordinator, limit: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(session) = coordinator.active_session_id() {
        lines.push(format!(
            "session {session}  ·  {}  ·  gen {}",
            coordinator.run_state().as_str(),
            coordinator.generation()
        ));
    } else {
        lines.push(format!(
            "no active session  ·  last {}",
            coordinator.run_state().as_str()
        ));
    }
    if let Some(packet) = coordinator.active_packet() {
        lines.push(format!("goal: {}", packet.goal));
        lines.push(format!(
            "authority: edit={} command={} network={} commit={}",
            packet.authority.edit,
            packet.authority.command,
            packet.authority.network,
            packet.authority.commit
        ));
    }
    lines.push(String::new());
    lines.push("receipt (newest last):".to_owned());
    let receipt = coordinator.receipt();
    let start = receipt.len().saturating_sub(limit);
    for event in &receipt[start..] {
        lines.push(format!(
            "{:>3}. [{}] {}",
            event.sequence,
            event.kind.as_str(),
            event.summary
        ));
        if let Some(path) = &event.path {
            lines.push(format!("      path {}", path.display()));
        }
    }
    if receipt.is_empty() {
        lines.push("  (empty)".to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentCoordinator;
    use std::time::Duration;

    #[test]
    fn soft_events_drop_when_channel_full() {
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(send_job_event(
            &tx,
            AgentJobEvent::Notice("one".to_owned()),
            SendPriority::Soft,
            None,
        ));
        // Channel full: soft drop.
        assert!(!send_job_event(
            &tx,
            AgentJobEvent::Notice("two".to_owned()),
            SendPriority::Soft,
            None,
        ));
        // Drain and hard finish still works.
        assert!(matches!(rx.try_recv(), Ok(AgentJobEvent::Notice(_))));
        assert!(send_job_event(
            &tx,
            AgentJobEvent::Finished {
                cancelled: false,
                error: None,
            },
            SendPriority::Hard,
            None,
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentJobEvent::Finished {
                cancelled: false,
                error: None
            })
        ));
    }

    #[test]
    fn hard_event_waits_for_slot_unless_cancelled() {
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(send_job_event(
            &tx,
            AgentJobEvent::Notice("fill".to_owned()),
            SendPriority::Soft,
            None,
        ));
        let cancel = Arc::new(AtomicBool::new(false));
        let tx2 = tx.clone();
        let cancel2 = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            send_job_event(
                &tx2,
                AgentJobEvent::Finished {
                    cancelled: false,
                    error: None,
                },
                SendPriority::Hard,
                Some(cancel2.as_ref()),
            )
        });
        thread::sleep(Duration::from_millis(20));
        // Free a slot so hard send completes.
        let _ = rx.try_recv();
        assert!(handle.join().unwrap());
        assert!(matches!(rx.try_recv(), Ok(AgentJobEvent::Finished { .. })));
    }

    #[test]
    fn fake_runtime_events_admit_to_review() {
        let mut coordinator = AgentCoordinator::new(42);
        let session = new_session_id();
        let packet = work_packet_for_goal(42, "/tmp/wscrpt-demo", "demo goal").unwrap();
        let generation = coordinator
            .start_run(session.clone(), packet, false)
            .unwrap();
        let (job, port) = spawn_fake_agent(42, session, generation, FakeAgent::happy_path_edit());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match port.try_recv() {
                Ok(AgentJobEvent::Event(event)) => {
                    coordinator.admit(event).unwrap();
                }
                Ok(AgentJobEvent::Finished { cancelled, error }) => {
                    assert!(!cancelled);
                    assert!(error.is_none());
                    break;
                }
                Ok(AgentJobEvent::Notice(_)) => {}
                Ok(AgentJobEvent::PermissionNeeded(_)) => {
                    panic!("fake agent does not request permission")
                }
                Err(TryRecvError::Empty) => {
                    if std::time::Instant::now() > deadline {
                        panic!("timeout waiting for fake agent");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => panic!("disconnected"),
            }
        }
        drop(job);
        assert_eq!(coordinator.run_state(), AgentRunState::Review);
        assert_eq!(coordinator.receipt().len(), 5);
    }

    #[test]
    fn cancel_stops_before_completion() {
        let mut coordinator = AgentCoordinator::new(7);
        let session = new_session_id();
        let packet = work_packet_for_goal(7, "/tmp/wscrpt-demo", "cancel me").unwrap();
        let generation = coordinator
            .start_run(session.clone(), packet, false)
            .unwrap();
        let (job, port) = spawn_fake_agent(7, session, generation, FakeAgent::happy_path_edit());
        job.cancel();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut finished = false;
        while std::time::Instant::now() < deadline {
            match port.try_recv() {
                Ok(AgentJobEvent::Finished { cancelled, .. }) => {
                    assert!(cancelled);
                    finished = true;
                    break;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
        assert!(finished, "expected cancelled finished event");
    }
}
