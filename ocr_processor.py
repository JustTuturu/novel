import os
import re
from collections.abc import Sequence
from typing import cast

# Use EasyOCR and store models for reuse under project `models/easyocr`
try:
    import easyocr
except ImportError:
    # Defer import error until runtime; user should install dependencies (see README)
    easyocr = None


class OCRProcessor:
    def __init__(self, lang="ch", gpu=False):
        # Use project-local `models/easyocr` directory to store models
        model_dir = os.path.join(os.getcwd(), "models", "easyocr")
        os.makedirs(model_dir, exist_ok=True)

        # Map short language code to EasyOCR language codes
        lang_map = {"ch": "ch_sim", "en": "en"}
        langs = [lang_map.get(lang, lang)]

        if easyocr is None:
            raise RuntimeError(
                "easyocr is not installed. Please install dependencies (pip install -r requirements or update pyproject)."
            )

        # Try to instruct EasyOCR to store/download models in our model_dir
        try:
            # modern easyocr supports model_storage_directory and download_enabled
            self.reader = easyocr.Reader(
                lang_list=langs,
                gpu=gpu,
                model_storage_directory=model_dir,
                download_enabled=True,
            )
        except TypeError:
            # Fallback: set an environment variable and retry
            os.environ["EASYOCR_MODEL_DIR"] = model_dir
            self.reader = easyocr.Reader(
                lang_list=langs, gpu=gpu, download_enabled=True
            )

        self.model_dir = model_dir

    def process_folder(self, folder_path):
        """Processes all images in a folder and returns joined text."""
        valid_extensions = (".png", ".jpg", ".jpeg", ".bmp", ".tiff")
        image_files = [
            f for f in os.listdir(folder_path) if f.lower().endswith(valid_extensions)
        ]

        # Sort files naturally (segment_1, segment_2, ..., segment_10)
        image_files.sort(
            key=lambda f: [
                int(s) if s.isdigit() else s.lower() for s in re.split(r"(\d+)", f)
            ]
        )

        image_paths = [os.path.join(folder_path, f) for f in image_files]
        return self.process_images(image_paths)

    def process_images(self, image_paths):
        full_text: list[str] = []
        for path in image_paths:
            print(f"Processing {path}...")
            # EasyOCR returns a list of (bbox, text, confidence)
            results = cast(
                list[Sequence[object]], self.reader.readtext(path, detail=1)
            )
            for item in results:
                if len(item) >= 2:
                    text = item[1]
                    if isinstance(text, str):
                        full_text.append(text)
        return "\n".join(full_text)


if __name__ == "__main__":
    print("OCR Processor ready.")
