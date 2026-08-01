//! Bounded startup, snapshot, and trusted mutation workers.
//!
//! Every result carries an `App` workspace identity and a per-service
//! generation. `App` accepts results only through this coordinator, so an old
//! scan can never replace a newer workspace snapshot.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

use crate::git::{GitMutation, GitMutationResult, GitRepository, RepositoryStatus};
use crate::project::ProjectIndex;
use crate::recovery::{RecoveryListing, RecoveryStore};

const EVENT_CAPACITY: usize = 32;
static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ServiceTag {
    pub workspace_id: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServiceKind {
    Project,
    Git,
    GitMutation,
    Recovery,
}

#[derive(Debug)]
pub(crate) struct GitSnapshot {
    pub repository: GitRepository,
    pub status: RepositoryStatus,
}

#[derive(Debug)]
pub(crate) struct RecoverySnapshot {
    pub store: Option<RecoveryStore>,
    pub listing: RecoveryListing,
    pub notice: Option<String>,
}

#[derive(Debug)]
pub(crate) enum ServiceEvent {
    Project {
        tag: ServiceTag,
        result: Result<ProjectIndex, String>,
    },
    Git {
        tag: ServiceTag,
        result: Result<Option<GitSnapshot>, String>,
    },
    GitMutation {
        tag: ServiceTag,
        result: Result<GitMutationResult, String>,
    },
    Recovery {
        tag: ServiceTag,
        snapshot: RecoverySnapshot,
    },
}

impl ServiceEvent {
    pub fn tag(&self) -> ServiceTag {
        match self {
            Self::Project { tag, .. }
            | Self::Git { tag, .. }
            | Self::GitMutation { tag, .. }
            | Self::Recovery { tag, .. } => *tag,
        }
    }

    pub const fn kind(&self) -> ServiceKind {
        match self {
            Self::Project { .. } => ServiceKind::Project,
            Self::Git { .. } => ServiceKind::Git,
            Self::GitMutation { .. } => ServiceKind::GitMutation,
            Self::Recovery { .. } => ServiceKind::Recovery,
        }
    }
}

#[derive(Debug)]
struct JobToken(Arc<AtomicBool>);

impl JobToken {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}

#[derive(Debug)]
pub(crate) struct ServiceCoordinator {
    workspace_id: u64,
    sender: SyncSender<ServiceEvent>,
    receiver: Receiver<ServiceEvent>,
    project_generation: u64,
    git_generation: u64,
    git_mutation_generation: u64,
    recovery_generation: u64,
    project_token: Option<JobToken>,
    git_token: Option<JobToken>,
    git_mutation_token: Option<JobToken>,
    recovery_token: Option<JobToken>,
}

impl ServiceCoordinator {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(EVENT_CAPACITY);
        Self {
            workspace_id: NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed),
            sender,
            receiver,
            project_generation: 0,
            git_generation: 0,
            git_mutation_generation: 0,
            recovery_generation: 0,
            project_token: None,
            git_token: None,
            git_mutation_token: None,
            recovery_token: None,
        }
    }

    pub fn start_all(&mut self, root: PathBuf) -> (u64, u64, u64) {
        let project = self.start_project(root.clone());
        let git = self.start_git(root.clone());
        let recovery = self.start_recovery(root);
        (project, git, recovery)
    }

    pub fn start_project(&mut self, root: PathBuf) -> u64 {
        self.start_project_with(move || {
            ProjectIndex::build(&root).map_err(|error| error.to_string())
        })
    }

    fn start_project_with<F>(&mut self, work: F) -> u64
    where
        F: FnOnce() -> Result<ProjectIndex, String> + Send + 'static,
    {
        if let Some(token) = self.project_token.take() {
            token.cancel();
        }
        self.project_generation = next_generation(self.project_generation);
        let tag = ServiceTag {
            workspace_id: self.workspace_id,
            generation: self.project_generation,
        };
        let token = JobToken::new();
        spawn_event(
            "wscrpt-index",
            self.sender.clone(),
            token.flag(),
            move || ServiceEvent::Project {
                tag,
                result: work(),
            },
        );
        self.project_token = Some(token);
        tag.generation
    }

    pub fn start_git(&mut self, root: PathBuf) -> u64 {
        self.start_git_with(move || match GitRepository::discover(&root) {
            Ok(repository) => repository
                .status()
                .map(|status| Some(GitSnapshot { repository, status }))
                .map_err(|error| error.to_string()),
            Err(_) => Ok(None),
        })
    }

    fn start_git_with<F>(&mut self, work: F) -> u64
    where
        F: FnOnce() -> Result<Option<GitSnapshot>, String> + Send + 'static,
    {
        if let Some(token) = self.git_token.take() {
            token.cancel();
        }
        self.git_generation = next_generation(self.git_generation);
        let tag = ServiceTag {
            workspace_id: self.workspace_id,
            generation: self.git_generation,
        };
        let token = JobToken::new();
        spawn_event("wscrpt-git", self.sender.clone(), token.flag(), move || {
            ServiceEvent::Git {
                tag,
                result: work(),
            }
        });
        self.git_token = Some(token);
        tag.generation
    }

    pub fn start_git_mutation(
        &mut self,
        repository: GitRepository,
        mutation: GitMutation,
    ) -> Result<u64, String> {
        self.start_git_mutation_with(move || {
            repository
                .apply_mutation(mutation)
                .map_err(|error| error.to_string())
        })
    }

    fn start_git_mutation_with<F>(&mut self, work: F) -> Result<u64, String>
    where
        F: FnOnce() -> Result<GitMutationResult, String> + Send + 'static,
    {
        if self.git_mutation_token.is_some() {
            return Err("A Git operation is already running".to_owned());
        }
        self.git_mutation_generation = next_generation(self.git_mutation_generation);
        let tag = ServiceTag {
            workspace_id: self.workspace_id,
            generation: self.git_mutation_generation,
        };
        let token = JobToken::new();
        spawn_event(
            "wscrpt-git-mutation",
            self.sender.clone(),
            token.flag(),
            move || ServiceEvent::GitMutation {
                tag,
                result: work(),
            },
        );
        self.git_mutation_token = Some(token);
        Ok(tag.generation)
    }

    pub fn finish_git_mutation(&mut self, generation: u64) -> bool {
        if generation != self.git_mutation_generation {
            return false;
        }
        self.git_mutation_token.take().is_some()
    }

    fn start_recovery(&mut self, root: PathBuf) -> u64 {
        if let Some(token) = self.recovery_token.take() {
            token.cancel();
        }
        self.recovery_generation = next_generation(self.recovery_generation);
        let tag = ServiceTag {
            workspace_id: self.workspace_id,
            generation: self.recovery_generation,
        };
        let token = JobToken::new();
        spawn_event(
            "wscrpt-recovery",
            self.sender.clone(),
            token.flag(),
            move || {
                let snapshot = match RecoveryStore::from_env() {
                    Ok(store) => match store.list_with_warnings() {
                        Ok(mut listing) => {
                            listing
                                .records
                                .retain(|record| record.workspace_root == root);
                            RecoverySnapshot {
                                store: Some(store),
                                listing,
                                notice: None,
                            }
                        }
                        Err(error) => RecoverySnapshot {
                            store: Some(store),
                            listing: RecoveryListing::default(),
                            notice: Some(format!("Recovery scan unavailable: {error}")),
                        },
                    },
                    Err(error) => RecoverySnapshot {
                        store: None,
                        listing: RecoveryListing::default(),
                        notice: Some(format!("Recovery unavailable: {error}")),
                    },
                };
                ServiceEvent::Recovery { tag, snapshot }
            },
        );
        self.recovery_token = Some(token);
        tag.generation
    }

    pub fn try_recv(&self) -> Result<ServiceEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn is_current(&self, event: &ServiceEvent) -> bool {
        let tag = event.tag();
        tag.workspace_id == self.workspace_id
            && tag.generation
                == match event.kind() {
                    ServiceKind::Project => self.project_generation,
                    ServiceKind::Git => self.git_generation,
                    ServiceKind::GitMutation => self.git_mutation_generation,
                    ServiceKind::Recovery => self.recovery_generation,
                }
    }

    pub fn cancel_all(&mut self) {
        for token in [
            self.project_token.take(),
            self.git_token.take(),
            self.git_mutation_token.take(),
            self.recovery_token.take(),
        ]
        .into_iter()
        .flatten()
        {
            token.cancel();
        }
        self.project_generation = next_generation(self.project_generation);
        self.git_generation = next_generation(self.git_generation);
        self.git_mutation_generation = next_generation(self.git_mutation_generation);
        self.recovery_generation = next_generation(self.recovery_generation);
    }

    #[cfg(test)]
    pub fn start_project_test_job<F>(&mut self, work: F) -> u64
    where
        F: FnOnce() -> Result<ProjectIndex, String> + Send + 'static,
    {
        self.start_project_with(work)
    }

    #[cfg(test)]
    pub fn start_git_test_job<F>(&mut self, work: F) -> u64
    where
        F: FnOnce() -> Result<Option<GitSnapshot>, String> + Send + 'static,
    {
        self.start_git_with(work)
    }

    #[cfg(test)]
    pub fn start_git_mutation_test_job<F>(&mut self, work: F) -> Result<u64, String>
    where
        F: FnOnce() -> Result<GitMutationResult, String> + Send + 'static,
    {
        self.start_git_mutation_with(work)
    }

    #[cfg(test)]
    pub fn test_tag(&self, generation: u64) -> ServiceTag {
        ServiceTag {
            workspace_id: self.workspace_id,
            generation,
        }
    }

    #[cfg(test)]
    pub fn send_test_event(&self, event: ServiceEvent) {
        self.sender.send(event).expect("queue test service event");
    }
}

impl Drop for ServiceCoordinator {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

fn next_generation(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}

fn spawn_event<F>(name: &str, sender: SyncSender<ServiceEvent>, cancelled: Arc<AtomicBool>, work: F)
where
    F: FnOnce() -> ServiceEvent + Send + 'static,
{
    let _ = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let event = work();
            if !cancelled.load(Ordering::Acquire) {
                let _ = sender.send(event);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn blocked_worker_launch_is_non_blocking_and_old_generation_is_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let mut services = ServiceCoordinator::new();

        let first_generation = services.start_project_test_job({
            let root = root.clone();
            move || {
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                ProjectIndex::build(root).map_err(|error| error.to_string())
            }
        });
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker started without blocking its caller");
        assert!(matches!(services.try_recv(), Err(TryRecvError::Empty)));

        let second_generation = services.start_project(root);
        assert_ne!(first_generation, second_generation);
        let stale = ServiceEvent::Project {
            tag: ServiceTag {
                workspace_id: services.workspace_id,
                generation: first_generation,
            },
            result: ProjectIndex::build(directory.path()).map_err(|error| error.to_string()),
        };
        assert!(!services.is_current(&stale));
        release_sender.send(()).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let event = loop {
            match services.try_recv() {
                Ok(event) => break event,
                Err(TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(TryRecvError::Empty) => panic!("current worker did not finish before deadline"),
                Err(TryRecvError::Disconnected) => panic!("service channel disconnected"),
            }
        };
        assert!(services.is_current(&event));
        assert_eq!(event.tag().generation, second_generation);
    }

    #[test]
    fn git_mutation_is_single_flight_and_independent_from_snapshot_refresh() {
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let mut services = ServiceCoordinator::new();

        let mutation_generation = services
            .start_git_mutation_test_job(move || {
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                Ok(GitMutationResult::Staged(PathBuf::from("current.txt")))
            })
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("mutation worker started without blocking its caller");
        assert!(
            services
                .start_git_mutation_test_job(|| Ok(GitMutationResult::Unstaged(PathBuf::from(
                    "second.txt"
                ))))
                .unwrap_err()
                .contains("already running")
        );

        let snapshot_generation = services.start_git_test_job(|| Ok(None));
        let snapshot = recv_before_deadline(&services);
        assert!(matches!(snapshot, ServiceEvent::Git { .. }));
        assert!(services.is_current(&snapshot));
        assert_eq!(snapshot.tag().generation, snapshot_generation);

        release_sender.send(()).unwrap();
        let mutation = recv_before_deadline(&services);
        assert!(matches!(mutation, ServiceEvent::GitMutation { .. }));
        assert!(services.is_current(&mutation));
        assert_eq!(mutation.tag().generation, mutation_generation);
        assert!(services.finish_git_mutation(mutation_generation));
        assert!(!services.finish_git_mutation(mutation_generation));

        let next_generation = services
            .start_git_mutation_test_job(|| {
                Ok(GitMutationResult::Unstaged(PathBuf::from("second.txt")))
            })
            .unwrap();
        assert_ne!(next_generation, mutation_generation);
        let next = recv_before_deadline(&services);
        assert!(services.is_current(&next));
        assert!(services.finish_git_mutation(next_generation));
    }

    fn recv_before_deadline(services: &ServiceCoordinator) -> ServiceEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match services.try_recv() {
                Ok(event) => return event,
                Err(TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(TryRecvError::Empty) => panic!("worker did not finish before deadline"),
                Err(TryRecvError::Disconnected) => panic!("service channel disconnected"),
            }
        }
    }
}
