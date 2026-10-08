# novel

Download Chinese web novels from **起点 (qidian.com)** and **SF轻小说
(sfacg.com)**, then clean and translate them. Site rules are ported from
[404-novel-project/novel-downloader](https://github.com/404-novel-project/novel-downloader).

A single Rust binary. No Python involved.

## Install

```bash
cargo build --release     # needs Rust 1.98+ (rust-version floor)
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
novel extract <url> --jobs 6       # parallel chapter workers

novel login qidian                  # capture a session, then VIP chapters unlock
novel login sfacg

novel ocr screenshots               # OCR images or a folder of them
novel translate outputs/sfacg_755866.txt   # zh -> vi, chapter by chapter
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

One engine: [oar-ocr](https://crates.io/crates/oar-ocr) running PP-OCRv6 ONNX
models inside the process — no Python, no service, no model downloads at
runtime. Models aren't bundled, so download them yourself: grab the PP-OCRv6
**medium** det + rec pair plus the character dictionary (the `oar-ocr` project
mirrors them on ModelScope under `greatv/oar-ocr` as `pp-ocrv6_medium_det.onnx`,
`pp-ocrv6_medium_rec.onnx` and `ppocrv6_dict.txt`) and drop the three files
into `outputs/oar_models/` — or anywhere else, pointed at by `OAR_MODEL_DIR`.

Measurements on a 1920x5511 chapter page: the pinned detection config
(`limit_side_len = 64 / min`, the values paddlex resolves for PP-OCRv6 det
models) finds all ~131 lines, where the library default would silently lose
whole paragraphs; character similarity against a PaddleOCR medium reference
run is **0.9998**. Output is deterministic across runs.

Detections are re-sorted into reading order before being joined, so the Qidian
glyph-row decoding indexes a stable string.

## Translate

```bash
novel translate outputs/sfacg_755866.txt
novel translate outputs/sfacg_755866.txt --jobs 4 --rate 10
```

Reads the assembled book txt — or, when present, the per-chapter files from
`outputs/<site>_<id>_chapters/` — and translates chapter by chapter through an
OpenAI-compatible chat API (set `OPENAI_API_KEY`, `OPENAI_BASE_URL` and
`OPENAI_MODEL` in `.env`). Each finished chapter lands immediately in
`outputs/<site>_<id>_translated/NNNN.txt`, so a rerun resumes where it stopped,
and the translated book txt is assembled at the end in catalog order with the
same chapter framing as the extractor.

The rate limit is a hard cap: a token-bucket limiter (default `--rate 10`
requests/minute, no burst — calls are spaced 6 s apart) shared by all
`--jobs` workers, with 429 + `Retry-After` handled by exponential backoff.

Alongside each translation the model reports every proper noun it handled;
entries are upserted into a self-updating SQLite glossary at
`outputs/glossary.db` (first-writer-wins on the hanzi, so a name's translation
pins on first sight), and the most relevant entries are prepended to each
request. Inspect it with:

```bash
novel glossary list
novel glossary export --format json --output glossary.json
```

## Layout

```
src/
  cli.rs       # the subcommands
  rule.rs      # Rule trait, Book/Chapter, registry
  browser.rs   # CDP session: launch, attach, navigate, fetch, cookies
  qidian.rs    # Qidian rule + font glyph decoding
  sfacg.rs     # SFACG rule + VIP image OCR
  text.rs      # cleaning, PUA detection, attribute deobfuscation
  cleaner.rs   # OCR-artifact cleanup
  translate.rs # zh -> vi, chapter by chapter, self-updating glossary
  glossary.rs  # SQLite glossary (RAG retrieval + write-default upserts)
  ocr.rs       # in-process oar-ocr OCR client
  config.rs    # .env, cookies, paths
```

## Notes

- The browser layer speaks raw CDP over a websocket rather than Playwright or
  Puppeteer, so there's no driver-level automation fingerprint — only what
  Chrome itself exposes.
- Qidian's VIP font path has been verified in pieces (glyph rendering, OCR
  round-trip, cache) but not end-to-end, which needs an account that owns a VIP
  chapter.
