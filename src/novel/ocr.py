"""OCR chapter images to text with PaddleOCR.

This is the Python sidecar backing the Go CLI: `novel` renders/collects images
and asks this service to recognize them. PaddleOCR is used because it is the
declared dependency and handles Chinese well; `enable_mkldnn=False` is required
on this machine (Paddle 3.3's oneDNN path raises
`ConvertPirAttribute2RuntimeAttribute`).

`image_text(path, join="")` returns the recognized strings joined by `join` —
empty `join` keeps glyphs of a row adjacent, which is how the qidian glyph-map
rows are matched back to characters.
"""

import os
import re
import tempfile
from collections.abc import Sequence
from typing import cast

try:
    from paddleocr import PaddleOCR
except ImportError:  # pragma: no cover - dependency is declared in pyproject
    PaddleOCR = None

# PaddleOCR language codes (only what we use).
_LANG_MAP = {"ch": "ch", "zh": "ch", "en": "en"}

_IMAGE_EXTS = (".png", ".jpg", ".jpeg", ".bmp", ".tiff", ".tif", ".gif", ".webp")
# Formats PaddleOCR can't load directly; converted to PNG first.
_CONVERT_EXTS = (".gif", ".webp", ".tiff", ".tif", ".bmp")


class OCREngine:
    def __init__(self, lang: str = "ch", gpu: bool = False):
        if PaddleOCR is None:
            raise RuntimeError(
                "paddleocr is not installed. Run: uv sync (or pip install -e .)"
            )
        kwargs = {
            "lang": _LANG_MAP.get(lang, lang),
            "use_doc_orientation_classify": False,
            "use_doc_unwarping": False,
            "use_textline_orientation": False,
            "enable_mkldnn": False,
        }
        if gpu:
            kwargs["device"] = "gpu:0"
        self._ocr = PaddleOCR(**kwargs)

    def _predict(self, path: str) -> list[str]:
        results = cast(list[dict], self._ocr.predict(path))
        texts: list[str] = []
        for page in results:
            if not isinstance(page, dict):
                continue
            rec = page.get("rec_texts") or []
            for t in rec:
                if isinstance(t, str):
                    texts.append(t)
        return texts

    def image_text(self, path: str, join: str = "\n") -> str:
        """Recognize one image file -> text.

        `join=""` keeps characters of the same line adjacent (glyph-map rows).
        """
        tmp = None
        try:
            if path.lower().endswith(_CONVERT_EXTS):
                tmp = self._to_png(path)
                path = tmp
            texts = self._predict(path)
        finally:
            if tmp:
                try:
                    os.unlink(tmp)
                except OSError:
                    pass
        return join.join(texts)

    @staticmethod
    def _to_png(path: str) -> str | None:
        """Convert the first frame of gif/webp/tiff to a PNG PaddleOCR can read."""
        try:
            import cv2
            import numpy as np
        except ImportError:
            return None
        img = cv2.imread(path, cv2.IMREAD_UNCHANGED)
        if img is None:
            return None
        if img.ndim == 3 and img.shape[2] == 4:  # BGRA -> BGR
            img = cv2.cvtColor(img, cv2.COLOR_BGRA2BGR)
        elif img.ndim == 2:  # grayscale -> BGR
            img = cv2.cvtColor(img, cv2.COLOR_GRAY2BGR)
        fd, out = tempfile.mkstemp(suffix=".png", prefix="novel_ocr_")
        os.close(fd)
        cv2.imwrite(out, np.asarray(img))
        return out

    def process_folder(self, folder_path: str) -> str:
        image_files = [
            f
            for f in os.listdir(folder_path)
            if f.lower().endswith(_IMAGE_EXTS)
        ]
        image_files.sort(
            key=lambda f: [
                int(s) if s.isdigit() else s.lower() for s in re.split(r"(\d+)", f)
            ]
        )
        return self.process_images(
            [os.path.join(folder_path, f) for f in image_files]
        )

    def process_images(self, image_paths: list[str]) -> str:
        full_text: list[str] = []
        for path in image_paths:
            print(f"Processing {path}...", flush=True)
            text = self.image_text(path, join="\n")
            if text:
                full_text.append(text)
        return "\n".join(full_text)


_ENGINE: OCREngine | None = None


def get_engine(lang: str = "ch", gpu: bool = False) -> OCREngine:
    """Process-wide OCR engine (loading PaddleOCR models is expensive)."""
    global _ENGINE
    if _ENGINE is None:
        _ENGINE = OCREngine(lang=lang, gpu=gpu)
    return _ENGINE