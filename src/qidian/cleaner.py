"""Remove Qidian/QQ reader UI and OCR artifacts from chapter text."""

import re


class TextCleaner:
    _CHAPTER_TITLE_RE = re.compile(r"第[一二三四五六七八九十百千零〇两\d]+章")

    _FOOTER_ANCHORS = (
        re.compile(r"[-·—]?作家说"),
        re.compile(r"上起点中文网支持我"),
        re.compile(r"下载\s*(?:App|APP)"),
        re.compile(r"扫一扫[，,]?手机接着读"),
        re.compile(r"目录\s*下一章"),
    )

    _PUA_RE = re.compile(r"[\ue000-\uf8ff]")
    _CJK_RANGE = r"\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff"

    def is_obfuscated(self, text: str, threshold: float = 0.05) -> bool:
        return bool(text) and len(self._PUA_RE.findall(text)) / len(text) > threshold

    def clean(self, text: str) -> str:
        if not text:
            return ""

        text = text.replace("\r\n", "\n").replace("\r", "\n")
        text = self._trim_to_chapter(text)
        text = re.sub(r"·作家说[\s\S]*$", "", text)
        text = self._trim_footer(text)
        text = text.replace("\ufeff", "").replace("\u200b", "").replace("\xa0", " ")
        text = text.replace("\u3000", " ").replace("\t", " ")
        return self._normalise_lines(text)

    def _trim_to_chapter(self, text: str) -> str:
        match = self._CHAPTER_TITLE_RE.search(text)
        if not match:
            return text
        line_start = text.rfind("\n", 0, match.start())
        return text[line_start + 1 :] if line_start >= 0 else text[match.start() :]

    def _trim_footer(self, text: str) -> str:
        positions = [
            m.start()
            for pattern in self._FOOTER_ANCHORS
            if (m := pattern.search(text))
        ]
        return text[: min(positions)] if positions else text

    def _normalise_lines(self, text: str) -> str:
        cjk = self._CJK_RANGE

        text = re.sub(r"讨论热烈[^\n]*", "", text)
        text = re.sub(r"\d+字\d{4}年\d{2}月\d{2}日[^\n]*", "", text)
        text = re.sub(r"本作品由起点中文网[^\n]*", "", text)
        text = re.sub(r"@版权所有[^\n]*", "", text)
        text = re.sub(r"^书友\d+[^\n]*", "", text, flags=re.MULTILINE)
        text = re.sub(r"书详情|在书架|加书架|客户端|回书架|反馈|指南|举报|旧版", "", text)

        for label in ("目录", "投票", "设置"):
            text = re.sub(rf"(?<![{cjk}]){label}(?![{cjk}])", "", text)
        text = re.sub(rf"(?<![{cjk}])夜间(?![{cjk}，。！？、])", "", text)

        text = re.sub(r"^[:：三□✓√BCSQ②日]+\s*", "", text, flags=re.MULTILINE)
        text = re.sub(rf"\s+[□✓√BCSQ②]\s+(?=[{cjk}])", " ", text)
        text = re.sub(rf"(?<=[{cjk}。！？…」])\s+[□✓√BCSQ②]\s*", " ", text)
        text = re.sub(r"[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]", "", text)
        text = re.sub(r"\d{1,3}$", "", text, flags=re.MULTILINE)
        text = re.sub(r"^\d{1,3}\s*$", "", text, flags=re.MULTILINE)

        lines: list[str] = []
        for line in text.split("\n"):
            line = re.sub(r"[ \t]{2,}", " ", line).strip()
            if line:
                lines.append(line)
        return "\n".join(lines).strip()