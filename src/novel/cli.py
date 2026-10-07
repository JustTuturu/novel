"""Unified `novel` command-line interface.

Subcommands:
  extract    Download a book from a supported site (auto-detected from URL)
  login      Open a browser, save a logged-in session for a site
  ocr        OCR a folder of images / chapter screenshots -> text file
  translate  Translate an extracted text file (zh -> vi)
"""

import argparse
import os
import sys

from .browser import BrowserSession
from .config import OUTPUT_DIR, STORAGE_STATES, site_cookies


def cmd_extract(args) -> int:
    from .base import rule_for

    rule = rule_for(args.url)
    print(f"--- {rule.name}: extract ---")
    print(f"URL: {args.url}")

    session = BrowserSession()
    state = STORAGE_STATES.get(rule.name)
    storage_state = str(state) if state and state.exists() else None
    if args.cdp:
        session.attach(args.cdp)
        print("Using your real browser via CDP.")
    else:
        if storage_state:
            print(f"Using saved session: {state.name}")
        else:
            print("No saved session; free chapters only (see `novel login`).")
        session.start(site_cookies(rule.name), storage_state=storage_state,
                      headless=not args.headed)

    try:
        book = rule.book_parse(args.url, session)
        print(f"  {book.title} — {book.author}  [{len(book.chapters)} chapters, "
              f"{len(book.vip_chapters)} VIP]")
        chapters = book.chapters
        if args.start:
            chapters = [c for c in chapters if c.num >= args.start]
        if args.only_vip:
            chapters = book.vip_chapters
        if args.limit > 0:
            chapters = chapters[: args.limit]

        os.makedirs(args.output_dir, exist_ok=True)
        combined: list[str] = []
        for ch in chapters:
            section = f"{ch.section_name} / " if ch.section_name else ""
            print(f"  [{ch.num}] {section}{ch.title}...", end="", flush=True)
            result = rule.chapter_parse(ch, session)
            if result.locked:
                print(" LOCKED (need login / purchase)")
                continue
            if result.needs_ocr and not result.text:
                print(" NEEDS OCR")
                continue
            flag = " [needs OCR]" if result.needs_ocr else ""
            print(f" {len(result.text)} chars{flag}")
            combined.append(f"# {result.title}\n{result.text}")

        if combined:
            safe = f"{rule.name}_{os.path.basename(book.url.rstrip('/'))}"
            out = os.path.join(args.output_dir, f"{safe}.txt")
            with open(out, "w", encoding="utf-8") as f:
                f.write("\n\n".join(combined))
            print(f"Saved {len(combined)} chapters -> {out}")
            return 0
        print("No chapters extracted.")
        return 1
    finally:
        session.close()


def cmd_login(args) -> int:
    from .login import login

    login(args.site, timeout_s=args.timeout, cdp=args.cdp)
    return 0


def cmd_ocr(args) -> int:
    from .cleaner import TextCleaner
    from .ocr import OCREngine

    target = args.target
    if not os.path.exists(target):
        print(f"Error: '{target}' does not exist.")
        return 1

    engine = OCREngine()
    if os.path.isdir(target):
        raw = engine.process_folder(target)
    else:
        raw = engine.image_text(target)
    if not raw:
        print("Error: No text recognized.")
        return 1
    final = TextCleaner().clean(raw)

    os.makedirs(args.output_dir, exist_ok=True)
    name = os.path.basename(os.path.normpath(target))
    out = os.path.join(args.output_dir, f"{name}_cleaned.txt")
    with open(out, "w", encoding="utf-8") as f:
        f.write(final)
    print(f"Saved to {out}")
    return 0


def cmd_translate(args) -> int:
    from .translate import Translator

    Translator().translate_file(args.input, args.output)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="novel",
        description="Download and translate Chinese web novels (Qidian, SFACG).",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_x = sub.add_parser("extract", help="Download a book (URL selects the site)")
    p_x.add_argument("url", help="book/catalog URL (qidian.com or sfacg.com)")
    p_x.add_argument("--output-dir", default=str(OUTPUT_DIR))
    p_x.add_argument("--limit", type=int, default=0, help="max chapters (0 = all)")
    p_x.add_argument("--start", type=int, default=0, help="start from chapter N")
    p_x.add_argument("--only-vip", action="store_true", help="only VIP chapters")
    p_x.add_argument("--headed", action="store_true", help="show the browser window")
    p_x.add_argument("--cdp", default=None, metavar="URL",
                     help="attach to your real browser (e.g. http://localhost:9222)")
    p_x.set_defaults(func=cmd_extract)

    p_l = sub.add_parser("login", help="Save a logged-in session for a site")
    p_l.add_argument("--site", required=True, choices=sorted(STORAGE_STATES))
    p_l.add_argument("--timeout", type=int, default=600, help="seconds to wait")
    p_l.add_argument("--cdp", default=None, metavar="URL",
                     help="attach to your real browser instead of opening one")
    p_l.set_defaults(func=cmd_login)

    p_o = sub.add_parser("ocr", help="OCR an image or folder of images -> text")
    p_o.add_argument("target", help="image file or folder")
    p_o.add_argument("--output-dir", default=str(OUTPUT_DIR))
    p_o.set_defaults(func=cmd_ocr)

    p_t = sub.add_parser("translate", help="Translate extracted text (zh -> vi)")
    p_t.add_argument("input", help="input .txt file")
    p_t.add_argument("output", nargs="?", default=None,
                     help="output file (default: <input>_vi.txt)")
    p_t.set_defaults(func=cmd_translate)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())