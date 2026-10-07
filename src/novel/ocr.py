"""OCR chapter screenshots to text with EasyOCR."""

import os
import re
from collections.abc import Sequence
from typing import cast

try:
    import easyocr
except ImportError:
    easyocr = None


_LANG_MAP = {"ch": "ch_sim", "en": "en"}


class OCREngine:
    def __init__(self, lang: str = "ch", gpu: bool = False):
        model_dir = os.path.join(os.getcwd(), "models", "easyocr")
        os.makedirs(model_dir, exist_ok=True)
        langs = [_LANG_MAP.get(lang, lang)]

        if easyocr is None:
            raise RuntimeError(
                "easyocr is not installed. Run: uv sync  (or pip install -e .)"
            )

        try:
            self.reader = easyocr.Reader(
                lang_list=langs,
                gpu=gpu,
                model_storage_directory=model_dir,
                download_enabled=True,
            )
        except TypeError:
            os.environ["EASYOCR_MODEL_DIR"] = model_dir
            self.reader = easyocr.Reader(
                lang_list=langs, gpu=gpu, download_enabled=True
            )

    def image_text(self, path: str, join: str = "\n") -> str:
        """Recognize one image file -> text. `join`="" keeps characters of the
        same line adjacent (used for glyph-map rows)."""
        results = cast(
            list[Sequence[object]], self.reader.readtext(path, detail=1)
        )
        parts: list[str] = []
        for item in results:
            if len(item) >= 2:
                text = item[1]
                if isinstance(text, str):
                    parts.append(text)
        return join.join(parts)

    def process_folder(self, folder_path: str) -> str:
        valid_extensions = (".png", ".jpg", ".jpeg", ".bmp", ".tiff", ".gif", ".webp")
        image_files = [
            f
            for f in os.listdir(folder_path)
            if f.lower().endswith(valid_extensions)
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
            print(f"Processing {path}...")
            text = self.image_text(path, join="\n")
            if text:
                full_text.append(text)
        return "\n".join(full_text)


_ENGINE: OCREngine | None = None


def get_engine(lang: str = "ch", gpu: bool = False) -> OCREngine:
    """Process-wide OCR engine (loading EasyOCR models is expensive)."""
    global _ENGINE
    if _ENGINE is None:
        _ENGINE = OCREngine(lang=lang, gpu=gpu)
    return _ENGINE