"""Extract chapter text from Qidian/QQ Books using Playwright.

Decrypts QQ Books font obfuscation by recovering plaintext from
attribute values embedded in random-named tags (Option A). PUA
glyphs in <samp> cannot be recovered without the font file — those
chapters are marked as needing OCR.
"""

import os
import pathlib
import re
import time

from bs4 import BeautifulSoup
from playwright.sync_api import Error as PwError
from playwright.sync_api import TimeoutError as PwTimeout
from playwright.sync_api import sync_playwright

PUA_RE = re.compile(r"[\ue000-\uf8ff]")

# Use real Brave (not the bundled Chromium) so the site can't fingerprint the
# automation binary. Override with QQ_BROWSER env. Falls back to bundled Chromium.
_BRAVE = "/opt/brave.com/brave-origin/brave"
BINARY = (
    os.environ.get("QQ_BROWSER")
    or (_BRAVE if os.path.exists(_BRAVE) else None)
    or "/home/suzu/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome"
)

# Comprehensive anti-detection: hide automation fingerprints the site's JS
# checks (navigator.webdriver, plugins, languages, hardware concurrency,
# headless UA, missing Chrome properties).
STEALTH_JS = """
(() => {
  const ua = navigator.userAgent.replace(/HeadlessChrom(e|ium)\\/\\d+/g, '');
  Object.defineProperty(navigator, 'userAgent', { get: () => ua, enumerable: true });
  Object.defineProperty(navigator, 'webdriver', { get: () => undefined, enumerable: true });
  Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => undefined, enumerable: true });
  const plugins = ['Chrome PDF Viewer','Chrome PDF','Skype for Business','Native Client','Google Docs','Google Drive','Google Hangouts','YouTube'];
  Object.defineProperty(navigator, 'plugins', { get: () => plugins, enumerable: true });
  if (Object.getOwnPropertyDescriptor(navigator, 'languages') === undefined) {
    Object.defineProperty(navigator, 'languages', { get: () => ['zh-CN','zh','en-US','en'], enumerable: true });
  }
  Object.defineProperty(navigator, 'hardwareConcurrency', { get: () => 8, enumerable: true });
  const screenValues = { width: 1920, height: 1080, availWidth: 1920, availHeight: 1040, colorDepth: 24, pixelDepth: 24 };
  for (const [k, v] of Object.entries(screenValues)) {
    try { Object.defineProperty(screen, k, { get: () => v, enumerable: true }); } catch (e) {}
  }
  // Chrome-specific markers real installs have.
  window.chrome = window.chrome || { loadTimes: {}, runtime: {} };
  try {
    const gpu = { deviceId: 0x0001, vendorId: 0x10de, deviceString: 'NVIDIA GeForce RTX 3060' };
    const webgl = document.createElement('canvas').getContext('webgl');
    if (webgl) {
      Object.defineProperty(webgl, 'getRendererInfoString', { value: () => 'NVIDIA GeForce RTX 3060' });
    }
  } catch (e) {}
})();
"""

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
        self._attached = False  # True when attached via CDP (never kill the browser)

    def start(self, cookies: list[dict] | None = None, *, storage_state: str | None = None, headless: bool = True):
        self._playwright = sync_playwright().start()
        # Scrub automation fingerprints Playwright adds at the binary level:
        # - --enable-automation turns on navigator.webdriver and the infobar
        # - --disable-blink-features=AutomationControlled keeps Blink honest
        launch_args = ["--disable-blink-features=AutomationControlled"]
        if headless:
            launch_args += ["--disable-gpu", "--no-sandbox", "--disable-dev-shm-usage"]
        self._browser = self._playwright.chromium.launch(
            headless=headless,
            executable_path=BINARY,
            args=launch_args,
            ignore_default_args=[
                "--enable-automation",
                "--disable-component-extensions-with-background-pages",
                "--use-mock-keychain",
                "--password-store=basic",
            ],
        )
        # Real Chrome-family tab defaults (Brave uses a plain Chrome UA string).
        ua = (
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) "
            "Chrome/126.0.0.0 Safari/537.36"
        )
        if storage_state:
            context = self._browser.new_context(
                viewport={"width": 1920, "height": 1080},
                user_agent=ua,
                locale="zh-CN",
                timezone_id="Asia/Shanghai",
                storage_state=storage_state,
            )
        else:
            context = self._browser.new_context(
                viewport={"width": 1920, "height": 1080},
                user_agent=ua,
                locale="zh-CN",
                timezone_id="Asia/Shanghai",
            )
        if cookies and not storage_state:
            try:
                context.add_cookies(cookies)  # type: ignore[arg-type]
            except PwError:
                pass
        context.add_init_script(STEALTH_JS)
        self._page = context.new_page()
        return context

    def attach(self, cdp_url: str):
        """Attach to an already-running real browser over CDP.

        The site then sees the user's genuine browser (fingerprint, extensions,
        real login) — no stealth needed and Fock anti-piracy behaves normally.
        Never closes the user's browser on close().
        """
        self._attached = True
        self._playwright = sync_playwright().start()
        self._browser = self._playwright.chromium.connect_over_cdp(
            cdp_url, timeout=30000
        )
        ctx = (
            self._browser.contexts[0]
            if self._browser.contexts
            else self._browser.new_context()
        )
        self._page = ctx.pages[0] if ctx.pages else ctx.new_page()
        return ctx

    @property
    def attached(self) -> bool:
        return self._attached

    @property
    def context(self):
        if not self._page:
            raise RuntimeError("Call start()/attach() first")
        return self._page.context

    def close(self):
        if self._attached:
            # Never kill the user's real browser; just disconnect.
            if self._playwright:
                try:
                    self._playwright.stop()
                except PwError:
                    self._playwright = None
                self._playwright = None
            self._browser = None
            self._page = None
            self._attached = False
            return
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

    def _lock_is_vip(self, row) -> bool:
        """novel-downloader QQBook rule: VIP = row contains a `.lock` icon."""
        return row.select_one(".lock") is not None

    def list_chapters(self, book_url: str, max_chapters: int = 0):
        page = self.page
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
                    # QQBook rule (novel-downloader): the full chapter list
                    # lives in the second .tab-panel's second .book-dir, as
                    # li.list rows; VIP = row has a .lock icon.
                    rows = soup.select(".tab-panel:nth-of-type(2) .book-dir:nth-of-type(2) li.list") or soup.select("li.list")
                    seen_urls = set()
                    if rows:
                        for s in rows:
                            a = s.select_one("a")
                            if a is None:
                                continue
                            href = str(a.get("href") or "")
                            if not href.startswith("http"):
                                href = "https:" + href if href.startswith("//") else "https://book.qq.com" + href
                            if href in seen_urls:
                                continue
                            seen_urls.add(href)
                            items.append({
                                "title": a.text.strip(),
                                "url": href,
                                "is_vip": self._lock_is_vip(s),
                                "wordCount": 0,
                            })
                    if not items:
                        # Fallback: original selector sweep across any link.
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
                    break
            except PwError:
                continue

        if not items:
            raise RuntimeError(f"No chapters found for {book_url}")

        print(f"  Catalog loaded ({len(items)} chapters)")
        result = [{"num": i + 1, **c} for i, c in enumerate(items)]
        if max_chapters > 0:
            result = result[:max_chapters]
        return result

    def _decrypt_node(self, node) -> str:
        """Recursively decrypt a node using attribute-based font obfuscation.

        A tag whose attributes carry single CJK characters holds the real
        plaintext there (Layer 2); its *visible* text is the font-mapped decoy
        (Layer 1) and must be dropped, not kept. Other nodes recurse into
        children so their plaintext attributes are also picked up.
        """
        if isinstance(node, str):
            return node

        if node.name == "br":
            return "\n"

        # Layer 2: any single-CJK attribute value is real plaintext. Grab the
        # first one as the char this tag displays, and DROP the visible text
        # (which is the Layer-1 font-mapped decoy). Multi-attr tags carry the
        # plaintext in the first attr plus decoy/alt glyphs in the rest.
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
                h = page.inner_html(sel)
                if h and len(h) > 40:
                    dom_html = h
                    try:
                        dom_text = (page.text_content(sel) or "").strip()
                    except PwError:
                        dom_text = ""
                    if dom_text or is_qq:
                        break
            except PwError:
                continue

        # QQ Books: attribute-based decryption (Layer 2) is the primary path.
        # The container HTML carries the plaintext inside random-named tag
        # attributes even when the rendered text is font-mapped (Layer 1), so
        # text_content() must NOT gate us here.
        if is_qq:
            if dom_html:
                decrypted = self._decrypt_html(dom_html)
                cleaned = self._clean_dom_text(decrypted, is_qq=True)
                if len(cleaned) > 20:
                    if expected_word_count > 200 and len(cleaned) < expected_word_count * 0.3:
                        return {"title": title, "text": cleaned, "source": "dom", "locked": True}
                    return {"title": title, "text": cleaned, "source": "dom"}
            return {"title": title, "text": "", "source": "dom"}

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
            # novel-downloader qqbook.ts rm2 filter: 谷X ad paragraphs
            text = re.sub(r"^谷[\u4e00-\u9fa5]{0,1}$", "", text, flags=re.MULTILINE)
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