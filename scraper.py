import asyncio
import os
from playwright.async_api import async_playwright

class QidianScraper:
    def __init__(self, output_dir="screenshots"):
        self.output_dir = output_dir
        if not os.path.exists(self.output_dir):
            os.makedirs(self.output_dir)

    async def capture_chapter(self, url):
        async with async_playwright() as p:
            # Set headless=False so you can see the browser and log in
            browser = await p.chromium.launch(headless=False)
            context = await browser.new_context(
                viewport={'width': 1280, 'height': 800},
                user_agent="Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36"
            )
            page = await context.new_page()
            
            print(f"Navigating to {url}...")
            await page.goto(url, wait_until="networkidle")
            
            print("--- ACTION REQUIRED ---")
            print("Please log in or navigate to the correct page in the opened browser.")
            print("Press Enter in this terminal when you are ready to capture screenshots...")
            input() 
            
            # Hide potential popups or sidebars if needed
            # For Qidian, the main content is usually in .read-content or similar
            
            # Get scroll height
            scroll_height = await page.evaluate("document.body.scrollHeight")
            viewport_height = 800
            
            screenshot_paths = []
            for i, offset in enumerate(range(0, scroll_height, viewport_height)):
                await page.evaluate(f"window.scrollTo(0, {offset})")
                await asyncio.sleep(0.5) # Wait for elements to stabilize
                path = os.path.join(self.output_dir, f"segment_{i}.png")
                await page.screenshot(path=path)
                screenshot_paths.append(path)
                print(f"Captured segment {i}")
                
            await browser.close()
            return screenshot_paths

if __name__ == "__main__":
    # Example usage
    scraper = QidianScraper()
    # Replace with a real Qidian chapter URL for testing
    # asyncio.run(scraper.capture_chapter("https://read.qidian.com/chapter/..."))
    print("Scraper ready.")
