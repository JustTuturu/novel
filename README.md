# novel

Download Chinese web novels from **起点 (qidian.com)** and **SF轻小说
(sfacg.com)**, then clean and translate them. Site rules are ported from
[404-novel-project/novel-downloader](https://github.com/404-novel-project/novel-downloader).

Only two sites are supported; everything else was removed.

## Requirements

- Python 3.12 (managed with `uv`)
- A Chromium-family browser — Brave is auto-detected at `/opt/brave.com/brave-origin/brave`,
  override with `NOVEL_BROWSER=/path/to/browser`; fallback is Playwright's Chromium
  (`uv run playwright install chromium`)
- OCR (PaddleOCR) is used for the two hard cases: Qidian's encrypted font glyphs
  and SFACG's VIP chapter images

## Install

```bash
uv sync
```

## Usage

```bash
# Extract a whole book (free chapters unless logged in)
uv run novel extract https://www.qidian.com/book/1010868264/
uv run novel extract https://book.sfacg.com/Novel/755866/

# Filters
uv run novel extract <url> --start 50        # from chapter 50 on
uv run novel extract <url> --only-vip        # VIP chapters only
uv run novel extract <url> --limit 10        # first 10 chapters

# Log in once (headed), then VIP chapters unlock
uv run novel login qidian
uv run novel login sfacg

# OCR a folder of screenshots
uv run novel ocr path/to/screenshots

# Translate extracted text (zh -> vi)
uv run novel translate outputs/qidian_诡秘之主.txt
```

Useful flags: `--output-dir` (default `outputs`), `--headed`, `--cdp http://localhost:9222`
to attach to your own already-running browser instead of launching one.

## VIP handling

**Qidian** — VIP pages ship text in a custom encrypted font. `qidian.py` renders
each unknown glyph to a canvas inside the real browser, OCRs the rendered image,
and builds a glyph→character map (cached in `outputs/qidian_fontmap.json`, keyed
by font hash), then rewrites the text. Chapters that stay undecodable fall back
to a screenshot and are reported `NEEDS OCR`.

**SFACG** — VIP chapters are a single image (`#vipImage`). It's fetched with the
chapter URL as `Referer`, retried while the response is a known failure size, OCRed
into text, and the image is kept under `outputs/sfacg_<id>_images/`.

Both sites need a logged-in session for chapters you own:

```env
# .env (gitignored)
QIDIAN_COOKIE="_csrfToken=...;..."
SFACG_COOKIE="..."
```

or run `novel login <site>` to capture a browser session into
`outputs/<site>_storage.json`.

## Structure

```
src/novel/
  cli.py       # `novel` entry point (extract/login/ocr/translate)
  base.py      # Rule base class, Chapter/Book/ChapterResult, rule registry
  browser.py   # BrowserSession (Playwright + stealth, CDP attach, HTTP fetch)
  qidian.py    # Qidian rule + glyph-map OCR decoding
  sfacg.py     # SFACG rule + VIP image OCR
  login.py     # interactive session capture
  ocr.py       # PaddleOCR wrapper
  cleaner.py   # text noise stripping
  translate.py # zh -> vi translation
outputs/       # extracted .txt files, fonts maps, OCR images
```

## Notes

- Chapters you don't own are reported `LOCKED` and skipped; nothing bypasses
  purchase walls.
- Automation detection is real: both sites fingerprint the browser. Prefer
  `--cdp` against your daily browser for VIP work.