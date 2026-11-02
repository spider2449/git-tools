//! REST access to GitHub (including Enterprise Server) and Gitea.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;
use ureq::Agent;
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{RootCerts, TlsConfig};

use crate::config::{ProviderKind, is_safe_segment};
use crate::i18n::tf;

/// Prefix of the description on repositories this tool creates. It identifies
/// which source a backup repository belongs to.
pub const BACKUP_MARKER: &str = "Backup of ";

const MAX_PAGES: u32 = 1000;

#[derive(Clone, Debug)]
pub struct Repo {
    pub owner: String,
    pub name: String,
    pub private: bool,
    pub fork: bool,
    pub archived: bool,
    pub description: String,
}

impl Repo {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Whether this repository is itself a backup made by this tool.
    pub fn is_backup_copy(&self) -> bool {
        self.description.starts_with(BACKUP_MARKER)
    }
}

#[derive(Deserialize)]
struct ApiRepo {
    full_name: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    fork: bool,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Deserialize)]
struct ApiOrg {
    username: String,
}

/// A repository listing, plus problems that did not stop it.
pub struct Listing {
    pub login: String,
    pub repos: Vec<Repo>,
    pub warnings: Vec<String>,
}

pub struct Client {
    kind: ProviderKind,
    base_url: String,
    token: String,
    agent: Agent,
}

impl Client {
    pub fn new(kind: ProviderKind, base_url: &str, token: &str) -> Self {
        // The platform verifier honors company CAs for self-hosted servers.
        // Authorization headers are dropped on redirects, so a redirect can
        // never hand the token to another host.
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(Some(Duration::from_secs(120)))
            .redirect_auth_headers(RedirectAuthHeaders::Never)
            .user_agent("git-backup")
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        Self {
            kind,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            agent,
        }
    }

    fn api_url(&self, path: &str) -> String {
        match self.kind {
            ProviderKind::GitHub if self.base_url == "https://github.com" => {
                format!("https://api.github.com{path}")
            }
            ProviderKind::GitHub => format!("{}/api/v3{path}", self.base_url),
            ProviderKind::Gitea => format!("{}/api/v1{path}", self.base_url),
        }
    }

    /// Git URL of a repository. It is built from the configured server rather
    /// than taken from the API's `clone_url`, because the token is sent to
    /// this URL and must only ever go to the server the user configured.
    pub fn git_url(&self, owner: &str, name: &str) -> String {
        format!("{}/{owner}/{name}.git", self.base_url)
    }

    fn authorization(&self) -> String {
        match self.kind {
            ProviderKind::GitHub => format!("Bearer {}", self.token),
            ProviderKind::Gitea => format!("token {}", self.token),
        }
    }

    fn request(
        &self,
        path: &str,
        post_body: Option<serde_json::Value>,
    ) -> Result<(u16, String)> {
        let url = self.api_url(path);
        let auth = self.authorization();
        let accept = "application/vnd.github+json, application/json";
        let response = match post_body {
            Some(body) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Accept", accept)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            None => self
                .agent
                .get(&url)
                .header("Authorization", &auth)
                .header("Accept", accept)
                .call(),
        };
        let mut response = response.map_err(|error| {
            anyhow!(tf("Cannot connect to {}: {}", &[&self.base_url, &error.to_string()]))
        })?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let (status, text) = self.request(path, None)?;
        if status != 200 {
            bail!(self.status_error(status, &text));
        }
        serde_json::from_str(&text).map_err(|error| {
            anyhow!(tf("Unexpected response from {}: {}", &[&self.base_url, &error.to_string()]))
        })
    }

    fn status_error(&self, status: u16, body: &str) -> String {
        let detail = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| body.chars().take(200).collect());
        match status {
            401 => tf("The token was rejected by {} (HTTP 401): {}", &[&self.base_url, &detail]),
            _ => tf("{} returned HTTP {}: {}", &[&self.base_url, &status.to_string(), &detail]),
        }
    }

    /// Reads every page of a list endpoint. The page size is not used to
    /// detect the end, because Gitea servers can cap it below what was asked.
    fn get_all<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Vec<T>> {
        let page_size = match self.kind {
            ProviderKind::GitHub => "per_page=100",
            ProviderKind::Gitea => "limit=50",
        };
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let items: Vec<T> = self.get_json(&format!("{path}{separator}{page_size}&page={page}"))?;
            if items.is_empty() {
                return Ok(all);
            }
            all.extend(items);
        }
        bail!(tf("{} returned too many pages", &[&self.base_url]))
    }

    pub fn current_user(&self) -> Result<String> {
        Ok(self.get_json::<ApiUser>("/user")?.login)
    }

    pub fn list_repos(&self) -> Result<Listing> {
        let login = self.current_user()?;
        let mut warnings = Vec::new();
        let mut api_repos: Vec<ApiRepo> = match self.kind {
            ProviderKind::GitHub => self.get_all(
                "/user/repos?affiliation=owner,collaborator,organization_member&sort=full_name",
            )?,
            ProviderKind::Gitea => self.get_all("/user/repos")?,
        };
        if self.kind == ProviderKind::Gitea {
            // Gitea's /user/repos may leave out organization repositories.
            // A token without organization scope should still list the rest.
            match self.get_all::<ApiOrg>("/user/orgs") {
                Ok(orgs) => {
                    for org in orgs {
                        if !is_safe_segment(&org.username) {
                            warnings.push(tf("Skipped invalid name: {}", &[&org.username]));
                            continue;
                        }
                        match self.get_all(&format!("/orgs/{}/repos", org.username)) {
                            Ok(repos) => api_repos.extend(repos),
                            Err(error) => warnings.push(error.to_string()),
                        }
                    }
                }
                Err(error) => warnings.push(error.to_string()),
            }
        }

        let mut seen = BTreeSet::new();
        let mut repos = Vec::new();
        for api in api_repos {
            let parts = api.full_name.split_once('/');
            let valid = parts.filter(|(owner, name)| is_safe_segment(owner) && is_safe_segment(name));
            let Some((owner, name)) = valid else {
                warnings.push(tf("Skipped invalid name: {}", &[&api.full_name]));
                continue;
            };
            if seen.insert(api.full_name.clone()) {
                repos.push(Repo {
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                    private: api.private,
                    fork: api.fork,
                    archived: api.archived,
                    description: api.description.clone().unwrap_or_default(),
                });
            }
        }
        repos.sort_by_key(|repo| repo.full_name().to_lowercase());
        Ok(Listing { login, repos, warnings })
    }

    /// Makes sure `owner/name` exists as a private backup of `source_url`.
    /// An existing repository is only used if its description says it is the
    /// backup of that same source; otherwise pushing could overwrite an
    /// unrelated project or mix two sources whose names collide.
    pub fn ensure_backup_repo(
        &self,
        login: &str,
        owner: &str,
        name: &str,
        source_url: &str,
    ) -> Result<()> {
        let marker = format!("{BACKUP_MARKER}{source_url}");
        let (status, text) = self.request(&format!("/repos/{owner}/{name}"), None)?;
        match status {
            200 => {
                let existing: ApiRepo = serde_json::from_str(&text)?;
                if existing.description.as_deref() != Some(marker.as_str()) {
                    bail!(tf(
                        "{}/{} already exists and is not a backup of this source. To use it anyway, set its description to: {}",
                        &[owner, name, &marker]
                    ));
                }
                if !existing.private {
                    bail!(tf(
                        "{}/{} is public, refusing to push a backup to it",
                        &[owner, name]
                    ));
                }
                Ok(())
            }
            404 => {
                let path = if owner.eq_ignore_ascii_case(login) {
                    "/user/repos".to_owned()
                } else {
                    format!("/orgs/{owner}/repos")
                };
                let body = json!({
                    "name": name,
                    "description": marker,
                    "private": true,
                    "auto_init": false,
                });
                let (status, text) = self.request(&path, Some(body))?;
                if status == 201 {
                    Ok(())
                } else {
                    bail!(self.status_error(status, &text))
                }
            }
            _ => bail!(self.status_error(status, &text)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// Serves canned responses chosen by `route(method, path_and_query)` and
    /// records each request line along with its Authorization header and body.
    fn serve(route: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> (String, Log) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log: Log = Arc::default();
        let log_for_thread = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap().to_owned();
                let path = parts.next().unwrap().to_owned();
                let (mut auth, mut length) = (String::new(), 0usize);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim();
                    if header.is_empty() {
                        break;
                    }
                    let lower = header.to_lowercase();
                    if lower.starts_with("authorization:") {
                        auth = header[14..].trim().to_owned();
                    }
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                log_for_thread.lock().unwrap().push(format!(
                    "{method} {path} [{auth}] {}",
                    String::from_utf8_lossy(&body)
                ));
                let (status, text) = route(&method, &path);
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
        });
        (base, log)
    }

    fn query_page(path: &str) -> u32 {
        path.split(['?', '&'])
            .find_map(|part| part.strip_prefix("page="))
            .unwrap_or("1")
            .parse()
            .unwrap()
    }

    #[test]
    fn gitea_lists_all_pages_and_org_repos_without_duplicates() {
        let (base, log) = serve(|_, path| {
            let page = query_page(path);
            let body = match path.split('?').next().unwrap() {
                "/api/v1/user" => r#"{"login":"me"}"#,
                // A short first page must not be mistaken for the last page.
                "/api/v1/user/repos" if page == 1 => {
                    r#"[{"full_name":"me/b","private":true},{"full_name":"org/shared"}]"#
                }
                "/api/v1/user/repos" if page == 2 => r#"[{"full_name":"me/A","fork":true}]"#,
                "/api/v1/user/orgs" if page == 1 => r#"[{"username":"org"}]"#,
                "/api/v1/orgs/org/repos" if page == 1 => {
                    r#"[{"full_name":"org/shared"},{"full_name":"org/only","archived":true}]"#
                }
                _ => "[]",
            };
            (200, body.to_owned())
        });
        let listing = Client::new(ProviderKind::Gitea, &base, "secret").list_repos().unwrap();
        let names: Vec<_> = listing.repos.iter().map(Repo::full_name).collect();
        assert_eq!(names, ["me/A", "me/b", "org/only", "org/shared"]);
        assert!(listing.warnings.is_empty());
        assert!(listing.repos[1].private && listing.repos[0].fork && listing.repos[2].archived);
        assert!(log.lock().unwrap().iter().all(|line| line.contains("[token secret]")));
    }

    #[test]
    fn github_enterprise_uses_v3_paths_and_bearer_auth() {
        let (base, log) = serve(|_, path| {
            let body = match path.split('?').next().unwrap() {
                "/api/v3/user" => r#"{"login":"me"}"#,
                "/api/v3/user/repos" if query_page(path) == 1 => r#"[{"full_name":"me/x"}]"#,
                _ => "[]",
            };
            (200, body.to_owned())
        });
        let listing = Client::new(ProviderKind::GitHub, &base, "secret").list_repos().unwrap();
        assert_eq!(listing.repos.len(), 1);
        let log = log.lock().unwrap();
        assert!(log.iter().all(|line| line.contains("[Bearer secret]")));
        assert!(log.iter().any(|line| line.contains("per_page=100")));
    }

    #[test]
    fn malicious_names_are_dropped_with_a_warning() {
        let (base, _log) = serve(|_, path| {
            let body = match path.split('?').next().unwrap() {
                "/api/v1/user" => r#"{"login":"me"}"#,
                "/api/v1/user/repos" if query_page(path) == 1 => {
                    r#"[{"full_name":"../../evil"},{"full_name":"me/.."},{"full_name":"me/ok"}]"#
                }
                _ => "[]",
            };
            (200, body.to_owned())
        });
        let listing = Client::new(ProviderKind::Gitea, &base, "t").list_repos().unwrap();
        assert_eq!(listing.repos.len(), 1);
        assert_eq!(listing.warnings.len(), 2);
    }

    #[test]
    fn rejected_token_is_an_error() {
        let (base, _log) = serve(|_, _| (401, r#"{"message":"Bad credentials"}"#.to_owned()));
        let error = Client::new(ProviderKind::GitHub, &base, "t").list_repos().err().unwrap();
        assert!(error.to_string().contains("Bad credentials"));
    }

    #[test]
    fn missing_backup_repo_is_created_private() {
        let (base, log) = serve(|method, _| match method {
            "POST" => (201, "{}".to_owned()),
            _ => (404, "{}".to_owned()),
        });
        let client = Client::new(ProviderKind::Gitea, &base, "t");
        client.ensure_backup_repo("me", "me", "a-b", "https://src/a/b.git").unwrap();
        client.ensure_backup_repo("me", "team", "a-b", "https://src/a/b.git").unwrap();
        let log = log.lock().unwrap();
        assert!(log[1].starts_with("POST /api/v1/user/repos"));
        assert!(log[1].contains(r#""private":true"#));
        assert!(log[3].starts_with("POST /api/v1/orgs/team/repos"));
    }

    #[test]
    fn existing_repo_needs_the_matching_private_marker() {
        let source = "https://src/a/b.git";
        let matching =
            format!(r#"{{"full_name":"m/n","private":true,"description":"Backup of {source}"}}"#);
        let public =
            format!(r#"{{"full_name":"m/n","private":false,"description":"Backup of {source}"}}"#);
        let foreign = r#"{"full_name":"m/n","private":true,"description":"my project"}"#.to_owned();

        for (body, ok) in [(matching, true), (public, false), (foreign, false)] {
            let (base, log) = serve(move |_, _| (200, body.clone()));
            let client = Client::new(ProviderKind::Gitea, &base, "t");
            let result = client.ensure_backup_repo("m", "m", "n", source);
            assert_eq!(result.is_ok(), ok);
            assert_eq!(log.lock().unwrap().len(), 1, "must not attempt to create");
        }
    }
}
