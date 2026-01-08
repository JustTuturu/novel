import re

class TextCleaner:
    def __init__(self):
        # Specific Qidian noise and OCR artifacts
        self.noise_patterns = [
            r"Qidian\.com",
            r"起点中文网",
            r"手机访问",
            r"http[s]?://\S+",
            r"本章未完", # Chapter not finished
            r"点击下一页继续阅读", # Click next page to continue
            r"【.*】", # Brackets often contain noise
            r"[\u4e00-\u9fa5]{1,2}\.[\u4e00-\u9fa5]{1,2}", # Likely tracking noise
        ]

    def clean(self, text):
        # Remove common OCR artifacts (like random spaces in the middle of words/sentences)
        # This is simple; more advanced regex can be added as needed
        lines = text.split('\n')
        cleaned_lines = []
        
        for line in lines:
            line = line.strip()
            if not line or len(line) < 2:
                continue
            
            # Remove specific noise signatures
            is_noise = False
            for pattern in self.noise_patterns:
                if re.search(pattern, line):
                    is_noise = True
                    break
            
            if not is_noise:
                cleaned_lines.append(line)
        
        # Join lines into paragraphs
        # Basic heuristic: if a line ends with a sentence ender, it's likely a paragraph end
        result = "\n".join(cleaned_lines)
        return result

if __name__ == "__main__":
    cleaner = TextCleaner()
    print("Cleaner ready.")
