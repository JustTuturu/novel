"""Translate Chinese novel text to Vietnamese, keeping character names as Pinyin."""

import re
import time

import requests


class Translator:
    NAME_MAP = {
        "墨画": "Mo Hua",
        "安小富": "An Xiaofu",
        "安少爷": "An Xiaofu",
        "钱兴": "Qian Xing",
        "钱公子": "Qian Xing",
        "钱少爷": "Qian Xing",
        "瘦公子": "Qian Xing",
        "小胖子": "An Xiaofu",
        "小胖子 ": "An Xiaofu ",
        "钱家": "nha ho Qian",
        "安家": "nha An",
    }

    def __init__(self, src: str = "zh-cn", dest: str = "vi", delay: float = 0.5):
        self.src = src
        self.dest = dest
        self.delay = delay
        self._session = requests.Session()

    def _gt(self, text: str) -> str:
        payload = (
            f"q={text}&sl={self.src}&tl={self.dest}&dt=t&ie=UTF-8&oe=UTF-8&client=gtx"
        )
        url = f"https://translate.googleapis.com/translate_a/single?{payload}"
        r = self._session.get(url, timeout=15)
        r.raise_for_status()
        parts = r.json()[0]
        return "".join(p[0] for p in parts if isinstance(p, list) and len(p) > 0)

    def _replace_names(self, text: str) -> tuple[str, dict[str, str]]:
        reverse = {}
        for i, (cn, pinyin) in enumerate(self.NAME_MAP.items()):
            placeholder = f"\xa7N{i}\xa7"
            reverse[placeholder] = pinyin
            text = text.replace(cn, placeholder)
        return text, reverse

    def _restore_names(self, text: str, reverse: dict[str, str]) -> str:
        for placeholder, pinyin in reverse.items():
            text = text.replace(placeholder, pinyin)
        return text

    def translate_paragraph(self, para: str, on_error: bool = True) -> str:
        safe, reverse = self._replace_names(para)
        try:
            translated = self._gt(safe)
        except Exception as e:
            if on_error:
                print(f"  ERROR: {e}")
            return para
        return self._restore_names(translated, reverse)

    def translate_file(self, input_path: str, output_path: str) -> None:
        with open(input_path, "r", encoding="utf-8") as f:
            raw = f.read()

        raw = re.sub(r"\b\d+\u3000\u3000", "", raw)
        lines = raw.split("\n")
        out_lines: list[str] = []
        total = len(lines)

        for i, line in enumerate(lines):
            if not line.strip():
                out_lines.append("")
                continue
            if line.startswith("# "):
                out_lines.append(line)
                continue

            translated = self.translate_paragraph(line)
            out_lines.append(translated)
            out_lines.append("")

            if (i + 1) % 5 == 0 or i == total - 1:
                print(f"  [{i+1}/{total}] lines done")
            time.sleep(self.delay)

        result = "\n".join(out_lines).strip()
        with open(output_path, "w", encoding="utf-8") as f:
            f.write(result)
        print(f"\nSaved to: {output_path}")