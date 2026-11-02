//! Executes a backup run on background threads and reports through events.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Result, anyhow};

use crate::config::{ProviderKind, is_safe_segment};
use crate::git::{Git, GitAuth};
use crate::i18n::{t, tf};
use crate::provider::{Client, Repo};

#[derive(Clone, Debug, PartialEq)]
pub enum RepoStatus {
    Idle,
    Queued,
    Running,
    Done,
    /// Some destinations succeeded while others failed.
    Partial(String),
    Failed(String),
    Cancelled,
}

#[derive(Debug)]
pub enum Event {
    Status { key: String, status: RepoStatus },
    Log { error: bool, text: String },
    Finished { done: usize, failed: usize, cancelled: bool },
}

pub struct RepoJob {
    pub key: String,
    pub repo: Repo,
    pub source: ServerAccess,
}

/// Everything needed to talk to one account's server.
#[derive(Clone)]
pub struct ServerAccess {
    pub kind: ProviderKind,
    pub base_url: String,
    pub token: String,
    pub login: String,
}

impl ServerAccess {
    fn client(&self) -> Client {
        Client::new(self.kind, &self.base_url, &self.token)
    }

    fn git_auth(&self) -> GitAuth {
        GitAuth::new(&self.base_url, &self.login, &self.token)
    }

    /// Host and port as one folder name, e.g. `nas_3000`, plus any URL path.
    fn folder(&self) -> PathBuf {
        let host = self.base_url.split_once("://").map_or(&*self.base_url, |(_, rest)| rest);
        host.split('/').map(|part| part.replace(':', "_")).collect()
    }
}

pub struct RemoteTarget {
    pub label: String,
    pub access: ServerAccess,
    pub namespace: String,
}

pub struct Plan {
    pub jobs: Vec<RepoJob>,
    pub local: Vec<PathBuf>,
    pub remote: Vec<RemoteTarget>,
    pub prune: bool,
    pub workers: usize,
}

/// Removes jobs that would copy the same repository twice, which happens when
/// two accounts can both see it, and which would race on the same folder.
pub fn dedupe_jobs(jobs: Vec<RepoJob>) -> Vec<RepoJob> {
    let mut seen = BTreeSet::new();
    jobs.into_iter()
        .filter(|job| seen.insert(job.source.client().git_url(&job.repo.owner, &job.repo.name)))
        .collect()
}

/// Folder of a repository's mirror inside a local destination.
fn mirror_path(root: &Path, job: &RepoJob) -> PathBuf {
    root.join(job.source.folder())
        .join(&job.repo.owner)
        .join(format!("{}.git", job.repo.name))
}

/// Name a repository gets on a remote destination. The owner is part of it
/// because one namespace may receive repositories from many owners.
fn remote_name(repo: &Repo) -> String {
    format!("{}-{}", repo.owner, repo.name)
}

/// Backs up one repository to all destinations and collects per-destination
/// errors, so that one bad destination does not stop the others.
fn process(git: &Git, plan: &Plan, job: &RepoJob) -> (usize, Vec<String>) {
    let source_client = job.source.client();
    let source_url = source_client.git_url(&job.repo.owner, &job.repo.name);
    let source_auth = job.source.git_auth();
    let mut errors = Vec::new();
    let mut succeeded = 0;
    // A mirror already up to date from this run, to push from.
    let mut fresh_mirror: Option<PathBuf> = None;

    for root in &plan.local {
        let target = mirror_path(root, job);
        match git.sync_mirror(Some(&source_auth), &source_url, &target, plan.prune) {
            Ok(()) => {
                succeeded += 1;
                fresh_mirror.get_or_insert(target);
            }
            Err(error) => errors.push(format!("{}: {error}", root.display())),
        }
    }

    if plan.remote.is_empty() {
        return (succeeded, errors);
    }
    // Backing up a backup would create a new copy with a longer name on every
    // run when a server is both a source and a destination.
    if job.repo.is_backup_copy() {
        errors.push(t("This repository is itself a backup, not copied to remote destinations").to_owned());
        return (succeeded, errors);
    }

    let mut _temp_dir = None;
    if fresh_mirror.is_none() {
        let prepared = (|| -> Result<PathBuf> {
            let dir = tempfile::Builder::new().prefix("git-backup-").tempdir()?;
            let path = dir.path().join("mirror.git");
            git.sync_mirror(Some(&source_auth), &source_url, &path, false)?;
            _temp_dir = Some(dir);
            Ok(path)
        })();
        match prepared {
            Ok(path) => fresh_mirror = Some(path),
            Err(error) => {
                errors.push(error.to_string());
                return (succeeded, errors);
            }
        }
    }
    let Some(mirror) = fresh_mirror else {
        return (succeeded, errors);
    };

    for target in &plan.remote {
        let result = push_to_remote(git, plan, job, target, &mirror, &source_url);
        match result {
            Ok(()) => succeeded += 1,
            Err(error) => errors.push(format!("{}: {error}", target.label)),
        }
    }
    (succeeded, errors)
}

fn push_to_remote(
    git: &Git,
    plan: &Plan,
    job: &RepoJob,
    target: &RemoteTarget,
    mirror: &Path,
    source_url: &str,
) -> Result<()> {
    if git.cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("{}", t("Cancelled")));
    }
    let client = target.access.client();
    let owner = if target.namespace.is_empty() {
        &target.access.login
    } else {
        &target.namespace
    };
    let name = remote_name(&job.repo);
    client.ensure_backup_repo(&target.access.login, owner, &name, source_url)?;
    let url = client.git_url(owner, &name);
    git.push_mirror(Some(&target.access.git_auth()), mirror, &url, plan.prune)
}

/// Checks the things that would fail every repository in the same way.
fn preflight(git: &Git, plan: &mut Plan) -> Result<()> {
    git.check_installed()?;
    for root in &plan.local {
        std::fs::create_dir_all(root)
            .map_err(|error| anyhow!("{}: {error}", root.display()))?;
    }
    for target in &mut plan.remote {
        if !target.namespace.is_empty() && !is_safe_segment(&target.namespace) {
            return Err(anyhow!(tf("Skipped invalid name: {}", &[&target.namespace])));
        }
        // Also proves the token works before any repository is downloaded.
        target.access.login = target.access.client().current_user()?;
    }
    Ok(())
}

/// Runs the plan on a worker pool. Returns immediately.
pub fn start(
    mut plan: Plan,
    cancel: Arc<AtomicBool>,
    events: Sender<Event>,
    repaint: impl Fn() + Send + Sync + 'static,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let send = |event: Event| {
            let _ = events.send(event);
            repaint();
        };
        let git = Git { cancel: cancel.clone() };
        if let Err(error) = preflight(&git, &mut plan) {
            // A cancellation falls through: the workers mark each job cancelled.
            if !cancel.load(Ordering::Relaxed) {
                let text = error.to_string();
                send(Event::Log { error: true, text: text.clone() });
                for job in &plan.jobs {
                    let status = RepoStatus::Failed(text.clone());
                    send(Event::Status { key: job.key.clone(), status });
                }
                send(Event::Finished { done: 0, failed: plan.jobs.len(), cancelled: false });
                return;
            }
        }

        let queue: Mutex<VecDeque<&RepoJob>> = Mutex::new(plan.jobs.iter().collect());
        let totals = Mutex::new((0usize, 0usize));
        std::thread::scope(|scope| {
            for _ in 0..plan.workers.clamp(1, 16) {
                scope.spawn(|| {
                    let git = Git { cancel: cancel.clone() };
                    loop {
                        let next = queue.lock().unwrap().pop_front();
                        let Some(job) = next else { break };
                        if cancel.load(Ordering::Relaxed) {
                            send(Event::Status { key: job.key.clone(), status: RepoStatus::Cancelled });
                            continue;
                        }
                        send(Event::Status { key: job.key.clone(), status: RepoStatus::Running });
                        let (succeeded, errors) = process(&git, &plan, job);
                        let name = job.repo.full_name();
                        let status = if cancel.load(Ordering::Relaxed) && !errors.is_empty() {
                            RepoStatus::Cancelled
                        } else if errors.is_empty() {
                            RepoStatus::Done
                        } else if succeeded > 0 {
                            RepoStatus::Partial(errors.join("\n"))
                        } else {
                            RepoStatus::Failed(errors.join("\n"))
                        };
                        {
                            let mut totals = totals.lock().unwrap();
                            match status {
                                RepoStatus::Done => totals.0 += 1,
                                RepoStatus::Cancelled => {}
                                _ => totals.1 += 1,
                            }
                        }
                        if status != RepoStatus::Cancelled {
                            if errors.is_empty() {
                                send(Event::Log { error: false, text: tf("Backed up {}", &[&name]) });
                            }
                            for error in &errors {
                                send(Event::Log { error: true, text: format!("{name}: {error}") });
                            }
                        }
                        send(Event::Status { key: job.key.clone(), status });
                    }
                });
            }
        });
        let (done, failed) = *totals.lock().unwrap();
        send(Event::Finished { done, failed, cancelled: cancel.load(Ordering::Relaxed) });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc::channel;

    fn repo(owner: &str, name: &str) -> Repo {
        Repo {
            owner: owner.into(),
            name: name.into(),
            private: false,
            fork: false,
            archived: false,
            description: String::new(),
        }
    }

    fn access(base_url: &str) -> ServerAccess {
        ServerAccess {
            kind: ProviderKind::Gitea,
            base_url: base_url.into(),
            token: "t".into(),
            login: "me".into(),
        }
    }

    fn job(base_url: &str, owner: &str, name: &str) -> RepoJob {
        RepoJob {
            key: format!("{owner}/{name}"),
            repo: repo(owner, name),
            source: access(base_url),
        }
    }

    #[test]
    fn mirror_paths_separate_servers_and_owners() {
        let root = Path::new("backup");
        let path = mirror_path(root, &job("http://nas:3000/gitea", "o", "r"));
        assert_eq!(path, root.join("nas_3000").join("gitea").join("o").join("r.git"));
    }

    #[test]
    fn same_repository_seen_by_two_accounts_runs_once() {
        let jobs = vec![
            job("https://a", "o", "r"),
            job("https://a", "o", "r"),
            job("https://b", "o", "r"),
        ];
        assert_eq!(dedupe_jobs(jobs).len(), 2);
    }

    #[test]
    fn backup_copies_are_not_copied_to_remote_destinations() {
        let mut copy = repo("me", "a-b");
        copy.description = "Backup of https://src/a/b.git".into();
        assert!(copy.is_backup_copy());
        assert!(!repo("me", "a-b").is_backup_copy());
    }

    /// Runs the whole pipeline against an unreachable server: each repository
    /// must fail individually, and the run must still finish and count them.
    #[test]
    fn run_reports_failures_and_finishes() {
        let root = tempfile::tempdir().unwrap();
        let plan = Plan {
            jobs: vec![job("http://127.0.0.1:1", "o", "a"), job("http://127.0.0.1:1", "o", "b")],
            local: vec![root.path().join("one"), root.path().join("two")],
            remote: vec![],
            prune: false,
            workers: 2,
        };
        let (sender, receiver) = channel();
        start(plan, Arc::new(AtomicBool::new(false)), sender, || {});
        let events: Vec<Event> = receiver.iter().collect();
        assert!(matches!(
            events.last(),
            Some(Event::Finished { done: 0, failed: 2, cancelled: false })
        ));
        let failures = events
            .iter()
            .filter(|event| matches!(event, Event::Status { status: RepoStatus::Failed(_), .. }))
            .count();
        assert_eq!(failures, 2);
        assert!(Command::new("git").arg("--version").output().is_ok());
    }

    #[test]
    fn cancelled_run_marks_remaining_jobs() {
        let root = tempfile::tempdir().unwrap();
        let plan = Plan {
            jobs: vec![job("http://127.0.0.1:1", "o", "a")],
            local: vec![root.path().to_path_buf()],
            remote: vec![],
            prune: false,
            workers: 1,
        };
        let (sender, receiver) = channel();
        start(plan, Arc::new(AtomicBool::new(true)), sender, || {});
        let events: Vec<Event> = receiver.iter().collect();
        assert!(matches!(events.last(), Some(Event::Finished { cancelled: true, .. })));
        assert!(events.iter().any(|event| {
            matches!(event, Event::Status { status: RepoStatus::Cancelled, .. })
        }));
    }
}
