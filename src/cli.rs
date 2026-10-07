//! CLI — port of internal/cli/cli.go (src/novel/cli.py).
//!
//! Async: chromiumoxide is a tokio client, so every command runs in the
//! runtime started by main.rs.

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::browser::{self, Session};
use crate::cleaner::TextCleaner;
use crate::config;
use crate::ocr;
use crate::rule::{self, Chapter, ChapterResult, Track};
use crate::translate::Translator;

#[derive(Parser, Debug)]
#[command(
    name = "novel",
    version,
    about = "Download Chinese web novels from Qidian and SFACG."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Extract a book (free chapters unless logged in)
    Extract {
        url: String,
        #[arg(long, default_value = "outputs")]
        output_dir: String,
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        start: usize,
        /// Only VIP chapters
        #[arg(long)]
        only_vip: bool,
        /// Show the browser window
        #[arg(long)]
        headed: bool,
        /// Attach to your real browser over CDP, e.g. http://localhost:9222
        #[arg(long)]
        cdp: Option<String>,
        /// OCR engine for chapters that need it: `sidecar` (default, Python PaddleOCR service) or `native` (in-process Rust/ONNX, no Python needed)
        #[arg(long, value_name = "sidecar|native")]
        ocr_backend: Option<String>,
    },
    /// Log in and save the session for later runs
    Login {
        #[arg(long)]
        site: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[arg(long)]
        cdp: Option<String>,
    },
    /// OCR a folder of screenshots (default backend: sidecar)
    Ocr {
        target: String,
        #[arg(long, default_value = "outputs")]
        output_dir: String,
        /// OCR engine: `sidecar` (default, Python PaddleOCR service) or `native` (in-process Rust/ONNX, no Python needed)
        #[arg(long, value_name = "sidecar|native")]
        ocr_backend: Option<String>,
    },
    /// Translate extracted text (zh -> vi)
    Translate { input: String, output: Option<String> },
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Extract {
            url,
            output_dir,
            limit,
            start,
            only_vip,
            headed,
            cdp,
            ocr_backend,
        } => {
            apply_ocr_backend(ocr_backend.as_deref())?;
            extract(&url, &output_dir, limit, start, only_vip, headed, cdp).await
        }
        Command::Login { site, timeout, cdp } => login(&site, timeout, cdp.as_deref()).await,
        Command::Ocr {
            target,
            output_dir,
            ocr_backend,
        } => ocr_folder(&target, &output_dir, ocr_backend.as_deref()).await,
        Command::Translate { input, output } => {
            let out = output.unwrap_or_else(|| default_vi_path(&input));
            Translator::new().file(&input, &out).await
        }
    }
}

/// Validate `--ocr-backend` (or fail loudly) and publish it so the rule-level
/// `ocr::Client::new(None)` calls pick it up from the environment.
fn apply_ocr_backend(value: Option<&str>) -> Result<()> {
    let backend = ocr::resolve_backend(value)?;
    // SAFETY: single-threaded at this point (start of the command, before any
    // worker task is spawned).
    std::env::set_var("NOVEL_OCR_BACKEND", backend.as_str());
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared-client pacing (Phase 2)
// ---------------------------------------------------------------------------

/// Jittered pacing for the shared reqwest client: HTTP chapter fetches are
/// spaced out by a randomised interval so parallel workers never settle into a
/// fixed request cadence (anti-rate-limit). Clock is a monotonic deadline
/// queue over atomics, so tasks can share one pacing queue without a mutex.
pub struct Pacer {
    /// Monotonic deadline (ms since process start) reserved by the last caller.
    next_deadline_ms: AtomicU64,
    base_ms: u64,
    jitter_ms: u64,
    seed: AtomicU64,
}

impl Pacer {
    /// [base, base + jitter) ms between the starts of two HTTP fetches.
    const fn new(base_ms: u64, jitter_ms: u64) -> Self {
        Self {
            next_deadline_ms: AtomicU64::new(0),
            base_ms,
            jitter_ms,
            seed: AtomicU64::new(0),
        }
    }

    fn start_ms() -> u64 {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(Instant::now)
            .elapsed()
            .as_millis() as u64
    }

    /// xorshift64* — tiny, dependency-free, good enough for pacing jitter.
    fn next_rand(&self) -> u64 {
        let mut x = match self.seed.load(Ordering::Relaxed) {
            0 => 0x9E3779B97F4A7C15,
            s => s,
        };
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed.store(x, Ordering::Relaxed);
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn span_ms(&self) -> u64 {
        self.base_ms + (self.next_rand() % self.jitter_ms.max(1))
    }

    /// Reserve the next slot and wait out the (always-positive) remainder.
    async fn pause(&self) {
        let span = self.span_ms();
        let slot = self
            .next_deadline_ms
            .fetch_add(span, Ordering::Relaxed)
            .max(Self::start_ms());
        let now = Self::start_ms();
        if slot > now {
            tokio::time::sleep(Duration::from_millis(slot - now)).await;
        }
    }
}

/// HTTP pacing constants keeping us clear of the sites' rate limits.
const HTTP_PACING_BASE_MS: u64 = 150;
const HTTP_PACING_JITTER_MS: u64 = 350;

/// `section / title`, or just the title when there is no section.
fn chapter_label(ch: &Chapter) -> String {
    match ch.section_name.is_empty() {
        true => ch.title.clone(),
        false => format!("{} / {}", ch.section_name, ch.title),
    }
}

fn default_vi_path(input: &str) -> String {
    match input.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}_vi.{ext}"),
        None => format!("{input}_vi.txt"),
    }
}

async fn extract(
    url: &str,
    output_dir: &str,
    limit: usize,
    start: usize,
    only_vip: bool,
    headed: bool,
    cdp: Option<String>,
) -> Result<()> {
    let r = rule::for_url(url)?;
    println!("--- {}: extract ---", r.name());
    println!("URL: {url}");

    let session = open_session(r.name(), cdp.as_deref(), headed).await?;
    let book = match r.book_parse(url, &session).await {
        Ok(b) => b,
        Err(e) => {
            session_shutdown(&session);
            return Err(e);
        }
    };
    println!(
        "  {} — 作者:{}  [{} chapters, {} VIP]",
        book.title,
        book.author,
        book.chapters.len(),
        book.vip_chapters()
    );

    let mut selected: Vec<Chapter> = book
        .chapters
        .iter()
        .filter(|c| start == 0 || c.num >= start)
        .filter(|c| !only_vip || c.is_vip)
        .cloned()
        .collect();
    if limit > 0 {
        selected.truncate(limit);
    }

    let mut combined: Vec<String> = Vec::new();
    // Phase 2: jittered pacing for everything that goes over the shared HTTP
    // client (Qidian free chapters use it too — the browser is fallback-only).
    let http_pacer = Pacer::new(HTTP_PACING_BASE_MS, HTTP_PACING_JITTER_MS);
    for ch in &selected {
        // Catalog-known-locked chapters are never scheduled at all: no request
        // and certainly no hammering of the purchase wall.
        if ch.is_paid == Some(false) {
            println!("  [{}] {}... LOCKED (catalog)", ch.num, chapter_label(ch));
            continue;
        }
        if r.chapter_track(ch) == Track::Http {
            http_pacer.pause().await;
        }
        let label = chapter_label(ch);
        print!("  [{}] {}...", ch.num, label);
        use std::io::Write;
        std::io::stdout().flush().ok();

        let res: ChapterResult = match r.chapter_parse(ch, &session).await {
            Ok(res) => res,
            Err(e) => {
                println!(" ERROR: {e}");
                continue;
            }
        };
        if res.locked {
            println!(" LOCKED (need login / purchase)");
            continue;
        }
        if res.needs_ocr && res.text.is_empty() {
            println!(" NEEDS OCR");
            combined.push(format!("# {}\n[OCR needed]", res.title));
            continue;
        }
        let mut note = String::new();
        if res.needs_ocr {
            note = " [needs OCR]".to_string();
        }
        println!(" {} chars{}", res.text.chars().count(), note);
        combined.push(format!("# {}\n{}", res.title, res.text));
    }

    if combined.is_empty() {
        println!("No chapters extracted.");
        session_shutdown(&session);
        return Ok(());
    }

    let dir = Path::new(output_dir);
    std::fs::create_dir_all(dir).ok();
    let base = book.url.trim_end_matches('/').rsplit('/').next().unwrap_or("book");
    let file = dir.join(format!("{}_{base}.txt", r.name()));
    std::fs::write(&file, combined.join("\n\n"))?;
    println!("Saved {} chapters -> {}", combined.len(), file.display());
    session_shutdown(&session);
    Ok(())
}

async fn open_session(site: &str, cdp: Option<&str>, headed: bool) -> Result<Session> {
    if let Some(url) = cdp {
        println!("Attaching to your real browser via CDP: {url}");
        let s = Session::attach(url).await?;
        println!(
            "  real session: {} cookies harvested for the HTTP fast path",
            s.cookie_map().await.len()
        );
        return Ok(s);
    }
    let state = config::storage_state(site);
    let state_path = state.filter(|p| Path::new(p).exists());
    if state_path.is_none() && !config::has_session_hint(site) {
        println!("No saved session; free chapters only (see `novel login`).");
    }
    Session::start(&config::site_cookies(site), state_path.as_deref(), headed).await
}

fn session_shutdown(s: &Session) {
    // Browser is dropped with the session; nothing to kill (the Handler task
    // ends when the process exits). Attached browsers are left untouched.
    let _ = s;
}

fn login_target(site: &str) -> Result<&'static str> {
    match site {
        "qidian" => Ok("https://www.qidian.com/"),
        "sfacg" => Ok("https://www.sfacg.com/"),
        _ => Err(anyhow!(
            "unknown site {site}; choose from {}",
            config::storage_state_sites().join(", ")
        )),
    }
}

fn logged_in(html: &str, site: &str) -> bool {
    if site == "sfacg" {
        !html.contains("SF游客") && (html.to_lowercase().contains("logout") || html.contains("退出"))
    } else {
        let lower = html.to_lowercase();
        lower.contains("quser") || html.contains("我的书架") || lower.contains("user-info")
    }
}

async fn login(site: &str, timeout_s: u64, cdp: Option<&str>) -> Result<()> {
    let target = login_target(site)?;
    let r = rule::for_url(target)?;
    let state_path = r.storage_state_path();
    if let Some(parent) = Path::new(&state_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let session = match cdp {
        Some(url) => {
            println!("Attaching to real browser via CDP: {url}");
            Session::attach(url).await?
        }
        None => {
            let s = Session::start(&config::site_cookies(site), None, true).await?;
            if let Err(e) = s.goto(target).await {
                println!("[login] Could not open {target}: {e}");
            }
            println!("[login] Browser opened on {target}");
            s
        }
    };

    if cdp.is_some() {
        println!("[login] Watching your tabs on {target}; log in there.");
    } else {
        println!("[login] Log in with your account, then leave this running.");
    }
    println!("[login] The session is saved as soon as your nickname appears.");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
    while std::time::Instant::now() < deadline {
        if let Ok(content) = session.content().await {
            if logged_in(&content, site) {
                session.save_state(&state_path).await?;
                println!("[login] Logged in -> session saved to {state_path}");
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    println!("[login] Timed out waiting for login; nothing saved.");
    Ok(())
}

async fn ocr_folder(target: &str, output_dir: &str, backend: Option<&str>) -> Result<()> {
    let path = PathBuf::from(target);
    let backend = ocr::resolve_backend(backend)?;
    if backend == ocr::Backend::Native {
        println!("[ocr] backend: native (models: {})", ocr::native_model_dir().display());
    } else {
        println!("[ocr] backend: sidecar");
    }
    let mut client = ocr::Client::with_backend(None, backend);
    let results: Vec<(PathBuf, String)> = if path.is_dir() {
        client.process_folder(&path).await?
    } else {
        let text = client.image_text(&path, "\n").await?;
        vec![(path.clone(), text)]
    };
    let cleaner = TextCleaner;
    let cleaned: Vec<String> = results
        .iter()
        .map(|(_, t)| cleaner.clean(t))
        .filter(|t| !t.is_empty())
        .collect();
    if cleaned.is_empty() {
        return Err(anyhow!("no text recognized in {target}"));
    }
    std::fs::create_dir_all(output_dir).ok();
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "images".to_string());
    let file = Path::new(output_dir).join(format!("{name}_cleaned.txt"));
    std::fs::write(&file, cleaned.join("\n\n"))?;
    println!("Saved to {}", file.display());
    Ok(())
}

/// Print the detected browser binary (used by `novel doctor`).
pub fn browser_hint() -> String {
    let b = browser::binary();
    if b.is_empty() {
        "no binary configured; chromiumoxide will auto-detect".to_string()
    } else {
        b
    }
}
