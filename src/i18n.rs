//! Minimal runtime translation layer.
//!
//! Source code uses English strings as lookup keys; translations live in
//! `locales/*.json`. A missing translation falls back to the English key.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

static ZH_TW: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../locales/zh-TW.json")).expect("valid zh-TW locale file")
});

static USE_ZH_TW: AtomicBool = AtomicBool::new(true);

pub fn set_language(language: &str) {
    USE_ZH_TW.store(language != "en", Ordering::Relaxed);
}

/// Translates a key.
pub fn t(key: &'static str) -> &'static str {
    if USE_ZH_TW.load(Ordering::Relaxed) {
        ZH_TW.get(key).copied().unwrap_or(key)
    } else {
        key
    }
}

/// Translates a key and replaces each `{}` with the next argument.
pub fn tf(key: &'static str, args: &[&str]) -> String {
    let mut parts = t(key).split("{}");
    let mut out = parts.next().unwrap_or_default().to_owned();
    for (index, part) in parts.enumerate() {
        out.push_str(args.get(index).copied().unwrap_or("{}"));
        out.push_str(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extracts the string literal that starts at `text`, handling escapes
    /// and multi-line calls.
    fn literal_after(text: &str) -> Option<String> {
        let mut chars = text.trim_start().strip_prefix('"')?.chars();
        let mut out = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => out.push(chars.next()?),
                other => out.push(other),
            }
        }
        None
    }

    /// Every key used in the sources must have a zh-TW translation.
    #[test]
    fn all_keys_are_translated() {
        let sources = [
            include_str!("app.rs"),
            include_str!("backup.rs"),
            include_str!("config.rs"),
            include_str!("provider.rs"),
            include_str!("git.rs"),
        ];
        let mut missing = Vec::new();
        for source in sources {
            for marker in ["t(", "tf("] {
                for (position, _) in source.match_indices(marker) {
                    let is_call = source[..position]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.');
                    if !is_call {
                        continue;
                    }
                    if let Some(key) = literal_after(&source[position + marker.len()..])
                        && !ZH_TW.contains_key(key.as_str())
                    {
                        missing.push(key);
                    }
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "untranslated keys: {missing:#?}");
    }

    #[test]
    fn placeholders_match_between_languages() {
        for (key, value) in ZH_TW.iter() {
            assert_eq!(
                key.matches("{}").count(),
                value.matches("{}").count(),
                "placeholder mismatch for {key:?}"
            );
        }
    }
}
