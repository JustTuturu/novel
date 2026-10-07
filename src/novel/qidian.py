"""Qidian (book.qidian.com / www.qidian.com) — port of the qidian rule from
404-novel-project/novel-downloader.

Notable differences from the userscript:

* The userscript can only *render* the encrypted font through a real browser and
  then OCRs glyph canvases; we do the same, but inside our own Playwright page
  (or the user's real browser over CDP), which also satisfies qidian's cookie /
  _csrfToken requirements for VIP chapters.
* Font deobfuscation = build a glyph map once (render the custom-font glyphs to
  a canvas in rows of 30, OCR the rows, zip them back together), cache it, then
  rewrite every PUA character in the chapter text. Per-glyph rendering is the
  fallback when a row OCRs to the wrong length.
"""

import base64
import json
import re
import tempfile
from pathlib import Path

from .base import Book, Chapter, ChapterResult, Rule, register
from .browser import clean_text, pua_ratio
from .config import OUTPUT_DIR, STORAGE_STATES

CONTENT_SELECTORS = [
    "main.content",
    "main[id^='c-']",
    ".read-content",
    ".chapter-content",
    ".text-wrap .read-content",
]

LOCK_MARKERS = ("这是VIP章节", "订阅本章", "登录后阅读本章", "您还不是VIP章节")

_GLYPHMAP_CACHE = Path("outputs/qidian_fontmap.json")

# Renders the given characters in the page's own (encrypted) font onto a canvas
# and returns one data-URL per row of `perRow` glyphs.
_RENDER_ROWS_JS = """
({chars, font, perRow}) => {
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
}
"""

_RENDER_SINGLE_JS = """
({chars, font}) => chars.map(ch => {
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
})
"""


def _cache_key(font: str, ch: str) -> str:
    return f"{font}|{ch}"


def load_fontmap() -> dict[str, str]:
    if _GLYPHMAP_CACHE.exists():
        try:
            return json.loads(_GLYPHMAP_CACHE.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}
    return {}


def save_fontmap(fontmap: dict[str, str]) -> None:
    try:
        _GLYPHMAP_CACHE.parent.mkdir(parents=True, exist_ok=True)
        _GLYPHMAP_CACHE.write_text(
            json.dumps(fontmap, ensure_ascii=False), encoding="utf-8"
        )
    except OSError:
        pass


def _ocr_rows(rows: list[str]) -> list[str]:
    """OCR each data-URL image row -> list of text strings (one per row)."""
    from .ocr import get_engine  # lazy: pulls in torch

    engine = get_engine()
    out = []
    with tempfile.TemporaryDirectory() as tmp:
        for i, data_url in enumerate(rows):
            raw = base64.b64decode(data_url.split(",", 1)[1])
            path = Path(tmp) / f"row_{i}.png"
            path.write_bytes(raw)
            out.append(engine.image_text(str(path), join=""))
    return out


def build_glyph_map(session, chars: list[str], selector: str) -> dict[str, str]:
    """OCR the page's encrypted font and return {PUA char -> real char}."""
    font = session.page.evaluate(
        """(sel) => {
            const el = document.querySelector(sel) || document.body;
            return getComputedStyle(el).fontFamily;
        }""",
        selector,
    )
    per_row = 30
    rows = session.page.evaluate(
        _RENDER_ROWS_JS, {"chars": chars, "font": font, "perRow": per_row}
    )
    ocr_texts = _ocr_rows(rows)
    mapping: dict[str, str] = {}
    per_char_needed: list[str] = []
    for idx, text in enumerate(ocr_texts):
        row_chars = chars[idx * per_row : (idx + 1) * per_row]
        if len(text) == len(row_chars):
            mapping.update(zip(row_chars, text))
        else:
            per_char_needed.extend(row_chars)

    if per_char_needed:  # rows OCR'd inconsistently -> one glyph at a time
        singles = session.page.evaluate(
            _RENDER_SINGLE_JS, {"chars": per_char_needed, "font": font}
        )
        for ch, text in zip(per_char_needed, _ocr_rows(singles)):
            if len(text) == 1:
                mapping[ch] = text

    keyed = {_cache_key(font, k): v for k, v in mapping.items()}
    fontmap = load_fontmap()
    fontmap.update(keyed)
    save_fontmap(fontmap)
    return mapping


@register
class Qidian(Rule):
    name = "qidian"
    domains = ("qidian.com",)
    concurrency_limit = 1

    # -- catalog ----------------------------------------------------------

    def book_parse(self, url: str, session) -> Book:
        book_id = self._book_id(url)
        catalog_url = self._catalog_url(url)
        soup = session.dom_soup(catalog_url, scroll=True)
        if soup is None:
            raise RuntimeError(f"Failed to load {catalog_url}")

        book = Book(site=self.name, url=f"https://book.qidian.com/info/{book_id}/",
                    title=self._meta(soup, [("title", "h1"), ("title", "h1.bookName"),
                                            ("property", 'meta[property="og:title"]')]),
                    author=self._meta(soup, [("name", ".author"), ("name", ".author-name"),
                                             ("property", 'meta[name="author"]')]),
                    intro=self._meta(soup, [("class", ".book-intro-detail"),
                                            ("class", "#book-intro-detail"),
                                            ("name", 'meta[name="description"]')]),
                    chapters=self._parse_chapters(soup))
        return book

    def _book_id(self, url: str) -> str:
        m = (re.search(r"/(?:book|info)/(\d+)", url)
             or re.search(r"([0-9]{5,})", url))
        return m.group(1) if m else url.strip("/").split("/")[-1]

    def _catalog_url(self, url: str) -> str:
        book_id = self._book_id(url)
        if "www.qidian.com" in url:
            return f"https://www.qidian.com/book/{book_id}/"
        return f"https://book.qidian.com/info/{book_id}/"

    def _meta(self, soup, candidates) -> str:
        for kind, sel in candidates:
            el = soup.select_one(sel)
            if not el:
                continue
            if kind == "property":
                text = str(el.get("content") or "")
            else:
                text = el.get_text(" ", strip=True)
            text = text.strip()
            if text:
                return text
        return ""

    @staticmethod
    def _clean_chapter_title(raw: str) -> str:
        """Drop the catalog tooltip metadata Qidian puts in the title attribute.

        e.g. "第一章 绯红 首发时间：2018-04-01 12:28:26 章节字数：2425"
        -> "第一章 绯红"
        """
        title = raw.strip()
        # Cut at the first metadata label; also drop a trailing time/date run.
        title = re.split(
            r"\s*(?:首发时间|章节字数|更新时间|最后更新|更新时间：|字数：)\s*",
            title,
            maxsplit=1,
        )[0]
        title = re.split(r"\s+\d{4}-\d{2}-\d{2}", title)[0]
        title = re.sub(r"\s*（.*?）\s*$", "", title).strip()
        return title or raw.strip()

    def _parse_chapters(self, soup) -> list[Chapter]:
        """Port of the qidian rule's bookParse: www.qidian.com uses
        `.catalog-volume` + `li.chapter-item` (locked = `.chapter-locked`);
        book.qidian.com uses `#j-catalogWrap .volume` where a volume header
        containing "VIP" marks the whole volume VIP (free ones carry an
        `<em class=iconfont>`)."""
        chapters: list[Chapter] = []
        volumes = soup.select(".catalog-volume") or soup.select("#j-catalogWrap .volume")

        if volumes:
            section_number = 0
            num = 0
            for vol in volumes:
                header = vol.select_one("h2.head, h2, .head")
                section_name = header.get_text(" ", strip=True) if header else ""
                volume_vip = "VIP" in (section_name or "").upper()
                if section_name:
                    section_number += 1
                section_chapter = 0
                for li in vol.select("li.chapter-item") or vol.select("li"):
                    a = li.select_one("a.chapter-name") or li.select_one("a")
                    if not a or not a.get("href"):
                        continue
                    locked = li.select_one(".chapter-locked") is not None
                    free_vip = li.select_one("em.iconfont") is not None
                    is_vip = locked or (volume_vip and not free_vip)
                    title = self._clean_chapter_title(
                        str(a.get("title") or a.get_text(" ", strip=True))
                    )
                    wc = re.search(r"章节字数：(\d+)", str(a.get("title") or ""))
                    num += 1
                    section_chapter += 1
                    chapters.append(Chapter(
                        num=num, title=title,
                        url=self._abs(a["href"]),
                        is_vip=is_vip,
                        is_paid=False if locked else (None if is_vip else True),
                        section_name=section_name or None,
                        section_number=section_number,
                        word_count=int(wc.group(1)) if wc else 0,
                    ))
        else:
            num = 0
            for li in soup.select("li.chapter-item"):
                a = li.select_one("a.chapter-name") or li.select_one("a")
                if not a or not a.get("href"):
                    continue
                locked = li.select_one(".chapter-locked, .iconfont.lock") is not None
                num += 1
                chapters.append(Chapter(
                    num=num,
                    title=self._clean_chapter_title(
                        str(a.get("title") or a.get_text(" ", strip=True))
                    ),
                    url=self._abs(a["href"]),
                    is_vip=locked,
                    is_paid=not locked,
                ))

        chapters.sort(key=lambda c: c.num)
        return chapters

    def _abs(self, href: str) -> str:
        href = str(href)
        if href.startswith("//"):
            return "https:" + href
        if href.startswith("http"):
            return href
        return "https://book.qidian.com" + href

    # -- chapters ---------------------------------------------------------

    def chapter_parse(self, chapter: Chapter, session) -> ChapterResult:
        session.goto(chapter.url)
        page = session.page

        body = page.text_content("body") or ""
        if any(m in body for m in LOCK_MARKERS) or page.query_selector(".vip-limit-wrap"):
            return ChapterResult(title=chapter.title, locked=True)

        text, selector = "", ""
        for sel in CONTENT_SELECTORS:
            try:
                text = (page.text_content(sel) or "").strip()
            except Exception:
                continue
            if text:
                selector = sel
                break

        title = chapter.title
        try:
            title = (page.text_content("h1.j_chapterName, .chapter-name, h1")
                     or chapter.title).strip().split("\n")[0]
        except Exception:
            pass

        if not text:
            return ChapterResult(title=title, needs_ocr=True)

        if pua_ratio(text) <= 0.05:
            return ChapterResult(title=title, text=clean_text(text))

        # Encrypted font: decode PUA glyphs via the cached font map, then
        # rebuild the text.
        text = self._decode_pua(session, text, selector, chapter)
        if pua_ratio(text) > 0.05:
            self._screenshot(chapter, session)
            return ChapterResult(title=title, needs_ocr=True)
        return ChapterResult(title=title, text=clean_text(text))

    def _decode_pua(self, session, text: str, selector: str, chapter: Chapter) -> str:
        pua_chars = sorted(set(re.findall(r"[\ue000-\uf8ff]", text)))
        font = session.page.evaluate(
            """(sel) => {
                const el = document.querySelector(sel) || document.body;
                return getComputedStyle(el).fontFamily;
            }""",
            selector,
        )
        cached = load_fontmap()
        mapping = {c: cached[_cache_key(font, c)] for c in pua_chars
                   if _cache_key(font, c) in cached}
        missing = [c for c in pua_chars if c not in mapping]
        if missing:
            print(f"    font: {len(missing)} glyph(s) to OCR "
                  f"({len(missing) // 30 + 1} row(s))", flush=True)
            mapping.update(build_glyph_map(session, missing, selector))
        return "".join(mapping.get(ch, ch) for ch in text)

    def _screenshot(self, chapter: Chapter, session) -> Path | None:
        """Keep the encrypted-font rendering so `novel ocr` can recover it."""
        try:
            outdir = OUTPUT_DIR / f"qidian_{chapter.url.split('/')[3]}_screenshots"
            outdir.mkdir(parents=True, exist_ok=True)
            cid = chapter.url.rstrip("/").split("/")[-1]
            path = outdir / f"{cid}.png"
            session.page.screenshot(path=str(path), full_page=False)
            return path
        except Exception:
            return None

    # -- auth -------------------------------------------------------------

    @staticmethod
    def storage_state_path() -> Path:
        return STORAGE_STATES["qidian"]