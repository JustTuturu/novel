"""VIP QQ阅读 (book.qq.com) chapter extractor via a logged-in real browser.

Unlocks VIP chapters by injecting QQ reading cookies (ywguid/ywkey) into a
Playwright Chromium context, then reuses the SSR `book-read` DOM path from
`.dom`. Output lands in `outputs/qqbook_{bid}_ch{cid}.txt`.

Auth (read from .env or env — see qidian.config.qq_auth_cookies):
  QQ_YWGUID, QQ_YWKEY   -> ywguid / ywkey cookie values
  QQ_YWSESSION          -> optional ywsession cookie value
  QQ_COOKIE             -> optional: full raw Cookie string, overrides the above
"""

import re

import requests

from .config import OUTPUT_DIR, STORAGE_STATE, load_env, qq_auth_cookies
from .dom import QQ_CONTENT_SELECTORS, DomExtractor

# Confirmed via live capture on 2026-09-09: the web reader's catalog API.
CHAPTERS_API = "https://book.qq.com/api/book/detail/chapters?bid={bid}"


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


def run(bid: str, *, limit: int = 0, start_cid: int = 0, probe: bool = False):
    if STORAGE_STATE.exists():
        cookies = None
        storage_state = str(STORAGE_STATE)
        print(f"Using saved session: {STORAGE_STATE.name}")
    else:
        storage_state = None
        cookies = qq_auth_cookies()
        if not cookies:
            print("No QQ_YWGUID/QQ_YWKEY (or QQ_COOKIE) set; VIP chapters stay locked.")
            print("Tip: run `qidian qq --bid <id> --login` once to save a full session to")
            print(f"  {STORAGE_STATE}")
        else:
            print(f"Auth: {len(cookies)} cookie(s) injected for .qq.com")

    book_url = f"https://book.qq.com/book-detail/{bid}/"
    ext = DomExtractor()
    ext.start(cookies, storage_state=storage_state)
    try:
        if probe:
            chapters = ext.list_chapters(book_url)
            return _probe(ext, chapters, bid)

        chapters = catalog_via_api(bid) or ext.list_chapters(book_url)
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
            result = ext.extract_chapter(ch["url"])
            if result.get("locked"):
                print(" LOCKED (cookie expired / not entitled)")
                continue
            cid = ch.get("cid")
            if cid is None:
                cid = _cid_of(ch["url"])
            out = OUTPUT_DIR / f"qqbook_{bid}_ch{cid}.txt"
            out.write_text(f"# {result['title']}\n{result['text']}", encoding="utf-8")
            print(f" {len(result['text'])} chars -> {out.name}")
            combined.append(f"# {result['title']}\n{result['text']}")

        if combined:
            merged = OUTPUT_DIR / f"qqbook_{bid}.txt"
            merged.write_text("\n\n".join(combined), encoding="utf-8")
            print(f"\nMerged -> {merged}")
    finally:
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


def login(bid: str = "46077914", cid: int = 442, timeout_s: int = 600) -> bool:
    """Open a headed browser; once the user logs in with an account that owns
    the book, and the given chapter's body becomes readable, save the full
    session to STORAGE_STATE for reuse by `qidian qq`."""
    load_env()
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    url = f"https://book.qq.com/book-read/{bid}/{cid}"

    ext = DomExtractor()
    ext.start(qq_auth_cookies(), headless=False)
    p = ext.page
    try:
        p.goto("https://book.qq.com/", wait_until="domcontentloaded", timeout=60000)
        p.wait_for_timeout(2000)
        print("[login] Browser opened. Log in with an account that can read")
        print(f"[login]   book {bid} cid {cid} ({url})")
        print("[login] Watching for the chapter body to become readable...", flush=True)

        import time

        deadline = time.time() + timeout_s
        while time.time() < deadline:
            try:
                p.goto(url, wait_until="domcontentloaded", timeout=30000)
                p.wait_for_timeout(4000)
                # The body may be font-obfuscated, so "readable" is signaled by
                # the content container carrying real HTML (not by plaintext).
                for sel in QQ_CONTENT_SELECTORS:
                    try:
                        h = p.inner_html(sel)
                    except Exception:
                        continue
                    if h and len(h) > 200:
                        ctx = p.context
                        ctx.storage_state(path=str(STORAGE_STATE))
                        print(f"[login] Chapter container present ({len(h)} bytes HTML).")
                        print(f"[login] Session saved to {STORAGE_STATE}"
                              f" ({len(ctx.cookies())} cookies).")
                        return True
            except Exception:
                pass
            print("  ...not readable yet, still waiting", flush=True)
            p.wait_for_timeout(5000)

        print("[login] Timed out before the chapter became readable.")
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
    print("                   QQ_CONTENT_API and run `qidian qq --probe-api` again.")