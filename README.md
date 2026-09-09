# Qidian

Extract, clean, and translate Chinese web-novel chapters from **起点 (Qidian)**
(`book.qidian.com`) and **QQ阅读** (`book.qq.com`), including VIP chapters via
cookie-authenticated browser sessions.

## Requirements

- Python 3.11+ (managed with `uv`)
- Playwright Chromium binary (path set in `qidian/dom.py`): `uv run playwright install chromium`
- EasyOCR (models auto-download on first use to `models/easyocr/`)

## Installation

```bash
uv sync
```

## Usage

One CLI, four subcommands:

```bash
# OCR a folder of screenshots -> outputs/<folder>_cleaned.txt
uv run qidian ocr path/to/screenshots

# Extract a whole book (or single QQ chapter) via browser DOM
uv run qidian extract https://book.qidian.com/book/46077914/
uv run qidian extract https://book.qq.com/book-read/46077914/2/
uv run qidian extract --single https://book.qq.com/book-read/46077914/2/   # force single-page mode

# Translate an extracted file (zh -> vi, character names kept as pinyin)
uv run qidian translate outputs/46077914.txt                # -> outputs/46077914_vi.txt
uv run qidian translate outputs/46077914.txt out_vi.txt     # custom output path

# VIP QQ阅读 downloader (cookie-authenticated; see below)
uv run qidian qq --bid 46077914 --limit 5
uv run qidian qq --bid 46077914 --start-cid 442
```

Every subcommand accepts `--output-dir` (default `outputs`). Run
`uv run qidian <subcommand> --help` for per-command options.

## VIP QQ阅读 extraction

Unlocks VIP chapters by injecting your `ywguid`/`ywkey` cookies into a real
Chromium context, then extracts via the SSR `book-read` page. The chapter
catalog is pulled from the web reader's JSON API
(`/api/book/detail/chapters?bid={bid}`), so listings work cookie-free.

Put cookies in `.env` (gitignored):

```env
QQ_YWGUID=...
QQ_YWKEY=...
# optional: QQ_YWSESSION=..., or a full QQ_COOKIE="ywguid=..;ywkey=..;..."
```

`--bid` accepts a bare book id or a full `book.qq.com` URL. Then:

```bash
uv run qidian qq --bid 46077914 --start-cid 442   # VIP cids 442..end
uv run qidian qq --bid 46077914 --limit 5         # first 5 chapters
uv run qidian qq --bid 46077914 --probe           # dump one chapter + capture the live content API/XHR
uv run qidian qq --bid 46077914 --probe-api       # show what the pure-JSON route needs
```

Output: `outputs/qqbook_{bid}_ch{cid}.txt` per chapter, plus a merged
`outputs/qqbook_{bid}.txt`. Without valid cookies, VIP chapters are reported
`LOCKED`; free chapters still extract.

## Structure

```
src/qidian/           # the installable package
  cli.py              # unified `qidian` entry point (subcommands)
  config.py           # .env loading + QQ cookie helpers
  cleaner.py          # TextCleaner: strips reader UI / OCR noise
  ocr.py              # OCREngine: EasyOCR folder recognition
  dom.py              # DomExtractor: Playwright chapter extraction + font deobfuscation
  qq.py               # VIP QQ阅读: catalog API + cookie-authenticated downloader
  translate.py        # Translator: zh -> vi via Google Translate
tests/                # unittest suite (e.g. tests/test_cleaner.py)
outputs/              # generated text files
screenshots/          # sample chapter screenshots for OCR
.env                  # local secrets (gitignored)
```

## Tests

```bash
uv run python -m unittest discover -s tests -v
```

## Notes

- OCR results: EasyOCR returns each detection as `(bbox, text, confidence)`;
  the processor reads `item[1]` (the text).
- QQ Books font-embeds chapter text in random-named tags; `DomExtractor`
  recovers plaintext from those attribute values (`_decrypt_node`). PUA glyphs
  it can't resolve are flagged as needing OCR.
- Custom Playwright Chromium path is set in `qidian/dom.py` (`BINARY`).