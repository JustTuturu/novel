import re

class TextCleaner:
    def __init__(self):
        # Add common noise patterns here
        self.noise_patterns = [
            r"Qidian\.com",
            r"起点中文网",
            r"手机访问",
            r"http[s]?://\S+",
            # Add more patterns based on Qidian's actual watermarks/noise
        ]

    def clean(self, text):
        lines = text.split('\n')
        cleaned_lines = []
        
        for line in lines:
            line = line.strip()
            if not line:
                continue
            
            # Remove patterns
            is_noise = False
            for pattern in self.noise_patterns:
                if re.search(pattern, line, re.IGNORECASE):
                    is_noise = True
                    break
            
            if not is_noise:
                cleaned_lines.append(line)
        
        return "\n\n".join(cleaned_lines)

if __name__ == "__main__":
    cleaner = TextCleaner()
    print("Cleaner ready.")
