"""Extract chapter text from Qidian/QQ Books using Playwright.

Decrypts QQ Books font obfuscation by recovering plaintext from
attribute values embedded in random-named tags (Option A). PUA
glyphs in <samp> cannot be recovered without the font file — those
chapters are marked as needing OCR.
"""

import pathlib
import re
import time

from bs4 import BeautifulSoup
from playwright.sync_api import Error as PwError
from playwright.sync_api import TimeoutError as PwTimeout
from playwright.sync_api import sync_playwright

PUA_RE = re.compile(r"[\ue000-\uf8ff]")

BINARY = "/home/suzu/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome"

QIDIAN_SELECTORS = [
    "main.content",
    "main[id^='c-']",
    ".read-content",
    ".chapter-content",
    ".text-wrap .read-content",
]

QQ_CONTENT_SELECTORS = [
    "div.chapter-content",
    ".chapter-content",
    "#article",
    "article",
]


def _detect_obfuscation(text: str, threshold: float = 0.05) -> dict:
    if not text:
        return {"obfuscated": True, "pua_ratio": 0}
    matches = PUA_RE.findall(text)
    ratio = len(matches) / len(text)
    return {"obfuscated": ratio > threshold, "pua_ratio": ratio}


class DomExtractor:
    def __init__(self, timeout: int = 30000):
        self.timeout = timeout
        self._playwright = None
        self._browser = None
        self._page = None

    def start(self, cookies: list[dict] | None = None):
        self._playwright = sync_playwright().start()
        self._browser = self._playwright.chromium.launch(
            headless=True,
            executable_path=BINARY,
            args=[
                "--disable-blink-features=AutomationControlled",
                "--disable-gpu",
                "--no-sandbox",
                "--disable-dev-shm-usage",
            ],
        )
        context = self._browser.new_context(
            viewport={"width": 1366, "height": 900},
            user_agent=(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
                "AppleWebKit/537.36 (KHTML, like Gecko) "
                "Chrome/127.0.0.0 Safari/537.36"
            ),
            locale="zh-CN",
            timezone_id="Asia/Shanghai",
        )
        if cookies:
            try:
                context.add_cookies(cookies)
            except PwError:
                pass
        context.add_init_script(
            "Object.defineProperty(navigator, 'webdriver', { get: () => undefined })"
        )
        self._page = context.new_page()
        return context

    def close(self):
        if self._browser:
            try:
                self._browser.close()
            except PwError:
                self._browser = None
            self._browser = None
        if self._playwright:
            try:
                self._playwright.stop()
            except PwError:
                self._playwright = None
            self._playwright = None

    @property
    def page(self):
        if not self._page:
            raise RuntimeError("Call start() first")
        return self._page

    def _is_qq(self, book_url: str) -> bool:
        return "book.qq.com" in book_url

    def _catalog_urls(self, book_url: str) -> list[str]:
        if self._is_qq(book_url):
            m = re.search(r"/book-read/(\d+)", book_url)
            if not m:
                m = re.search(r"/book(?:-detail|-chapter)/(\d+)", book_url)
            book_id = m.group(1) if m else book_url
            return [
                f"https://book.qq.com/book-chapter/{book_id}/",
                f"https://book.qq.com/book-detail/{book_id}/",
            ]
        m = re.search(r"/book/(\d+)", book_url)
        book_id = m.group(1) if m else book_url
        return [
            f"https://book.qidian.com/info/{book_id}/",
            f"https://www.qidian.com/book/{book_id}/",
            f"https://m.qidian.com/book/{book_id}/catalog",
        ]

    def list_chapters(self, book_url: str, max_chapters: int = 0):
        page = self.page
        used_url = ""
        items = []

        is_qq = self._is_qq(book_url)

        for url in self._catalog_urls(book_url):
            try:
                page.goto(url, wait_until="domcontentloaded", timeout=self.timeout)
                if is_qq:
                    page.evaluate("window.scrollTo(0, document.body.scrollHeight)")
                    page.wait_for_timeout(2000)
                    try:
                        page.wait_for_selector("[class*='chapter'] a", timeout=15000)
                    except PwTimeout:
                        pass
                else:
                    try:
                        page.wait_for_selector("li.chapter-item", timeout=15000)
                    except PwTimeout:
                        page.evaluate("window.scrollTo(0, document.body.scrollHeight)")
                        page.wait_for_timeout(2000)
                        try:
                            page.wait_for_selector("li.chapter-item", timeout=5000)
                        except PwTimeout:
                            pass

                html = page.content()
                soup = BeautifulSoup(html, "html.parser")
                items = []

                if is_qq:
                    seen_urls = set()
                    for a in soup.select("a[href*='/book-read/']"):
                        href = str(a.get("href") or "")
                        if not href.startswith("http"):
                            href = "https:" + href if href.startswith("//") else "https://book.qq.com" + href
                        if href in seen_urls:
                            continue
                        seen_urls.add(href)
                        items.append({
                            "title": a.text.strip(),
                            "url": href,
                            "is_vip": False,
                            "wordCount": 0,
                        })
                else:
                    for li in soup.select("li.chapter-item"):
                        a = li.select_one("a.chapter-name")
                        if not a:
                            continue
                        href = str(a.get("href") or "")
                        if href.startswith("//"):
                            href = "https:" + href
                        elif not href.startswith("http"):
                            href = "https://book.qidian.com" + href
                        is_vip = bool(li.select_one(".chapter-locked, .iconfont.lock"))
                        title_attr = str(a.get("title") or "")
                        wc_match = re.search(r"章节字数：(\d+)", title_attr)
                        word_count = int(wc_match.group(1)) if wc_match else 0
                        items.append({
                            "title": a.text.strip(),
                            "url": href,
                            "is_vip": is_vip,
                            "wordCount": word_count,
                        })

                if items:
                    used_url = url
                    break
            except PwError:
                continue

        if not used_url:
            raise RuntimeError(f"No chapters found for {book_url}")

        print(f"  Catalog loaded ({len(items)} chapters)")
        result = [{"num": i + 1, **c} for i, c in enumerate(items)]
        if max_chapters > 0:
            result = result[:max_chapters]
        return result

    def _decrypt_node(self, node) -> str:
        """Recursively decrypt a node using attribute-based font obfuscation.

        For tags with attributes containing single CJK characters, the attribute
        value is the real text. For other nodes, children are processed recursively.
        """
        if isinstance(node, str):
            return node

        if node.name == "br":
            return "\n"

        for attr_val in node.attrs.values():
            if isinstance(attr_val, str) and len(attr_val) == 1:
                cp = ord(attr_val)
                if (0x3400 <= cp <= 0x9FFF or
                    0xF900 <= cp <= 0xFAFF or
                    cp in (0x2018, 0x2019, 0x201C, 0x201D,
                           0x3000, 0x3001, 0x3002,
                           0xFF0C, 0xFF01, 0xFF1A, 0xFF1B,
                           0xFF08, 0xFF09, 0x2026)):
                    return attr_val

        return "".join(self._decrypt_node(child) for child in node.children)

    def _decrypt_html(self, html: str) -> str:
        """Parse obfuscated HTML and return decrypted text.

        Recovers plaintext from attribute values embedded in random-named tags.
        Tags without attribute mappings retain their (font-obfuscated) visible text.
        """
        soup = BeautifulSoup(html, "html.parser")
        return self._decrypt_node(soup)

    def extract_chapter(self, chapter_url: str, *, expected_word_count: int = 0):
        page = self.page
        is_qq = self._is_qq(chapter_url)

        try:
            page.goto(chapter_url, wait_until="domcontentloaded", timeout=self.timeout)
        except PwError:
            return {"title": "Untitled", "text": "", "source": "dom", "locked": True}

        page.wait_for_selector("body", timeout=15000)
        page.wait_for_timeout(3000)

        if is_qq:
            is_locked = page.evaluate(
                """() => {
                    const t = document.body?.innerText || '';
                    if (t.includes('登录订阅本章') || t.includes('订阅本章')) return true;
                    if (t.includes('上QQ阅读APP看后续精彩内容')) return true;
                    return false;
                }"""
            )
            if is_locked:
                title = (page.inner_text("h1") or "Untitled").split("\n")[0].strip()
                return {"title": title, "text": "", "source": "dom", "locked": True}
        else:
            is_locked = page.evaluate(
                """() => {
                    const t = document.body?.innerText || '';
                    if (t.includes('这是VIP章节') || t.includes('订阅本章')) return true;
                    return !!document.querySelector('.lock-chapter, .vip-tips, .chapter-buy-tips, .buy-chapter-wrap');
                }"""
            )
            if is_locked:
                title = (
                    page.text_content("h1.j_chapterName, .chapter-name, h1") or "Untitled"
                ).strip()
                return {"title": title, "text": "", "source": "dom", "locked": True}

        page.evaluate(
            """() => {
                let total = 0, lastH = 0, stuck = 0;
                const step = 300;
                return new Promise(r => {
                    const t = setInterval(() => {
                        window.scrollBy(0, step);
                        total += step;
                        const h = Math.max(document.body?.scrollHeight || 0,
                                           document.documentElement?.scrollHeight || 0);
                        if (h === lastH) stuck++; else stuck = 0;
                        lastH = h;
                        if (total >= h || h === 0 || stuck > 5) {
                            clearInterval(t);
                            r();
                        }
                    }, 100);
                });
            }"""
        )

        title_sel = (
            "h1.j_chapterName, .chapter-name, .chapter-content h3, .print h1, .print h2, h1"
            if not is_qq
            else "h1"
        )
        title = (
            page.inner_text(title_sel) or "Untitled"
        ).strip()
        if is_qq and "\n" in title:
            title = title.split("\n")[0].strip()

        selectors = QQ_CONTENT_SELECTORS if is_qq else QIDIAN_SELECTORS
        dom_html = ""
        dom_text = ""
        for sel in selectors:
            try:
                t = page.text_content(sel)
                if t and len(t.strip()) > 20:
                    dom_text = t.strip()
                    dom_html = page.inner_html(sel)
                    break
            except PwError:
                continue

        if not dom_text:
            return {"title": title, "text": "", "source": "dom"}

        # For QQ Books: try attribute-based decryption first
        if is_qq:
            decrypted = self._decrypt_html(dom_html)
            obf = _detect_obfuscation(decrypted)
            if decrypted and len(decrypted) > 20 and not obf["obfuscated"]:
                cleaned = self._clean_dom_text(decrypted, is_qq=True)
                if expected_word_count > 200 and len(cleaned) < expected_word_count * 0.3:
                    return {"title": title, "text": cleaned, "source": "dom", "locked": True}
                return {"title": title, "text": cleaned, "source": "dom"}
            # If decrypted text is still PUA-heavy, fall back to OCR
            return {"title": title, "text": "", "source": "ocr_needed"}

        # For Qidian: check for obfuscation
        obf = _detect_obfuscation(dom_text)
        if dom_text and len(dom_text) > 20 and not obf["obfuscated"]:
            # Try attribute-based decryption for any remaining obfuscation
            if dom_html:
                decrypted = self._decrypt_html(dom_html)
                if len(decrypted) > len(dom_text) * 0.8:
                    text = decrypted
                else:
                    text = dom_text
            else:
                text = dom_text
            cleaned = self._clean_dom_text(text)
            if expected_word_count > 200 and len(cleaned) < expected_word_count * 0.3:
                return {"title": title, "text": cleaned, "source": "dom", "locked": True}
            return {"title": title, "text": cleaned, "source": "dom"}

        return {"title": title, "text": "", "source": "ocr_needed"}

    def extract_single_page(self, page_url: str) -> dict:
        return self.extract_chapter(page_url)

    def _clean_dom_text(self, text: str, is_qq: bool = False) -> str:
        text = re.sub(r"[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]", "", text)
        text = re.sub(r"^\d{1,3}\s*$", "", text, flags=re.MULTILINE)
        text = re.sub(
            r"^(目录|书详情|加书架|投票|夜间|设置|客户端|在书架|反馈|指南|举报|旧版|回书架)\s*$",
            "",
            text,
            flags=re.MULTILINE,
        )
        text = re.sub(r"本作品由起点中文网[^\n]*", "", text)
        text = re.sub(r"@版权所有[^\n]*", "", text)
        text = re.sub(r"·作家说[\s\S]*$", "", text)
        text = re.sub(r"讨论热烈[^\n]*", "", text)
        text = re.sub(r"^书友\d+[^\n]*", "", text, flags=re.MULTILINE)
        if is_qq:
            text = re.sub(r"请下载QQ阅读[^\n]*", "", text)
            text = re.sub(r"^.*本章字数：\d+字.*$", "", text, flags=re.MULTILINE)
            text = re.sub(r"^.*更新时间：[\d\-\s:]+\d+.*$", "", text, flags=re.MULTILINE)
        text = re.sub(r"\n{3,}", "\n\n", text)
        return text.strip()


def extract_book(
    book_url: str, *, max_chapters: int = 0, output_dir: str = "outputs"
) -> str:
    extractor = DomExtractor()
    extractor.start()
    try:
        chapters = extractor.list_chapters(book_url, max_chapters=max_chapters)
        print(f"Extracting {len(chapters)} chapters...")

        texts = []
        for ch in chapters:
            print(f"  Ch {ch['num']}: {ch['title']}...", end="", flush=True)
            result = extractor.extract_chapter(
                ch["url"], expected_word_count=ch.get("wordCount", 0)
            )
            if result.get("locked"):
                print(" LOCKED")
                continue
            if result["source"] == "ocr_needed":
                print(" OBFUSCATED (needs OCR)")
                texts.append(f"# {result['title']}\n[OCR needed]")
                continue
            print(f" {len(result['text'])} chars")
            texts.append(f"# {result['title']}\n{result['text']}")
            time.sleep(0.5)

        combined = "\n\n".join(texts)
        safe_name = (
            re.sub(r"[^\w\-_]", "_", book_url.strip("/").split("/")[-2] if "/" in book_url else "book")[:50]
        )
        out_path = pathlib.Path(output_dir)
        out_path.mkdir(parents=True, exist_ok=True)
        out_file = out_path / f"{safe_name}_dom.txt"
        out_file.write_text(combined, encoding="utf-8")
        print(f"\nSaved to {out_file}")
        return str(out_file)
    finally:
        extractor.close()