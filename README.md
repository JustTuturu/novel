# bookqq

Extract, clean, and translate Chinese web-novel chapters from **QQ阅读**
(`book.qq.com`, primary) and **起点 (Qidian)** (`book.qidian.com`), including
VIP chapters via cookie-authenticated browser sessions.

Catalog/VIP logic ported from the QQBook rule of
[404-novel-project/novel-downloader](https://github.com/404-novel-project/novel-downloader).

## Requirements

- Python 3.12 (managed with `uv`)
- Playwright Chromium binary (path set in `bookqq/dom.py`): `uv run playwright install chromium`
- EasyOCR (models auto-download on first use to `models/easyocr/`)

## Installation

```bash
uv sync
```

## Usage

One CLI (`bookqq`), four subcommands:

```bash
# OCR a folder of screenshots -> outputs/<folder>_cleaned.txt
uv run bookqq ocr path/to/screenshots

# Extract a whole book (or single chapter) via browser DOM
uv run bookqq extract https://book.qq.com/book-detail/46077914/
uv run bookqq extract https://book.qq.com/book-read/46077914/2/
uv run bookqq extract --single https://book.qq.com/book-read/46077914/2/   # force single-page mode

# Translate an extracted file (zh -> vi, character names kept as pinyin)
uv run bookqq translate outputs/46077914.txt                # -> outputs/46077914_vi.txt
uv run bookqq translate outputs/46077914.txt out_vi.txt     # custom output path

# VIP QQ阅读 downloader (cookie-authenticated; see below)
uv run bookqq bookqq --bid 46077914 --limit 5
uv run bookqq bookqq --bid 46077914 --start-cid 442
```

Every subcommand accepts `--output-dir` (default `outputs`). Run
`uv run bookqq <subcommand> --help` for per-command options.

## VIP QQ阅读 extraction

Unlocks VIP chapters by injecting your `ywguid`/`ywkey` cookies into a real
Chromium context, then extracts via the SSR `book-read` page. The chapter
catalog is pulled from the web reader's JSON API
(`/api/book/detail/chapters?bid={bid}`), with the DOM catalog
(`li.list` rows in `.tab-panel`/`.book-dir`, `.lock` icon = VIP, per the
novel-downloader QQBook rule) as fallback, so listings work cookie-free.

Put cookies in `.env` (gitignored):

```env
QQ_YWGUID=...
QQ_YWKEY=...
# optional: QQ_YWSESSION=..., or a full QQ_COOKIE="ywguid=..;ywkey=..;..."
```

`--bid` accepts a bare book id or a full `book.qq.com` URL. Then:

```bash
uv run bookqq bookqq --bid 46077914 --start-cid 442   # VIP cids 442..end
uv run bookqq bookqq --bid 46077914 --limit 5         # first 5 chapters
uv run bookqq bookqq --bid 46077914 --probe           # dump one chapter + capture the live content API/XHR
uv run bookqq bookqq --bid 46077914 --probe-api       # show what the pure-JSON route needs
```

Output: `outputs/qqbook_{bid}_ch{cid}.txt` per chapter, plus a merged
`outputs/qqbook_{bid}.txt`. Without valid cookies, VIP chapters are reported
`LOCKED`; free chapters still extract.

## Structure

```
src/bookqq/            # the installable package
  cli.py              # unified `bookqq` entry point (subcommands)
  config.py           # .env loading + QQ cookie helpers
  cleaner.py          # TextCleaner: strips reader UI / OCR noise
  ocr.py              # OCREngine: EasyOCR folder recognition
  dom.py              # DomExtractor: Playwright extraction + font deobfuscation
  bookqq.py           # VIP QQ阅读: catalog API + cookie-authenticated downloader
  translate.py        # Translator: zh -> vi via Google Translate
outputs/              # generated text files
screenshots/          # sample chapter screenshots for OCR
.env                  # local secrets (gitignored)
```

## Notes

- OCR results: EasyOCR returns each detection as `(bbox, text, confidence)`;
  the processor reads `item[1]` (the text).
- QQ Books font-embeds chapter text in random-named tags; `DomExtractor`
  recovers plaintext from those attribute values (`_decrypt_node`). PUA glyphs
  it can't resolve are flagged as needing OCR.
- Custom Playwright Chromium path is set in `bookqq/dom.py` (`BINARY`).
