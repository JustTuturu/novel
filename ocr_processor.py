import os
from paddleocr import PaddleOCR

class OCRProcessor:
    def __init__(self, lang='ch'):
        # Initialize PaddleOCR (downloads models if not present)
        self.ocr = PaddleOCR(use_angle_cls=True, lang=lang, show_log=False)

    def process_images(self, image_paths):
        full_text = []
        for path in image_paths:
            print(f"Processing {path}...")
            result = self.ocr.ocr(path, cls=True)
            if result and result[0]:
                # result[0] is a list of [box, (text, confidence)]
                for line in result[0]:
                    text = line[1][0]
                    full_text.append(text)
        return "\n".join(full_text)

if __name__ == "__main__":
    # Example usage
    # processor = OCRProcessor()
    # text = processor.process_images(["screenshots/segment_0.png"])
    # print(text)
    print("OCR Processor ready.")
