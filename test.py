# test_ocr.py
import paddle
from paddleocr import PaddleOCR

print(f"PaddlePaddle version: {paddle.__version__}")
print(f"CUDA available: {paddle.device.is_compiled_with_cuda()}")

# lang='ch' is optimized for Chinese (simplified and traditional)
ocr = PaddleOCR(use_angle_cls=True, lang='ch')

# Thay bằng đường dẫn ảnh truyện tiếng Trung của bạn
img_path = 'sample.jpg'

result = ocr.ocr(img_path, cls=True)

if result and result[0]:
    for line in result[0]:
        text = line[1][0]
        confidence = line[1][1]
        print(f"[{confidence:.4f}] {text}")
