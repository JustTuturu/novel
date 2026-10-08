//! SFACG rule (book.sfacg.com / www.sfacg.com) — port of src/novel/sfacg.py,
//! itself a port of the sfacg rule from
//! 404-novel-project/novel-downloader, matching the verified Go port
//! (internal/sfacg/rule.go).
//!
//! The key difference from Qidian: SFACG does not obfuscate its text, but its
//! VIP chapters are delivered as a single long IMAGE (`.article-content >
//! #vipImage`) that only loads with the right Referer + cookies. So VIP =
//! download the image, keep it, and OCR it into text.
//!
//! SFACG novels live on `book.sfacg.com` as `/Novel/<id>/`; `www.sfacg.com` is
//! the (now comics-oriented) portal, so catalog links are resolved against
//! book.sfacg.com.

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use regex::Regex;
use scraper::{CaseSensitivity, ElementRef, Html, Node, Selector};
use std::sync::OnceLock;

use crate::browser::{self, Session};
use crate::config;
use crate::ocr;
use crate::rule::{self, Book, Chapter, ChapterResult};
use crate::text;

/// The registry key, the storage-state key and the output prefix.
const SITE_NAME: &str = "sfacg";

/// Payload sizes that mean the VIP image request failed; the userscript rule
/// retries on exactly these values.
const BAD_IMAGE_SIZES: [usize; 2] = [53658, 42356];

/// Page strings that prove a chapter is not readable.
const PUBLIC_CHAPTER_MARKERS: [&str; 4] = [
    "登录后阅读本章",
    "本章为VIP章节",
    "订阅本章",
    "请先登录",
];

/// The `accept` header the userscript sends with a VIP image request.
const IMAGE_ACCEPT: &str = "image/avif,image/webp,image/apng,image/*,*/*;q=0.8";

/// Pulls the novel id out of a `/Novel/<id>/` URL.
fn novel_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/Novel/(\d+)").unwrap())
}

/// Pulls the novel id out of an older `/s/<id>/` URL.
fn short_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/s/(\d+)").unwrap())
}

/// Strips the catalog suffix back to the novel's root URL.
fn main_index_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/MainIndex/?$").unwrap())
}

/// Drops the `(12)` popularity count SFACG appends to a catalog tag.
fn tag_count_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\(\d+\)$").unwrap())
}

/// Fallback for SFACG's leading 【书名】 tag when the novel name differs.
fn book_tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^【[^】]*】\s*").unwrap())
}

pub struct Sfacg;

impl Sfacg {
    // -- catalog ---------------------------------------------------------

    /// Normalizes any SFACG novel URL to its MainIndex catalog page.
    ///
    /// SFACG novels live on book.sfacg.com as /Novel/<id>/; the catalog is
    /// /Novel/<id>/MainIndex/ (older /s/<id>/MainIndex still appears in links).
    fn catalog_url(&self, url: &str) -> String {
        for re in [novel_id_re(), short_id_re()] {
            if let Some(c) = re.captures(url) {
                return format!("https://book.sfacg.com/Novel/{}/MainIndex/", &c[1]);
            }
        }
        let base = url.replace("/MainIndex", "");
        format!("{}/MainIndex/", base.trim_end_matches('/'))
    }

    /// Reads the top-right user bar: it shows the account name, or the literal
    /// "SF游客" for a guest session.
    ///
    /// The Python/Go ports compute this and then never consult it (the catalog
    /// never reveals entitlement), so it is kept for parity only.
    #[allow(dead_code)]
    fn logged_in(&self, doc: &Html) -> bool {
        match first_of(doc, &[".user-bar > .top-link > .normal-link"]) {
            None => false,
            Some(el) => !el_text(el, " ").contains("SF游客"),
        }
    }

    /// Walks the catalog links. `.story-catalog` blocks are volumes (name in
    /// `.catalog-title`), `.catalog-list a` are chapters (name in the `title`
    /// attribute), and VIP chapters carry a leading `icn_vip` icon.
    fn parse_chapters(&self, doc: &Html, bookname: &str) -> Vec<Chapter> {
        let anchors = browser::selector(".catalog-list a");
        let catalog_title = browser::selector(".catalog-title");

        let mut chapters: Vec<Chapter> = Vec::new();
        let mut section_number = 0usize;
        let mut num = 0usize;
        let mut section_name = String::new();

        for a in doc.select(&anchors) {
            let href = a.attr("href").unwrap_or_default();
            if href.is_empty() {
                continue;
            }
            let url = absolute_url(href);

            // The catalog name lives in `title`; the visible text is the same
            // string with an icon glyph in front of it.
            let raw_title = match a.attr("title") {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => el_text(a, " "),
            };
            let title = strip_book_tag(&raw_title, bookname);

            // Section name = the nearest .story-catalog ancestor's .catalog-title.
            let mut new_section = String::new();
            if let Some(list) = nearest_ancestor_with_class(a, "catalog-list") {
                if let Some(container) = nearest_ancestor_with_class(list, "story-catalog") {
                    if let Some(t) = container.select(&catalog_title).next() {
                        new_section = strip_book_tag(&el_text(t, " "), bookname);
                    }
                }
            }
            if !new_section.is_empty() && new_section != section_name {
                section_name = new_section;
                section_number += 1;
            }
            num += 1;

            let is_vip = has_vip_icon(a);
            chapters.push(Chapter {
                num,
                title,
                url,
                is_vip,
                // SFACG's catalog doesn't reveal entitlement for VIP chapters;
                // those are decided at parse time.
                is_paid: if is_vip { None } else { Some(true) },
                section_name: section_name.clone(),
                section_number,
                word_count: 0,
            });
        }
        chapters
    }

    /// The catalog's tag chips. The `span.icn` glyph is not part of the name
    /// and a trailing `(N)` popularity count is dropped.
    fn parse_tags(&self, doc: &Html) -> Vec<String> {
        let chips = browser::selector("ul.tag-list > li.tag > a");
        let icon = browser::selector("span.icn");
        doc.select(&chips)
            .map(|a| {
                let label = text_excluding(a, &icon, " ");
                tag_count_re().replace(&label, "").trim().to_string()
            })
            .collect()
    }

    // -- chapters --------------------------------------------------------

    /// Handles SFACG VIP = one long image. Fetch it (Referer + cookies), keep
    /// the image next to the outputs, and OCR it into text.
    async fn vip_chapter(
        &self,
        ch: &Chapter,
        s: &Session,
        title: String,
        img_url: &str,
    ) -> Result<ChapterResult> {
        let out_dir = config::output_path(&format!("{}_images", session_novel_id(&ch.url)));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("sfacg: creating {}", out_dir.display()))?;
        let ext = url_ext(img_url);
        let img_path = out_dir.join(format!("{}{ext}", chapter_id(&ch.url)));

        // The userscript retries the image up to 4 times and rejects the known
        // placeholder payloads by size.
        let mut raw: Option<Vec<u8>> = None;
        for _ in 0..4 {
            let Ok(resp) = s
                .fetch_bytes(img_url, Some(&ch.url), &[("accept", IMAGE_ACCEPT)])
                .await
            else {
                continue;
            };
            if !resp.bytes.is_empty() && !BAD_IMAGE_SIZES.contains(&resp.bytes.len()) {
                raw = Some(resp.bytes);
                break;
            }
        }
        let Some(raw) = raw else {
            return Ok(ChapterResult {
                title,
                locked: true,
                ..Default::default()
            });
        };
        std::fs::write(&img_path, &raw)
            .with_context(|| format!("sfacg: writing {}", img_path.display()))?;

        println!(
            "    VIP image {} ({} KB), OCRing...",
            img_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            raw.len() / 1024
        );

        // The ONNX engine loads lazily on first use, so the client is cheap
        // to build per call: the models stay loaded per process afterwards.
        let mut client = ocr::Client::new();
        let ocr_text = client
            .image_text(&img_path, "\n")
            .await
            .with_context(|| format!("sfacg: ocr {}", img_path.display()))?;

        if ocr_text.trim().is_empty() {
            return Ok(ChapterResult {
                title,
                text: format!("[VIP chapter saved as image: {}]", img_path.display()),
                needs_ocr: true,
                ..Default::default()
            });
        }
        Ok(ChapterResult {
            title,
            text: text::clean_text(&ocr_text, &[]),
            ..Default::default()
        })
    }
}

impl rule::Rule for Sfacg {
    fn name(&self) -> &'static str {
        "sfacg"
    }
    fn domains(&self) -> &'static [&'static str] {
        &["sfacg.com", "sfacg.cn"]
    }
    // All SFACG outcomes are plain-HTTP already: free chapters come from the
    // server-rendered article, VIP chapters from an HTTP image fetch.
    fn chapter_track(&self, _ch: &Chapter) -> rule::Track {
        rule::Track::Http
    }
    fn storage_state_path(&self) -> String {
        "outputs/sfacg_storage.json".to_string()
    }
    fn book_parse<'a>(&'a self, url: &'a str, s: &'a Session) -> BoxFuture<'a, Result<Book>> {
        Box::pin(async move {
            let catalog = self.catalog_url(url);
            let novel_url = main_index_re()
                .replace(&catalog, "")
                .trim_end_matches('/')
                .to_string();

            let doc = s
                .dom_soup(&catalog, true)
                .await
                .with_context(|| format!("sfacg: failed to load {catalog}"))?;

            let bookname = first_of(&doc, &["h1.story-title", "h1"])
                .map(|el| el_text(el, " "))
                .unwrap_or_default();

            Ok(Book {
                url: novel_url,
                title: bookname.clone(),
                author: first_of(&doc, &[".author-name"])
                    .map(|el| el_text(el, " "))
                    .unwrap_or_default(),
                chapters: self.parse_chapters(&doc, &bookname),
            })
        })
    }
    fn chapter_parse<'a>(
        &'a self,
        ch: &'a Chapter,
        s: &'a Session,
    ) -> BoxFuture<'a, Result<ChapterResult>> {
        Box::pin(async move {
            // Plain HTTP, not the browser: the chapter page is server-rendered.
            // Scraper's `Html` is not `Send` (its tendrils hold a Cell), so the
            // page is fetched and reduced to owned data in one inner scope that
            // closes before any further await.
            let outcome = {
                let Ok(doc) = s.soup(&ch.url).await else {
                    // An unreadable page counts as locked, not a hard failure.
                    return Ok(ChapterResult {
                        title: ch.title.clone(),
                        locked: true,
                        ..Default::default()
                    });
                };
                classify(&doc, ch)
            };
            match outcome {
                Page::Locked(title) => Ok(ChapterResult {
                    title,
                    locked: true,
                    ..Default::default()
                }),
                Page::NeedsOcr(title) => Ok(ChapterResult {
                    title,
                    needs_ocr: true,
                    ..Default::default()
                }),
                Page::VipImage { title, src } => self.vip_chapter(ch, s, title, &src).await,
                Page::Text {
                    title,
                    text: body,
                    ..
                } => Ok(ChapterResult {
                    title,
                    text: body,
                    ..Default::default()
                }),
            }
        })
    }
}

static RULE: Sfacg = Sfacg;

/// Registered once when the crate initialises.
pub fn register() {
    rule::register(&RULE);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// What a chapter page turned out to be, reduced to owned data so the parsed
/// `Html` (which is not `Send`) never has to live across an await.
enum Page {
    /// Paywalled or otherwise unreadable.
    Locked(String),
    /// No article body and no paywall wording: the CLI should OCR it.
    NeedsOcr(String),
    /// VIP = one long image that has to be fetched and OCRed.
    VipImage { title: String, src: String },
    /// Ordinary text chapter.
    Text { title: String, text: String },
}

/// Reads one chapter page: title, paywall state, and whichever of the four
/// outcomes applies.
fn classify(doc: &Html, ch: &Chapter) -> Page {
    let body = doc_text(doc, " ");
    let title = first_of(doc, &["h1.article-title"])
        .map(|el| el_text(el, " "))
        .unwrap_or_else(|| ch.title.clone());

    // Paywall: .pay-section present = not entitled.
    if doc.select(&browser::selector(".pay-section")).next().is_some() {
        return Page::Locked(title);
    }

    if ch.is_vip {
        let src = first_of(doc, &[".article-content > #vipImage", "#vipImage"])
            .and_then(|el| el.attr("src").map(|s| s.to_string()))
            .filter(|s| !s.is_empty());
        if let Some(src) = src {
            return Page::VipImage { title, src };
        }
    }

    let Some(content) = doc.select(&browser::selector(".article-content")).next() else {
        if PUBLIC_CHAPTER_MARKERS.iter().any(|m| body.contains(m)) {
            return Page::Locked(title);
        }
        return Page::NeedsOcr(title);
    };

    Page::Text {
        title,
        text: text::clean_text(&el_text(content, "\n"), &[]),
    }
}

/// goquery's `get_text(sep)` for one element: descendant text nodes,
/// trimmed, empty ones dropped, joined with `sep` (BeautifulSoup's
/// `get_text(" ", strip=True)`).
fn el_text(el: ElementRef, sep: &str) -> String {
    el.text()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

/// The whole document's text — the pool the paywall markers are searched in.
fn doc_text(doc: &Html, sep: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_text(doc.tree.root(), None, &mut parts);
    parts
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

/// [`el_text`] with whole subtrees removed — the Go port clones the tag chip,
/// drops `span.icn`, and reads what is left.
fn text_excluding(el: ElementRef, skip: &Selector, sep: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_text(*el, Some(skip), &mut parts);
    parts
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Descendant text nodes in document order, optionally pruning subtrees.
fn collect_text(node: ego_tree::NodeRef<'_, Node>, skip: Option<&Selector>, out: &mut Vec<String>) {
    if let Some(sel) = skip {
        if let Some(el) = ElementRef::wrap(node) {
            if sel.matches(&el) {
                return;
            }
        }
    }
    match node.value() {
        Node::Text(t) => out.push(t.text.to_string()),
        _ => {
            for child in node.children() {
                collect_text(child, skip, out);
            }
        }
    }
}

/// `select_one(a) or select_one(b)`: the first selector that matches anything
/// wins (not the first one with non-empty text).
fn first_of<'a>(doc: &'a Html, selectors: &[&str]) -> Option<ElementRef<'a>> {
    for css in selectors {
        let Ok(sel) = Selector::parse(css) else {
            continue;
        };
        if let Some(el) = doc.select(&sel).next() {
            return Some(el);
        }
    }
    None
}

/// Walks up from `el` to the nearest ancestor carrying `class`.
fn nearest_ancestor_with_class<'a>(el: ElementRef<'a>, class: &str) -> Option<ElementRef<'a>> {
    el.ancestors()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().has_class(class, CaseSensitivity::CaseSensitive))
}

/// Reports whether the anchor's first *descendant* element carries the
/// `icn_vip` icon (BeautifulSoup's `a.find(True)`).
///
/// `ElementRef::descendent_elements` yields the anchor itself first, so the
/// search starts at its children.
fn has_vip_icon(a: ElementRef) -> bool {
    a.child_elements()
        .find_map(|first| first.descendent_elements().next())
        .map(|first| {
            first
                .value()
                .has_class("icn_vip", CaseSensitivity::CaseSensitive)
        })
        .unwrap_or(false)
}

/// Expands a catalog link: protocol-relative links get https, and root-relative
/// chapter paths (/Novel/..., /s/...) live on book.sfacg.com rather than
/// www.sfacg.com.
fn absolute_url(href: &str) -> String {
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    if href.starts_with('/') {
        let host = if href.contains("/Novel/") || href.contains("/s/") {
            "book.sfacg.com"
        } else {
            "www.sfacg.com"
        };
        return format!("https://{host}{href}");
    }
    href.to_string()
}

/// Removes the 【书名】 prefix SFACG puts on every catalog title and volume
/// name: the exact novel name first, any other bracketed tag after.
fn strip_book_tag(s: &str, bookname: &str) -> String {
    let s = s.trim();
    let exact = format!("【{bookname}】");
    if let Some(rest) = s.strip_prefix(&exact) {
        return rest.trim().to_string();
    }
    book_tag_re().replace(s, "").trim().to_string()
}

/// The suffix of an image URL's last path segment; `.gif` when it has none.
fn url_ext(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("");
    match tail.rfind('.') {
        Some(i) => tail[i..].to_string(),
        None => ".gif".to_string(),
    }
}

/// The second-to-last path segment of a chapter URL, i.e. the id in
/// `.../1004423/9330412/`.
fn chapter_id(url: &str) -> String {
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    if parts.len() < 2 {
        return String::new();
    }
    parts[parts.len() - 2].to_string()
}

/// Turns `https://www.sfacg.com/s/12345/chapter/999` into `sfacg_12345`, the
/// folder the VIP images of one novel are kept in.
fn session_novel_id(chapter_url: &str) -> String {
    match short_id_re().captures(chapter_url) {
        Some(c) => format!("{SITE_NAME}_{}", &c[1]),
        None => SITE_NAME.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Catalog parsing tests. The fixture mirrors the live sfacg MainIndex layout
// (captured 2025/09 from book.sfacg.com/Novel/755866/MainIndex/); the expected
// values follow the Python reference run in /tmp/opencode/py_sfacg.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>目录</title></head><body>
  <div class="user-bar">
    <div class="top-link">
      <div class="normal-link">SF游客</div>
    </div>
  </div>
  <div class="story-head"><h1 class="story-title">魔女小姐，这契约代价不太对吧？</h1></div>
  <div class="books-box" id="hasTicket">
    <div class="pic"><a href="/Novel/755866/"><img src="//rs.sfacg.com/cover.jpg"></a></div>
  </div>
  <p class="introduce">克雷西亚曾是一位天资卓越的天才魔法师。</p>
  <ul class="tag-list clearfix">
    <li class="tag"><a href="/stag/10/" class="highlight"><span class="icn">&#xe909;</span><span class="text">战斗</span></a></li>
    <li class="tag"><a href="/stag/16/"><span class="icn">&#xe909;</span><span class="text">西幻</span><span class="text">(123)</span></a></li>
  </ul>
  <div class="story-catalog">
    <div class="catalog-hd"><h3 class="catalog-title">【魔女小姐，这契约代价不太对吧？】 人设图</h3></div>
    <div class="catalog-list">
      <ul class="clearfix">
        <li><a href="/Novel/755866/1004423/9330412/" title="克蕾茜娅人设图" class=""> 克蕾茜娅人设图</a></li>
        <li><a href="//book.sfacg.com/Novel/755866/1004423/9371731/" title="芙灵&#183;卡斯德伊（三皇女）"><span class="icn_vip">&#xe905;</span>芙灵·卡斯德伊（三皇女）</a></li>
        <li><span>没有链接</span></li>
      </ul>
    </div>
  </div>
  <div class="story-catalog">
    <div class="catalog-hd"><h3 class="catalog-title">【魔女小姐，这契约代价不太对吧？】 第一卷</h3></div>
    <div class="catalog-list">
      <ul class="clearfix">
        <li><a href="/Novel/755866/1001733/9288573/" title="第一章 一根玩具引发的血案">第一章 一根玩具引发的血案</a></li>
        <li><a href="https://book.sfacg.com/Novel/755866/1001733/9290148/">第二章 契约已签订……布兑！</a></li>
        <li><a href="/vip/c/9961749/" title="上一章链接"></a></li>
      </ul>
    </div>
  </div>
  <div class="catalog-list">
    <a href="/Novel/755866/9999999/1/" title="没有卷名的章节">没有卷名的章节</a>
  </div>
</body></html>"#;

    fn paid(p: Option<bool>) -> String {
        match p {
            None => "None".to_string(),
            Some(b) => b.to_string(),
        }
    }

    fn dump(html: &str) -> String {
        let rule = Sfacg;
        let doc = Html::parse_document(html);
        let bookname = first_of(&doc, &["h1.story-title", "h1"])
            .map(|el| el_text(el, " "))
            .unwrap_or_default();
        let mut out = format!(
            "NAME|{:?}\nAUTHOR|{:?}\nINTRO|{:?}\nCOVER|{:?}\nTAGS|{:?}\nLOGGED|{}\n",
            bookname,
            first_of(&doc, &[".author-name"]).map(|el| el_text(el, " ")),
            first_of(&doc, &[".introduce"]).map(|el| el_text(el, " ")),
            first_of(&doc, &["#hasTicket div.pic img", "div.pic img"]).and_then(|el| el.attr("src")),
            rule.parse_tags(&doc),
            rule.logged_in(&doc),
        );
        for c in rule.parse_chapters(&doc, &bookname) {
            out.push_str(&format!(
                "CH|{}|{:?}|{:?}|vip={}|paid={}|sec={:?}|sn={}\n",
                c.num, c.title, c.url, c.is_vip, paid(c.is_paid), c.section_name, c.section_number
            ));
        }
        out
    }

    #[test]
    fn catalog_matches_python() {
        assert_eq!(
            dump(CATALOG_HTML),
            "NAME|\"魔女小姐，这契约代价不太对吧？\"\n\
             AUTHOR|None\n\
             INTRO|Some(\"克雷西亚曾是一位天资卓越的天才魔法师。\")\n\
             COVER|Some(\"//rs.sfacg.com/cover.jpg\")\n\
             TAGS|[\"战斗\", \"西幻\"]\n\
             LOGGED|false\n\
             CH|1|\"克蕾茜娅人设图\"|\"https://book.sfacg.com/Novel/755866/1004423/9330412/\"|vip=false|paid=true|sec=\"人设图\"|sn=1\n\
             CH|2|\"芙灵·卡斯德伊（三皇女）\"|\"https://book.sfacg.com/Novel/755866/1004423/9371731/\"|vip=true|paid=None|sec=\"人设图\"|sn=1\n\
             CH|3|\"第一章 一根玩具引发的血案\"|\"https://book.sfacg.com/Novel/755866/1001733/9288573/\"|vip=false|paid=true|sec=\"第一卷\"|sn=2\n\
             CH|4|\"第二章 契约已签订……布兑！\"|\"https://book.sfacg.com/Novel/755866/1001733/9290148/\"|vip=false|paid=true|sec=\"第一卷\"|sn=2\n\
             CH|5|\"上一章链接\"|\"https://www.sfacg.com/vip/c/9961749/\"|vip=false|paid=true|sec=\"第一卷\"|sn=2\n\
             CH|6|\"没有卷名的章节\"|\"https://book.sfacg.com/Novel/755866/9999999/1/\"|vip=false|paid=true|sec=\"第一卷\"|sn=2\n",
        );
    }

    #[test]
    fn catalog_url_normalizes_every_shape() {
        let rule = Sfacg;
        for (url, want) in [
            (
                "https://book.sfacg.com/Novel/755866/",
                "https://book.sfacg.com/Novel/755866/MainIndex/",
            ),
            (
                "https://book.sfacg.com/Novel/755866/MainIndex/",
                "https://book.sfacg.com/Novel/755866/MainIndex/",
            ),
            (
                "https://www.sfacg.com/s/30277/MainIndex/",
                "https://book.sfacg.com/Novel/30277/MainIndex/",
            ),
            (
                "https://book.sfacg.com/Novel/755866/MainIndex",
                "https://book.sfacg.com/Novel/755866/MainIndex/",
            ),
        ] {
            assert_eq!(rule.catalog_url(url), want, "catalog_url({url})");
        }
    }

    #[test]
    fn novel_url_strips_the_catalog_suffix() {
        let catalog = Sfacg.catalog_url("https://book.sfacg.com/Novel/755866/");
        assert_eq!(
            main_index_re().replace(&catalog, "").trim_end_matches('/'),
            "https://book.sfacg.com/Novel/755866"
        );
    }

    #[test]
    fn strip_book_tag_handles_both_shapes() {
        let b = "魔女小姐";
        assert_eq!(strip_book_tag("【魔女小姐】 人设图", b), "人设图");
        assert_eq!(strip_book_tag("【别的书】 第一卷", b), "第一卷");
        assert_eq!(strip_book_tag("第一章", b), "第一章");
        assert_eq!(strip_book_tag("  【魔女小姐】  ", b), "");
    }

    #[test]
    fn image_path_helpers() {
        assert_eq!(
            session_novel_id("https://www.sfacg.com/s/30277/chapter/999/"),
            "sfacg_30277"
        );
        assert_eq!(
            session_novel_id("https://book.sfacg.com/Novel/755866/1004423/9330412/"),
            "sfacg"
        );
        assert_eq!(
            chapter_id("https://book.sfacg.com/Novel/755866/1004423/9330412/"),
            "1004423"
        );
        assert_eq!(chapter_id("https://book.sfacg.com/"), "");
        assert_eq!(url_ext("https://rs.sfacg.com/x/1.png"), ".png");
        assert_eq!(url_ext("https://rs.sfacg.com/x/1.gif"), ".gif");
        assert_eq!(url_ext("https://rs.sfacg.com/x/1"), ".gif");
    }

    #[test]
    fn paywall_markers_are_the_python_set() {
        for m in ["登录后阅读本章", "本章为VIP章节", "订阅本章", "请先登录"] {
            assert!(PUBLIC_CHAPTER_MARKERS.contains(&m), "missing marker {m}");
        }
        assert!(doc_text(
            &Html::parse_document("<body><p>本章为VIP章节，请订阅后阅读</p></body>"),
            " "
        )
        .contains("本章为VIP章节"));
    }
}