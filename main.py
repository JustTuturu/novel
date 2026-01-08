import os
import sys
from ocr_processor import OCRProcessor
from cleaner import TextCleaner

def main(folder_path):
    if not os.path.exists(folder_path):
        print(f"Error: Folder '{folder_path}' does not exist.")
        return

    print(f"--- Starting Manual OCR Workflow ---")
    print(f"Folder: {folder_path}")
    
    # 1. OCR Processing
    print("\n[Stage 1/2] Recognizing text from images...")
    processor = OCRProcessor()
    raw_text = processor.process_folder(folder_path)
    
    if not raw_text:
        print("Error: No text recognized or no images found in the folder.")
        return

    # 2. Cleaning
    print("\n[Stage 2/2] Cleaning recognized text...")
    cleaner = TextCleaner()
    final_text = cleaner.clean(raw_text)
    
    # 3. Save results
    output_dir = "outputs"
    if not os.path.exists(output_dir):
        os.makedirs(output_dir)
        
    folder_name = os.path.basename(os.path.normpath(folder_path))
    output_file = os.path.join(output_dir, f"{folder_name}_cleaned.txt")
    
    with open(output_file, "w", encoding="utf-8") as f:
        f.write(final_text)
    
    print(f"\n--- Processing Complete! ---")
    print(f"Result saved to: {output_file}")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python main.py <folder_containing_screenshots>")
    else:
        folder_path = sys.argv[1]
        main(folder_path)
