# novel

Download Chinese web novels from **起点 (qidian.com)** and **SF轻小说
(sfacg.com)**, then clean and translate them. Site rules are ported from
[404-novel-project/novel-downloader](https://github.com/404-novel-project/novel-downloader).

A single Rust binary. Python survives only as an optional OCR engine.

## Install

```bash
cargo build --release     # needs Rust 1.95+ (oar-ocr's floor)
uv sync                   # only needed for the default OCR backend
```

You'll also want a Chromium-family browser. Brave at
`/opt/brave.com/brave-origin/brave` is used automatically; otherwise set
`NOVEL_BROWSER`, or let it find `chromium` / `google-chrome` on `PATH`.

## Use

```bash
novel extract https://www.qidian.com/book/1010868264/
novel extract https://book.sfacg.com/Novel/755866/

novel extract <url> --limit 10     # first ten chapters
novel extract <url> --start 50     # from chapter 50
novel extract <url> --only-vip     # VIP chapters only
novel extract <url> --headed       # show the browser
novel extract <url> --cdp http://localhost:9222   # use your own browser

novel login qidian                  # capture a session, then VIP chapters unlock
novel login sfacg

novel ocr screenshots              # OCR images or a folder of them
novel translate outputs/qidian_诡秘之主.txt   # zh -> vi
```

Chapters you don't own come back `LOCKED` and are skipped; nothing bypasses a
paywall. Prefer `--cdp` for VIP work — both sites fingerprint browsers, and your
daily profile is the least suspicious one you have.

## How the two hard cases work

**Qidian VIP** renders text in a custom encrypted font. The CLI draws each
unknown glyph on a canvas inside the real browser, OCRs the drawing, builds a
glyph→character map (cached in `outputs/qidian_fontmap.json`) and rewrites the
text. Whatever stays undecodable becomes a screenshot marked `NEEDS OCR`.

**SFACG VIP** is a single image (`#vipImage`). It's fetched with the chapter URL
as `Referer`, retried while the response is a known failure size, then OCRed;
the image is kept under `outputs/sfacg_<id>_images/`.

Both need a logged-in session:

```env
# .env (gitignored)
QIDIAN_COOKIE="_csrfToken=...;..."
SFACG_COOKIE="..."
```

or run `novel login <site>`, which saves to `outputs/<site>_storage.json`.

## OCR

Pick with `--ocr-backend <sidecar|native>` or `NOVEL_OCR_BACKEND`.

**`sidecar`** (default) runs PaddleOCR out-of-process as a small Python HTTP
service the CLI auto-starts on first use. Highest accuracy, needs the venv.

**`native`** runs PP-OCRv6 ONNX models inside the process via
[oar-ocr](https://crates.io/crates/oar-ocr) — no Python, no service, ~9× faster
on this machine. Models aren't bundled and are never fetched at runtime:

```bash
./scripts/fetch_ocr_models.sh          # medium tier, ~138 MB, verifies SHA-256
./scripts/fetch_ocr_models.sh small    # ~31 MB if you'd rather not
```

Measured on one 1920x5511 chapter page, character similarity against the
sidecar: **0.9998** on the medium tier (one stray glyph from the page's vote
counter across 91 lines) and 0.9967 on small. Native output is deterministic
across runs.

Either way, detections are re-sorted into reading order before being joined, so
the Qidian glyph-row decoding works the same on both.

## Layout

```
src/
  cli.rs       # the four subcommands
  rule.rs      # Rule trait, Book/Chapter, registry
  browser.rs   # CDP session: launch, attach, navigate, fetch, cookies
  qidian.rs    # Qidian rule + font glyph decoding
  sfacg.rs     # SFACG rule + VIP image OCR
  text.rs      # cleaning, PUA detection, attribute deobfuscation
  cleaner.rs   # OCR-artifact cleanup
  translate.rs # zh -> vi
  ocr.rs       # both OCR backends
  config.rs    # .env, cookies, paths
sidecar/       # the Python OCR service
src/novel/     # PaddleOCR wrapper — the only Python involved
```

## Notes

- The browser layer speaks raw CDP over a websocket rather than Playwright or
  Puppeteer, so there's no driver-level automation fingerprint — only what
  Chrome itself exposes.
- Qidian's VIP font path has been verified in pieces (glyph rendering, OCR
  round-trip, cache) but not end-to-end, which needs an account that owns a VIP
  chapter.
