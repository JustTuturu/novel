"""SFACG (www.sfacg.com) — port of the sfacg rule from
404-novel-project/novel-downloader.

The key difference from Qidian: SFACG does not obfuscate its text, but its
VIP chapters are delivered as a single long IMAGE (`.article-content > #vipImage`)
that only loads with the right Referer + cookies. So VIP = download the image,
keep it, and OCR it into text.
"""

import re
from pathlib import Path

from .base import Book, Chapter, ChapterResult, Rule, register
from .browser import clean_text
from .config import OUTPUT_DIR, STORAGE_STATES

# A returned blob of these sizes means the image request failed (the userscript
# rule retries on exactly these values).
BAD_IMAGE_SIZES = (53658, 42356)

PUBLIC_CHAPTER_MARKERS = ("登录后阅读本章", "本章为VIP章节", "订阅本章", "请先登录")


@register
class Sfacg(Rule):
    name = "sfacg"
    domains = ("sfacg.com", "sfacg.cn")
    concurrency_limit = 1

    # -- catalog ----------------------------------------------------------

    def catalog_url(self, url: str) -> str:
        """Normalize any SFACG novel URL to its MainIndex catalog page.

        SFACG novels live on book.sfacg.com as /Novel/<id>/; the catalog is
        /Novel/<id>/MainIndex/ (older /s/<id>/MainIndex still appears in links).
        """
        m = re.search(r"/Novel/(\d+)", url) or re.search(r"/s/(\d+)", url)
        if m:
            return f"https://book.sfacg.com/Novel/{m.group(1)}/MainIndex/"
        base = url.replace("/MainIndex", "").rstrip("/")
        return f"{base}/MainIndex/"

    def book_parse(self, url: str, session) -> Book:
        catalog_url = self.catalog_url(url)
        novel_url = re.sub(r"/MainIndex/?$", "", catalog_url).rstrip("/")
        soup = session.dom_soup(catalog_url, scroll=True)
        if soup is None:
            raise RuntimeError(f"Failed to load {catalog_url}")

        title_el = soup.select_one("h1.story-title") or soup.select_one("h1")
        bookname = (title_el.get_text(" ", strip=True) if title_el else "")
        author_el = soup.select_one(".author-name")
        intro_el = soup.select_one(".introduce")
        cover_el = soup.select_one("#hasTicket div.pic img") or soup.select_one(
            "div.pic img"
        )
        cover = str(cover_el.get("src") or "") if cover_el else None

        tags = []
        for a in soup.select("ul.tag-list > li.tag > a"):
            for icn in a.select("span.icn"):
                icn.extract()
            tags.append(re.sub(r"\(\d+\)$", "", a.get_text(" ", strip=True)).strip())

        logged_in = self._logged_in(soup)
        chapters = self._parse_chapters(soup, bookname, logged_in)

        return Book(
            site=self.name,
            url=novel_url,
            title=bookname,
            author=author_el.get_text(" ", strip=True) if author_el else "",
            intro=intro_el.get_text(" ", strip=True) if intro_el else "",
            tags=tags,
            cover=cover,
            chapters=chapters,
        )

    def _logged_in(self, soup) -> bool:
        bar = soup.select_one(".user-bar > .top-link > .normal-link")
        if bar is None:
            return False
        return "SF游客" not in bar.get_text(" ", strip=True)

    def _parse_chapters(self, soup, bookname: str, logged_in: bool) -> list[Chapter]:
        """`.story-catalog` blocks are volumes (name in `.catalog-title`),
        `.catalog-list a` are chapters (name in the `title` attribute).
        VIP chapters carry a leading `icn_vip` icon."""
        chapters: list[Chapter] = []
        section_number = 0
        section_chapter = 0
        section_name = ""
        num = 0
        for a in soup.select(".catalog-list a"):
            url = str(a.get("href") or "")
            if not url:
                continue
            if url.startswith("//"):
                url = "https:" + url
            elif url.startswith("/"):
                # Chapter paths (/Novel/...) live on book.sfacg.com, not www.
                host = "book.sfacg.com" if "/Novel/" in url or "/s/" in url else "www.sfacg.com"
                url = f"https://{host}{url}"
            title = str(a.get("title") or a.get_text(" ", strip=True)).strip()
            if title.startswith(f"【{bookname}】"):
                title = title[len(f"【{bookname}】") :].strip()

            # Section name = nearest ancestor .story-catalog's .catalog-title
            parent = a.find_parent(class_="catalog-list")
            container = parent.find_parent(class_="story-catalog") if parent else None
            new_section = ""
            if container is not None:
                t = container.select_one(".catalog-title")
                new_section = t.get_text(" ", strip=True) if t else ""
                if new_section.startswith(f"【{bookname}】"):
                    new_section = new_section[len(f"【{bookname}】") :].strip()
            if new_section and new_section != section_name:
                section_name = new_section
                section_number += 1
                section_chapter = 0
            section_chapter += 1
            num += 1

            first_child = a.find(True)
            is_vip = (
                first_child is not None
                and "icn_vip" in " ".join(first_child.get("class") or [])
            )
            chapters.append(Chapter(
                num=num, title=title, url=url, is_vip=is_vip,
                # SFACG's catalog doesn't reveal entitlement; decided at parse time.
                is_paid=None if is_vip else True,
                section_name=section_name or None,
                section_number=section_number,
            ))
        return chapters

    # -- chapters ---------------------------------------------------------

    def chapter_parse(self, chapter: Chapter, session) -> ChapterResult:
        soup = session.soup(chapter.url)
        if soup is None:
            return ChapterResult(title=chapter.title, locked=True)

        body = soup.get_text(" ", strip=True)
        title_el = soup.select_one("h1.article-title")
        title = title_el.get_text(" ", strip=True) if title_el else chapter.title

        # Paywall: .pay-section present = not entitled.
        if soup.select_one(".pay-section"):
            return ChapterResult(title=title, locked=True)

        if chapter.is_vip:
            vip_img = soup.select_one(".article-content > #vipImage") or soup.select_one(
                "#vipImage"
            )
            if vip_img is not None and vip_img.get("src"):
                return self._vip_chapter(chapter, session, title, str(vip_img["src"]))

        content = soup.select_one(".article-content")
        if content is None:
            if any(m in body for m in PUBLIC_CHAPTER_MARKERS):
                return ChapterResult(title=title, locked=True)
            return ChapterResult(title=title, needs_ocr=True)

        text = content.get_text("\n", strip=True)
        images = [
            str(img.get("src") or "")
            for img in content.select("img[src]")
            if img.get("src")
        ]
        return ChapterResult(title=title, text=clean_text(text), images=images)

    def _vip_chapter(self, chapter: Chapter, session, title: str,
                     img_url: str) -> ChapterResult:
        """SFACG VIP = one long image. Fetch it (Referer + cookies), keep the
        image next to the outputs, and OCR it into text."""
        outdir = OUTPUT_DIR / f"{Path(session_novel_id(chapter.url))}_images"
        outdir.mkdir(parents=True, exist_ok=True)
        chapter_id = chapter.url.rstrip("/").split("/")[-2]  # .../MainIndex/<id>/...
        path = outdir / f"{chapter_id}{Path(img_url).suffix or '.gif'}"

        raw = None
        for _ in range(4):
            raw = session.fetch_bytes(
                img_url,
                referer=chapter.url,
                headers={"accept": "image/avif,image/webp,image/apng,image/*,*/*;q=0.8"},
            )
            if raw and len(raw) not in BAD_IMAGE_SIZES:
                break
            raw = None
        if raw is None:
            return ChapterResult(title=title, locked=True)
        path.write_bytes(raw)

        print(f"    VIP image {path.name} ({len(raw) // 1024} KB), OCRing...",
              flush=True)
        from .ocr import get_engine  # lazy: pulls in torch

        text = get_engine().image_text(str(path), join="\n")
        if not text.strip():
            return ChapterResult(
                title=title,
                text=f"[VIP chapter saved as image: {path}]",
                images=[str(path)],
                needs_ocr=True,
            )
        return ChapterResult(title=title, text=clean_text(text), images=[str(path)])

    # -- auth -------------------------------------------------------------

    @staticmethod
    def storage_state_path() -> Path:
        return STORAGE_STATES["sfacg"]


def session_novel_id(chapter_url: str) -> str:
    """`https://www.sfacg.com/s/12345/chapter/999` -> `sfacg_12345`."""
    m = re.search(r"/s/(\d+)", chapter_url)
    return f"sfacg_{m.group(1)}" if m else "sfacg"