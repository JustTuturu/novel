"""VIP QQ阅读 (book.qq.com) chapter extractor via a logged-in real browser.

Unlocks VIP chapters by injecting QQ reading cookies (ywguid/ywkey) into a
Playwright Chromium context, then reuses the SSR `book-read` DOM path from
`.dom`. Output lands in `outputs/qqbook_{bid}_ch{cid}.txt`.

Auth (read from .env or env — see bookqq.config.qq_auth_cookies):
  QQ_YWGUID, QQ_YWKEY   -> ywguid / ywkey cookie values
  QQ_YWSESSION          -> optional ywsession cookie value
  QQ_COOKIE             -> optional: full raw Cookie string, overrides the above
"""

import json
import pathlib
import re

import requests
from bs4 import BeautifulSoup

from .config import OUTPUT_DIR, STORAGE_STATE, load_env, qq_auth_cookies
from .dom import QQ_CONTENT_SELECTORS, DomExtractor

# Confirmed via live capture on 2026-09-09: the web reader's catalog API.
CHAPTERS_API = "https://book.qq.com/api/book/detail/chapters?bid={bid}"

# Mirrors ggetHtmlDOM (GM_xmlhttpRequest): browser-like headers so the SSR page
# renders identically; cookies are attached per-request below.
_HTTP_HEADERS = {
    "User-Agent": (
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/126.0.0.0 Safari/537.36"
    ),
    "Referer": "https://book.qq.com/",
    "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
}

_LOCK_MARKERS = ("登录订阅本章", "订阅本章", "上QQ阅读APP看后续精彩内容")


def _cookies_from_storage(path) -> dict[str, str]:
    """Extract qq.com cookies (name -> value) from a Playwright storage state."""
    try:
        data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return {
        c["name"]: c["value"]
        for c in data.get("cookies", [])
        if "qq.com" in (c.get("domain") or "")
        and c.get("name") in ("ywguid", "ywkey", "ywsession")
    }


def _qq_title(soup) -> str:
    # novel-downloader qqbook.ts: title from h1.chapter-title
    node = soup.select_one("h1.chapter-title") or soup.select_one("h1")
    if node is None:
        return "Untitled"
    return node.get_text(strip=True).split("\n")[0].strip() or "Untitled"


def _http_chapter(chapter_url: str, cookies: dict[str, str], *, is_vip: bool, decryptor: DomExtractor):
    """Fast path mirroring novel-downloader's qqbook.ts fetch order.

    Plain authenticated GET of the SSR chapter page; for VIP chapters the
    response is only trusted when `#article` exists with >= 10 element
    children (the `childElementCount < 10` check). Returns a chapter result
    dict, or None to signal fallback to the Playwright DOM path (the
    equivalent of qqbook.ts's iframe fallback).
    """
    if not cookies:
        return None
    try:
        r = requests.get(
            chapter_url, headers=_HTTP_HEADERS, cookies=cookies, timeout=15
        )
        r.raise_for_status()
    except requests.RequestException:
        return None

    soup = BeautifulSoup(r.text, "html.parser")
    title = _qq_title(soup)
    body_text = soup.get_text(" ", strip=True)
    if any(marker in body_text for marker in _LOCK_MARKERS):
        return {"title": title, "text": "", "source": "http", "locked": True}

    article = soup.select_one("#article") or soup.select_one("div.chapter-content")
    if article is None:
        return None
    if is_vip:
        # Only trust the HTTP path for VIP when the SSR body is fully rendered.
        element_children = [c for c in article.children if getattr(c, "name", None)]
        if len(element_children) < 10:
            return None

    decrypted = decryptor._decrypt_html(str(article))
    cleaned = decryptor._clean_dom_text(decrypted, is_qq=True)
    if len(cleaned) <= 20:
        return None
    return {"title": title, "text": cleaned, "source": "http"}


def _book_id_from(bid_or_url: str) -> str:
    m = re.search(r"([0-9]{5,})", bid_or_url)
    return m.group(1) if m else bid_or_url


def _content_field_candidates(resp_json) -> list[tuple[str, object]]:
    """Locate plausible chapter-text fields in an arbitrary JSON response."""
    found = []

    def walk(obj, path=""):
        if isinstance(obj, dict):
            for k, v in obj.items():
                walk(v, f"{path}.{k}" if path else k)
        elif isinstance(obj, list):
            for i, v in enumerate(obj[:200]):
                walk(v, f"{path}[{i}]")
        elif isinstance(obj, str) and len(obj) > 50 and "\u3000" in obj:
            found.append((path, obj[:120]))

    walk(resp_json)
    return found[:5]


def catalog_via_api(bid: str) -> list[dict]:
    """Fast catalog from the confirmed chapters API. Returns [] on any failure."""
    try:
        r = requests.get(CHAPTERS_API.format(bid=bid), timeout=15)
        r.raise_for_status()
        data = (r.json() or {}).get("data")
    except (requests.RequestException, ValueError, TypeError):
        return []
    if not isinstance(data, list):
        return []
    items = []
    for i, node in enumerate(data, start=1):
        cid = node.get("cid")
        if cid is None:
            continue
        items.append({
            "num": i,
            "cid": cid,
            "title": node.get("chapterName") or f"第{cid}章",
            "url": f"https://book.qq.com/book-read/{bid}/{cid}",
            "is_vip": node.get("free") != 1,
            "wordCount": 0,
        })
    return items


def _cid_of(url: str) -> int:
    m = re.search(r"/(\d+)$", url)
    return int(m.group(1)) if m else 0


def run(bid: str, *, limit: int = 0, start_cid: int = 0, probe: bool = False,
        cdp: str | None = None):
    storage_state = None
    http_cookies: dict[str, str] = {}
    ext: DomExtractor | None = None
    if cdp:
        # Attach to the user's REAL browser: it already has the session and
        # is not subject to the automation-detection the Fock lib does.
        print(f"Attaching to real browser via CDP: {cdp}")
        ext = DomExtractor()
        ext.attach(cdp)
        http_cookies = {
            c["name"]: c["value"]
            for c in ext.context.cookies("https://book.qq.com")
        }
        print(f"  real session: {len(http_cookies)} cookies harvested for HTTP fast path")
    elif STORAGE_STATE.exists():
        storage_state = str(STORAGE_STATE)
        http_cookies = _cookies_from_storage(storage_state)
        print(f"Using saved session: {STORAGE_STATE.name}")
    else:
        cookies = qq_auth_cookies()
        if not cookies:
            print("No QQ_YWGUID/QQ_YWKEY (or QQ_COOKIE) set; VIP chapters stay locked.")
            print("Tip: run `bookqq --bid <id> --login` once to save a full session to")
            print(f"  {STORAGE_STATE}")
        else:
            http_cookies = {c["name"]: c["value"] for c in cookies}
            print(f"Auth: {len(cookies)} cookie(s) injected for .qq.com")

    book_url = f"https://book.qq.com/book-detail/{bid}/"
    # Decryptor reuses the attribute-deobfuscation/cleaning helpers; start()
    # is deferred so no Chromium launches while the HTTP fast path succeeds.
    decryptor = DomExtractor()

    def get_ext() -> DomExtractor:
        nonlocal ext
        if ext is None:
            ext = DomExtractor()
            ext.start(cookies, storage_state=storage_state)
        return ext

    try:
        if probe:
            chapters = catalog_via_api(bid) or get_ext().list_chapters(book_url)
            return _probe(get_ext(), chapters, bid)

        chapters = catalog_via_api(bid) or get_ext().list_chapters(book_url)
        if not chapters:
            print("No chapters found.")
            return

        if start_cid:
            chapters = [c for c in chapters if _cid_of(c["url"]) >= start_cid]
        if limit > 0:
            chapters = chapters[:limit]

        OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
        combined = []
        for ch in chapters:
            print(f"  Ch {ch['num']}: {ch['title']}...", end="", flush=True)
            result = _http_chapter(
                ch["url"], http_cookies,
                is_vip=bool(ch.get("is_vip")), decryptor=decryptor,
            )
            if result is None:
                result = get_ext().extract_chapter(ch["url"])
            if result.get("locked"):
                print(" LOCKED (cookie expired / not entitled)")
                continue
            cid = ch.get("cid")
            if cid is None:
                cid = _cid_of(ch["url"])
            out = OUTPUT_DIR / f"qqbook_{bid}_ch{cid}.txt"
            out.write_text(f"# {result['title']}\n{result['text']}", encoding="utf-8")
            src = " [http]" if result.get("source") == "http" else ""
            print(f" {len(result['text'])} chars{src} -> {out.name}")
            combined.append(f"# {result['title']}\n{result['text']}")

        if combined:
            merged = OUTPUT_DIR / f"qqbook_{bid}.txt"
            merged.write_text("\n\n".join(combined), encoding="utf-8")
            print(f"\nMerged -> {merged}")
    finally:
        if ext is not None:
            ext.close()


def _probe(ext: DomExtractor, chapters: list[dict], bid: str):
    """Open the first chapter, print its DOM text, and capture live JSON XHR so
    the real content API (and its field layout) can be recovered without guessing."""
    captured = []

    def on_response(resp):
        url = resp.url
        matched = "mqq" in url or "content" in url or "chapter" in url
        if matched and "json" in (resp.headers.get("content-type") or ""):
            try:
                captured.append((url, resp.json()))
            except ValueError:
                pass  # non-JSON body; probe is best-effort capture

    ext.page.on("response", on_response)
    ch = chapters[0]
    print(f"Probing {ch['url']}")
    result = ext.extract_chapter(ch["url"])
    print(f"Title: {result['title']}")
    print(f"Locked: {result.get('locked')}  source: {result.get('source')}")
    if result.get("text"):
        print("--- DOM text (first 400) ---")
        print(result["text"][:400])

    print("\n--- Captured JSON responses ---")
    if not captured:
        print("(none matched mqq/content/chapter; the reading API may be HTML/SSR)")
    for url, body in captured:
        print(f"\n{url}")
        for path, sample in _content_field_candidates(body):
            print(f"    {path}: {sample!r}")
        print(f"    (top-level keys: {list(body.keys()) if isinstance(body, dict) else type(body).__name__})")


def login(bid: str = "46077914", cid: int = 442, timeout_s: int = 600,
          cdp: str | None = None) -> bool:
    """Open a headed browser; once the user logs in with an account that owns
    the book, and the given chapter's body becomes readable, save the full
    session to STORAGE_STATE for reuse by `bookqq`.

    With `cdp`, attach to the user's REAL browser instead of launching one:
    log in there manually, open the chapter, and this only watches + saves.
    The browser is never navigated remotely: open the chapter once, then
    passively poll all open tabs so the user can log in / purchase at will.
    """
    load_env()
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    url = f"https://book.qq.com/book-read/{bid}/{cid}"

    ext = DomExtractor()
    if cdp:
        print(f"Attaching to real browser via CDP: {cdp}")
        ext.attach(cdp)
        print(f"[login] Watching tabs of YOUR browser for chapter {bid}/{cid}...")
        print("[login] Log in and open the chapter there; the session is saved")
        print("[login] automatically once text appears. This terminal just waits.",
              flush=True)
    else:
        ext.start(qq_auth_cookies(), headless=False)
        p = ext.page
        try:
            p.goto(url, wait_until="domcontentloaded", timeout=60000)
        except Exception as e:
            print(f"[login] Could not open {url}: {type(e).__name__}: {e}")
        print("[login] Browser opened at the target chapter. In THIS browser:")
        print("[login]   1. log in with an account that can read the chapter")
        print("[login]   2. if needed, buy/subscribe it")
        print("[login]   3. the session is saved automatically once text appears")
        print("[login] Leave this terminal running; you can browse freely.", flush=True)
    try:
        def _readable(pg) -> int:
            # The body may be font-obfuscated, so "readable" is signaled by the
            # content container carrying real HTML (not by plaintext).
            for sel in QQ_CONTENT_SELECTORS:
                try:
                    h = pg.inner_html(sel)
                except Exception:
                    continue
                if h and len(h) > 200:
                    return len(h)
            return 0

        import time

        deadline = time.time() + timeout_s
        notified = False
        while time.time() < deadline:
            # Poll every open tab: the user may log in through other pages.
            pages = []
            try:
                pages = list(ext.context.pages)
            except Exception:
                pass
            for pg in pages:
                size = _readable(pg)
                if size:
                    ctx = pg.context
                    ctx.storage_state(path=str(STORAGE_STATE))
                    print(f"\n[login] Chapter container present ({size} bytes HTML)")
                    print(f"[login]   on {pg.url}")
                    print(f"[login] Session saved to {STORAGE_STATE}"
                          f" ({len(ctx.cookies())} cookies).")
                    return True
            if not notified and time.time() > deadline - 60:
                print("[login] One minute left before timeout.", flush=True)
                notified = True
            time.sleep(3)

        print("[login] Timed out before the chapter became readable.")
        return False
    except Exception as e:
        print(f"[login] Stopped: {type(e).__name__}: {e}")
        print("[login] (If the browser was closed early, nothing was saved.)")
        return False
    finally:
        ext.close()


def probe_api(bid: str):
    """Print exactly what a pure-JSON route would send, so you can fill in the
    real endpoint after capturing it (the content endpoint is not unpackaged)."""
    import os

    from .config import load_env

    load_env()
    content_api = os.environ.get("QQ_CONTENT_API")
    catalog_api = os.environ.get("QQ_CATALOG_API")
    print(f"bid={bid}")
    print(f"Catalog API      : {catalog_api or 'UNSET (set QQ_CATALOG_API in .env)'}")
    print(f"Content API      : {content_api or 'UNSET (set QQ_CONTENT_API in .env)'}")
    print("Headers to send  : Cookie: ywguid=...;ywkey=... (from QQ_YWGUID/QQ_YWKEY)")
    print("Capture tip      : pipe a proxied device through mqqapi.reader.qq.com and read a")
    print("                   VIP chapter on m.q.qq.com *on the phone-shaped UA*; the GET/POST")
    print("                   carrying the正文 is the content API. Put it in QQ_CATALOG_API /")
    print("                   QQ_CONTENT_API and run `bookqq --probe-api` again.")