# novel

Download Chinese web novels from **起点 (qidian.com)** and **SF轻小说
(sfacg.com)**, then clean and translate them. Site rules are ported from
[404-novel-project/novel-downloader](https://github.com/404-novel-project/novel-downloader).

The CLI is **Rust**. Python survives only as the OCR engine behind a tiny local
HTTP sidecar.

## Requirements

- Rust (stable; built and tested with 1.98)
- A Chromium-family browser — Brave is auto-detected at
  `/opt/brave.com/brave-origin/brave`, override with `NOVEL_BROWSER=/path/to/browser`.
  Otherwise the first of `chromium`, `chromium-browser`, `google-chrome`,
  `google-chrome-stable`, `brave-browser` on `PATH` is used.
- Python 3.12 with `uv` — only for OCR (PaddleOCR).

## Install

```bash
cargo build --release          # ./target/release/novel
uv sync                        # OCR sidecar only
```

## Usage

```bash
# Extract a whole book (free chapters unless logged in)
novel extract https://www.qidian.com/book/1010868264/
novel extract https://book.sfacg.com/Novel/755866/

# Filters
novel extract <url> --start 50        # from chapter 50 on
novel extract <url> --only-vip        # VIP chapters only
novel extract <url> --limit 10        # first 10 chapters

# Log in once (headed), then VIP chapters unlock
novel login qidian
novel login sfacg

# OCR a folder of screenshots (default backend: Python sidecar)
novel ocr path/to/screenshots
novel ocr path/to/screenshots --ocr-backend native   # in-process Rust OCR

# Translate extracted text (zh -> vi)
novel translate outputs/qidian_诡秘之主.txt
```

Useful flags: `--output-dir` (default `outputs`), `--ocr-backend <sidecar|native>`
(default `sidecar`; see [OCR backends](#ocr-backends)), `--headed`, `--cdp http://localhost:9222`
to attach to your own already-running browser instead of launching one. Prefer
`--cdp` for VIP work: both sites fingerprint browsers, and your daily profile is
the least suspicious one you have.

## VIP handling

**Qidian** — VIP pages ship text in a custom encrypted font. The CLI renders
each unknown glyph to a canvas inside the real browser, OCRs the rendered image
through the sidecar, and builds a glyph→character map (cached in
`outputs/qidian_fontmap.json`, keyed by font), then rewrites the text. Chapters
that stay undecodable fall back to a screenshot and are reported `NEEDS OCR`.

**SFACG** — VIP chapters are a single image (`#vipImage`). It's fetched with the
chapter URL as `Referer`, retried while the response is a known failure size
(53658 / 42356 bytes), OCRed into text, and the image is kept under
`outputs/sfacg_<id>_images/`.

Both sites need a logged-in session for chapters you own:

```env
# .env (gitignored)
QIDIAN_COOKIE="_csrfToken=...;..."
SFACG_COOKIE="..."
```

or run `novel login <site>` to capture a browser session into
`outputs/<site>_storage.json`.

## OCR backends

There are two interchangeable OCR backends. **`sidecar` is the default**; the
native one is opt-in.

### `sidecar` (default) — Python PaddleOCR service

PaddleOCR has no Rust runtime, so it runs out-of-process as a stdlib Python HTTP
service:

- `GET /health` → `{"ok": true}`
- `POST /ocr {"path": "…", "join": "\n"}` → `{"text": "…"}`

The CLI **auto-spawns it** on first use (`.venv/bin/python sidecar/ocr_service.py`,
log in `.ocr_sidecar.log`). To run it yourself:

```bash
.venv/bin/python sidecar/ocr_service.py --port 8765
NOVEL_OCR_URL=http://127.0.0.1:8765   # if it's elsewhere
```

### `native` — in-process Rust OCR

`rapidocr-core` runs the PP-OCRv6 ONNX models through ONNX Runtime inside the
`novel` process: no Python, no sidecar, no HTTP.

```bash
novel ocr screenshots --ocr-backend native
```

Models are **not** bundled. The first run downloads them (~30MB, needs network);
every later run loads them from the cache:

```bash
RAPIDOCR_MODEL_DIR=outputs/rapidocr_models   # default
RAPIDOCR_MODEL_SET=ppocrv6-small             # default; also: ppocrv6-tiny, ppocrv6-medium, ppocrv5-*
```

If the download fails (offline), the error says so and suggests falling back to
`--ocr-backend sidecar`. `outputs/` is gitignored, so the model cache is never
committed.

### Choosing a backend

Precedence: CLI flag → `NOVEL_OCR_BACKEND` env var → `sidecar`.

```bash
novel ocr screenshots --ocr-backend native
NOVEL_OCR_BACKEND=native novel extract <url>
```

An unknown value (`NOVEL_OCR_BACKEND=paddle`) is a hard error, never a silent
fallback. Both backends honour the same contract: detections in reading order
(top-to-bottom, then left-to-right), joined by the requested separator —
`join=""` keeps a glyph row's characters adjacent, which is what the Qidian
glyph-map decoder relies on.

## Structure

```
src/
  main.rs        # entry point, registers the rules
  cli.rs         # extract / login / ocr / translate
  rule.rs        # Rule trait, Book/Chapter/ChapterResult, registry
  browser.rs     # CDP browser session (launch, attach, navigate, fetch, cookies)
  qidian.rs      # Qidian rule + encrypted-font glyph-map decoding
  sfacg.rs       # SFACG rule + VIP image OCR
  text.rs        # text cleaning, PUA detection, attribute deobfuscation
  cleaner.rs     # OCR-artifact cleanup (shared with the Python flow)
  translate.rs   # zh -> vi translation
  ocr.rs         # OCR client: sidecar + native rapidocr-core backends
  config.rs      # .env, cookies, output paths
sidecar/
  ocr_service.py # stdlib HTTP OCR service
src/novel/
  ocr.py         # PaddleOCR wrapper (the only Python the CLI needs)
outputs/         # extracted .txt files, glyph maps, OCR images, sessions
```

## Notes

- Chapters you don't own are reported `LOCKED` and skipped; nothing bypasses
  purchase walls.
- The browser layer speaks raw CDP over a websocket (no Playwright/Puppeteer), so
  there is no automation fingerprint from a driver library — only what Chrome
  itself exposes.
- Qidian's VIP font path is the only part not exercised end-to-end without a
  logged-in account; the glyph rendering, OCR round-trip and cache were verified
  separately against a live page.
