//! End-to-end tests of a whole backup run. Real git clients talk over HTTP to
//! the real `git http-backend`; only the REST API is simulated, following
//! Gitea's routes. Git requests without the expected credentials get a 401.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::json;

use crate::backup::{Event, Plan, RemoteTarget, RepoJob, RepoStatus, ServerAccess, start};
use crate::config::ProviderKind;
use crate::provider::{Client, Repo};

#[derive(Clone)]
struct RepoMeta {
    description: String,
    private: bool,
}

struct ServerState {
    root: PathBuf,
    login: String,
    token: String,
    repos: Mutex<BTreeMap<String, RepoMeta>>,
}

struct FakeServer {
    base_url: String,
    state: Arc<ServerState>,
}

impl FakeServer {
    fn start(root: &Path, login: &str, token: &str, port: u16) -> Self {
        let root = root.join(format!("server-{login}"));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(ServerState {
            root,
            login: login.to_owned(),
            token: token.to_owned(),
            repos: Mutex::default(),
        });
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || handle(stream, &state));
            }
        });
        Self { base_url, state }
    }

    fn git_dir(&self, full_name: &str) -> PathBuf {
        self.state.root.join(format!("{full_name}.git"))
    }

    fn access(&self, token: &str) -> ServerAccess {
        ServerAccess {
            kind: ProviderKind::Gitea,
            base_url: self.base_url.clone(),
            token: token.to_owned(),
            login: self.state.login.clone(),
        }
    }

    /// Adds a bare repository that has main, feature, a tag and a read-only
    /// style pull request ref, and returns its work tree for later commits.
    fn add_source_repo(&self, owner: &str, name: &str) -> PathBuf {
        let full = format!("{owner}/{name}");
        let bare = self.git_dir(&full);
        create_bare(&bare);
        self.state.repos.lock().unwrap().insert(
            full,
            RepoMeta { description: "demo project".to_owned(), private: true },
        );
        let work = self.state.root.join(format!("work-{owner}-{name}"));
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "first").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "first"]);
        git(&work, &["tag", "v1"]);
        git(&work, &["branch", "feature"]);
        git(&work, &["update-ref", "refs/pull/1/head", "HEAD"]);
        git(&work, &["push", "-q", bare.to_str().unwrap(), "refs/*:refs/*"]);
        work
    }

    fn add_commit(&self, work: &Path, owner: &str, name: &str) -> String {
        std::fs::write(work.join("file.txt"), "second").unwrap();
        git(work, &["add", "."]);
        git(work, &["commit", "-qm", "second"]);
        let bare = self.git_dir(&format!("{owner}/{name}"));
        git(work, &["push", "-q", bare.to_str().unwrap(), "main"]);
        git(work, &["rev-parse", "HEAD"])
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn create_bare(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "--bare", "-b", "main"]);
    // Lets the server's own unauthenticated test pushes through.
    git(path, &["config", "http.receivepack", "true"]);
}

fn refs(git_dir: &Path) -> String {
    let dir = git_dir.to_str().unwrap();
    git(git_dir, &["--git-dir", dir, "for-each-ref", "--format=%(refname)"])
}

fn rev(git_dir: &Path, name: &str) -> String {
    git(git_dir, &["--git-dir", git_dir.to_str().unwrap(), "rev-parse", name])
}

fn respond(stream: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn handle(mut stream: TcpStream, state: &ServerState) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() {
            return;
        }
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.to_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    if path.starts_with("/api/v1/") {
        handle_api(&mut stream, state, &method, path, query, &headers, &body);
    } else {
        handle_git(&mut stream, state, &method, path, query, &headers, &body);
    }
}

fn handle_api(
    stream: &mut TcpStream,
    state: &ServerState,
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) {
    let json_header = "Content-Type: application/json\r\n";
    if headers.get("authorization").map(String::as_str) != Some(&format!("token {}", state.token)) {
        respond(stream, 401, json_header, br#"{"message":"bad token"}"#);
        return;
    }
    let route = path.trim_start_matches("/api/v1");
    let repo_json = |full: &str, meta: &RepoMeta| {
        json!({"full_name": full, "private": meta.private, "description": meta.description})
    };
    let reply = match (method, route) {
        ("GET", "/user") => (200, json!({"login": state.login})),
        ("GET", "/user/orgs") => (200, json!([])),
        ("GET", "/user/repos") => {
            let first_page = query.split('&').any(|part| part == "page=1");
            let repos = state.repos.lock().unwrap();
            let list: Vec<_> = repos
                .iter()
                .filter(|_| first_page)
                .map(|(full, meta)| repo_json(full, meta))
                .collect();
            (200, json!(list))
        }
        ("POST", "/user/repos") => {
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            let full = format!("{}/{}", state.login, request["name"].as_str().unwrap());
            create_bare(&state.root.join(format!("{full}.git")));
            state.repos.lock().unwrap().insert(
                full,
                RepoMeta {
                    description: request["description"].as_str().unwrap().to_owned(),
                    private: request["private"].as_bool().unwrap(),
                },
            );
            (201, json!({}))
        }
        ("PATCH", route) if route.starts_with("/repos/") => {
            let full = route.trim_start_matches("/repos/");
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            let mut repos = state.repos.lock().unwrap();
            let meta = repos.get_mut(full).unwrap();
            meta.private = request["private"].as_bool().unwrap();
            (200, repo_json(full, meta))
        }
        ("GET", route) if route.starts_with("/repos/") => {
            let full = route.trim_start_matches("/repos/");
            match state.repos.lock().unwrap().get(full) {
                Some(meta) => (200, repo_json(full, meta)),
                None => (404, json!({"message": "not found"})),
            }
        }
        _ => (404, json!({"message": "no route"})),
    };
    respond(stream, reply.0, json_header, reply.1.to_string().as_bytes());
}

/// Checks credentials like a real server, then runs `git http-backend`.
fn handle_git(
    stream: &mut TcpStream,
    state: &ServerState,
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) {
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", state.login, state.token));
    if headers.get("authorization").map(String::as_str) != Some(&format!("Basic {encoded}")) {
        respond(stream, 401, "WWW-Authenticate: Basic realm=\"git\"\r\n", b"");
        return;
    }
    let header = |name: &str| headers.get(name).cloned().unwrap_or_default();
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", &state.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REMOTE_USER", &state.login)
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = body.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().unwrap();
    let _ = writer.join();

    let split = output.stdout.windows(4).position(|w| w == b"\r\n\r\n");
    let Some(split) = split else {
        respond(stream, 500, "", b"bad cgi output");
        return;
    };
    let cgi_headers = String::from_utf8_lossy(&output.stdout[..split]).into_owned();
    let mut status = 200;
    let mut forwarded = String::new();
    for line in cgi_headers.lines() {
        match line.strip_prefix("Status:") {
            Some(value) => {
                status = value.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(500);
            }
            None => forwarded.push_str(&format!("{line}\r\n")),
        }
    }
    respond(stream, status, &forwarded, &output.stdout[split + 4..]);
}

fn job(server: &FakeServer, token: &str, owner: &str, name: &str) -> RepoJob {
    RepoJob {
        key: format!("{owner}/{name}"),
        repo: Repo {
            owner: owner.to_owned(),
            name: name.to_owned(),
            private: true,
            fork: false,
            archived: false,
            description: "demo project".to_owned(),
        },
        source: server.access(token),
    }
}

fn remote(server: &FakeServer, token: &str) -> RemoteTarget {
    let mut access = server.access(token);
    access.login = String::new(); // The run must resolve this by itself.
    RemoteTarget { label: "dest".to_owned(), access, namespace: String::new() }
}

/// Runs a plan to the end and returns the final status of each job.
fn run(plan: Plan) -> (BTreeMap<String, RepoStatus>, Vec<Event>) {
    let (sender, receiver) = channel();
    let handle = start(plan, Arc::new(AtomicBool::new(false)), sender, || {});
    let events: Vec<Event> = receiver.iter().collect();
    handle.join().unwrap();
    let mut statuses = BTreeMap::new();
    for event in &events {
        if let Event::Status { key, status } = event {
            statuses.insert(key.clone(), status.clone());
        }
    }
    (statuses, events)
}

fn tree_contains(dir: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).unwrap().flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            tree_contains(&path, needle)
        } else {
            std::fs::read(&path).unwrap().windows(needle.len()).any(|w| w == needle)
        }
    })
}

#[test]
fn backs_up_to_local_and_remote_then_updates_both() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 0);
    let dest = FakeServer::start(root.path(), "bob", "dest-secret", 0);
    let work = source.add_source_repo("alice", "proj");
    let folder = root.path().join("backup");
    let plan = || Plan {
        jobs: vec![job(&source, "source-secret", "alice", "proj")],
        local: vec![folder.clone()],
        remote: vec![remote(&dest, "dest-secret")],
        prune: false,
        workers: 2,
    };

    let (statuses, _) = run(plan());
    assert_eq!(statuses["alice/proj"], RepoStatus::Done, "{statuses:?}");

    let host = source.base_url.trim_start_matches("http://").replace(':', "_");
    let mirror = folder.join(host).join("alice").join("proj.git");
    let mirror_refs = refs(&mirror);
    assert!(mirror_refs.contains("refs/heads/feature"));
    assert!(mirror_refs.contains("refs/tags/v1"));
    assert!(mirror_refs.contains("refs/pull/1/head"), "local backup keeps every ref");

    let pushed = dest.git_dir("bob/alice-proj");
    let pushed_refs = refs(&pushed);
    assert!(pushed_refs.contains("refs/heads/main"));
    assert!(pushed_refs.contains("refs/heads/feature"));
    assert!(pushed_refs.contains("refs/tags/v1"));
    assert!(!pushed_refs.contains("refs/pull"), "pull refs are read-only on servers");

    let meta = dest.state.repos.lock().unwrap()["bob/alice-proj"].clone();
    assert!(meta.private, "a private source must get a private backup");
    assert_eq!(meta.description, format!("Backup of {}/alice/proj.git", source.base_url));

    // Nothing written to disk may contain the credentials.
    let basic = base64::engine::general_purpose::STANDARD.encode("alice:source-secret");
    assert!(!tree_contains(&mirror, b"source-secret"));
    assert!(!tree_contains(&mirror, basic.as_bytes()));

    let new_head = source.add_commit(&work, "alice", "proj");
    let (statuses, _) = run(plan());
    assert_eq!(statuses["alice/proj"], RepoStatus::Done, "{statuses:?}");
    assert_eq!(rev(&mirror, "refs/heads/main"), new_head);
    assert_eq!(rev(&pushed, "refs/heads/main"), new_head);
}

#[test]
fn remote_only_destination_needs_no_local_folder() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 0);
    let dest = FakeServer::start(root.path(), "bob", "dest-secret", 0);
    source.add_source_repo("alice", "proj");
    let (statuses, _) = run(Plan {
        jobs: vec![job(&source, "source-secret", "alice", "proj")],
        local: vec![],
        remote: vec![remote(&dest, "dest-secret")],
        prune: false,
        workers: 1,
    });
    assert_eq!(statuses["alice/proj"], RepoStatus::Done, "{statuses:?}");
    assert!(refs(&dest.git_dir("bob/alice-proj")).contains("refs/heads/main"));
}

#[test]
fn backup_visibility_follows_the_source() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 0);
    let dest = FakeServer::start(root.path(), "bob", "dest-secret", 0);
    source.add_source_repo("alice", "proj");
    let plan = |private: bool| {
        let mut job = job(&source, "source-secret", "alice", "proj");
        job.repo.private = private;
        Plan {
            jobs: vec![job],
            local: vec![],
            remote: vec![remote(&dest, "dest-secret")],
            prune: false,
            workers: 1,
        }
    };
    // A public source, then the same source turned private, then public again.
    for private in [false, true, false] {
        let (statuses, _) = run(plan(private));
        assert_eq!(statuses["alice/proj"], RepoStatus::Done, "{statuses:?}");
        assert_eq!(dest.state.repos.lock().unwrap()["bob/alice-proj"].private, private);
    }
}

#[test]
fn wrong_source_token_fails_without_prompting_or_leftovers() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 0);
    source.add_source_repo("alice", "proj");
    let folder = root.path().join("backup");
    let started = std::time::Instant::now();
    let (statuses, events) = run(Plan {
        jobs: vec![job(&source, "wrong", "alice", "proj")],
        local: vec![folder.clone()],
        remote: vec![],
        prune: false,
        workers: 1,
    });
    assert!(started.elapsed().as_secs() < 30, "must fail fast, not wait on a prompt");
    assert!(matches!(statuses["alice/proj"], RepoStatus::Failed(_)), "{statuses:?}");
    assert!(matches!(events.last(), Some(Event::Finished { failed: 1, .. })));
    let host = source.base_url.trim_start_matches("http://").replace(':', "_");
    let owner_dir = folder.join(host).join("alice");
    assert_eq!(std::fs::read_dir(&owner_dir).unwrap().count(), 0, "no partial download remains");
}

#[test]
fn wrong_destination_token_stops_before_downloading() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 0);
    let dest = FakeServer::start(root.path(), "bob", "dest-secret", 0);
    source.add_source_repo("alice", "proj");
    let folder = root.path().join("backup");
    let (statuses, _) = run(Plan {
        jobs: vec![job(&source, "source-secret", "alice", "proj")],
        local: vec![folder.clone()],
        remote: vec![remote(&dest, "wrong")],
        prune: false,
        workers: 1,
    });
    assert!(matches!(statuses["alice/proj"], RepoStatus::Failed(_)), "{statuses:?}");
    assert!(!folder.join("anything").exists());
}

#[test]
fn unrelated_project_at_destination_is_left_alone_and_local_still_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 0);
    let dest = FakeServer::start(root.path(), "bob", "dest-secret", 0);
    source.add_source_repo("alice", "proj");
    // The destination user already has their own project under the same name.
    dest.add_source_repo("bob", "alice-proj");
    let existing = dest.git_dir("bob/alice-proj");
    let before = rev(&existing, "refs/heads/main");

    let (statuses, _) = run(Plan {
        jobs: vec![job(&source, "source-secret", "alice", "proj")],
        local: vec![root.path().join("backup")],
        remote: vec![remote(&dest, "dest-secret")],
        prune: false,
        workers: 1,
    });
    assert!(matches!(&statuses["alice/proj"], RepoStatus::Partial(_)), "{statuses:?}");
    assert_eq!(rev(&existing, "refs/heads/main"), before);
}

#[test]
fn backup_copies_do_not_multiply_when_a_server_is_source_and_destination() {
    let root = tempfile::tempdir().unwrap();
    let server = FakeServer::start(root.path(), "alice", "secret", 0);
    server.add_source_repo("alice", "proj");
    let (statuses, _) = run(Plan {
        jobs: vec![job(&server, "secret", "alice", "proj")],
        local: vec![],
        remote: vec![remote(&server, "secret")],
        prune: false,
        workers: 1,
    });
    assert_eq!(statuses["alice/proj"], RepoStatus::Done, "{statuses:?}");

    // Select all on the next run now also lists the backup itself.
    let listing = Client::new(ProviderKind::Gitea, &server.base_url, "secret").list_repos().unwrap();
    let names: Vec<String> = listing.repos.iter().map(Repo::full_name).collect();
    assert_eq!(names, ["alice/alice-proj", "alice/proj"]);
    let jobs = listing
        .repos
        .into_iter()
        .map(|repo| RepoJob { key: repo.full_name(), repo, source: server.access("secret") })
        .collect();
    let (statuses, _) = run(Plan {
        jobs,
        local: vec![],
        remote: vec![remote(&server, "secret")],
        prune: false,
        workers: 1,
    });
    assert_eq!(statuses["alice/proj"], RepoStatus::Done, "{statuses:?}");
    assert!(matches!(statuses["alice/alice-proj"], RepoStatus::Failed(_)));
    assert!(!server.git_dir("alice/alice-alice-proj").exists());
}

/// Not a test of its own: serves two demo servers on fixed ports so the GUI
/// can be tried by hand. Run with `cargo test demo_servers -- --ignored`.
#[test]
#[ignore]
fn demo_servers() {
    let root = tempfile::tempdir().unwrap();
    let source = FakeServer::start(root.path(), "alice", "source-secret", 3901);
    let _dest = FakeServer::start(root.path(), "bob", "dest-secret", 3902);
    for name in ["website", "api-server", "mobile-app", "docs", "tools", "infra"] {
        source.add_source_repo("alice", name);
    }
    crate::config::store_token("demo-source", "source-secret").unwrap();
    crate::config::store_token("demo-dest", "dest-secret").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(600));
}
