"""Shared browser session: stealth Chromium launch, CDP attach to a real
browser, authenticated HTTP fetch, and DOM helpers.

Both site rules run through one BrowserSession so that cookies, UA, and the
"real browser vs automated browser" choice are handled in a single place.
"""

import os
import re

from bs4 import BeautifulSoup
from playwright.sync_api import Error as PwError
from playwright.sync_api import TimeoutError as PwTimeout
from playwright.sync_api import sync_playwright

# Use real Brave (not the bundled Chromium) so the site can't fingerprint the
# automation binary. Override with NOVEL_BROWSER env. Falls back to Playwright's
# own Chromium.
_BRAVE = "/opt/brave.com/brave-origin/brave"
BINARY = (
    os.environ.get("NOVEL_BROWSER")
    or (_BRAVE if os.path.exists(_BRAVE) else None)
    or None
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
  window.chrome = window.chrome || { loadTimes: {}, runtime: {} };
  try {
    const gpu = { vendorId: 0x10de, renderer: 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)' };
    const gl = document.createElement('canvas').getContext('webgl');
    if (gl) {
      const info = gl.getExtension('WEBGL_debug_renderer_info');
      if (info) {
        Object.defineProperty(gl, 'getParameter', { value: (p) => p === info.UNMASKED_VENDOR_WEBGL ? gpu.vendorId : p === info.UNMASKED_RENDERER_WEBGL ? gpu.renderer : gl.getParameter(p), enumerable: true });
      }
    }
  } catch (e) {}
})();
"""

UA = (
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/126.0.0.0 Safari/537.36"
)

PUA_RE = re.compile(r"[\ue000-\uf8ff]")

VIEWPORT = {"width": 1920, "height": 1080}


class BrowserSession:
    """A Chromium page (either launched by us or the user's real browser via
    CDP), plus an authenticated HTTP helper that shares its cookies."""

    def __init__(self, timeout: int = 45000):
        self.timeout = timeout
        self._pw = None
        self._browser = None
        self._ctx = None
        self._page = None
        self._attached = False  # True when attached via CDP (never kill the browser)

    # -- lifecycle --------------------------------------------------------

    def start(self, cookies: list[dict] | None = None, *, storage_state: str | None = None,
              headless: bool = True):
        self._pw = sync_playwright().start()
        args = ["--disable-blink-features=AutomationControlled"]
        if headless:
            args += ["--disable-gpu", "--no-sandbox", "--disable-dev-shm-usage"]
        self._browser = self._pw.chromium.launch(
            headless=headless,
            executable_path=BINARY,
            args=args,
            ignore_default_args=[
                "--enable-automation",
                "--disable-component-extensions-with-background-pages",
                "--use-mock-keychain",
                "--password-store=basic",
            ],
        )
        self._ctx = self._browser.new_context(
            viewport=VIEWPORT, user_agent=UA, locale="zh-CN",
            timezone_id="Asia/Shanghai", storage_state=storage_state,
        )
        if cookies and not storage_state:
            try:
                self._ctx.add_cookies(cookies)
            except PwError:
                pass
        self._ctx.add_init_script(STEALTH_JS)
        self._page = self._ctx.new_page()
        return self._ctx

    def attach(self, cdp_url: str):
        """Attach to an already-running real browser (e.g. Brave started with
        --remote-debugging-port). The site then sees a genuine browser, which
        matters for sites that refuse to render paywalled text otherwise."""
        self._attached = True
        self._pw = sync_playwright().start()
        self._browser = self._pw.chromium.connect_over_cdp(cdp_url, timeout=30000)
        self._ctx = (self._browser.contexts[0] if self._browser.contexts
                     else self._browser.new_context())
        self._page = self._ctx.pages[0] if self._ctx.pages else self._ctx.new_page()
        return self._ctx

    def close(self):
        if self._attached:
            if self._pw:
                try:
                    self._pw.stop()
                except PwError:
                    pass
                self._pw = None
            self._browser = self._ctx = self._page = None
            self._attached = False
            return
        for closer in (lambda: self._browser.close() if self._browser else None,
                       lambda: self._pw.stop() if self._pw else None):
            try:
                closer()
            except PwError:
                pass
        self._browser = self._pw = self._ctx = self._page = None

    # -- accessors --------------------------------------------------------

    @property
    def page(self):
        if not self._page:
            raise RuntimeError("Call start()/attach() first")
        return self._page

    @property
    def context(self):
        if not self._ctx:
            raise RuntimeError("Call start()/attach() first")
        return self._ctx

    def cookies(self, url: str | None = None) -> dict[str, str]:
        """Current cookies as {name: value}, optionally for one URL only."""
        try:
            items = (self._ctx.cookies(url) if url else self._ctx.cookies())
        except PwError:
            return {}
        return {c["name"]: c["value"] for c in items}

    def storage_state(self, path) -> None:
        self._ctx.storage_state(path=str(path))

    # -- fetching ---------------------------------------------------------

    def fetch(self, url: str, *, referer: str | None = None, headers: dict | None = None,
              retries: int = 2) -> str | None:
        """Authenticated HTTP GET via the browser context (shares cookies + UA).

        Mirrors novel-downloader's ggetHtmlDOM (userscript GM_xmlhttpRequest):
        the same logged-in cookies are sent without running page JS.
        """
        hdrs = {"accept-language": "zh-CN,zh;q=0.9"}
        if referer:
            hdrs["referer"] = referer
        if headers:
            hdrs.update(headers)
        for _ in range(retries + 1):
            try:
                r = self._ctx.request.get(url, headers=hdrs, timeout=20000)
                if r.ok:
                    return r.text()
            except PwError:
                pass
        return None

    def fetch_bytes(self, url: str, *, referer: str | None = None,
                    headers: dict | None = None) -> bytes | None:
        hdrs = {}
        if referer:
            hdrs["referer"] = referer
        if headers:
            hdrs.update(headers)
        try:
            r = self._ctx.request.get(url, headers=hdrs, timeout=30000)
            return r.body() if r.ok else None
        except PwError:
            return None

    def goto(self, url: str) -> None:
        self.page.goto(url, wait_until="domcontentloaded", timeout=self.timeout)
        try:
            self.page.wait_for_selector("body", timeout=15000)
        except PwTimeout:
            pass
        self.page.wait_for_timeout(1500)

    def scroll_to_bottom(self, max_steps: int = 60) -> None:
        """Trigger lazy-loaded catalog lists."""
        self.page.evaluate(
            """(maxSteps) => new Promise(resolve => {
                let last = 0, stuck = 0, n = 0;
                const step = () => {
                    window.scrollBy(0, 600);
                    const h = Math.max(document.body?.scrollHeight || 0,
                                       document.documentElement?.scrollHeight || 0);
                    stuck = (h === last) ? stuck + 1 : 0;
                    last = h; n++;
                    if (n > maxSteps || stuck > 5 || window.scrollY + window.innerHeight >= h) resolve();
                    else setTimeout(step, 120);
                };
                step();
            })""",
            max_steps,
        )

    def soup(self, url: str) -> BeautifulSoup | None:
        html = self.fetch(url)
        return BeautifulSoup(html, "html.parser") if html else None

    def dom_soup(self, url: str, *, scroll: bool = False) -> BeautifulSoup | None:
        """Render with JS (for SPA/catalog pages) then parse the live DOM."""
        try:
            self.goto(url)
        except PwError:
            return None
        if scroll:
            self.scroll_to_bottom()
        return BeautifulSoup(self.page.content(), "html.parser")


def decrypt_attr_obfuscation(node) -> str:
    """Recover plaintext from QQ/Qidian-style attribute font obfuscation.

    A tag whose attributes carry single CJK characters holds the real plaintext
    there; its *visible* text is a font-mapped decoy and must be dropped. Other
    nodes recurse into children so their plaintext attributes are picked up.
    """
    if isinstance(node, str):
        return node
    if node.name == "br":
        return "\n"
    for attr_val in node.attrs.values():
        if isinstance(attr_val, str) and len(attr_val) == 1:
            cp = ord(attr_val)
            if (0x3400 <= cp <= 0x9FFF or 0xF900 <= cp <= 0xFAFF
                    or cp in (0x2018, 0x2019, 0x201C, 0x201D, 0x3000, 0x3001,
                              0x3002, 0xFF0C, 0xFF01, 0xFF1A, 0xFF1B, 0xFF08,
                              0xFF09, 0x2026)):
                return attr_val
    return "".join(decrypt_attr_obfuscation(child) for child in node.children)


def decrypt_html(html: str) -> str:
    return decrypt_attr_obfuscation(BeautifulSoup(html, "html.parser"))


NOISE_PATTERNS = [
    r"[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]",
    r"^\d{1,3}\s*$",
    r"^(目录|书详情|加书架|投票|夜间|设置|客户端|在书架|反馈|指南|举报|旧版|回书架|书友圈)\s*$",
    r"本作品由起点中文网[^\n]*",
    r"@版权所有[^\n]*",
    r"·作家说[\s\S]*$",
    r"讨论热烈[^\n]*",
    r"^书友\d+[^\n]*",
    r"请下载(?:起点)?(?:中文网)?[^\n]*客户端[^\n]*",
    r"^.*本章字数：\d+字.*$",
    r"^.*更新时间：[\d\-\s:]+\d+.*$",
    r"^谷[\u4e00-\u9fa5]{0,1}$",
]


def clean_text(text: str, *, extra: list[str] | None = None) -> str:
    for pat in (extra or []) + NOISE_PATTERNS:
        text = re.sub(pat, "", text, flags=re.MULTILINE)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def pua_ratio(text: str) -> float:
    if not text:
        return 1.0
    return len(PUA_RE.findall(text)) / len(text)