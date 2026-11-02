//! Persistent settings. Tokens are never written here; they live in the
//! operating system's credential store.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::i18n::t;

const KEYRING_SERVICE: &str = "git-backup";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    GitHub,
    Gitea,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::GitHub => "GitHub",
            ProviderKind::Gitea => "Gitea",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    /// Whether this account's repositories are listed for backup. An account
    /// used only as a push destination can turn this off.
    pub use_as_source: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DestinationKind {
    /// A folder that receives one bare mirror per repository.
    Local { path: PathBuf },
    /// An account that receives one repository per source repository, private
    /// or public like its source.
    Remote {
        account_id: String,
        /// Organization or user to create repositories under. Empty means
        /// the account's own user.
        namespace: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Destination {
    pub id: String,
    pub enabled: bool,
    pub kind: DestinationKind,
}

/// The repository list column that decides the row order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortColumn {
    #[default]
    Name,
    Created,
    Modified,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortOrder {
    pub column: SortColumn,
    pub descending: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub language: String,
    pub accounts: Vec<Account>,
    pub destinations: Vec<Destination>,
    pub selected: BTreeSet<String>,
    pub workers: usize,
    /// Also delete refs that no longer exist on the source. Off by default,
    /// so a branch deleted upstream is still recoverable from the backup.
    pub prune: bool,
    pub sort: SortOrder,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: "zh-TW".to_owned(),
            accounts: Vec::new(),
            destinations: Vec::new(),
            selected: BTreeSet::new(),
            workers: 4,
            prune: false,
            sort: SortOrder::default(),
        }
    }
}

impl Config {
    fn path() -> Result<PathBuf> {
        // An override keeps test runs away from the real settings.
        if let Some(dir) = std::env::var_os("GIT_BACKUP_CONFIG_DIR") {
            return Ok(PathBuf::from(dir).join("config.json"));
        }
        let base = dirs::config_dir().context("no configuration directory on this system")?;
        Ok(base.join("git-backup").join("config.json"))
    }

    /// Loads the settings. A missing file yields the defaults; an unreadable
    /// one is an error, so the caller never overwrites it with empty settings.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("cannot parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&Self::path()?, &serde_json::to_vec_pretty(self)?)
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.id == id)
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("configuration path has no parent")?;
    fs::create_dir_all(dir)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, data)?;
    fs::rename(&temp, path)?;
    Ok(())
}

/// Generates an identifier unique within this installation.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn keyring_entry(account_id: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, account_id)?)
}

pub fn store_token(account_id: &str, token: &str) -> Result<()> {
    keyring_entry(account_id)?
        .set_password(token)
        .map_err(|error| anyhow!("{}: {error}", t("Cannot save the token to the credential store")))
}

pub fn load_token(account_id: &str) -> Result<String> {
    keyring_entry(account_id)?
        .get_password()
        .map_err(|error| anyhow!("{}: {error}", t("Cannot read the token from the credential store")))
}

pub fn delete_token(account_id: &str) -> Result<()> {
    match keyring_entry(account_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Normalizes a user-entered server address to `scheme://host[:port][/path]`.
pub fn normalize_base_url(input: &str) -> Result<String> {
    let input = input.trim();
    let url = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let (scheme, rest) = url
        .strip_prefix("https://")
        .map(|rest| ("https", rest))
        .or_else(|| url.strip_prefix("http://").map(|rest| ("http", rest)))
        .ok_or_else(|| anyhow!("{}", t("The server address must use http or https")))?;
    let rest = rest.trim_end_matches('/');
    let valid_host = !rest.is_empty()
        && !rest.starts_with('/')
        && !rest
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '?' | '#' | '\\'));
    if !valid_host {
        bail!("{}", t("The server address is not valid"));
    }
    Ok(format!("{scheme}://{rest}"))
}

/// Whether a name is safe to use as one URL and file-system path segment.
/// GitHub and Gitea only produce names in this alphabet, so anything else is
/// refused rather than rewritten, which could make two repositories collide.
pub fn is_safe_segment(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_normalized() {
        assert_eq!(normalize_base_url(" github.com/ ").unwrap(), "https://github.com");
        assert_eq!(
            normalize_base_url("http://nas:3000/gitea/").unwrap(),
            "http://nas:3000/gitea"
        );
    }

    #[test]
    fn bad_base_urls_are_rejected() {
        for bad in ["", "ftp://x", "https://", "https://u:p@x", "https://a b", "https:///x"] {
            assert!(normalize_base_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unsafe_segments_are_rejected() {
        for good in ["repo", "my.repo_1-x", "a..b"] {
            assert!(is_safe_segment(good), "{good:?}");
        }
        for bad in ["", "..", ".git", "-x", "a/b", "a\\b", "x.", "a b", "a:b", "{5132}{5b58}"] {
            assert!(!is_safe_segment(bad), "{bad:?}");
        }
    }
}
