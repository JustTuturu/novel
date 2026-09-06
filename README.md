# Qidian OCR

Qidian OCR extracts text from comic or screenshot images, removes common Qidian/OCR noise, and saves the cleaned result as a text file.

## Requirements

- Python 3.12 or newer
- EasyOCR and its dependencies
- A folder containing `.png`, `.jpg`, `.jpeg`, `.bmp`, or `.tiff` images

## Installation

Using `uv`:

```bash
uv sync
```

Using `pip`:

```bash
pip install -e .
```

EasyOCR downloads its models on first use and stores them in `models/easyocr`.

## Usage

Put the images to process in a folder, then run:

```bash
python main.py path/to/image-folder
```

For example:

```bash
python main.py screenshots
```

Images are processed in natural filename order, so `segment_2` comes before `segment_10`.

The cleaned text is saved to:

```text
outputs/<folder-name>_cleaned.txt
```

## Project files

- `main.py` — runs the OCR and cleaning workflow.
- `ocr_processor.py` — reads text from images with EasyOCR.
- `cleaner.py` — removes common noise and empty/very short lines.
- `outputs/` — contains generated text files.

## OCR result format

EasyOCR returns each detection as `(bounding_box, text, confidence)`. The processor uses the second value, `item[1]`, because that is the recognized text; `item[0]` is only the bounding box.

## Troubleshooting

If EasyOCR is missing, install the project dependencies and run the command again. If no text is recognized, check that the folder exists, the images use a supported extension, and the images are clear enough for OCR.
