//! Host-local agent run coordinator and deterministic fake agent (W0).
//!
//! The coordinator is the single admission point for agent events. Every event
//! must match the active workspace identity and run generation; sequences are
//! strictly monotonic; path-touch events are checked against the work packet
//! scope. Cancelling or replacing a run advances generation so stale traffic
//! cannot affect the current workspace.
//!
//! This module does not launch real agents, speak ACP wire protocol, or mutate
//! the filesystem. Those arrive in later phases behind the same contracts.

use std::fmt;
use std::path::PathBuf;

use crate::agent_contract::{
    AgentAuthority, AgentEvent, AgentEventKind, AgentRunState, ContractError, PathScope,
    WorkPacket, WorktreeBinding, normalize_relative, sample_work_packet, validate_id,
};

/// Maximum retained receipt events for the active (or last closed) run.
pub const MAX_RECEIPT_EVENTS: usize = 256;
/// Maximum events a fake agent script may contain.
pub const MAX_FAKE_SCRIPT_STEPS: usize = 128;

/// Outcome of a successful admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmitOutcome {
    pub sequence: u64,
    pub run_state: AgentRunState,
    pub receipt_len: usize,
}

/// Why an event was refused. Refusals never mutate coordinator state except
/// where noted (none of these do).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmitError {
    NoActiveRun,
    RunClosed,
    WrongWorkspace { expected: u64, actual: u64 },
    WrongSession,
    StaleGeneration { expected: u64, actual: u64 },
    ReplayOrOutOfOrder { expected: u64, actual: u64 },
    Contract(ContractError),
    PathOutOfScope(PathBuf),
    EditAuthorityRequired,
    ReceiptFull,
}

impl fmt::Display for AdmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveRun => write!(f, "no active agent run"),
            Self::RunClosed => write!(f, "agent run is closed"),
            Self::WrongWorkspace { expected, actual } => {
                write!(f, "workspace mismatch: expected {expected}, got {actual}")
            }
            Self::WrongSession => write!(f, "session id does not match the active run"),
            Self::StaleGeneration { expected, actual } => {
                write!(f, "stale generation: expected {expected}, got {actual}")
            }
            Self::ReplayOrOutOfOrder { expected, actual } => {
                write!(
                    f,
                    "sequence must be {expected}, got {actual} (replay or out of order)"
                )
            }
            Self::Contract(error) => write!(f, "{error}"),
            Self::PathOutOfScope(path) => {
                write!(f, "path out of work-packet scope: {}", path.display())
            }
            Self::EditAuthorityRequired => {
                write!(f, "path_touched requires edit authority on the work packet")
            }
            Self::ReceiptFull => write!(f, "receipt event capacity exhausted"),
        }
    }
}

impl std::error::Error for AdmitError {}

impl From<ContractError> for AdmitError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

/// Why a run could not start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartError {
    Contract(ContractError),
    WorkspaceMismatch { coordinator: u64, packet: u64 },
    AlreadyActive,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => write!(f, "{error}"),
            Self::WorkspaceMismatch {
                coordinator,
                packet,
            } => write!(
                f,
                "packet workspace {packet} does not match coordinator {coordinator}"
            ),
            Self::AlreadyActive => write!(f, "an agent run is already active"),
        }
    }
}

impl std::error::Error for StartError {}

impl From<ContractError> for StartError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveRun {
    session_id: String,
    packet: WorkPacket,
    generation: u64,
}

/// Single admission point for one workspace's agent runs.
#[derive(Debug)]
pub struct AgentCoordinator {
    workspace_id: u64,
    generation: u64,
    last_sequence: u64,
    run_state: AgentRunState,
    active: Option<ActiveRun>,
    receipt: Vec<AgentEvent>,
    closed_session_id: Option<String>,
}

impl AgentCoordinator {
    pub fn new(workspace_id: u64) -> Self {
        Self {
            workspace_id,
            generation: 0,
            last_sequence: 0,
            run_state: AgentRunState::Closed,
            active: None,
            receipt: Vec::new(),
            closed_session_id: None,
        }
    }

    pub fn workspace_id(&self) -> u64 {
        self.workspace_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn run_state(&self) -> AgentRunState {
        self.run_state
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn active_session_id(&self) -> Option<&str> {
        self.active.as_ref().map(|run| run.session_id.as_str())
    }

    pub fn active_packet(&self) -> Option<&WorkPacket> {
        self.active.as_ref().map(|run| &run.packet)
    }

    pub fn receipt(&self) -> &[AgentEvent] {
        &self.receipt
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some() && self.run_state != AgentRunState::Closed
    }

    /// Start a new run. Cancels any incomplete prior run by advancing generation.
    ///
    /// Pass `replace_active = true` to supersede an in-flight run; otherwise an
    /// active non-closed run returns [`StartError::AlreadyActive`].
    pub fn start_run(
        &mut self,
        session_id: impl Into<String>,
        packet: WorkPacket,
        replace_active: bool,
    ) -> Result<u64, StartError> {
        let session_id = session_id.into();
        validate_id(&session_id, "session_id").map_err(StartError::Contract)?;
        packet.validate()?;
        if packet.workspace_id != self.workspace_id {
            return Err(StartError::WorkspaceMismatch {
                coordinator: self.workspace_id,
                packet: packet.workspace_id,
            });
        }
        if self.is_active() && !replace_active {
            return Err(StartError::AlreadyActive);
        }

        self.generation = next_generation(self.generation);
        self.last_sequence = 0;
        self.receipt.clear();
        self.closed_session_id = None;
        self.run_state = AgentRunState::Brief;
        self.active = Some(ActiveRun {
            session_id,
            packet,
            generation: self.generation,
        });
        Ok(self.generation)
    }

    /// Cancel the active run. Subsequent events for the old generation are stale.
    pub fn cancel_run(&mut self) -> Option<CancelReceipt> {
        let active = self.active.take()?;
        let generation = self.generation;
        self.generation = next_generation(self.generation);
        self.last_sequence = 0;
        self.run_state = AgentRunState::Closed;
        self.closed_session_id = Some(active.session_id.clone());
        Some(CancelReceipt {
            session_id: active.session_id,
            cancelled_generation: generation,
            current_generation: self.generation,
        })
    }

    /// Admit one event into the active run receipt.
    pub fn admit(&mut self, mut event: AgentEvent) -> Result<AdmitOutcome, AdmitError> {
        let active = self.active.as_ref().ok_or(AdmitError::NoActiveRun)?;
        if self.run_state == AgentRunState::Closed {
            return Err(AdmitError::RunClosed);
        }
        if event.workspace_id != self.workspace_id {
            return Err(AdmitError::WrongWorkspace {
                expected: self.workspace_id,
                actual: event.workspace_id,
            });
        }
        if event.session_id != active.session_id {
            return Err(AdmitError::WrongSession);
        }
        if event.generation != active.generation {
            return Err(AdmitError::StaleGeneration {
                expected: active.generation,
                actual: event.generation,
            });
        }
        let expected_sequence = self.last_sequence.saturating_add(1);
        if event.sequence != expected_sequence {
            return Err(AdmitError::ReplayOrOutOfOrder {
                expected: expected_sequence,
                actual: event.sequence,
            });
        }

        event.validate_structure()?;

        if let Some(path) = event.path.clone() {
            let normalized = normalize_relative(&path)?;
            let allowed = active.packet.allows_path(&normalized)?;
            if !allowed {
                return Err(AdmitError::PathOutOfScope(normalized));
            }
            event.path = Some(normalized);
        }

        if event.kind == AgentEventKind::PathTouched && !active.packet.authority.edit {
            return Err(AdmitError::EditAuthorityRequired);
        }

        if self.receipt.len() >= MAX_RECEIPT_EVENTS {
            return Err(AdmitError::ReceiptFull);
        }

        if let Some(state) = event.run_state {
            self.run_state = state;
        } else if event.kind == AgentEventKind::ReviewReady {
            self.run_state = AgentRunState::Review;
        }

        self.last_sequence = event.sequence;
        self.receipt.push(event);

        if self.run_state == AgentRunState::Closed {
            // Keep packet identity for inspection but mark inactive for new work.
            if let Some(active) = self.active.take() {
                self.closed_session_id = Some(active.session_id);
            }
        }

        Ok(AdmitOutcome {
            sequence: self.last_sequence,
            run_state: self.run_state,
            receipt_len: self.receipt.len(),
        })
    }

    /// Drain-style helper: admit a batch, stopping at the first error.
    pub fn admit_all(
        &mut self,
        events: impl IntoIterator<Item = AgentEvent>,
    ) -> Result<AdmitOutcome, AdmitError> {
        let mut last = AdmitOutcome {
            sequence: self.last_sequence,
            run_state: self.run_state,
            receipt_len: self.receipt.len(),
        };
        for event in events {
            last = self.admit(event)?;
        }
        Ok(last)
    }
}

/// Result of cancelling a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelReceipt {
    pub session_id: String,
    pub cancelled_generation: u64,
    pub current_generation: u64,
}

fn next_generation(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}

// ---------------------------------------------------------------------------
// Fake agent
// ---------------------------------------------------------------------------

/// One scripted step before identity (workspace/session/generation/sequence)
/// is filled in by [`FakeAgent::materialize`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeStep {
    pub kind: AgentEventKind,
    pub summary: String,
    pub path: Option<PathBuf>,
    pub git_object: Option<String>,
    pub artifact_ref: Option<String>,
    pub check_ok: Option<bool>,
    pub run_state: Option<AgentRunState>,
    pub sensitive: bool,
}

impl FakeStep {
    pub fn state(state: AgentRunState, summary: impl Into<String>) -> Self {
        Self {
            kind: AgentEventKind::State,
            summary: summary.into(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: Some(state),
            sensitive: false,
        }
    }

    pub fn plan(summary: impl Into<String>) -> Self {
        Self {
            kind: AgentEventKind::Plan,
            summary: summary.into(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        }
    }

    pub fn path_touched(path: impl Into<PathBuf>, summary: impl Into<String>) -> Self {
        Self {
            kind: AgentEventKind::PathTouched,
            summary: summary.into(),
            path: Some(path.into()),
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        }
    }

    pub fn check(ok: bool, summary: impl Into<String>) -> Self {
        Self {
            kind: AgentEventKind::CheckResult,
            summary: summary.into(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: Some(ok),
            run_state: None,
            sensitive: false,
        }
    }

    pub fn review_ready(summary: impl Into<String>) -> Self {
        Self {
            kind: AgentEventKind::ReviewReady,
            summary: summary.into(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: Some(AgentRunState::Review),
            sensitive: false,
        }
    }

    pub fn notice(summary: impl Into<String>) -> Self {
        Self {
            kind: AgentEventKind::Notice,
            summary: summary.into(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        }
    }
}

/// Deterministic agent that emits a fixed script of events.
#[derive(Clone, Debug, Default)]
pub struct FakeAgent {
    steps: Vec<FakeStep>,
}

impl FakeAgent {
    pub fn new(steps: Vec<FakeStep>) -> Result<Self, ContractError> {
        if steps.len() > MAX_FAKE_SCRIPT_STEPS {
            return Err(ContractError::TooMany {
                field: "fake_script",
                limit: MAX_FAKE_SCRIPT_STEPS,
            });
        }
        Ok(Self { steps })
    }

    /// A useful happy-path script: plan → edit in scope → check → review.
    pub fn happy_path_edit() -> Self {
        Self {
            steps: vec![
                FakeStep::state(AgentRunState::Working, "starting"),
                FakeStep::plan("1. touch src/lib.rs\n2. run cargo test"),
                FakeStep::path_touched("src/lib.rs", "added module export"),
                FakeStep::check(true, "cargo test --locked"),
                FakeStep::review_ready("ready for human review"),
            ],
        }
    }

    pub fn steps(&self) -> &[FakeStep] {
        &self.steps
    }

    /// Bind script steps to a live run identity with 1-based sequences.
    pub fn materialize(
        &self,
        workspace_id: u64,
        session_id: &str,
        generation: u64,
        start_timestamp_ms: u64,
    ) -> Vec<AgentEvent> {
        self.steps
            .iter()
            .enumerate()
            .map(|(index, step)| AgentEvent {
                workspace_id,
                session_id: session_id.to_owned(),
                generation,
                sequence: (index as u64).saturating_add(1),
                timestamp_unix_ms: start_timestamp_ms.saturating_add(index as u64),
                kind: step.kind,
                summary: step.summary.clone(),
                path: step.path.clone(),
                git_object: step.git_object.clone(),
                artifact_ref: step.artifact_ref.clone(),
                check_ok: step.check_ok,
                run_state: step.run_state,
                sensitive: step.sensitive,
            })
            .collect()
    }
}

/// Convenience: validated sample packet scoped for unit tests.
pub fn test_packet(workspace_id: u64) -> WorkPacket {
    sample_work_packet(workspace_id)
}

/// Expand a sample packet's writable scope for tests that need root-level writes.
pub fn test_packet_with_root_writable(workspace_id: u64) -> WorkPacket {
    let mut packet = sample_work_packet(workspace_id);
    packet.writable_paths = vec![PathScope::new(".").expect("root scope")];
    packet.worktree = WorktreeBinding::CurrentTree {
        root: PathBuf::from("/tmp/wscrpt-workspace"),
    };
    packet.authority = AgentAuthority::review_oriented();
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_contract::{MAX_SUMMARY_BYTES, sample_work_packet};

    fn start_happy(coordinator: &mut AgentCoordinator) -> (String, u64) {
        let session = "run-happy".to_owned();
        let generation = coordinator
            .start_run(
                session.clone(),
                test_packet(coordinator.workspace_id()),
                false,
            )
            .expect("start");
        (session, generation)
    }

    #[test]
    fn happy_path_fake_agent_reaches_review() {
        let mut coordinator = AgentCoordinator::new(7);
        let (session, generation) = start_happy(&mut coordinator);
        let events = FakeAgent::happy_path_edit().materialize(7, &session, generation, 100);
        let outcome = coordinator.admit_all(events).expect("admit all");
        assert_eq!(outcome.run_state, AgentRunState::Review);
        assert_eq!(outcome.sequence, 5);
        assert_eq!(coordinator.receipt().len(), 5);
        assert!(coordinator.is_active());
    }

    #[test]
    fn stale_generation_after_cancel_is_rejected() {
        let mut coordinator = AgentCoordinator::new(1);
        let (session, generation) = start_happy(&mut coordinator);
        let cancel = coordinator.cancel_run().expect("cancel");
        assert_ne!(cancel.cancelled_generation, cancel.current_generation);

        let event = AgentEvent {
            workspace_id: 1,
            session_id: session,
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::Notice,
            summary: "late notice".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert_eq!(coordinator.admit(event), Err(AdmitError::NoActiveRun));
    }

    #[test]
    fn replace_run_invalidates_prior_generation() {
        let mut coordinator = AgentCoordinator::new(2);
        let session_a = "run-a";
        let gen_a = coordinator
            .start_run(session_a, test_packet(2), false)
            .unwrap();
        let session_b = "run-b";
        let gen_b = coordinator
            .start_run(session_b, test_packet(2), true)
            .unwrap();
        assert_ne!(gen_a, gen_b);

        let stale = AgentEvent {
            workspace_id: 2,
            session_id: session_a.to_owned(),
            generation: gen_a,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::Notice,
            summary: "from a".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert!(matches!(
            coordinator.admit(stale),
            Err(AdmitError::WrongSession | AdmitError::StaleGeneration { .. })
        ));

        let live = AgentEvent {
            workspace_id: 2,
            session_id: session_b.to_owned(),
            generation: gen_b,
            sequence: 1,
            timestamp_unix_ms: 2,
            kind: AgentEventKind::Notice,
            summary: "from b".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        coordinator.admit(live).expect("live");
    }

    #[test]
    fn replayed_and_out_of_order_sequences_are_rejected() {
        let mut coordinator = AgentCoordinator::new(3);
        let (session, generation) = start_happy(&mut coordinator);
        let first = AgentEvent {
            workspace_id: 3,
            session_id: session.clone(),
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::Notice,
            summary: "one".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        coordinator.admit(first.clone()).unwrap();
        assert_eq!(
            coordinator.admit(first),
            Err(AdmitError::ReplayOrOutOfOrder {
                expected: 2,
                actual: 1
            })
        );
        let jump = AgentEvent {
            workspace_id: 3,
            session_id: session,
            generation,
            sequence: 4,
            timestamp_unix_ms: 2,
            kind: AgentEventKind::Notice,
            summary: "jump".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert_eq!(
            coordinator.admit(jump),
            Err(AdmitError::ReplayOrOutOfOrder {
                expected: 2,
                actual: 4
            })
        );
    }

    #[test]
    fn wrong_workspace_is_rejected() {
        let mut coordinator = AgentCoordinator::new(4);
        let (session, generation) = start_happy(&mut coordinator);
        let event = AgentEvent {
            workspace_id: 99,
            session_id: session,
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::Notice,
            summary: "wrong workspace".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert_eq!(
            coordinator.admit(event),
            Err(AdmitError::WrongWorkspace {
                expected: 4,
                actual: 99
            })
        );
    }

    #[test]
    fn invalid_path_and_out_of_scope_path_are_rejected() {
        let mut coordinator = AgentCoordinator::new(5);
        let (session, generation) = start_happy(&mut coordinator);

        let escape = AgentEvent {
            workspace_id: 5,
            session_id: session.clone(),
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::PathTouched,
            summary: "escape".to_owned(),
            path: Some(PathBuf::from("../secret")),
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert!(matches!(
            coordinator.admit(escape),
            Err(AdmitError::Contract(ContractError::InvalidPath(_)))
        ));

        // Sequence was not consumed; retry with out-of-scope but valid relative path.
        let outside = AgentEvent {
            workspace_id: 5,
            session_id: session,
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::PathTouched,
            summary: "docs".to_owned(),
            path: Some(PathBuf::from("docs/README.md")),
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert_eq!(
            coordinator.admit(outside),
            Err(AdmitError::PathOutOfScope(PathBuf::from("docs/README.md")))
        );
        assert_eq!(coordinator.last_sequence(), 0);
        assert!(coordinator.receipt().is_empty());
    }

    #[test]
    fn protected_path_is_out_of_scope() {
        let mut coordinator = AgentCoordinator::new(6);
        let (session, generation) = start_happy(&mut coordinator);
        let event = AgentEvent {
            workspace_id: 6,
            session_id: session,
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::PathTouched,
            summary: "touch tasks".to_owned(),
            path: Some(PathBuf::from(".wscrpt/tasks.toml")),
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert_eq!(
            coordinator.admit(event),
            Err(AdmitError::PathOutOfScope(PathBuf::from(
                ".wscrpt/tasks.toml"
            )))
        );
    }

    #[test]
    fn oversized_event_is_rejected_without_advancing_sequence() {
        let mut coordinator = AgentCoordinator::new(8);
        let (session, generation) = start_happy(&mut coordinator);
        let event = AgentEvent {
            workspace_id: 8,
            session_id: session,
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::Notice,
            summary: "y".repeat(MAX_SUMMARY_BYTES + 1),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert!(matches!(
            coordinator.admit(event),
            Err(AdmitError::Contract(ContractError::Oversized { .. }))
        ));
        assert_eq!(coordinator.last_sequence(), 0);
    }

    #[test]
    fn closing_state_ends_active_run() {
        let mut coordinator = AgentCoordinator::new(9);
        let (session, generation) = start_happy(&mut coordinator);
        let event = AgentEvent {
            workspace_id: 9,
            session_id: session,
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::State,
            summary: "done".to_owned(),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: Some(AgentRunState::Closed),
            sensitive: false,
        };
        let outcome = coordinator.admit(event).unwrap();
        assert_eq!(outcome.run_state, AgentRunState::Closed);
        assert!(!coordinator.is_active());
        assert_eq!(
            coordinator.admit(AgentEvent {
                workspace_id: 9,
                session_id: "run-happy".to_owned(),
                generation,
                sequence: 2,
                timestamp_unix_ms: 2,
                kind: AgentEventKind::Notice,
                summary: "after close".to_owned(),
                path: None,
                git_object: None,
                artifact_ref: None,
                check_ok: None,
                run_state: None,
                sensitive: false,
            }),
            Err(AdmitError::NoActiveRun)
        );
    }

    #[test]
    fn edit_without_authority_is_rejected() {
        let mut coordinator = AgentCoordinator::new(10);
        let mut packet = sample_work_packet(10);
        packet.authority.edit = false;
        let generation = coordinator.start_run("run-no-edit", packet, false).unwrap();
        let event = AgentEvent {
            workspace_id: 10,
            session_id: "run-no-edit".to_owned(),
            generation,
            sequence: 1,
            timestamp_unix_ms: 1,
            kind: AgentEventKind::PathTouched,
            summary: "edit".to_owned(),
            path: Some(PathBuf::from("src/x.rs")),
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state: None,
            sensitive: false,
        };
        assert_eq!(
            coordinator.admit(event),
            Err(AdmitError::EditAuthorityRequired)
        );
    }

    #[test]
    fn already_active_requires_replace_flag() {
        let mut coordinator = AgentCoordinator::new(11);
        coordinator.start_run("a", test_packet(11), false).unwrap();
        assert_eq!(
            coordinator.start_run("b", test_packet(11), false),
            Err(StartError::AlreadyActive)
        );
    }

    #[test]
    fn packet_workspace_must_match_coordinator() {
        let mut coordinator = AgentCoordinator::new(12);
        assert_eq!(
            coordinator.start_run("a", test_packet(99), false),
            Err(StartError::WorkspaceMismatch {
                coordinator: 12,
                packet: 99
            })
        );
    }
}
