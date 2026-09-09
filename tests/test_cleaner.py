import unittest

from qidian.cleaner import TextCleaner


class TextCleanerTests(unittest.TestCase):
    def setUp(self):
        self.cleaner = TextCleaner()

    def test_cleans_qidian_ocr_chrome_without_losing_prose(self):
        source = """起点读书
目录 加书架 夜间
第十二章 风起
讨论热烈 36人正在讨论
他看见【一封密信】，没有停步。②
:三 继续向前。
2463字2023年03月18日 12:00
上起点中文网支持我
下载App
"""
        self.assertEqual(
            self.cleaner.clean(source),
            "第十二章 风起\n他看见【一封密信】，没有停步。\n继续向前。",
        )

    def test_trims_footer_at_earliest_anchor(self):
        source = "第1章 开始\n正文。\n作家说\n后记\n目录 下一章"
        self.assertEqual(self.cleaner.clean(source), "第1章 开始\n正文。")

    def test_detects_private_use_font_obfuscation(self):
        self.assertTrue(self.cleaner.is_obfuscated("正常\ue123\ue456\ue789文本"))
        self.assertTrue(self.cleaner.is_obfuscated("正常文本\ue123"))


if __name__ == "__main__":
    unittest.main()