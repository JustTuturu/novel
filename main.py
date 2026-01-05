import asyncio
import sys
from scraper import QidianScraper
from ocr_processor import OCRProcessor
from cleaner import TextCleaner

async def main(url):
    print("Starting Qidian Chapter Extraction...")
    
    # 1. Scraping
    scraper = QidianScraper()
    image_paths = await scraper.capture_chapter(url)
    
    if not image_paths:
        print("Failed to capture any screenshots.")
        return

    # 2. OCR
    processor = OCRProcessor()
    raw_text = processor.process_images(image_paths)
    
    # 3. Cleaning
    cleaner = TextCleaner()
    final_text = cleaner.clean(raw_text)
    
    # 4. Save results
    output_file = "chapter_output.txt"
    with open(output_file, "w", encoding="utf-8") as f:
        f.write(final_text)
    
    print(f"\nProcessing complete! Content saved to {output_file}")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python main.py <Qidian_Chapter_URL>")
    else:
        chapter_url = sys.argv[1]
        asyncio.run(main(chapter_url))
        
