//! Rule base types and registry — port of internal/rule/rule.go.

use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use std::sync::{Mutex, OnceLock};

use crate::browser::Session;

#[derive(Debug, Clone)]
pub struct Chapter {
    pub num: usize,
    pub title: String,
    pub url: String,
    pub is_vip: bool,
    /// `None` mirrors Python's `is_paid: bool | None` ("catalog can't tell").
    pub is_paid: Option<bool>,
    pub section_name: String,
    pub section_number: usize,
    pub word_count: usize,
}

#[derive(Debug, Clone)]
pub struct Book {
    pub site: String,
    pub url: String,
    pub title: String,
    pub author: String,
    pub intro: String,
    pub tags: Vec<String>,
    pub cover: String,
    pub chapters: Vec<Chapter>,
}

impl Book {
    pub fn vip_chapters(&self) -> usize {
        self.chapters.iter().filter(|c| c.is_vip).count()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChapterResult {
    pub title: String,
    pub text: String,
    pub locked: bool,
    pub needs_ocr: bool,
    pub images: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// Server-rendered HTML: plain HTTP through the shared client (fast,
    /// jittered pacing; no browser target needed).
    Http,
    /// Needs a rendered DOM/JS canvas (Qidian VIP iframe text, glyph
    /// rendering): one browser page target.
    Browser,
}

pub trait Rule: Send + Sync {
    fn name(&self) -> &'static str;
    fn domains(&self) -> &'static [&'static str];
    fn concurrency_limit(&self) -> usize {
        1
    }
    fn storage_state_path(&self) -> String;
    /// How [`crate::cli::extract`] should fetch this chapter (Phase 2,
    /// HTTP-first): server-rendered pages go through the shared reqwest
    /// client, and the browser is reserved for DOM/JS-required renders.
    /// Catalog-covered pre-checks (e.g. Qidian `is_paid == Some(false)`
    /// lockouts) are filtered out by the caller before this is consulted.
    fn chapter_track(&self, ch: &Chapter) -> Track {
        let _ = ch;
        Track::Browser
    }
    fn book_parse<'a>(
        &'a self,
        url: &'a str,
        s: &'a Session,
    ) -> BoxFuture<'a, Result<Book>>;
    fn chapter_parse<'a>(
        &'a self,
        ch: &'a Chapter,
        s: &'a Session,
    ) -> BoxFuture<'a, Result<ChapterResult>>;
}

fn registry() -> &'static Mutex<Vec<&'static dyn Rule>> {
    static REG: OnceLock<Mutex<Vec<&'static dyn Rule>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a rule. Rules live for the whole process, hence the `'static`.
pub fn register(r: &'static dyn Rule) {
    registry().lock().unwrap().push(r);
}

pub fn known_sites() -> Vec<&'static str> {
    registry().lock().unwrap().iter().map(|r| r.name()).collect()
}

pub fn for_url(url: &str) -> Result<&'static dyn Rule> {
    // Find the match while holding the lock, then release before building the
    // error — `known_sites()` locks the same non-reentrant mutex, so calling it
    // inside the closure would self-deadlock.
    let found = {
        let reg = registry().lock().unwrap();
        reg.iter()
            .find(|r| r.domains().iter().any(|d| url.contains(d)))
            .copied()
    };
    found.ok_or_else(|| {
        anyhow!(
            "No rule for {url}. Known sites: {}",
            known_sites().join(", ")
        )
    })
}
