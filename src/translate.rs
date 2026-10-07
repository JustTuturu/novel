//! zh -> vi translation via an OpenAI-compatible chat API.
//!
//! Chapter-by-chapter (replacing the old serial line-by-line Google endpoint):
//! the input is the assembled book txt, or — preferred when present — the
//! per-chapter files from `outputs/<site>_<id>_chapters/`. Each chapter is one
//! API call; every finished chapter lands immediately in
//! `outputs/<site>_<id>_translated/NNNN.txt` (resume on rerun, same scheme as
//! extraction), and the translated book txt is assembled at the end in catalog
//! order, byte-compatible with the extractor's chapter framing
//! ("# {title}\n{text}" pieces joined by "\n\n").
//!
//! Rate limiting is a HARD cap: a token-bucket rate limiter (default 10
//! req/min, no burst) shared by all workers, plus 429/Retry-After handling
//! with exponential backoff + jitter. Alongside each translation the model
//! returns a structured list of proper nouns it handled, which is upserted
//! into the self-updating glossary (src/glossary.rs) after EVERY call; the
//! most relevant entries are prepended to each request.

use crate::config;
use crate::glossary::{self, Glossary, NewEntry};
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use regex::Regex;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

fn ideographic_space_re() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| Regex::new(r"\b[0-9]+\u{3000}\u{3000}").unwrap())
}

// ---------------------------------------------------------------------------
// Token-bucket rate limiter (hard cap)
// ---------------------------------------------------------------------------

/// A hard cap on API calls per minute. Capacity is one token (no burst), so
/// calls are spaced at `60 / per_minute` seconds apart — with the default of
/// 10/min, that is 6 s between the starts of two calls, no matter how many
/// workers or retries race for it.
pub struct RateLimiter {
    state: TokioMutex<Bucket>,
    per_minute: f64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            state: TokioMutex::new(Bucket {
                tokens: 1.0,
                last: Instant::now(),
            }),
            per_minute: per_minute.max(1) as f64,
        }
    }

    /// The pure bucket math, separable from async so it can be unit-tested
    /// with synthetic clocks.
    ///
    /// Grants one token if available (decrementing the balance), otherwise
    /// returns the duration the caller must wait before retrying.
    fn grant_or_wait(
        bucket: &mut Bucket,
        per_minute: f64,
        now: Instant,
    ) -> std::result::Result<(), Duration> {
        let elapsed = now
            .checked_duration_since(bucket.last)
            .unwrap_or_default()
            .as_secs_f64();
        // Refill continuously, clamped at capacity 1.0 (no burst).
        bucket.tokens = (bucket.tokens + elapsed * per_minute / 60.0).min(1.0);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let need = (1.0 - bucket.tokens) * 60.0 / per_minute;
            Err(Duration::from_secs_f64(need))
        }
    }

    /// Wait for (and consume) one token. Prints pacing when it actually has
    /// to hold a worker back, so the cadence is observable in the logs.
    pub async fn acquire(&self) {
        let mut announced = false;
        loop {
            let wait = {
                let mut b = self.state.lock().await;
                match Self::grant_or_wait(&mut b, self.per_minute, Instant::now()) {
                    Ok(()) => None,
                    Err(d) => Some(d),
                }
            };
            let Some(d) = wait else { return };
            if !announced {
                println!(
                    "[rate] pacing {:.1}s (limit {} req/min)",
                    d.as_secs_f64(),
                    self.per_minute
                );
                announced = true;
            }
            tokio::time::sleep(d).await;
            // Loop: the token is only truly consumed when grant_or_wait says
            // so — two waiters waking at the same instant must NOT both pass
            // (the second one sees the balance the first just emptied).
        }
    }
}

// ---------------------------------------------------------------------------
// Chapters
// ---------------------------------------------------------------------------

/// One input chapter, in reading (catalog) order.
struct ChapterIn {
    /// 1-based position / on-disk number (NNNN.txt).
    num: usize,
    title: String,
    body: String,
}

/// Splits an assembled book txt into chapters. A line starting with "# "
/// begins a new chapter; everything until the next such line is the body.
fn chapters_from_txt(raw: &str) -> Vec<ChapterIn> {
    let mut out: Vec<ChapterIn> = Vec::new();
    for line in raw.split('\n') {
        if let Some(title) = line.strip_prefix("# ") {
            out.push(ChapterIn {
                num: out.len() + 1,
                title: title.trim().to_string(),
                body: String::new(),
            });
        } else if let Some(last) = out.last_mut() {
            if !last.body.is_empty() {
                last.body.push('\n');
            }
            last.body.push_str(line);
        }
    }
    out
}

/// Per-chapter files from the extractor (`NNNN.txt`), numbered as on disk.
fn chapters_from_dir(dir: &Path) -> Result<Vec<ChapterIn>> {
    let mut files: Vec<(usize, PathBuf)> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
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
        .collect();
    files.sort_by_key(|(num, _)| *num);

    Ok(files
        .into_iter()
        .filter_map(|(num, p)| {
            let raw = std::fs::read_to_string(&p).ok()?;
            let (title, body) = raw.split_once('\n').unwrap_or((raw.trim(), ""));
            Some(ChapterIn {
                num,
                title: title.strip_prefix("# ")?.trim().to_string(),
                body: body.to_string(),
            })
        })
        .collect())
}

/// Preferred source: `<stem>_chapters/NNNN.txt` (numbers match the catalog,
/// so resume spans runs with different --start/--limit); fallback: the
/// assembled book txt, numbered sequentially.
fn load_chapters(input: &Path) -> Result<(Vec<ChapterIn>, String)> {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("book")
        .to_string();
    let parent = input.parent().unwrap_or(Path::new("."));
    let chapters_dir = parent.join(format!("{stem}_chapters"));
    if chapters_dir.is_dir() {
        let chs = chapters_from_dir(&chapters_dir)?;
        if !chs.is_empty() {
            return Ok((
                chs,
                format!("{} (per-chapter files)", chapters_dir.display()),
            ));
        }
    }
    let raw = std::fs::read_to_string(input)
        .with_context(|| format!("reading {input}", input = input.display()))?;
    let raw = ideographic_space_re().replace_all(&raw, "").into_owned();
    Ok((chapters_from_txt(&raw), input.display().to_string()))
}

// ---------------------------------------------------------------------------
// Translator
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = "You are a professional literary translator working from Chinese (simplified) to Vietnamese.\
\nTranslate the chapter given by the user completely and faithfully; do not omit or summarize anything.\
\nPreserve the paragraph structure (blank lines) of the body.\
\nUse the known proper-noun translations exactly as given.\
\nReply with ONLY a JSON object - no markdown fences, no commentary - shaped like:\
\n{\"title\": \"<chapter title translated into Vietnamese>\", \"translation\": \"<the full Vietnamese translation>\", \"glossary\": [{\"hanzi\": \"<Chinese term>\", \"pinyin\": \"<toneless lowercase pinyin>\", \"role\": \"<what this is and why it matters, in English>\", \"importance\": <1|2|3>}]}\
\nThe glossary must list EVERY proper noun you handled (people, places, organizations, sects, titles), including ones already in the known list. importance: 3 = main character or central entity, 2 = named recurring side entity, 1 = minor one-off mention.";

const MAX_RETRIES: usize = 4;
const BASE_RETRY_MS: u64 = 2000;

pub struct Translator {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: String,
    rate: Arc<RateLimiter>,
    glossary: Arc<Glossary>,
    book: String,
}

/// What one API call produced: translated title + body, plus the proper nouns
/// the model claims to have handled.
struct ChapterOut {
    title: String,
    translation: String,
    entries: Vec<NewEntry>,
}

impl Translator {
    pub fn new(book: &str, per_minute: u32, glossary: Arc<Glossary>) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if api_key.is_empty() {
            bail!("OPENAI_API_KEY is not set; add it to .env (see .env template)");
        }
        let base = std::env::var("OPENAI_BASE_URL")
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let model = std::env::var("OPENAI_MODEL")
            .map(|v| v.trim().to_string())
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
            url: format!("{base}/chat/completions"),
            model,
            api_key,
            rate: Arc::new(RateLimiter::new(per_minute)),
            glossary,
            book: book.to_string(),
        })
    }

    /// One API call with hard pacing and 429/Retry-After + exponential
    /// backoff. Every attempt (including retries) goes through the limiter.
    async fn call(&self, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let mut attempt = 0usize;
        loop {
            self.rate.acquire().await;
            let resp = self
                .http
                .post(&self.url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("POST {}", self.url))?;
            let status = resp.status();
            if status.as_u16() == 429 || status.is_server_error() {
                if attempt >= MAX_RETRIES {
                    bail!("API kept returning {status} after {attempt} retries");
                }
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let mut drain = String::new();
                let _ = resp.text().await.map(|t| drain.push_str(&t));
                let delay = retry_after
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| {
                        let exp = BASE_RETRY_MS.saturating_mul(1 << attempt.min(6));
                        Duration::from_millis(exp + jitter_ms())
                    });
                println!(
                    "[translate] {status} from the API; retry {}/{} in {:.1}s",
                    attempt + 1,
                    MAX_RETRIES,
                    delay.as_secs_f64()
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                bail!("API returned {status}: {}", truncate(&detail, 300));
            }
            let v: serde_json::Value = resp
                .json()
                .await
                .context("API response was not JSON")?;
            return v
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("API response has no choices[0].message.content"));
        }
    }

    /// Translate one chapter: retrieve relevant glossary entries, call the
    /// API, parse the (tolerated-messy) reply, upsert the glossary.
    async fn translate_chapter(&self, ch: &ChapterIn) -> Result<ChapterOut> {
        let body = ch.body.trim();
        let known = self.glossary.relevant(body, 12)?;
        let mut system = SYSTEM_PROMPT.to_string();
        if !known.is_empty() {
            system.push_str("\n\nKnown proper nouns (translate exactly like this):");
            for e in &known {
                system.push_str(&format!(
                    "\n  {} -> {} [importance {}] {}",
                    e.hanzi,
                    best_translation(&e),
                    e.importance,
                    e.role
                ));
            }
        }
        let user = format!("Title: {}\n\nBody:\n{body}", ch.title);
        let content = self.call(&system, &user).await?;
        let out = parse_reply(&content, &ch.title);

        // Self-updating glossary: admit what the model reported, after EVERY
        // call, first-writer-wins keyed on hanzi.
        if !out.entries.is_empty() {
            let first_seen = format!("{}#{:04}", self.book, ch.num);
            match self.glossary.admit(&out.entries, &first_seen) {
                Ok(n) if n > 0 => {
                    println!("  [glossary] +{n} new entries (total: {})", self.glossary.count().unwrap_or(0))
                }
                Ok(_) => {}
                Err(e) => println!("  [glossary] upsert failed (non-fatal): {e:#}"),
            }
        }
        Ok(out)
    }
}

/// What the model should use for a known term: its role text usually says the
/// established translation; pinyin is the neutral fallback.
fn best_translation(e: &crate::glossary::Entry) -> String {
    if e.pinyin.is_empty() && e.role.is_empty() {
        e.hanzi.clone()
    } else {
        e.pinyin.clone()
    }
}

/// Cap long payloads for error messages.
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// xorshift-ish jitter (0..512 ms) for backoff — no rand dep needed.
fn jitter_ms() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % 512
}

/// Parse the model reply: it should be a JSON object with `title`,
/// `translation` and `glossary`; anything else (plain prose, fenced markdown,
/// trailing chatter) degrades to "the whole reply is the translation", because
/// the translated text is what matters — glossary data is best-effort.
fn parse_reply(content: &str, fallback_title: &str) -> ChapterOut {
    let trimmed = content.trim();
    // Strip a markdown code fence if the model added one anyway.
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_start())
        .unwrap_or(trimmed)
        .trim_end();
    let stripped = stripped.strip_suffix("```").unwrap_or(stripped).trim();

    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}')) {
        if end > start {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&stripped[start..=end]) {
                let translation = v
                    .get("translation")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !translation.is_empty() {
                    let title = v
                        .get("title")
                        .and_then(|t| t.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(fallback_title)
                        .to_string();
                    let entries = v
                        .get("glossary")
                        .and_then(|g| g.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| {
                                    Some(NewEntry {
                                        hanzi: item
                                            .get("hanzi")
                                            .and_then(|h| h.as_str())?
                                            .to_string(),
                                        pinyin: item
                                            .get("pinyin")
                                            .and_then(|p| p.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        role: item
                                            .get("role")
                                            .and_then(|r| r.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        importance: item
                                            .get("importance")
                                            .and_then(|i| i.as_i64())
                                            .unwrap_or(1),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    return ChapterOut {
                        title,
                        translation,
                        entries,
                    };
                }
            }
        }
    }
    ChapterOut {
        title: fallback_title.to_string(),
        translation: stripped.to_string(),
        entries: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Book driver
// ---------------------------------------------------------------------------

/// Translate the whole book at `input` (chapter by chapter, resumable), then
/// assemble the translated book txt at `output` (or `<stem>_vi.txt`).
pub async fn translate_book(
    input: &str,
    output: Option<&str>,
    jobs: usize,
    per_minute: u32,
) -> Result<()> {
    config::load_env();
    let input = PathBuf::from(input);
    if !input.exists() {
        bail!("input {} does not exist", input.display());
    }
    let (chapters, source) = load_chapters(&input)?;
    if chapters.is_empty() {
        bail!("no chapters found in {}", input.display());
    }

    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("book")
        .to_string();
    let parent = input.parent().unwrap_or(Path::new(".")).to_path_buf();
    let translated_dir = parent.join(format!("{stem}_translated"));
    std::fs::create_dir_all(&translated_dir).ok();
    let out_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| parent.join(format!("{stem}_vi.txt")));

    let glossary = Arc::new(Glossary::open(&PathBuf::from(glossary::DB_PATH))?);
    let translator = Translator::new(&stem, per_minute, glossary.clone())?;
    println!(
        "translate: {} chapters from {source}; model {} @ {} req/min, jobs {jobs}",
        chapters.len(),
        translator.model,
        per_minute
    );

    // Resume: chapters already on disk are skipped.
    let total = chapters.len();
    let done = Arc::new(AtomicUsize::new(0));
    let failures: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let mut pending: Vec<ChapterIn> = Vec::new();
    for ch in chapters {
        if translated_dir.join(format!("{:04}.txt", ch.num)).exists() {
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            println!("  [{n}/{total}] ch{:04} {}... resumed", ch.num, ch.title);
            continue;
        }
        pending.push(ch);
    }

    let translated_dir_for_task = Arc::new(translated_dir.clone());
    let jobs = jobs.max(1);
    futures::stream::iter(pending)
        .map(|ch| {
            let translator = &translator;
            let translated_dir = translated_dir_for_task.clone();
            let done = done.clone();
            let failures = failures.clone();
            async move {
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                match translator.translate_chapter(&ch).await {
                    Ok(out) => {
                        let piece = format!("# {}\n{}", out.title, out.translation);
                        let path = translated_dir.join(format!("{:04}.txt", ch.num));
                        match std::fs::write(&path, &piece) {
                            Ok(()) => println!(
                                "  [{n}/{total}] ch{:04} {}... {} chars",
                                ch.num,
                                ch.title,
                                out.translation.chars().count()
                            ),
                            Err(e) => {
                                failures.lock().unwrap().push(format!("ch{:04}: {e}", ch.num));
                                println!("  [{n}/{total}] ch{:04} {}... ERROR: {e}", ch.num, ch.title);
                            }
                        }
                    }
                    Err(e) => {
                        failures
                            .lock()
                            .unwrap()
                            .push(format!("ch{:04} {}: {e}", ch.num, ch.title));
                        println!("  [{n}/{total}] ch{:04} {}... ERROR: {e}", ch.num, ch.title);
                    }
                }
            }
        })
        .buffer_unordered(jobs)
        .for_each(|_| async {})
        .await;

    // Assemble in catalog order — byte-compatible framing with the
    // extractor's book txt: "# {title}\n{text}" pieces joined by "\n\n".
    let mut files: Vec<(usize, PathBuf)> = std::fs::read_dir(&translated_dir)
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
    files.sort_by_key(|(num, _)| *num);
    let combined: Vec<String> = files
        .into_iter()
        .filter_map(|(_, p)| std::fs::read_to_string(p).ok())
        .collect();
    if combined.is_empty() {
        bail!("nothing translated; first error: {}",
            failures
                .lock()
                .unwrap()
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".into()));
    }
    std::fs::write(&out_path, combined.join("\n\n"))
        .with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "Saved {} translated chapters -> {}",
        combined.len(),
        out_path.display()
    );
    println!(
        "Glossary: {} entries at {}",
        glossary.count().unwrap_or(0),
        config::output_path("glossary.db").display()
    );
    let failed = failures.lock().unwrap();
    if !failed.is_empty() {
        println!("Failures ({}):", failed.len());
        for f in failed.iter() {
            println!("  {f}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_spaces_calls_at_the_interval() {
        let per_min = 10.0; // -> 6 s spacing
        let t0 = Instant::now();
        let mut b = Bucket {
            tokens: 1.0,
            last: t0,
        };
        // First call: immediate.
        assert!(RateLimiter::grant_or_wait(&mut b, per_min, t0).is_ok());
        // 3 s later: only half a token accrued -> must wait 3 more seconds.
        let err = RateLimiter::grant_or_wait(&mut b, per_min, t0 + Duration::from_secs(3))
            .unwrap_err();
        assert_eq!(err, Duration::from_secs_f64(3.0));
        // At the full 6 s mark: granted again.
        assert!(RateLimiter::grant_or_wait(&mut b, per_min, t0 + Duration::from_secs(6)).is_ok());
    }

    #[test]
    fn token_bucket_never_accumulates_burst() {
        let per_min = 10.0;
        let t0 = Instant::now();
        let mut b = Bucket {
            tokens: 1.0,
            last: t0,
        };
        assert!(RateLimiter::grant_or_wait(&mut b, per_min, t0).is_ok());
        // A long idle period must NOT bank extra tokens: capacity is 1.
        assert!(RateLimiter::grant_or_wait(&mut b, per_min, t0 + Duration::from_secs(3600)).is_ok());
        assert!(RateLimiter::grant_or_wait(&mut b, per_min, t0 + Duration::from_secs(3601)).is_err());
    }

    #[tokio::test]
    async fn rate_limiter_paces_calls_in_real_time() {
        // 30/min -> 2 s between the starts of two calls.
        let rl = RateLimiter::new(30);
        let t0 = Instant::now();
        rl.acquire().await; // first token: immediate
        rl.acquire().await; // second: must pace ~2 s
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1990),
            "calls must be paced apart, got {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(4), "no runaway, got {elapsed:?}");
    }

    #[test]
    fn chapters_split_on_hash_lines_even_with_blank_lines_in_bodies() {
        let raw = "# 第一章 山村\n  段落一。\n\n  段落二。\n\n# 第二章 出门\n正文。";
        let chs = chapters_from_txt(raw);
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0].title, "第一章 山村");
        assert_eq!(chs[0].num, 1);
        assert!(chs[0].body.contains("段落一。"));
        assert!(chs[0].body.contains("段落二。"));
        assert_eq!(chs[1].num, 2);
        assert_eq!(chs[1].body.trim(), "正文。");
    }

    #[test]
    fn parse_reply_accepts_clean_json() {
        let reply = r#"{"title": "Chương một", "translation": "Làn sóng vỗ bờ.", "glossary": [{"hanzi": "墨画", "pinyin": "mo hoa", "role": "main character", "importance": 3}]}"#;
        let out = parse_reply(reply, "原标题");
        assert_eq!(out.title, "Chương một");
        assert_eq!(out.translation, "Làn sóng vỗ bờ.");
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].hanzi, "墨画");
    }

    #[test]
    fn parse_reply_survives_fences_chatter_and_broken_json() {
        // Fenced JSON with chatter around it.
        let fenced = "Here you go:\n```json\n{\"translation\": \"Xin chào.\", \"title\": \"Tieu de\"}\n```\nHope that helps!";
        let out = parse_reply(fenced, "F");
        assert_eq!(out.translation, "Xin chào.");
        assert_eq!(out.title, "Tieu de");

        // Malformed JSON -> whole reply becomes the translation (best effort).
        let broken = "{\"translation\": \"gibberish";
        let out = parse_reply(broken, "F");
        assert_eq!(out.translation, broken);
        assert_eq!(out.title, "F");
        assert!(out.entries.is_empty());
    }
}
