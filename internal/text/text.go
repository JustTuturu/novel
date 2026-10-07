// Package text holds the pure-text helpers shared by the site rules: font
// obfuscation decoding, reader-UI noise removal, and PUA (encrypted-font)
// glyph detection.
package text

import (
	"regexp"
	"strings"
	"unicode/utf8"

	"github.com/PuerkitoBio/goquery"
	"golang.org/x/net/html"
)

var (
	// puaRE matches the Private Use Area block that encrypted fonts map glyphs into.
	puaRE = regexp.MustCompile(`[\x{e000}-\x{f8ff}]`)

	// singleCJKRE matches an attribute value that is exactly one CJK character,
	// which is how qidian/sfacg smuggle plaintext past the font mapping.
	singleCJKRE = regexp.MustCompile(`^[㐀-鿿豈-﫿‘’“”、。，；：！？（）…]$`)
)

// DecryptHTML recovers plaintext from attribute-based font obfuscation.
//
// A tag whose attributes carry single CJK characters holds the real plaintext
// there; its *visible* text is a font-mapped decoy and must be dropped. Other
// nodes recurse into children so their plaintext attributes are picked up too.
func DecryptHTML(h string) string {
	doc, err := html.Parse(strings.NewReader(h))
	if err != nil {
		return h
	}
	var b strings.Builder
	decryptNode(&b, doc)
	return b.String()
}

func decryptNode(b *strings.Builder, n *html.Node) {
	if n.Type == html.TextNode {
		b.WriteString(n.Data)
		return
	}
	if n.Type != html.ElementNode {
		return
	}
	if n.Data == "br" {
		b.WriteString("\n")
		return
	}
	for _, attr := range n.Attr {
		v := attr.Val
		if utf8.RuneCountInString(v) == 1 && singleCJKRE.MatchString(v) {
			b.WriteString(v)
			return
		}
	}
	for c := n.FirstChild; c != nil; c = c.NextSibling {
		decryptNode(b, c)
	}
}

// noisePatterns are reader-UI strings and ads that appear inside chapter text.
// Each is stripped as a MULTILINE regex; empty (or blank) lines collapse after.
var noisePatterns = []*regexp.Regexp{
	regexp.MustCompile(`(?m)[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]`),
	regexp.MustCompile(`(?m)^\d{1,3}\s*$`),
	regexp.MustCompile(`(?m)^(目录|书详情|加书架|投票|夜间|设置|客户端|在书架|反馈|指南|举报|旧版|回书架|书友圈)\s*$`),
	regexp.MustCompile(`本作品由起点中文网[^\n]*`),
	regexp.MustCompile(`@版权所有[^\n]*`),
	regexp.MustCompile(`·作家说[\s\S]*$`),
	regexp.MustCompile(`讨论热烈[^\n]*`),
	regexp.MustCompile(`(?m)^书友\d+[^\n]*`),
	regexp.MustCompile(`请下载(?:起点)?(?:中文网)?[^\n]*客户端[^\n]*`),
	regexp.MustCompile(`(?m)^.*本章字数：\d+字.*$`),
	regexp.MustCompile(`(?m)^.*更新时间：[\d\-\s:]+\d+.*$`),
	regexp.MustCompile(`(?m)^谷[\x{4e00}-\x{9fa5}]{0,1}$`),
}

var multiNewlineRE = regexp.MustCompile(`\n{3,}`)

// CleanText strips reader UI, ads, and metadata lines from chapter text.
// `extra` holds rule-specific patterns applied first.
func CleanText(text string, extra ...*regexp.Regexp) string {
	for _, re := range extra {
		if re != nil {
			text = re.ReplaceAllString(text, "")
		}
	}
	for _, re := range noisePatterns {
		text = re.ReplaceAllString(text, "")
	}
	text = multiNewlineRE.ReplaceAllString(text, "\n\n")
	return strings.TrimSpace(text)
}

// PUARatio is the fraction of Private Use Area characters in text — a high
// ratio means the text came out in the encrypted font and needs OCR decoding.
func PUARatio(text string) float64 {
	if text == "" {
		return 1.0
	}
	return float64(len(puaRE.FindAllString(text, -1))) / float64(utf8.RuneCountInString(text))
}

// PUAChars returns the sorted, deduplicated PUA characters present in text.
func PUAChars(text string) []string {
	seen := map[rune]bool{}
	for _, r := range text {
		if r >= 0xe000 && r <= 0xf8ff {
			seen[r] = true
		}
	}
	out := make([]rune, 0, len(seen))
	for r := range seen {
		out = append(out, r)
	}
	// Sort by codepoint for deterministic canvas rows.
	for i := 1; i < len(out); i++ {
		for j := i; j > 0 && out[j] < out[j-1]; j-- {
			out[j], out[j-1] = out[j-1], out[j]
		}
	}
	res := make([]string, 0, len(out))
	for _, r := range out {
		res = append(res, string(r))
	}
	return res
}

// ReplaceMap rewrites every key of m found in text; unknown chars pass through.
func ReplaceMap(text string, m map[string]string) string {
	if len(m) == 0 {
		return text
	}
	var b strings.Builder
	b.Grow(len(text))
	for _, r := range text {
		if v, ok := m[string(r)]; ok {
			b.WriteString(v)
			continue
		}
		b.WriteRune(r)
	}
	return b.String()
}

// TextOf extracts an element's text with the given separator between block
// children, mirroring BeautifulSoup's get_text(" ", strip=True).
func TextOf(sel *goquery.Selection, sep string) string {
	if sel == nil || sel.Length() == 0 {
		return ""
	}
	parts := strings.Fields(sel.Text())
	return strings.Join(parts, sep)
}