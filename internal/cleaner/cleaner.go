// Package cleaner removes reader UI and OCR artifacts from recognized text.
// It's the Go port of novel.cleaner.TextCleaner.
package cleaner

import (
	"regexp"
	"strings"
)

var (
	chapterTitleRE = regexp.MustCompile(`第[一二三四五六七八九十百千零〇两0-9]+章`)

	footerAnchors = []*regexp.Regexp{
		regexp.MustCompile(`[-·—]?作家说`),
		regexp.MustCompile(`上起点中文网支持我`),
		regexp.MustCompile(`下载\s*(?:App|APP)`),
		regexp.MustCompile(`扫一扫[，,]?手机接着读`),
		regexp.MustCompile(`目录\s*下一章`),
	}

	puaRE = regexp.MustCompile(`[\x{e000}-\x{f8ff}]`)
	// CJK range used for boundary lookarounds.
	cjkRange = `\x{3400}-\x{4dbf}\x{4e00}-\x{9fff}\x{f900}-\x{faff}`

	// Go RE2 has no lookbehind, so the boundary-removal regexes are built to
	// consume the guard character and re-emit it via a capture group.
	boundaryLabels = []string{"目录", "投票", "设置"}
	boundaryNight  = "夜间"
)

var (
	lineNormalisers = buildNormalisers()
)

func buildNormalisers() []*regexp.Regexp {
	var out []*regexp.Regexp
	// Plain removals first.
	out = append(out,
		regexp.MustCompile(`讨论热烈[^\n]*`),
		regexp.MustCompile(`\d+字\d{4}年\d{2}月\d{2}日[^\n]*`),
		regexp.MustCompile(`本作品由起点中文网[^\n]*`),
		regexp.MustCompile(`@版权所有[^\n]*`),
		regexp.MustCompile(`(?m)^书友\d+[^\n]*`),
		regexp.MustCompile(`书详情|在书架|加书架|客户端|回书架|反馈|指南|举报|旧版`),
	)
	// CJK-boundary removals: the leading guard is captured and re-emitted.
	for _, label := range boundaryLabels {
		out = append(out,
			regexp.MustCompile(`(^|[^`+cjkRange+`])`+label+`([^`+cjkRange+`]|$)`),
		)
	}
	out = append(out,
		// 夜间 not followed by CJK/punctuation.
		regexp.MustCompile(`(^|[^`+cjkRange+`])`+boundaryNight+`([^`+cjkRange+`，。！？、]|$)`),
		// Leading OCR garbage glyphs.
		regexp.MustCompile(`(?m)^[:：三□✓√BCSQ②日]+\s*`),
		// A stray glyph surrounded by spaces before CJK.
		regexp.MustCompile(`\s+[□✓√BCSQ②]\s+(`+cjkRange+`)`),
		// A stray glyph after CJK/punctuation.
		regexp.MustCompile(`(`+cjkRange+`[。！？…」])\s+[□✓√BCSQ②]\s*`),
		// Circled numbers and stray digits.
		regexp.MustCompile(`[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]`),
		regexp.MustCompile(`(?m)\d{1,3}$`),
		regexp.MustCompile(`(?m)^\d{1,3}\s*$`),
	)
	return out
}

// IsObfuscated reports whether the text still carries encrypted-font glyphs.
func IsObfuscated(text string, threshold float64) bool {
	if text == "" {
		return false
	}
	n := float64(len(puaRE.FindAllString(text, -1)))
	return n/float64(len([]rune(text))) > threshold
}

// Clean normalizes OCR'd chapter text: cuts pre-chapter UI, drops author notes
// and footers, and strips reader artifacts line by line.
func Clean(text string) string {
	if text == "" {
		return ""
	}
	text = strings.ReplaceAll(text, "\r\n", "\n")
	text = strings.ReplaceAll(text, "\r", "\n")
	text = trimToChapter(text)
	text = regexp.MustCompile(`·作家说[\s\S]*$`).ReplaceAllString(text, "")
	text = trimFooter(text)
	text = strings.ReplaceAll(text, "\ufeff", "")
	text = strings.ReplaceAll(text, "\u200b", "")
	text = strings.ReplaceAll(text, "\u00a0", " ")
	text = strings.ReplaceAll(text, "\u3000", " ")
	text = strings.ReplaceAll(text, "\t", " ")
	return normaliseLines(text)
}

// trimToChapter drops everything before the line containing the 第N章 header.
func trimToChapter(text string) string {
	loc := chapterTitleRE.FindStringIndex(text)
	if loc == nil {
		return text
	}
	start := loc[0]
	if lineStart := strings.LastIndex(text[:loc[0]], "\n"); lineStart >= 0 {
		start = lineStart + 1
	}
	return text[start:]
}

// trimFooter cuts at the first footer anchor, whichever comes first.
func trimFooter(text string) string {
	best := -1
	for _, re := range footerAnchors {
		if loc := re.FindStringIndex(text); loc != nil && (best == -1 || loc[0] < best) {
			best = loc[0]
		}
	}
	if best >= 0 {
		return text[:best]
	}
	return text
}

var (
	collapseSpaces = regexp.MustCompile(`[ \t]{2,}`)
)

func normaliseLines(text string) string {
	// Apply the plain + boundary normalisers; the guard-capture regexes need a
	// replacement that re-emits the captured boundary char.
	for i, re := range lineNormalisers {
		if i >= len(boundaryLabels)+1 && i <= len(boundaryLabels)+1 {
			// guard-capture regexes: replace "$1$2" -> "$1$2" (keep guards)
			text = re.ReplaceAllString(text, "$1$2")
		} else {
			text = re.ReplaceAllString(text, "")
		}
	}

	var lines []string
	for _, line := range strings.Split(text, "\n") {
		line = collapseSpaces.ReplaceAllString(line, " ")
		line = strings.TrimSpace(line)
		if line != "" {
			lines = append(lines, line)
		}
	}
	return strings.TrimSpace(strings.Join(lines, "\n"))
}