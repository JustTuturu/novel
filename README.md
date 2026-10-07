# novel

Download Chinese web novels from **起点 (qidian.com)** and **SF轻小说
(sfacg.com)**, then clean and translate them. Site rules are ported from
[404-novel-project/novel-downloader](https://github.com/404-novel-project/novel-downloader).

Go binary + a small Python OCR sidecar. Only these two sites are supported.

## Requirements

- Go 1.24+ (`go build ./cmd/novel`)
- A Chromium-family browser for Playwright: `novel install chromium`, or point
  `NOVEL_BROWSER` at Brave/Chrome (auto-detected at `/opt/brave.com/brave-origin/brave`)
- Python only for the OCR sidecar: `uv sync` (EasyOCR/PaddleOCR models download
  on first use)

## Install

```bash
go build -o novel ./cmd/novel
./novel install chromium      # Playwright driver + browser
uv sync                       # sidecar deps
```

## Usage

```bash
# Extract a whole book (free chapters unless logged in)
./novel extract https://www.qidian.com/book/1010868264/
./novel extract https://book.sfacg.com/Novel/755866/

# Filters
./novel extract <url> --start 50        # from chapter 50 on
./novel extract <url> --only-vip        # VIP chapters only
./novel extract <url> --limit 10        # first 10 chapters

# Log in once (headed browser), then VIP chapters unlock
./novel login qidian
./novel login sfacg

# OCR a folder of screenshots
./novel ocr path/to/screenshots

# Translate extracted text (zh -> vi, names kept as pinyin)
./novel translate outputs/qidian_诡秘之主.txt
```

Useful flags: `--output-dir` (default `outputs`), `--headed`,
`--cdp http://localhost:9222` to attach to your own already-running browser
instead of launching one (recommended for VIP work — see Notes).

## VIP handling

**Qidian** — VIP pages ship text in a custom encrypted font. The rule renders
each unknown glyph to a canvas inside the real browser, OCRs the rendered
image, and builds a glyph→character map (cached in `outputs/qidian_fontmap.json`,
keyed by font hash), then rewrites the text. Chapters that stay undecodable fall
back to a screenshot and are reported `NEEDS OCR`.

**SFACG** — VIP chapters are a single image (`#vipImage`). It's fetched with the
chapter URL as `Referer`, retried while the response is a known failure size,
OCRed into text, and the image is kept under `outputs/sfacg_<id>_images/`.

Both sites need a logged-in session for chapters you own:

```env
# .env (gitignored)
QIDIAN_COOKIE="_csrfToken=...;..."
SFACG_COOKIE="..."
```

or run `./novel login <site>` to capture a browser session into
`outputs/<site>_storage.json`.

## OCR sidecar

OCR (EasyOCR) has no Go runtime, so it runs as a local HTTP service:

- `sidecar/ocr_service.py` — stdlib server, `GET /health`, `POST /ocr {path, join}`
- The Go CLI auto-spawns it on first OCR use (`.venv/bin/python sidecar/ocr_service.py`),
  logging to `.ocr_sidecar.log`. Set `NOVEL_OCR_URL` or pass `--ocr-url` to point
  at an already-running instance.
- To run it manually: `.venv/bin/python sidecar/ocr_service.py --port 8765`

## Structure

```
cmd/novel/          # binary entry point
internal/
  cli/              # subcommands: extract / login / ocr / translate / install
  rule/             # Rule interface, Chapter/Book/ChapterResult, registry
  browser/          # Playwright session (stealth, CDP attach, HTTP fetch)
  qidian/           # Qidian rule + glyph-map OCR decoding
  sfacg/            # SFACG rule + VIP image OCR
  text/             # attribute deobfuscation, noise stripping, PUA detection
  cleaner/          # reader-UI / OCR artifact cleanup
  ocr/              # sidecar HTTP client
  translate/        # zh -> vi via Google Translate
  config/           # .env, cookies, storage-state paths
sidecar/ocr_service.py
src/novel/ocr.py    # EasyOCR engine backing the sidecar (only Python kept)
outputs/            # extracted .txt, glyph maps, OCR images, sessions
```

## Notes

- Chapters you don't own are reported `LOCKED` and skipped; nothing bypasses
  purchase walls.
- Automation detection is real: both sites fingerprint the browser, and VIP
  readers break under automation. Prefer `--cdp` against your daily browser:

  ```bash
  brave --remote-debugging-port=9222 --user-data-dir=/tmp/brave-cdp &
  ./novel login qidian --cdp http://localhost:9222
  ./novel extract https://www.qidian.com/book/1010868264/ --cdp http://localhost:9222
  ```

- Downloads are serial per site (`ConcurrencyLimit = 1`) to avoid rate limits.