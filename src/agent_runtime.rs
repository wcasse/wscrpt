//! Host-local agent runtimes for W2.
//!
//! UX model follows Grok Build's agentic terminal loop: plan-first receipts,
//! explicit pause points (Needs You), cancellation that invalidates a generation,
//! and review handoff.
//!
//! - Fake agent: deterministic W0 script on a worker thread (CI default).
//! - Pi RPC: `crate::agent_pi` — `pi --mode rpc` + permission gate extension.
//! - ACP process: `crate::agent_acp` — generic ACP stdio (e.g. `grok agent stdio`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::agent::{AgentCoordinator, FakeAgent};
use crate::agent_contract::{
    AgentAuthority, AgentEvent, AgentRunState, MAX_STICKY_BRIEF_BYTES, PathScope, WorkPacket,
    WorktreeBinding, unix_now_ms,
};

const EVENT_CAPACITY: usize = 64;
const FAKE_STEP_PAUSE: Duration = Duration::from_millis(40);

/// Events produced by a background agent job for App admission.
#[derive(Debug)]
pub enum AgentJobEvent {
    /// One protocol event to pass through [`AgentCoordinator::admit`].
    Event(AgentEvent),
    /// Non-fatal notice for the status line.
    Notice(String),
    /// Job ended (success, cancel, or failure after the last event).
    Finished {
        cancelled: bool,
        error: Option<String>,
    },
}

/// Handle for one in-flight agent job.
#[derive(Debug)]
pub struct AgentJob {
    cancel: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl AgentJob {
    /// Build a job handle from a shared cancel flag and worker thread.
    pub fn from_parts(cancel: Arc<AtomicBool>, handle: JoinHandle<()>) -> Self {
        Self {
            cancel,
            _handle: handle,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
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
    pub fn from_receiver(receiver: Receiver<AgentJobEvent>) -> Self {
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
        AgentJob {
            cancel,
            _handle: handle,
        },
        AgentEventPort { receiver },
    )
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
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            });
            return;
        }
        if sender.send(AgentJobEvent::Event(event)).is_err() {
            return;
        }
        thread::sleep(FAKE_STEP_PAUSE);
    }
    if cancel.load(Ordering::Acquire) {
        let _ = sender.send(AgentJobEvent::Finished {
            cancelled: true,
            error: None,
        });
        return;
    }
    let _ = sender.send(AgentJobEvent::Finished {
        cancelled: false,
        error: None,
    });
}

/// Optional sticky notepad content attached as agent brief (user-visible notes).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StickyAttach {
    pub id: String,
    pub title: String,
    pub body: String,
}

/// Build a review-oriented work packet for the active workspace goal.
pub fn work_packet_for_goal(
    workspace_id: u64,
    workspace_root: impl Into<std::path::PathBuf>,
    goal: impl Into<String>,
) -> Result<WorkPacket, String> {
    work_packet_for_goal_with_sticky(workspace_id, workspace_root, goal, None)
}

/// Same as [`work_packet_for_goal`], optionally attaching one open sticky as brief.
pub fn work_packet_for_goal_with_sticky(
    workspace_id: u64,
    workspace_root: impl Into<std::path::PathBuf>,
    goal: impl Into<String>,
    sticky: Option<StickyAttach>,
) -> Result<WorkPacket, String> {
    let goal = goal.into();
    let goal_trimmed = goal.trim();
    if goal_trimmed.is_empty() {
        return Err("agent goal must not be empty".to_owned());
    }
    let root = workspace_root.into();
    let (sticky_ids, sticky_brief) = match sticky {
        Some(attach) => {
            let brief = format_sticky_brief(&attach);
            (vec![attach.id], Some(brief))
        }
        None => (Vec::new(), None),
    };
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
        sticky_ids,
        sticky_brief,
    };
    packet.validate().map_err(|error| error.to_string())?;
    Ok(packet)
}

/// Bounded human-readable sticky snapshot for the packet (and fake agent notice).
pub fn format_sticky_brief(attach: &StickyAttach) -> String {
    let mut brief = format!("# {}\n\n{}", attach.title.trim(), attach.body.trim());
    if brief.len() > MAX_STICKY_BRIEF_BYTES {
        brief.truncate(MAX_STICKY_BRIEF_BYTES);
        // Avoid splitting mid-char.
        while !brief.is_char_boundary(brief.len()) {
            brief.pop();
        }
        brief.push('…');
    }
    brief
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
        if !packet.sticky_ids.is_empty() {
            lines.push(format!("sticky: {}", packet.sticky_ids.join(", ")));
        }
        if let Some(brief) = &packet.sticky_brief {
            let one_line = brief.lines().next().unwrap_or("").trim();
            if !one_line.is_empty() {
                let mut line = format!("brief: {one_line}");
                if line.len() > 72 {
                    line.truncate(69);
                    line.push('…');
                }
                lines.push(line);
            }
        }
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
        if let Some(artifact) = &event.artifact_ref {
            lines.push(format!("      artifact {artifact}"));
        }
    }
    if receipt.is_empty() {
        lines.push("  (empty)".to_owned());
    }
    lines
}

/// Build bounded Markdown log bullets from a run receipt (S4 write-back source).
///
/// Skips pure state transitions and sensitive events. Prefers plan / path /
/// check / notice / artifact / review lines so the sticky stays human-readable.
pub fn receipt_log_bullets(
    receipt: &[crate::agent_contract::AgentEvent],
    max: usize,
) -> Vec<String> {
    use crate::agent_contract::AgentEventKind;
    let mut bullets = Vec::new();
    for event in receipt {
        if bullets.len() >= max {
            break;
        }
        if event.sensitive {
            continue;
        }
        match event.kind {
            AgentEventKind::State => continue,
            AgentEventKind::Plan
            | AgentEventKind::PathTouched
            | AgentEventKind::CheckResult
            | AgentEventKind::Artifact
            | AgentEventKind::ReviewReady
            | AgentEventKind::Notice
            | AgentEventKind::Approval => {}
        }
        let one_line = event
            .summary
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .to_owned();
        if one_line.is_empty() {
            continue;
        }
        let mut bullet = format!("{}: {one_line}", event.kind.as_str());
        if let Some(path) = &event.path {
            bullet.push_str(&format!(" ({})", path.display()));
        }
        if bullet.len() > crate::stickies::MAX_RECEIPT_LOG_LINE_BYTES {
            bullet.truncate(crate::stickies::MAX_RECEIPT_LOG_LINE_BYTES.saturating_sub(1));
            bullet.push('…');
        }
        bullets.push(bullet);
    }
    bullets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentCoordinator;
    use std::time::Duration;

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
    fn sticky_attach_is_validated_on_packet() {
        let attach = StickyAttach {
            id: "s-demo-1".to_owned(),
            title: "Ship pad".to_owned(),
            body: "- [ ] wire packet\n- [ ] demo\n".to_owned(),
        };
        let packet = work_packet_for_goal_with_sticky(
            1,
            "/tmp/wscrpt-demo",
            "finish stickies",
            Some(attach),
        )
        .unwrap();
        assert_eq!(packet.sticky_ids, vec!["s-demo-1".to_owned()]);
        assert!(
            packet
                .sticky_brief
                .as_ref()
                .is_some_and(|b| b.contains("Ship pad") && b.contains("wire packet"))
        );
        packet.validate().unwrap();
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
