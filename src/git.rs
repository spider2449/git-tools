//! Runs the `git` command line with credentials that are never written to
//! disk or exposed on a command line.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;

use crate::i18n::{t, tf};

/// Git refs that a backup pushes. A plain `--mirror` push would also send
/// `refs/pull/*`, which GitHub and Gitea reject as read-only.
const PUSH_REFSPECS: [&str; 3] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/notes/*:refs/notes/*",
];

/// Credentials for the single server a git command talks to.
#[derive(Clone)]
pub struct GitAuth {
    /// Server address; the header is only sent to URLs under it.
    pub scope_url: String,
    pub header: String,
}

impl GitAuth {
    pub fn new(base_url: &str, login: &str, token: &str) -> Self {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{login}:{token}"));
        Self {
            scope_url: format!("{}/", base_url.trim_end_matches('/')),
            header: format!("Authorization: Basic {encoded}"),
        }
    }
}

/// Drains a child process pipe on a background thread.
fn drain(stream: impl Read + Send + 'static) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = buffer.clone();
    let mut stream = stream;
    let handle = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(count) = stream.read(&mut chunk) {
            if count == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..count]);
        }
    });
    (buffer, handle)
}

pub struct Git {
    pub cancel: Arc<AtomicBool>,
}

impl Git {
    fn command(&self, auth: Option<&GitAuth>) -> Command {
        let mut settings: Vec<(String, String)> = vec![
            // Fail instead of opening a credential prompt or dialog.
            ("credential.helper".into(), String::new()),
            ("core.askPass".into(), String::new()),
            // Abort transfers that stall for five minutes.
            ("http.lowSpeedLimit".into(), "1024".into()),
            ("http.lowSpeedTime".into(), "300".into()),
            ("core.longpaths".into(), "true".into()),
        ];
        if let Some(auth) = auth {
            settings.push((format!("http.{}.extraHeader", auth.scope_url), auth.header.clone()));
        }

        let mut command = Command::new("git");
        command
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_CONFIG_COUNT", settings.len().to_string())
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        for (index, (key, value)) in settings.iter().enumerate() {
            command.env(format!("GIT_CONFIG_KEY_{index}"), key);
            command.env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
    }

    /// Runs git until it exits or the run is cancelled, then returns stdout.
    fn run(&self, auth: Option<&GitAuth>, args: &[&str]) -> Result<String> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("{}", t("Cancelled"));
        }
        let mut command = self.command(auth);
        command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| t("Cannot run git. Please check that Git is installed and on PATH"))?;
        let (stdout, stdout_reader) = drain(child.stdout.take().context("no stdout pipe")?);
        let (stderr, stderr_reader) = drain(child.stderr.take().context("no stderr pipe")?);

        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if self.cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                // Readers are not joined: a surviving helper process can hold
                // the pipes open, and they end by themselves when it exits.
                bail!("{}", t("Cancelled"));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();

        if status.success() {
            return Ok(String::from_utf8_lossy(&stdout.lock().unwrap()).into_owned());
        }
        let stderr = String::from_utf8_lossy(&stderr.lock().unwrap()).trim().to_owned();
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = &lines[lines.len().saturating_sub(4)..];
        Err(anyhow!(tail.join(" | ")))
    }

    pub fn check_installed(&self) -> Result<()> {
        self.run(None, &["--version"]).map(|_| ())
    }

    /// Creates or updates the bare mirror at `target`. A first download goes
    /// to a `.partial` folder and is renamed when complete, so an interrupted
    /// run never leaves something that looks like a finished backup.
    pub fn sync_mirror(
        &self,
        auth: Option<&GitAuth>,
        url: &str,
        target: &Path,
        prune: bool,
    ) -> Result<()> {
        let target_str = target.to_string_lossy();
        if target.exists() {
            let origin = self
                .run(None, &["--git-dir", &target_str, "config", "--get", "remote.origin.url"])
                .map_err(|_| anyhow!(tf("{} exists but is not a git mirror", &[&target_str])))?;
            // Never fetch another repository into this one: the mirror
            // refspec would replace its refs.
            if origin.trim() != url {
                bail!(tf(
                    "{} is a mirror of another repository: {}",
                    &[&target_str, origin.trim()]
                ));
            }
            let mut args = vec!["--git-dir", &target_str, "fetch", "--quiet"];
            if prune {
                args.push("--prune");
            }
            args.push("origin");
            self.run(auth, &args)?;
            return Ok(());
        }

        let mut partial = target.as_os_str().to_owned();
        partial.push(".partial");
        let partial = Path::new(&partial);
        if partial.exists() {
            std::fs::remove_dir_all(partial)
                .with_context(|| format!("cannot remove {}", partial.display()))?;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let partial_str = partial.to_string_lossy();
        let result = self.run(auth, &["clone", "--quiet", "--mirror", "--", url, &partial_str]);
        match result {
            Ok(_) => std::fs::rename(partial, target)
                .with_context(|| format!("cannot move into {}", target.display())),
            Err(error) => {
                let _ = std::fs::remove_dir_all(partial);
                Err(error)
            }
        }
    }

    /// Pushes branches, tags and notes from a local mirror to `url`.
    pub fn push_mirror(
        &self,
        auth: Option<&GitAuth>,
        mirror: &Path,
        url: &str,
        prune: bool,
    ) -> Result<()> {
        let mirror_str = mirror.to_string_lossy();
        let refs = self.run(
            None,
            &["--git-dir", &mirror_str, "for-each-ref", "--count=1", "refs/heads", "refs/tags"],
        )?;
        if refs.trim().is_empty() {
            return Ok(()); // An empty repository has nothing to push.
        }
        let mut args = vec!["--git-dir", &mirror_str, "push", "--quiet"];
        if prune {
            args.push("--prune");
        }
        args.push("--");
        args.push(url);
        args.extend(PUSH_REFSPECS);
        self.run(auth, &args).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn git(cancel: bool) -> Git {
        Git { cancel: Arc::new(AtomicBool::new(cancel)) }
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Creates a working repository with one commit, one tag and a `feature`
    /// branch, and returns its path.
    fn make_source(root: &Path) -> PathBuf {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        sh(&source, &["init", "-q", "-b", "main"]);
        std::fs::write(source.join("a.txt"), "one").unwrap();
        sh(&source, &["add", "."]);
        sh(&source, &["commit", "-qm", "first"]);
        sh(&source, &["tag", "v1"]);
        sh(&source, &["branch", "feature"]);
        source
    }

    fn refs(git_dir: &Path) -> String {
        sh(git_dir, &["--git-dir", git_dir.to_str().unwrap(), "for-each-ref", "--format=%(refname)"])
    }

    fn url(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn mirror_is_created_then_updated() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let target = root.path().join("backup/owner/repo.git");
        let git = git(false);

        git.sync_mirror(None, &url(&source), &target, false).unwrap();
        assert!(target.join("HEAD").exists());
        assert!(!root.path().join("backup/owner/repo.git.partial").exists());
        assert!(refs(&target).contains("refs/tags/v1"));

        std::fs::write(source.join("b.txt"), "two").unwrap();
        sh(&source, &["add", "."]);
        sh(&source, &["commit", "-qm", "second"]);
        git.sync_mirror(None, &url(&source), &target, false).unwrap();
        let head = |dir: &Path| {
            sh(dir, &["--git-dir", dir.to_str().unwrap(), "rev-parse", "refs/heads/main"])
        };
        assert_eq!(head(&target), head(&source.join(".git")));
    }

    #[test]
    fn deleted_branch_is_kept_unless_pruning() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let target = root.path().join("repo.git");
        let git = git(false);
        git.sync_mirror(None, &url(&source), &target, false).unwrap();

        sh(&source, &["branch", "-qD", "feature"]);
        git.sync_mirror(None, &url(&source), &target, false).unwrap();
        assert!(refs(&target).contains("refs/heads/feature"));
        git.sync_mirror(None, &url(&source), &target, true).unwrap();
        assert!(!refs(&target).contains("refs/heads/feature"));
    }

    #[test]
    fn mirror_of_another_repository_is_not_touched() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let target = root.path().join("repo.git");
        let git = git(false);
        git.sync_mirror(None, &url(&source), &target, false).unwrap();
        let before = refs(&target);

        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-q", "--bare"]);
        assert!(git.sync_mirror(None, &url(&other), &target, true).is_err());
        assert_eq!(refs(&target), before);
    }

    #[test]
    fn existing_non_git_folder_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let target = root.path().join("repo.git");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "data").unwrap();
        assert!(git(false).sync_mirror(None, &url(&source), &target, false).is_err());
        assert!(target.join("keep.txt").exists());
    }

    #[test]
    fn failed_clone_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("repo.git");
        let missing = url(&root.path().join("does-not-exist"));
        assert!(git(false).sync_mirror(None, &missing, &target, false).is_err());
        assert!(!target.exists());
        assert!(!root.path().join("repo.git.partial").exists());
    }

    #[test]
    fn stale_partial_download_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let target = root.path().join("repo.git");
        std::fs::create_dir_all(root.path().join("repo.git.partial/junk")).unwrap();
        git(false).sync_mirror(None, &url(&source), &target, false).unwrap();
        assert!(target.join("HEAD").exists());
        assert!(!root.path().join("repo.git.partial").exists());
    }

    #[test]
    fn push_sends_branches_and_tags_but_not_other_refs() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        // Simulate the read-only pull request refs a server exposes.
        sh(&source, &["update-ref", "refs/pull/1/head", "HEAD"]);
        let mirror = root.path().join("mirror.git");
        let git = git(false);
        git.sync_mirror(None, &url(&source), &mirror, false).unwrap();
        assert!(refs(&mirror).contains("refs/pull/1/head"));

        let dest = root.path().join("dest.git");
        std::fs::create_dir_all(&dest).unwrap();
        sh(&dest, &["init", "-q", "--bare"]);
        git.push_mirror(None, &mirror, &url(&dest), false).unwrap();
        let pushed = refs(&dest);
        assert!(pushed.contains("refs/heads/main"));
        assert!(pushed.contains("refs/heads/feature"));
        assert!(pushed.contains("refs/tags/v1"));
        assert!(!pushed.contains("refs/pull"));
    }

    #[test]
    fn push_prunes_only_when_asked() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let mirror = root.path().join("mirror.git");
        let dest = root.path().join("dest.git");
        std::fs::create_dir_all(&dest).unwrap();
        sh(&dest, &["init", "-q", "--bare"]);
        let git = git(false);
        git.sync_mirror(None, &url(&source), &mirror, false).unwrap();
        git.push_mirror(None, &mirror, &url(&dest), false).unwrap();

        sh(&source, &["branch", "-qD", "feature"]);
        git.sync_mirror(None, &url(&source), &mirror, true).unwrap();
        git.push_mirror(None, &mirror, &url(&dest), false).unwrap();
        assert!(refs(&dest).contains("refs/heads/feature"));
        git.push_mirror(None, &mirror, &url(&dest), true).unwrap();
        assert!(!refs(&dest).contains("refs/heads/feature"));
    }

    #[test]
    fn empty_repository_is_mirrored_and_push_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("empty.git");
        std::fs::create_dir_all(&source).unwrap();
        sh(&source, &["init", "-q", "--bare"]);
        let mirror = root.path().join("mirror.git");
        let git = git(false);
        git.sync_mirror(None, &url(&source), &mirror, false).unwrap();
        git.push_mirror(None, &mirror, "http://127.0.0.1:1/never-contacted.git", false).unwrap();
    }

    #[test]
    fn cancelled_run_does_nothing() {
        let root = tempfile::tempdir().unwrap();
        let source = make_source(root.path());
        let target = root.path().join("repo.git");
        assert!(git(true).sync_mirror(None, &url(&source), &target, false).is_err());
        assert!(!target.exists());
    }

    /// Cancelling must stop a transfer that is stuck on an unresponsive server
    /// and must not leave a partial download behind.
    #[test]
    fn cancel_stops_a_stuck_transfer() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("repo.git");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hung_url = format!("http://{}/o/r.git", listener.local_addr().unwrap());
        // Accepts connections but never answers them.
        std::thread::spawn(move || {
            let _connections: Vec<_> = listener.incoming().collect();
        });

        let git = git(false);
        let cancel = git.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            cancel.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let error = git.sync_mirror(None, &hung_url, &target, false).unwrap_err();
        assert_eq!(error.to_string(), t("Cancelled"));
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(!target.exists());
        assert!(!root.path().join("repo.git.partial").exists());
    }

    #[test]
    fn auth_header_is_scoped_and_token_is_not_on_the_command_line() {
        let auth = GitAuth::new("https://example.com/gitea/", "me", "tok");
        assert_eq!(auth.scope_url, "https://example.com/gitea/");
        assert_eq!(auth.header, "Authorization: Basic bWU6dG9r");
        let command = git(false).command(Some(&auth));
        assert!(command.get_args().next().is_none());
    }
}
