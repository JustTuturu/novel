//! Qidian rule (book.qidian.com / www.qidian.com) — port of the qidian rule
//! from 404-novel-project/novel-downloader, matching both the Python original
//! (src/novel/qidian.py) and the verified Go port (internal/qidian/rule.go).
//!
//! Notable differences from the userscript:
//!
//! * The userscript can only *render* the encrypted font through a real browser
//!   and then OCRs glyph canvases; we do the same, but inside our own CDP page
//!   (or the user's real browser), which also satisfies qidian's cookie /
//!   _csrfToken requirements for VIP chapters.
//! * Font deobfuscation = build a glyph map once (render the custom-font glyphs
//!   to a canvas in rows of 30, OCR the rows, zip them back together), cache it,
//!   then rewrite every PUA character in the chapter text. Per-glyph rendering
//!   is the fallback when a row OCRs to the wrong length.

use anyhow::{anyhow, Context, Result};
use futures::future::BoxFuture;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::browser::{self, Session};
use crate::config;
use crate::ocr;
use crate::rule::{self, Book, Chapter, ChapterResult};
use crate::text;

/// Tried in order until one yields non-empty chapter text.
const CONTENT_SELECTORS: &[&str] = &[
    "main.content",
    "main[id^='c-']",
    ".read-content",
    ".chapter-content",
    ".text-wrap .read-content",
];

/// Block-level element names for the [`Qidian::qd_inner_text`] innerText
/// emulation (`innerText` breaks the line before AND after a block element,
/// which is why consecutive paragraphs come out "\n\n"-separated).
const BLOCK_ELEMENTS: &[&str] = &[
    "p", "div", "main", "section", "article", "h1", "h2", "h3", "h4", "h5",
    "h6", "li", "tr", "blockquote", "pre", "ul", "ol", "table", "header",
    "footer", "nav", "aside", "figure", "figcaption", "dd", "dt",
];

/// Paywall strings in the page body; any hit means the chapter is not readable.
const LOCK_MARKERS: &[&str] = &[
    "这是VIP章节",
    "订阅本章",
    "登录后阅读本章",
    "您还不是VIP章节",
];

/// How many glyphs share one canvas row (see `RENDER_ROWS_JS`).
const PER_ROW: usize = 30;

/// Persistent `{font -> {PUA glyph -> real char}}` map.
fn glyphmap_path() -> PathBuf {
    config::output_path("qidian_fontmap.json")
}

/// On-disk glyph cache. Grouped by font-family so the raw key stays
/// `font|glyph` while the file stays a plain, diffable JSON object.
type FontMap = BTreeMap<String, BTreeMap<char, char>>;

/// Renders the given characters in the page's own (encrypted) font onto a canvas
/// and returns one data-URL per row of `perRow` glyphs.
const RENDER_ROWS_JS: &str = r##"({chars, font, perRow}) => {
  const cv = document.createElement('canvas');
  const ctx = cv.getContext('2d');
  const out = [];
  for (let i = 0; i < chars.length; i += perRow) {
    const row = chars.slice(i, i + perRow);
    const size = 48, scale = 2, pad = 8;
    cv.width = (size + pad) * row.length * scale;
    cv.height = (size + pad * 2) * scale;
    ctx.scale(scale, scale);
    ctx.font = `${size}px ${font}`;
    ctx.textBaseline = 'middle';
    ctx.fillStyle = '#fff';
    ctx.fillRect(0, 0, cv.width, cv.height);
    ctx.fillStyle = '#000';
    row.forEach((ch, j) => ctx.fillText(ch, j * (size + pad) + pad / 2, size + pad));
    out.push(cv.toDataURL('image/png'));
  }
  return out;
}"##;

/// Renders one glyph per canvas — the fallback when a row OCRs to the wrong
/// number of characters.
const RENDER_SINGLE_JS: &str = r##"({chars, font}) => chars.map(ch => {
  const size = 64, scale = 3, pad = 12;
  const cv = document.createElement('canvas');
  cv.width = (size + pad * 2) * scale;
  cv.height = (size + pad * 2) * scale;
  const ctx = cv.getContext('2d');
  ctx.scale(scale, scale);
  ctx.font = `${size}px ${font}`;
  ctx.textBaseline = 'middle';
  ctx.fillStyle = '#fff';
  ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.fillStyle = '#000';
  ctx.fillText(ch, pad, size + pad);
  return cv.toDataURL('image/png');
})"##;

/// Computed font-family of the chapter container, which names the (encrypted)
/// custom font.
const FONT_FAMILY_JS: &str = r##"(sel) => {
  const el = document.querySelector(sel) || document.body;
  return getComputedStyle(el).fontFamily;
}"##;

fn book_id_path_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/(?:book|info)/(\d+)").unwrap())
}

fn book_id_num_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"([0-9]{5,})").unwrap())
}

fn word_count_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"章节字数：(\d+)").unwrap())
}

/// The tooltip metadata Qidian hides in a chapter link's title attribute.
fn title_label_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\s*(?:首发时间|章节字数|更新时间|最后更新|更新时间：|字数：)\s*").unwrap()
    })
}

fn title_date_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s+\d{4}-\d{2}-\d{2}").unwrap())
}

fn title_paren_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s*（.*?）\s*$").unwrap())
}

/// One (how-to-read, selector) pair tried by [`Qidian::meta`].
///
/// The Python/Go originals tag these with the *attribute name* they were
/// looking for ("title", "name", "class", "property") and only ever test for
/// "property", which really means "read the `content` attribute" — that is what
/// `attr` says here.
struct MetaCandidate {
    attr: bool,
    sel: &'static str,
}

const TITLE_CANDIDATES: &[MetaCandidate] = &[
    MetaCandidate { attr: false, sel: "h1" },
    MetaCandidate {
        attr: false,
        sel: "h1.bookName",
    },
    MetaCandidate {
        attr: true,
        sel: r#"meta[property="og:title"]"#,
    },
];

const AUTHOR_CANDIDATES: &[MetaCandidate] = &[
    // ".author" renders as "作者：爱潜水的乌贼"; strip the label so the CLI's
    // own "作者:" prefix doesn't double up.
    MetaCandidate { attr: false, sel: ".author" },
    MetaCandidate {
        attr: false,
        sel: ".author-name",
    },
    MetaCandidate {
        attr: true,
        sel: r#"meta[name="author"]"#,
    },
];

const INTRO_CANDIDATES: &[MetaCandidate] = &[
    MetaCandidate {
        attr: false,
        sel: ".book-intro-detail",
    },
    MetaCandidate {
        attr: false,
        sel: "#book-intro-detail",
    },
    MetaCandidate {
        attr: true,
        sel: r#"meta[name="description"]"#,
    },
];

/// Plain-HTTP attempt result ([`Qidian::http_chapter`]).
enum HttpOutcome {
    /// Server-rendered HTML served the chapter; result ready.
    Served(ChapterResult),
    /// Paywall markers present in the served HTML.
    Locked,
    /// Could not serve it (anti-bot challenge page, empty render) — let the
    /// browser render it instead.
    Fallback,
}

pub struct Qidian;

impl Qidian {
    // -- catalog ---------------------------------------------------------

    /// Pulls the numeric book id out of any qidian book/info URL.
    fn book_id<'a>(&self, url: &'a str) -> &'a str {
        if let Some(c) = book_id_path_re().captures(url) {
            return c.get(1).map(|m| m.as_str()).unwrap_or(url);
        }
        if let Some(c) = book_id_num_re().captures(url) {
            return c.get(1).map(|m| m.as_str()).unwrap_or(url);
        }
        url.trim_matches('/').split('/').last().unwrap_or(url)
    }

    /// Maps a book URL onto the catalog page for the host that was given.
    fn catalog_url(&self, url: &str) -> String {
        let id = self.book_id(url);
        if url.contains("www.qidian.com") {
            format!("https://www.qidian.com/book/{id}/")
        } else {
            format!("https://book.qidian.com/info/{id}/")
        }
    }

    /// First non-empty value among the candidate selectors.
    fn meta(&self, doc: &Html, candidates: &[MetaCandidate]) -> String {
        self.meta_raw(doc, candidates).trim_start_matches("作者：").trim_start_matches("作者:").trim().to_string()
    }

    fn meta_raw(&self, doc: &Html, candidates: &[MetaCandidate]) -> String {
        for c in candidates {
            let Ok(sel) = Selector::parse(c.sel) else {
                continue;
            };
            let Some(el) = doc.select(&sel).next() else {
                continue;
            };
            let val = if c.attr {
                el.attr("content").unwrap_or_default().to_string()
            } else {
                el_text(el, " ")
            };
            let val = val.trim();
            if !val.is_empty() {
                return val.to_string();
            }
        }
        String::new()
    }

    /// Drop the catalog tooltip metadata Qidian puts in the title attribute.
    ///
    /// e.g. "第一章 绯红 首发时间：2018-04-01 12:28:26 章节字数：2425"
    ///     -> "第一章 绯红"
    fn clean_chapter_title(raw: &str) -> String {
        let raw = raw.trim();
        // Cut at the first metadata label; also drop a trailing time/date run.
        let title = title_label_re().split(raw).next().unwrap_or(raw);
        let title = title_date_re().split(title).next().unwrap_or(title);
        let title = title_paren_re().replace_all(title, "");
        let title = title.trim();
        if title.is_empty() {
            raw.to_string()
        } else {
            title.to_string()
        }
    }

    /// Make a catalog href absolute against qidian's book host.
    fn abs(href: &str) -> String {
        if href.starts_with("//") {
            return format!("https:{href}");
        }
        if href.starts_with("http") {
            return href.to_string();
        }
        format!("https://book.qidian.com{href}")
    }

    /// Maps (locked, is_vip) onto the tri-state `is_paid` field:
    /// locked -> Some(false), VIP-and-not-locked -> None ("catalog can't tell"),
    /// free -> Some(true).
    fn paid_flag(locked: bool, is_vip: bool) -> Option<bool> {
        match () {
            _ if locked => Some(false),
            _ if is_vip => None,
            _ => Some(true),
        }
    }

    /// Port of the qidian rule's bookParse: www.qidian.com uses
    /// `.catalog-volume` + `li.chapter-item` (locked = `.chapter-locked`);
    /// book.qidian.com uses `#j-catalogWrap .volume` where a volume header
    /// containing "VIP" marks the whole volume VIP (free ones carry an
    /// `<em class=iconfont>`).
    fn parse_chapters(&self, doc: &Html) -> Vec<Chapter> {
        let mut chapters: Vec<Chapter> = Vec::new();
        let volumes: Vec<ElementRef> = {
            let first: Vec<ElementRef> = doc.select(&browser::selector(".catalog-volume")).collect();
            if first.is_empty() {
                doc.select(&browser::selector("#j-catalogWrap .volume"))
                    .collect()
            } else {
                first
            }
        };

        if !volumes.is_empty() {
            let header_sel = browser::selector("h2.head, h2, .head");
            let items_sel = browser::selector("li.chapter-item");
            let plain_items_sel = browser::selector("li");
            let anchor_sel = browser::selector("a.chapter-name");
            let plain_anchor_sel = browser::selector("a");
            let locked_sel = browser::selector(".chapter-locked");
            let free_vip_sel = browser::selector("em.iconfont");

            let mut section_number = 0usize;
            let mut num = 0usize;
            for vol in volumes {
                let section_name = vol
                    .select(&header_sel)
                    .next()
                    .map(|h| el_text(h, " "))
                    .unwrap_or_default();
                let volume_vip = section_name.to_uppercase().contains("VIP");
                if !section_name.is_empty() {
                    section_number += 1;
                }
                let mut items: Vec<ElementRef> = vol.select(&items_sel).collect();
                if items.is_empty() {
                    items = vol.select(&plain_items_sel).collect();
                }
                for li in items {
                    let a = li
                        .select(&anchor_sel)
                        .next()
                        .or_else(|| li.select(&plain_anchor_sel).next());
                    let href = a.and_then(|el| el.attr("href")).unwrap_or_default();
                    if href.is_empty() {
                        continue;
                    }
                    let locked = li.select(&locked_sel).next().is_some();
                    let free_vip = li.select(&free_vip_sel).next().is_some();
                    let is_vip = locked || (volume_vip && !free_vip);
                    let attr_title = a.and_then(|el| el.attr("title")).unwrap_or_default();
                    let raw_title = match attr_title.is_empty() {
                        true => a.map(|el| el_text(el, " ")).unwrap_or_default(),
                        false => attr_title.to_string(),
                    };
                    let word_count = word_count_re()
                        .captures(attr_title)
                        .and_then(|c| c.get(1))
                        .and_then(|m| m.as_str().parse::<usize>().ok())
                        .unwrap_or(0);
                    num += 1;
                    chapters.push(Chapter {
                        num,
                        title: Self::clean_chapter_title(&raw_title),
                        url: Self::abs(href),
                        is_vip,
                        is_paid: Self::paid_flag(locked, is_vip),
                        section_name: section_name.clone(),
                        section_number,
                        word_count,
                    });
                }
            }
        } else {
            let items_sel = browser::selector("li.chapter-item");
            let anchor_sel = browser::selector("a.chapter-name");
            let plain_anchor_sel = browser::selector("a");
            let locked_sel = browser::selector(".chapter-locked, .iconfont.lock");

            let mut num = 0usize;
            for li in doc.select(&items_sel) {
                let a = li
                    .select(&anchor_sel)
                    .next()
                    .or_else(|| li.select(&plain_anchor_sel).next());
                let href = a.and_then(|el| el.attr("href")).unwrap_or_default();
                if href.is_empty() {
                    continue;
                }
                let locked = li.select(&locked_sel).next().is_some();
                let attr_title = a.and_then(|el| el.attr("title")).unwrap_or_default();
                let raw_title = match attr_title.is_empty() {
                    true => a.map(|el| el_text(el, " ")).unwrap_or_default(),
                    false => attr_title.to_string(),
                };
                num += 1;
                chapters.push(Chapter {
                    num,
                    title: Self::clean_chapter_title(&raw_title),
                    url: Self::abs(href),
                    is_vip: locked,
                    is_paid: Some(!locked),
                    section_name: String::new(),
                    section_number: 0,
                    word_count: 0,
                });
            }
        }

        chapters.sort_by_key(|c| c.num);
        chapters
    }

    // -- chapters --------------------------------------------------------

    /// Plain-HTTP attempt for server-rendered free chapters. Carries the live
    /// browser cookie jar, so a logged-in/entitled session benefits too; it
    /// only wins completely when the served PUA glyphs are all in the font
    /// cache (a cache miss still needs a browser canvas to render).
    async fn http_chapter(ch: &Chapter, s: &Session) -> Result<HttpOutcome> {
        let html = match s.fetch(&ch.url, None, &[]).await {
            Ok(t) => t,
            Err(_) => return Ok(HttpOutcome::Fallback),
        };
        // scraper::Html is not `Send`; parse + reduce inside one block that
        // closes before the next await (same pattern as the sfacg rule).
        let outcome = {
            let doc = Html::parse_document(&html);
            let body = doc_body_text(&doc, " ");
            if LOCK_MARKERS.iter().any(|m| body.contains(m)) {
                return Ok(HttpOutcome::Locked);
            }
            let mut txt = String::new();
            for sel in CONTENT_SELECTORS {
                let Ok(parsed) = Selector::parse(sel) else {
                    continue;
                };
                let Some(el) = doc.select(&parsed).next() else {
                    continue;
                };
                // innerText-equivalent extraction: the pre-Phase-2 browser
                // route read this container's `innerText` with real paragraph
                // breaks, so the plain-HTTP path must reproduce those bytes.
                let raw = Self::qd_inner_text(el).trim().to_string();
                if !raw.is_empty() {
                    txt = raw;
                    break;
                }
            }
            if txt.is_empty() {
                HttpOutcome::Fallback
            } else {
                let mut title = ch.title.clone();
                title = ["h1.j_chapterName", ".chapter-name", "h1"]
                    .iter()
                    .find_map(|css| {
                        Selector::parse(css)
                            .ok()
                            .map(|sel| Self::el_line_title(&doc, &sel))
                            .filter(|t| !t.is_empty())
                    })
                    .unwrap_or(title);
                if text::pua_ratio(&txt) <= 0.05 {
                    HttpOutcome::Served(ChapterResult {
                        title,
                        text: text::clean_text(&txt, &[]),
                        ..Default::default()
                    })
                } else if let Some(decoded) = Self::decode_pua_cached(&txt) {
                    HttpOutcome::Served(ChapterResult {
                        title,
                        text: text::clean_text(&decoded, &[]),
                        ..Default::default()
                    })
                } else {
                    // Encrypted font with glyphs the cache does not cover:
                    // the canvas render needs a browser.
                    HttpOutcome::Fallback
                }
            }
        };
        Ok(outcome)
    }

    /// First line of an element's innerText (`innerText` can render several
    /// lines for the chapter heading on qidian's redesigned pages).
        fn el_line_title(doc: &Html, sel: &Selector) -> String {
        doc.select(sel)
            .next()
            .map(|el| Self::qd_inner_text(el).trim().to_string())
            .map(|t| t.lines().next().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    /// Browser `innerText` emulation for a container subtree: text nodes are
    /// concatenated verbatim, script/style subtrees contribute nothing, and
    /// block-level children get a newline before and after, so successive
    /// paragraphs are separated by "\n\n" — exactly what `innerText` used to
    /// hand the CLI when the chapter was rendered in the browser.
    fn qd_inner_text(root: ElementRef) -> String {
        fn walk(node: ego_tree::NodeRef<'_, scraper::Node>, out: &mut String) {
            match node.value() {
                scraper::Node::Text(t) => out.push_str(&t.text),
                scraper::Node::Element(el) => {
                    let name = el.name().to_ascii_lowercase();
                    if matches!(name.as_str(), "script" | "style" | "noscript") {
                        return; // not rendered -> innerText sees nothing
                    }
                    if name == "br" {
                        out.push('\n');
                        return;
                    }
                    let block = BLOCK_ELEMENTS.contains(&name.as_str());
                    if block && !out.is_empty() {
                        // innerText prints one newline before and one after a
                        // block element ("\n\n" between paragraphs).
                        out.push('\n');
                    }
                    let before = out.len();
                    for child in node.children() {
                        walk(child, out);
                    }
                    if block && out.len() > before {
                        out.push('\n');
                    }
                }
                _ => {
                    for child in node.children() {
                        walk(child, out);
                    }
                }
            }
        }

        let mut out = String::new();
        walk(*root, &mut out);
        out
    }
    /// Best-effort PUA decode using ONLY the on-disk font cache (no page).
    /// Tries every cached font (Qidian names its encrypted fonts per book) and
    /// keeps the mapping that resolves the most glyphs. `None` when anything
    /// meaningful stays unresolved.
    fn decode_pua_cached(txt: &str) -> Option<String> {
        let pua_chars = text::pua_chars(txt);
        if pua_chars.is_empty() {
            return Some(txt.to_string());
        }
        let mut best: HashMap<char, char> = HashMap::new();
        let mut best_hits = 0usize;
        for (_, map) in load_fontmap() {
            let mut resolved: HashMap<char, char> = HashMap::new();
            for c in &pua_chars {
                if let Some(v) = map.get(c) {
                    resolved.insert(*c, *v);
                }
            }
            if resolved.len() > best_hits {
                best_hits = resolved.len();
                best = resolved;
            }
        }
        if best_hits < pua_chars.len() {
            return None;
        }
        Some(text::replace_map(txt, &best))
    }

    /// The browser-rendered path (unchanged behaviour: this is what ran before
    /// Phase 2, still needed for VIP iframe text and Plex/anti-bot pages).
    async fn browser_chapter(ch: &Chapter, s: &Session) -> Result<ChapterResult> {
        s.goto(&ch.url)
            .await
            .with_context(|| format!("qidian: goto {}", ch.url))?;

        let body = s.text_of("body").await.unwrap_or_default();
        if Self::is_locked(&body, s).await {
            return Ok(ChapterResult {
                title: ch.title.clone(),
                locked: true,
                ..Default::default()
            });
        }

        let mut txt = String::new();
        let mut selector = "";
        for sel in CONTENT_SELECTORS {
            let Ok(raw) = s.text_of(sel).await else {
                continue;
            };
            let raw = raw.trim().to_string();
            if !raw.is_empty() {
                txt = raw;
                selector = sel;
                break;
            }
        }

        let mut title = ch.title.clone();
        if let Ok(raw) = s.text_of("h1.j_chapterName, .chapter-name, h1").await {
            let raw = raw.trim();
            if !raw.is_empty() {
                title = raw.lines().next().unwrap_or("").to_string();
            }
        }

        if txt.is_empty() {
            return Ok(ChapterResult {
                title,
                needs_ocr: true,
                ..Default::default()
            });
        }

        if text::pua_ratio(&txt) <= 0.05 {
            eprintln!("[dbg] ch {} raw={} cleaned={}", ch.num, txt.chars().count(), text::clean_text(&txt, &[]).chars().count());
            return Ok(ChapterResult {
                title,
                text: text::clean_text(&txt, &[]),
                ..Default::default()
            });
        }

        // Encrypted font: decode PUA glyphs via the cached font map, then
        // rebuild the text.
        let decoded = Self::decode_pua(s, &txt, selector).await?;
        if text::pua_ratio(&decoded) > 0.05 {
            // best-effort: the CLI OCRs these screenshots later
            let _ = Self::screenshot(ch, s).await;
            return Ok(ChapterResult {
                title,
                needs_ocr: true,
                ..Default::default()
            });
        }
        eprintln!("[dbg] ch {} raw={} decoded={} cleaned={}", ch.num, txt.chars().count(), decoded.chars().count(), text::clean_text(&decoded, &[]).chars().count());
        Ok(ChapterResult {
            title,
            text: text::clean_text(&decoded, &[]),
            ..Default::default()
        })
    }
    /// The paywall: a lock marker in the body text, or the VIP limit overlay
    /// being present.
    async fn is_locked(body: &str, s: &Session) -> bool {
        if LOCK_MARKERS.iter().any(|m| body.contains(m)) {
            return true;
        }
        s.text_of(".vip-limit-wrap").await.is_ok()
    }

    /// Keep the encrypted-font rendering so `novel ocr` can recover it.
    async fn screenshot(ch: &Chapter, s: &Session) -> Result<PathBuf> {
        let outdir =
            config::output_path(&format!("qidian_{}_screenshots", url_segment(&ch.url, 3)));
        std::fs::create_dir_all(&outdir)
            .with_context(|| format!("creating {}", outdir.display()))?;
        let cid = url_segment(ch.url.trim_end_matches('/'), -1);
        let path = outdir.join(format!("{cid}.png"));
        s.screenshot(&path)
            .await
            .with_context(|| format!("screenshot {}", path.display()))?;
        Ok(path)
    }

    /// Rewrites every PUA character of `txt` through the font glyph map,
    /// OCRing only the glyphs that are not cached yet.
    async fn decode_pua(s: &Session, txt: &str, selector: &str) -> Result<String> {
        let pua_chars = text::pua_chars(txt);
        let font = Self::element_font_family(s, selector).await?;
        let cached = load_fontmap();
        let mut mapping: HashMap<char, char> = HashMap::new();
        let hit = cached.get(&font);
        for c in &pua_chars {
            if let Some(v) = hit.and_then(|m| m.get(c)) {
                mapping.insert(*c, *v);
            }
        }
        let missing: Vec<char> = pua_chars
            .iter()
            .copied()
            .filter(|c| !mapping.contains_key(c))
            .collect();
        if !missing.is_empty() {
            println!(
                "    font: {} glyph(s) to OCR ({} row(s))",
                missing.len(),
                missing.len() / PER_ROW + 1
            );
            // Single-threaded on purpose: `build_glyph_map` renders on the page
            // it is handed AND reads/writes the shared cache
            // (`outputs/qidian_fontmap.json`) — concurrent workers must not
            // interleave either side.
            let built = {
                let _gate = glyph_gate().lock().await;
                Self::build_glyph_map(s, &missing, selector).await
            };
            for (k, v) in built? {
                mapping.insert(k, v);
            }
        }
        Ok(text::replace_map(txt, &mapping))
    }

    // -- font map / OCR --------------------------------------------------

    /// Computed font-family of the chapter container; names the custom font.
    async fn element_font_family(s: &Session, selector: &str) -> Result<String> {
        let mut font = String::new();
        s.eval(FONT_FAMILY_JS, Some(serde_json::json!(selector)), &mut font)
            .await
            .context("reading the chapter font family")?;
        Ok(font)
    }

    /// OCRs the page's encrypted font and returns {PUA char -> real char}.
    async fn build_glyph_map(
        s: &Session,
        chars: &[char],
        selector: &str,
    ) -> Result<HashMap<char, char>> {
        let font = Self::element_font_family(s, selector).await?;
        let owned: Vec<String> = chars.iter().map(|c| c.to_string()).collect();

        let mut rows: Vec<String> = Vec::new();
        s.eval(
            RENDER_ROWS_JS,
            Some(serde_json::json!({
                "chars": owned,
                "font": font,
                "perRow": PER_ROW,
            })),
            &mut rows,
        )
        .await
        .context("rendering glyph rows")?;

        let tmp = glyph_tmp_dir();
        std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut client = ocr::Client::new();

        let mut mapping: HashMap<char, char> = HashMap::new();
        let mut per_char_needed: Vec<char> = Vec::new();
        let ocr_texts = ocr_rows(&mut client, &rows, &tmp).await?;
        for (idx, t) in ocr_texts.iter().enumerate() {
            let start = (idx * PER_ROW).min(chars.len());
            let end = (start + PER_ROW).min(chars.len());
            let row_chars = &chars[start..end];
            let glyphs: Vec<char> = t.chars().collect();
            if glyphs.len() == row_chars.len() {
                for (i, g) in glyphs.iter().enumerate() {
                    mapping.insert(row_chars[i], *g);
                }
            } else {
                per_char_needed.extend_from_slice(row_chars);
            }
        }

        if !per_char_needed.is_empty() {
            // rows OCR'd inconsistently -> one glyph at a time
            let singles: Vec<String> = per_char_needed.iter().map(|c| c.to_string()).collect();
            let mut images: Vec<String> = Vec::new();
            s.eval(
                RENDER_SINGLE_JS,
                Some(serde_json::json!({ "chars": singles, "font": font })),
                &mut images,
            )
            .await
            .context("rendering single glyphs")?;
            let single_texts = ocr_rows(&mut client, &images, &tmp).await?;
            for (i, ch) in per_char_needed.iter().enumerate() {
                let Some(t) = single_texts.get(i) else {
                    break;
                };
                let glyphs: Vec<char> = t.chars().collect();
                if glyphs.len() == 1 {
                    mapping.insert(*ch, glyphs[0]);
                }
            }
        }

        std::fs::remove_dir_all(&tmp).ok();

        let mut fontmap = load_fontmap();
        let entry = fontmap.entry(font).or_default();
        for (k, v) in &mapping {
            entry.insert(*k, *v);
        }
        save_fontmap(&fontmap);
        Ok(mapping)
    }
}

impl rule::Rule for Qidian {
    fn name(&self) -> &'static str {
        "qidian"
    }
    fn domains(&self) -> &'static [&'static str] {
        &["qidian.com"]
    }
    fn chapter_track(&self, ch: &Chapter) -> rule::Track {
        // Catalog-known-locked chapters never reach scheduling (the CLI skips
        // `is_paid == Some(false)`). Free chapters are server-rendered; VIP
        // entitlement is unknowable from the catalog, so those read as iframe
        // text through a browser page.
        if ch.is_vip {
            rule::Track::Browser
        } else {
            rule::Track::Http
        }
    }
    fn storage_state_path(&self) -> String {
        "outputs/qidian_storage.json".to_string()
    }
    fn book_parse<'a>(&'a self, url: &'a str, s: &'a Session) -> BoxFuture<'a, Result<Book>> {
        Box::pin(async move {
            let book_id = self.book_id(url);
            let catalog_url = self.catalog_url(url);
            let doc = s
                .dom_soup(&catalog_url, true)
                .await
                .with_context(|| format!("qidian: failed to load {catalog_url}"))?;
            Ok(Book {
                site: self.name().to_string(),
                url: format!("https://book.qidian.com/info/{book_id}/"),
                title: self.meta(&doc, TITLE_CANDIDATES),
                author: self.meta(&doc, AUTHOR_CANDIDATES),
                intro: self.meta(&doc, INTRO_CANDIDATES),
                tags: Vec::new(),
                cover: String::new(),
                chapters: self.parse_chapters(&doc),
            })
        })
    }
    fn chapter_parse<'a>(
        &'a self,
        ch: &'a Chapter,
        s: &'a Session,
    ) -> BoxFuture<'a, Result<ChapterResult>> {
        Box::pin(async move {
            // Phase 2: free chapters are server-rendered, so try plain HTTP
            // first (with the live browser cookie jar). Challenges, lock
            // markers and uncached encrypted fonts fall back below.
            if ch.is_paid != Some(false) {
                match Self::http_chapter(ch, s).await? {
                    HttpOutcome::Served(res) => return Ok(res),
                    HttpOutcome::Locked => {
                        return Ok(ChapterResult {
                            title: ch.title.clone(),
                            locked: true,
                            ..Default::default()
                        })
                    }
                    HttpOutcome::Fallback => {}
                }
            }

            // Browser render. Without --jobs this is the old sequential read;
            // with the worker pool only one HTTP-track fallback may use the
            // main page at a time (pool pages are never free here), so hold a
            // process-wide gate across the whole render.
            let gate = main_page_gate().lock().await;
            let res = Self::browser_chapter(ch, s).await;
            drop(gate);
            res
        })
    }
}

static RULE: Qidian = Qidian;

/// Registered once when the crate initialises.
pub fn register() {
    rule::register(&RULE);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Process-wide lock across the whole browser render of one chapter. HTTP-
/// track fallbacks use the session's main page, which is not a pooled page, so
/// only one may render at a time (`build_glyph_map` is additionally gated by
/// [`glyph_gate`]).
fn main_page_gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Single-threaded glyph-map build (shared cache + shared render page).
fn glyph_gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// goquery's `get_text(sep)` for one element: descendant text nodes, trimmed,
/// empty ones dropped, joined with `sep` (BeautifulSoup's `get_text(" ", …)`).
fn el_text(el: ElementRef, sep: &str) -> String {
    el.text()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Whole-document text pool, like the sfacg rule's `doc_text` (`sep`-joined
/// trimmed text nodes) — used by the plain-HTTP lock-marker check.
fn doc_body_text(doc: &Html, sep: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_body_text(doc.tree.root(), &mut parts);
    parts
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

fn collect_body_text(node: ego_tree::NodeRef<'_, scraper::Node>, out: &mut Vec<String>) {
    match node.value() {
        scraper::Node::Text(t) => out.push(t.text.to_string()),
        _ => {
            for child in node.children() {
                collect_body_text(child, out);
            }
        }
    }
}

/// The glyph cache; a missing or corrupt file yields an empty map.
///
/// Current layout is `{font: {glyph: char}}`. The Python/Go ports wrote a flat
/// `{"font|glyph": "char"}` map instead, so that shape is still accepted — an
/// existing cache keeps working and gets upgraded on the next save.
fn load_fontmap() -> FontMap {
    match std::fs::read_to_string(glyphmap_path()) {
        Ok(raw) => parse_fontmap(&raw),
        Err(_) => FontMap::new(),
    }
}

/// Parse either cache layout; anything unparseable yields an empty map.
fn parse_fontmap(raw: &str) -> FontMap {
    if let Ok(nested) = serde_json::from_str::<FontMap>(raw) {
        return nested;
    }
    let Ok(flat) = serde_json::from_str::<BTreeMap<String, String>>(raw) else {
        return FontMap::new();
    };
    let mut out = FontMap::new();
    for (key, glyph) in flat {
        let Some((font, ch)) = key.split_once('|') else {
            continue;
        };
        let (Some(c), Some(g)) = (ch.chars().next(), glyph.chars().next()) else {
            continue;
        };
        out.entry(font.to_string()).or_default().insert(c, g);
    }
    out
}

/// Persists the glyph cache, best-effort. serde_json keeps non-ASCII glyphs as
/// UTF-8, like Python's `ensure_ascii=False`.
fn save_fontmap(fontmap: &FontMap) {
    let path = glyphmap_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(raw) = serde_json::to_string(fontmap) {
        std::fs::write(&path, raw).ok();
    }
}

/// Scratch directory for the rendered glyph PNGs (cleaned up by the caller).
fn glyph_tmp_dir() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("novel-qidian-glyphs-{}-{stamp}", std::process::id()))
}

/// OCRs each data-URL image row and returns one text string per row. Glyph rows
/// must stay adjacent, hence `join = ""`.
async fn ocr_rows(client: &mut ocr::Client, rows: &[String], dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(rows.len());
    for (i, data_url) in rows.iter().enumerate() {
        let raw = decode_data_url(data_url)?;
        let path = dir.join(format!("row_{i}.png"));
        std::fs::write(&path, &raw)
            .with_context(|| format!("writing {}", path.display()))?;
        let text = client
            .image_text(&path, "")
            .await
            .with_context(|| format!("qidian: ocr glyph row {i}"))?;
        out.push(text);
    }
    Ok(out)
}

/// Extracts the base64 payload of a canvas PNG data-URL.
fn decode_data_url(data_url: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let (_prefix, payload) = data_url
        .split_once(',')
        .ok_or_else(|| anyhow!("malformed canvas data URL"))?;
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .context("decoding canvas data URL")
}

/// Splits `url` on "/" and returns segment `i` (negative counts from the end).
/// Mirrors the Python `url.split("/")[i]` indexing the original port used.
fn url_segment(url: &str, i: i32) -> String {
    let parts: Vec<&str> = url.split('/').collect();
    let idx = if i < 0 {
        i + parts.len() as i32
    } else {
        i
    };
    if idx < 0 {
        return String::new();
    }
    parts
        .get(idx as usize)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Catalog parsing tests (fixtures mirror the live qidian layouts; expected
// values come from the Python reference run in /tmp/opencode/qdcheck2).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const WWW_HTML: &str = r#"<!DOCTYPE html>
<html><head>
<meta property="og:title" content="凡人修仙传" />
<meta name="author" content="  忘语 " />
</head><body>
<h1 class="bookName">  凡人修仙传（精校版）  </h1>
<span class="author"><a>忘语</a></span>
<div class="book-intro-detail">
  山村少年  <b>修仙</b>  之路
</div>
<div class="catalog-volume">
  <h2 class="head">正文卷 第一卷 凡人世界</h2>
  <ul>
    <li class="chapter-item">
      <a class="chapter-name" href="//www.qidian.com/book/1010868264/741234567/"
         title="第一章 绯红 首发时间：2018-04-01 12:28:26 章节字数：2425">第一章 绯红</a>
    </li>
    <li class="chapter-item">
      <a class="chapter-name" href="/book/1010868264/741234568/" title="">第二章 山野</a>
      <i class="chapter-locked"></i>
    </li>
    <li class="chapter-item">
      <a href="https://www.qidian.com/book/1010868264/741234569/">第三章 无题</a>
    </li>
    <li class="chapter-item">
      <span>没有链接</span>
    </li>
  </ul>
</div>
<div class="catalog-volume">
  <h2>VIP卷 第二卷 灵界</h2>
  <ul>
    <li class="chapter-item">
      <a class="chapter-name" href="/book/1010868264/741234570/" title="第一百章（修）灵界 更新时间：2020-01-02 03:04:05">第一百章</a>
      <em class="iconfont"></em>
    </li>
    <li class="chapter-item">
      <a class="chapter-name" href="/book/1010868264/741234571/" title="第一百零一章">第一百零一章</a>
    </li>
  </ul>
</div>
<div class="catalog-volume">
  <h2 class="head">  </h2>
  <ul>
    <li class="chapter-item">
      <a class="chapter-name" href="/book/1010868264/741234572/">无标题卷章节</a>
      <i class="chapter-locked"></i>
    </li>
  </ul>
</div>
</body></html>"#;

    const BOOK_HTML: &str = r#"<!DOCTYPE html>
<html><head>
<meta name="description" content="  山村少年修仙之路。  " />
</head><body>
<h1>凡人修仙传</h1>
<span class="author-name">忘语</span>
<div id="book-intro-detail">山村少年 <b>修仙</b> 之路</div>
<div id="j-catalogWrap">
  <div class="volume">
    <h2 class="head">正文卷 免费章节</h2>
    <ul>
      <li class="chapter-item">
        <a class="chapter-name" href="/chapter/1010868264/741234567/" title="第一章 绯红 首发时间：2018-04-01 12:28:26 章节字数：2425">第一章</a>
      </li>
      <li class="chapter-item">
        <a class="chapter-name" href="/chapter/1010868264/741234568/">第二章 山野</a>
        <em class="iconfont"></em>
      </li>
    </ul>
  </div>
  <div class="volume">
    <h2>VIP卷 第二卷 灵界</h2>
    <ul>
      <li class="chapter-item">
        <a class="chapter-name" href="/chapter/1010868264/741234570/" title="第一百章 灵界">第一百章</a>
      </li>
      <li class="chapter-item">
        <a class="chapter-name" href="/chapter/1010868264/741234571/">第一百零一章</a>
        <i class="chapter-locked"></i>
      </li>
    </ul>
  </div>
</div>
</body></html>"#;

    const FLAT_HTML: &str = r#"<!DOCTYPE html>
<html><head><meta property="og:title" content="扁平目录" /></head><body>
<h1>扁平目录</h1>
<div class="catalog">
  <ul>
    <li class="chapter-item">
      <a class="chapter-name" href="/chapter/1/2/" title="第一章  首发时间：2019-05-06 章节字数：100">一</a>
    </li>
    <li class="chapter-item">
      <a href="/chapter/1/3/">锁住的第二章</a>
      <span class="iconfont lock"></span>
    </li>
    <li class="chapter-item">
      <span>无链接</span>
    </li>
    <li class="chapter-item">
      <a class="chapter-name" href="">空 href</a>
    </li>
    <li class="chapter-item">
      <a class="chapter-name" href="/chapter/1/4/" title="  ">   </a>
    </li>
  </ul>
</div>
</body></html>"#;

    fn paid(p: Option<bool>) -> String {
        match p {
            None => "None".to_string(),
            Some(b) => b.to_string(),
        }
    }

    fn dump(raw: &str) -> String {
        let q = Qidian;
        let doc = Html::parse_document(raw);
        let mut out = format!(
            "TITLE|{:?}\nAUTHOR|{:?}\nINTRO|{:?}\n",
            q.meta(&doc, TITLE_CANDIDATES),
            q.meta(&doc, AUTHOR_CANDIDATES),
            q.meta(&doc, INTRO_CANDIDATES)
        );
        for c in q.parse_chapters(&doc) {
            out.push_str(&format!(
                "CH|{}|{:?}|{:?}|vip={}|paid={}|sec={:?}|{}|wc={}\n",
                c.num,
                c.title,
                c.url,
                c.is_vip,
                paid(c.is_paid),
                c.section_name,
                c.section_number,
                c.word_count
            ));
        }
        out
    }

    #[test]
    fn www_catalog_matches_python() {
        assert_eq!(
            dump(BOOK_HTML),
            "TITLE|\"凡人修仙传\"\n\
             AUTHOR|\"忘语\"\n\
             INTRO|\"山村少年 修仙 之路\"\n\
             CH|1|\"第一章 绯红\"|\"https://book.qidian.com/chapter/1010868264/741234567/\"|vip=false|paid=true|sec=\"正文卷 免费章节\"|1|wc=2425\n\
             CH|2|\"第二章 山野\"|\"https://book.qidian.com/chapter/1010868264/741234568/\"|vip=false|paid=true|sec=\"正文卷 免费章节\"|1|wc=0\n\
             CH|3|\"第一百章 灵界\"|\"https://book.qidian.com/chapter/1010868264/741234570/\"|vip=true|paid=None|sec=\"VIP卷 第二卷 灵界\"|2|wc=0\n\
             CH|4|\"第一百零一章\"|\"https://book.qidian.com/chapter/1010868264/741234571/\"|vip=true|paid=false|sec=\"VIP卷 第二卷 灵界\"|2|wc=0\n",
        );
    }

    #[test]
    fn www_catalog_matches_python_www() {
        assert_eq!(
            dump(WWW_HTML),
            "TITLE|\"凡人修仙传（精校版）\"\n\
             AUTHOR|\"忘语\"\n\
             INTRO|\"山村少年 修仙 之路\"\n\
             CH|1|\"第一章 绯红\"|\"https://www.qidian.com/book/1010868264/741234567/\"|vip=false|paid=true|sec=\"正文卷 第一卷 凡人世界\"|1|wc=2425\n\
             CH|2|\"第二章 山野\"|\"https://book.qidian.com/book/1010868264/741234568/\"|vip=true|paid=false|sec=\"正文卷 第一卷 凡人世界\"|1|wc=0\n\
             CH|3|\"第三章 无题\"|\"https://www.qidian.com/book/1010868264/741234569/\"|vip=false|paid=true|sec=\"正文卷 第一卷 凡人世界\"|1|wc=0\n\
             CH|4|\"第一百章（修）灵界\"|\"https://book.qidian.com/book/1010868264/741234570/\"|vip=false|paid=true|sec=\"VIP卷 第二卷 灵界\"|2|wc=0\n\
             CH|5|\"第一百零一章\"|\"https://book.qidian.com/book/1010868264/741234571/\"|vip=true|paid=None|sec=\"VIP卷 第二卷 灵界\"|2|wc=0\n\
             CH|6|\"无标题卷章节\"|\"https://book.qidian.com/book/1010868264/741234572/\"|vip=true|paid=false|sec=\"\"|2|wc=0\n",
        );
    }

    #[test]
    fn flat_catalog_matches_python() {
        assert_eq!(
            dump(FLAT_HTML),
            "TITLE|\"扁平目录\"\n\
             AUTHOR|\"\"\n\
             INTRO|\"\"\n\
             CH|1|\"第一章\"|\"https://book.qidian.com/chapter/1/2/\"|vip=false|paid=true|sec=\"\"|0|wc=0\n\
             CH|2|\"锁住的第二章\"|\"https://book.qidian.com/chapter/1/3/\"|vip=true|paid=false|sec=\"\"|0|wc=0\n\
             CH|3|\"\"|\"https://book.qidian.com/chapter/1/4/\"|vip=false|paid=true|sec=\"\"|0|wc=0\n",
        );
    }

    #[test]
    fn book_id_and_catalog_url() {
        let q = Qidian;
        for (url, id) in [
            ("https://book.qidian.com/info/1010868264/", "1010868264"),
            ("https://www.qidian.com/book/1010868264/", "1010868264"),
            ("https://book.qidian.com/chapter/1010868264/741234567/", "1010868264"),
            ("https://m.qidian.com/book/99887766.html", "99887766"),
            ("book/12345", "12345"),
            ("https://book.qidian.com/info/abc", "abc"),
            ("https://book.qidian.com/", "book.qidian.com"),
        ] {
            assert_eq!(q.book_id(url), id, "book_id({url})");
        }
        assert_eq!(
            q.catalog_url("https://www.qidian.com/book/1010868264/"),
            "https://www.qidian.com/book/1010868264/"
        );
        assert_eq!(
            q.catalog_url("https://book.qidian.com/info/1010868264/"),
            "https://book.qidian.com/info/1010868264/"
        );
    }

    #[test]
    fn clean_chapter_title_drops_tooltip_metadata() {
        for (raw, want) in [
            ("第一章 绯红 首发时间：2018-04-01 12:28:26 章节字数：2425", "第一章 绯红"),
            ("第二章（修） 标题 更新时间：2020-01-02 03:04:05", "第二章（修） 标题"),
            ("  第三章  ", "第三章"),
            ("首发时间：2018-04-01", "首发时间：2018-04-01"),
            ("第五卷 天下无双 字数：1234", "第五卷 天下无双"),
            ("某章 字数：999", "某章"),
            ("正文里没有元数据", "正文里没有元数据"),
            ("第一章 最后更新 2019-01-01", "第一章"),
        ] {
            assert_eq!(Qidian::clean_chapter_title(raw), want, "title {raw:?}");
        }
    }

    #[test]
    fn fontmap_round_trips_as_json() {
        let mut m = FontMap::new();
        m.insert(
            "QidianFont".to_string(),
            BTreeMap::from([('\u{e123}', '疼'), ('\u{e124}', '！')]),
        );
        let raw = serde_json::to_string(&m).unwrap();
        assert!(raw.contains('疼'), "non-ASCII must stay UTF-8: {raw}");
        let back: FontMap = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn legacy_flat_fontmap_is_understood() {
        // The Python/Go cache shape: {"font|glyph": "real char"}.
        let raw = r#"{"QidianFont|\ue123":"疼","QidianFont|\ue124":"！","junk":"x"}"#;
        let m = parse_fontmap(raw);
        assert_eq!(
            m.get("QidianFont").and_then(|f| f.get(&'\u{e123}')),
            Some(&'疼')
        );
        assert_eq!(
            m.get("QidianFont").and_then(|f| f.get(&'\u{e124}')),
            Some(&'！')
        );
        assert_eq!(m.get("Nonsense"), None);
        assert!(parse_fontmap("not json").is_empty());
    }
}
