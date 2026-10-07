//! Config: output paths, .env loading, per-site cookies — port of
//! internal/config/config.go.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::browser::Cookie;

pub const OUTPUT_DIR: &str = "outputs";

fn storage_states() -> &'static HashMap<&'static str, &'static str> {
    static M: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    M.get_or_init(|| {
        HashMap::from([
            ("qidian", "outputs/qidian_storage.json"),
            ("sfacg", "outputs/sfacg_storage.json"),
        ])
    })
}

pub fn storage_state(site: &str) -> Option<String> {
    storage_states().get(site).map(|s| s.to_string())
}

pub fn storage_state_sites() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = storage_states().keys().copied().collect();
    v.sort_unstable();
    v
}

pub fn output_path(name: &str) -> PathBuf {
    Path::new(OUTPUT_DIR).join(name)
}

/// Load .env into the process env (setdefault semantics), once.
pub fn load_env() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let Ok(raw) = std::fs::read_to_string(".env") else {
            return;
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || !line.contains('=') {
                continue;
            }
            let (k, v) = line.split_once('=').unwrap();
            let k = k.trim();
            let v = v.trim();
            if std::env::var(k).is_err() {
                std::env::set_var(k, v);
            }
        }
    });
}

fn cookie_domain(site: &str) -> &'static str {
    match site {
        "qidian" => ".qidian.com",
        "sfacg" => ".sfacg.com",
        _ => "",
    }
}

fn parse_cookie_string(raw: &str, domain: &str) -> Vec<Cookie> {
    raw.split(';')
        .filter_map(|part| {
            let part = part.trim();
            let (name, value) = part.split_once('=')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(Cookie {
                name: name.to_string(),
                value: value.trim().to_string(),
                domain: domain.to_string(),
                path: "/".to_string(),
            })
        })
        .collect()
}

/// Cookies for `site` from `{SITE}_COOKIE` in .env/env.
pub fn site_cookies(site: &str) -> Vec<Cookie> {
    load_env();
    let key = format!(
        "{}_COOKIE",
        site.to_uppercase()
    );
    match std::env::var(&key) {
        Ok(raw) if !raw.trim().is_empty() => parse_cookie_string(&raw, cookie_domain(site)),
        _ => Vec::new(),
    }
}

pub fn has_session_hint(site: &str) -> bool {
    load_env();
    let upper = site.to_uppercase();
    std::env::var(format!("{upper}_COOKIE")).is_ok()
        || std::env::var(format!("{upper}_CSRFTOKEN")).is_ok()
}
