//! CLI — port of internal/cli/cli.go (src/novel/cli.py).
//!
//! Async: chromiumoxide is a tokio client, so every command runs in the
//! runtime started by main.rs.

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
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
        /// Max concurrent chapter workers. HTTP-fetch workers and browser
        /// page workers share the budget (browser pages also cap at 3);
        /// `--jobs 1` is the sequential read.
        #[arg(long, value_name = "N")]
        jobs: Option<usize>,
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
            jobs,
            ocr_backend,
        } => {
            apply_ocr_backend(ocr_backend.as_deref())?;
            extract(
                &url,
                &output_dir,
                limit,
                start,
                only_vip,
                headed,
                cdp,
                jobs,
            )
            .await
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

/// HTTP pacing constants keeping us clear of the sites' rate limits. The
/// spacing is reserved globally (one shared `Pacer`) so parallel workers
/// spread their fetches instead of firing together.
const HTTP_PACING_BASE_MS: u64 = 800;
const HTTP_PACING_JITTER_MS: u64 = 600;

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
    jobs: Option<usize>,
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

    // Phase 3: bounded worker set. HTTP-workers pull server-rendered chapters
    // over the shared reqwest client; browser workers run DOM/JS-required
    // renders on page targets multiplexed over the session's own CDP socket.
    let jobs = jobs.unwrap_or(DEFAULT_HTTP_JOBS).max(1);
    let http_cap = jobs;
    let page_cap = jobs.min(DEFAULT_BROWSER_PAGES).max(1);

    let session = Arc::new(session);
    let page_pool = session.page_pool(page_cap)?;
    let http_sem = Arc::new(tokio::sync::Semaphore::new(http_cap));
    // Phase 2: jittered pacing for everything that goes over the shared HTTP
    // client (Qidian free chapters use it too — the browser is fallback-only).
    let http_pacer = Arc::new(Pacer::new(HTTP_PACING_BASE_MS, HTTP_PACING_JITTER_MS));

    let dir = Path::new(output_dir);
    std::fs::create_dir_all(dir).ok();
    let base = book.url.trim_end_matches('/').rsplit('/').next().unwrap_or("book");
    let file = dir.join(format!("{}_{base}.txt", r.name()));
    // Phase 5: every finished chapter lands here immediately, so an
    // interrupted run (or a crashed one — kill -9 safe) can resume from disk.
    let chapters_dir = dir.join(format!("{}_{}_chapters", r.name(), base));
    std::fs::create_dir_all(&chapters_dir).ok();

    let total = selected.len();
    let done = Arc::new(AtomicUsize::new(0));
    let failures: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Catalog-known-locked chapters are never scheduled at all: no request and
    // certainly no hammering of the purchase wall. Already-extracted chapters
    // (NNNN.txt on disk from a previous run) resume-skip.
    let mut pending: Vec<(usize, Chapter)> = Vec::new();
    for (idx, ch) in selected.iter().enumerate() {
        if ch.is_paid == Some(false) {
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            println!("  [{n}/{total}] {}... LOCKED (catalog)", chapter_label(ch));
            continue;
        }
        if chapters_dir.join(format!("{:04}.txt", ch.num)).exists() {
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            println!("  [{n}/{total}] {}... resumed", chapter_label(ch));
            continue;
        }
        pending.push((idx, ch.clone()));
    }

    // Slots for the assembled output, so worker completion order doesn't
    // matter; `selected` order defines the final chapter order.
    let slots: Arc<StdMutex<Vec<Option<String>>>> = Arc::new(StdMutex::new(vec![None; selected.len()]));

    let http_for_task = http_sem.clone();
    let pacer_for_task = http_pacer.clone();
    let max_in_flight = http_cap.max(page_cap);
    stream::iter(pending)
        .map(|(idx, ch)| {
            let r = r;
            let session = session.clone();
            let http_sem = http_for_task.clone();
            let http_pacer = pacer_for_task.clone();
            let page_pool = page_pool.clone();
            async move {
                let label = chapter_label(&ch);
                let track = r.chapter_track(&ch);
                let attempt: Attempt = match track {
                    Track::Http => {
                        // Shared-client slot + jittered spacing.
                        let Ok(_permit) = http_sem.clone().acquire_owned().await else {
                            return (idx, ch, label, Err(anyhow!("HTTP worker pool closed")));
                        };
                        http_pacer.pause().await;
                        Attempt::Direct(r.chapter_parse(&ch, &session).await)
                    }
                    Track::Browser => {
                        // One page target per in-flight chapter, multiplexed
                        // over the SAME websocket (`run_on_page` routes every
                        // session call to this page inside the worker task).
                        let Ok(lease) = page_pool.acquire().await else {
                            return (idx, ch, label, Err(anyhow!("browser page pool closed")));
                        };
                        let page = lease.page().clone();
                        let res =
                            browser::run_on_page(page, r.chapter_parse(&ch, &session)).await;
                        // `lease` released here.
                        Attempt::Direct(res)
                    }
                };
                (idx, ch, label, attempt.into_result())
            }
        })
        .buffer_unordered(max_in_flight)
        .for_each(|(idx, ch, label, res)| {
            let slots = slots.clone();
            let done = done.clone();
            let failures = failures.clone();
            let chapters_dir = chapters_dir.clone();
            async move {
                let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                let res: ChapterResult = match res {
                    Ok(res) => res,
                    Err(e) => {
                        failures.lock().unwrap().push(format!("{label}: {e}"));
                        println!("  [{k}/{total}] {label}... ERROR: {e}");
                        return;
                    }
                };
                if res.locked {
                    println!("  [{k}/{total}] {label}... LOCKED (need login / purchase)");
                    return;
                }
                let piece = chapter_piece(&res);
                // Write the finished chapter file right away; the book txt is
                // assembled after the run from the chapter directory (Phase 5).
                let path = chapters_dir.join(format!("{:04}.txt", ch.num));
                if let Err(e) = std::fs::write(&path, &piece) {
                    failures
                        .lock()
                        .unwrap()
                        .push(format!("{label}: writing {}: {e}", path.display()));
                    println!("  [{k}/{total}] {label}... ERROR: {e}");
                    return;
                }
                slots.lock().unwrap()[idx] = Some(piece);
                let note = if res.needs_ocr { " [needs OCR]" } else { "" };
                println!(
                    "  [{k}/{total}] {label}... {} chars{}",
                    res.text.chars().count(),
                    note
                );
            }
        })
        .await;

    // Phase 5: assemble the book txt from the chapter directory (so resumed
    // chapters from earlier runs are included, in catalog order).
    let mut chapter_files: Vec<(usize, PathBuf)> = std::fs::read_dir(&chapters_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().and_then(|e| e.to_str()) == Some("txt")
                        && p.file_stem()
                            .and_then(|s| s.to_str())
                            .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
                })
                .map(|p| {
                    let num = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    (num, p)
                })
                .collect()
        })
        .unwrap_or_default();
    chapter_files.sort_by_key(|(num, _)| *num);
    chapter_files.dedup_by_key(|(num, _)| *num);

    let combined: Vec<String> = chapter_files
        .iter()
        .filter_map(|(_, p)| std::fs::read_to_string(p).ok())
        .collect();
    if combined.is_empty() {
        println!("No chapters extracted.");
        session_shutdown(&session);
        return Ok(());
    }

    std::fs::write(&file, combined.join("\n\n"))?;
    println!("Saved {} chapters -> {}", combined.len(), file.display());
    let failed = failures.lock().unwrap();
    if !failed.is_empty() {
        println!("Failures ({}):", failed.len());
        for f in failed.iter() {
            println!("  {f}");
        }
    }
    session_shutdown(&session);
    Ok(())
}

enum Attempt {
    Direct(Result<ChapterResult>),
}

impl Attempt {
    fn into_result(self) -> Result<ChapterResult> {
        match self {
            Attempt::Direct(res) => res,
        }
    }
}

/// Worker-set defaults: HTTP fetches first (plan range 4–6), browser pages set
/// to a conservative cap of 3 active targets.
const DEFAULT_HTTP_JOBS: usize = 4;
const DEFAULT_BROWSER_PAGES: usize = 3;

/// The per-chapter output piece — byte-compatible with the sequential
/// extractor's `combined` entries ("# {title}\n{text}", joined by "\n\n").
fn chapter_piece(res: &ChapterResult) -> String {
    if res.needs_ocr && res.text.is_empty() {
        format!("# {}\n[OCR needed]", res.title)
    } else {
        format!("# {}\n{}", res.title, res.text)
    }
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
