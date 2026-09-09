"""Unified `qidian` command-line interface.

Subcommands:
  ocr        OCR a folder of chapter screenshots -> cleaned text file
  extract    Extract a book/chapter from Qidian or QQ阅读 (DOM)
  translate  Translate an extracted text file (zh -> vi)
  qq         VIP QQ阅读 downloader (cookie-authenticated)
"""

import argparse
import os
import re
import sys

from .cleaner import TextCleaner

# --- OCR ----------------------------------------------------------------


def cmd_ocr(args) -> int:
    from . import ocr

    folder_path = args.target
    if not os.path.exists(folder_path):
        print(f"Error: Folder '{folder_path}' does not exist.")
        return 1

    print("--- OCR Workflow ---")
    print(f"Folder: {folder_path}")

    print("\n[Stage 1/2] Recognizing text from images...")
    processor = ocr.OCREngine()
    raw_text = processor.process_folder(folder_path)
    if not raw_text:
        print("Error: No text recognized or no images found in the folder.")
        return 1

    print("\n[Stage 2/2] Cleaning recognized text...")
    final_text = TextCleaner().clean(raw_text)

    output_dir = args.output_dir
    os.makedirs(output_dir, exist_ok=True)
    folder_name = os.path.basename(os.path.normpath(folder_path))
    output_file = os.path.join(output_dir, f"{folder_name}_cleaned.txt")
    with open(output_file, "w", encoding="utf-8") as f:
        f.write(final_text)
    print("\n--- Complete ---")
    print(f"Result saved to: {output_file}")
    return 0


def _is_qq_chapter_url(url: str) -> bool:
    return bool(re.search(r"book\.qq\.com/book-read/", url))


def cmd_extract(args) -> int:
    from . import dom

    print("--- DOM Extraction Workflow ---")
    print(f"URL: {args.url}")

    ext = dom.DomExtractor()
    ext.start()
    try:
        if _is_qq_chapter_url(args.url) and args.single is None:
            print("Single-page mode (QQ Books chapter URL detected)")
            result = ext.extract_single_page(args.url)
            title = result.get("title", "Untitled")
            text = result.get("text", "")
            locked = result.get("locked", False)

            print(f"  Title: {title}")
            if locked:
                print("  LOCKED (VIP chapter, needs login)")
                return 1
            print(f"  Text length: {len(text)} chars")

            os.makedirs(args.output_dir, exist_ok=True)
            m = re.search(r"/book-read/(\d+)/(\d+)", args.url)
            safe_name = f"qqbook_{m.group(1)}_ch{m.group(2)}" if m else "qqbook_page"
            output_file = os.path.join(args.output_dir, f"{safe_name}.txt")
            with open(output_file, "w", encoding="utf-8") as f:
                f.write(f"# {title}\n{text}")
            print(f"Saved to {output_file}")
            return 0

        chapters = ext.list_chapters(args.url)
        if not chapters:
            print("Error: No chapters found.")
            return 1

        print(f"Found {len(chapters)} chapters")
        os.makedirs(args.output_dir, exist_ok=True)

        all_text = []
        for ch in chapters:
            print(f"  Ch {ch['num']}: {ch['title']}...", end="", flush=True)
            result = ext.extract_chapter(
                ch["url"], expected_word_count=ch.get("wordCount", 0)
            )
            if result.get("locked"):
                print(" LOCKED (skip)")
                continue
            if result["source"] == "ocr_needed":
                print(" OBFUSCATED (needs OCR)")
                all_text.append(f"# {result['title']}\n[OCR needed]")
                continue
            print(f" {len(result['text'])} chars")
            all_text.append(f"# {result['title']}\n{result['text']}")

        combined = "\n\n".join(all_text)
        safe_name = args.url.strip("/").split("/")[-2] if "/" in args.url else "qidian_book"
        output_file = os.path.join(args.output_dir, f"{safe_name}.txt")
        with open(output_file, "w", encoding="utf-8") as f:
            f.write(combined)
        print(f"Saved to {output_file}")
        return 0
    finally:
        ext.close()


# --- translate ----------------------------------------------------------


def cmd_translate(args) -> int:
    from . import translate

    t = translate.Translator()
    t.translate_file(args.input, args.output)
    return 0


# --- qq VIP -------------------------------------------------------------


def cmd_qq(args) -> int:
    from . import qq

    bid = qq._book_id_from(args.bid)
    if args.login:
        qq.login(bid, cid=args.cid, timeout_s=args.timeout)
        return 0
    if args.probe_api:
        qq.probe_api(bid)
        return 0
    qq.run(bid, limit=args.limit, start_cid=args.start_cid, probe=args.probe)
    return 0


# --- main ---------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="qidian",
        description="Extract, clean, and translate Chinese web-novel chapters.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_ocr = sub.add_parser("ocr", help="OCR a folder of screenshots to text")
    p_ocr.add_argument("target", help="path to image folder")
    p_ocr.add_argument("--output-dir", default="outputs")
    p_ocr.set_defaults(func=cmd_ocr)

    p_extract = sub.add_parser("extract", help="Extract a book/chapter via browser DOM")
    p_extract.add_argument("url", help="Qidian/QQ阅读 book or chapter URL")
    p_extract.add_argument("--output-dir", default="outputs")
    p_extract.add_argument("--single", action="store_true", help="force single-page mode")
    p_extract.set_defaults(func=cmd_extract)

    p_t = sub.add_parser("translate", help="Translate extracted text (zh -> vi)")
    p_t.add_argument("input", help="input .txt file")
    p_t.add_argument("output", nargs="?", default=None, help="output file (default: <input>_vi.txt)")
    p_t.set_defaults(func=cmd_translate)

    p_qq = sub.add_parser("qq", help="VIP QQ阅读 downloader (cookie-authenticated)")
    p_qq.add_argument("--bid", required=True, help="QQ阅读 book id (or book url)")
    p_qq.add_argument("--limit", type=int, default=0, help="max chapters")
    p_qq.add_argument("--start-cid", type=int, default=0, help="start from this chapter cid")
    p_qq.add_argument("--probe", action="store_true", help="dump one chapter + capture live API")
    p_qq.add_argument("--probe-api", action="store_true", help="show what the JSON API route needs")
    p_qq.add_argument("--login", action="store_true", help="open a headed browser to save the full QQ session to outputs/qq_storage.json")
    p_qq.add_argument("--cid", type=int, default=442, help="chapter cid to verify during --login (default 442)")
    p_qq.add_argument("--timeout", type=int, default=600, help="seconds to wait for login (default 600)")
    p_qq.set_defaults(func=cmd_qq)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if not hasattr(args, "func"):
        parser.print_help()
        return 1
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())