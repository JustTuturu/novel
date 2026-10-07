// Package qidian implements the Qidian rule (book.qidian.com / www.qidian.com) —
// a port of the qidian rule from 404-novel-project/novel-downloader.
//
// Notable differences from the userscript:
//
//   - The userscript can only *render* the encrypted font through a real browser
//     and then OCRs glyph canvases; we do the same, but inside our own Playwright
//     page (or the user's real browser over CDP), which also satisfies qidian's
//     cookie / _csrfToken requirements for VIP chapters.
//   - Font deobfuscation = build a glyph map once (render the custom-font glyphs
//     to a canvas in rows of 30, OCR the rows, zip them back together), cache it,
//     then rewrite every PUA character in the chapter text. Per-glyph rendering is
//     the fallback when a row OCRs to the wrong length.
package qidian

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/PuerkitoBio/goquery"
	"github.com/mxschmitt/playwright-go"

	"novel/internal/browser"
	"novel/internal/config"
	"novel/internal/ocr"
	"novel/internal/rule"
	"novel/internal/text"
)

// contentSelectors are tried in order until one yields non-empty chapter text.
var contentSelectors = []string{
	"main.content",
	"main[id^='c-']",
	".read-content",
	".chapter-content",
	".text-wrap .read-content",
}

// lockMarkers are paywall strings in the page body; any hit means the chapter is
// not readable.
var lockMarkers = []string{"这是VIP章节", "订阅本章", "登录后阅读本章", "您还不是VIP章节"}

// glyphMapCache is the persistent {font|PUA glyph -> real char} map.
var glyphMapCache = filepath.Join(config.OutputDir, "qidian_fontmap.json")

// perRow is how many glyphs share one canvas row (see renderRowsJS).
const perRow = 30

// renderRowsJS renders the given characters in the page's own (encrypted) font
// onto a canvas and returns one data-URL per row of `perRow` glyphs.
//
// Ported verbatim from src/novel/qidian.py; the two `${...}` template literals
// are spliced in because a Go raw string cannot contain a backtick.
const renderRowsJS = `({chars, font, perRow}) => {
  const cv = document.createElement('canvas');
  const ctx = cv.getContext('2d');
  const out = [];
  for (let i = 0; i < chars.length; i += perRow) {
    const row = chars.slice(i, i + perRow);
    const size = 48, scale = 2, pad = 8;
    cv.width = (size + pad) * row.length * scale;
    cv.height = (size + pad * 2) * scale;
    ctx.scale(scale, scale);
    ctx.font = ` + "`${size}px ${font}`" + `;
    ctx.textBaseline = 'middle';
    ctx.fillStyle = '#fff';
    ctx.fillRect(0, 0, cv.width, cv.height);
    ctx.fillStyle = '#000';
    row.forEach((ch, j) => ctx.fillText(ch, j * (size + pad) + pad / 2, size + pad));
    out.push(cv.toDataURL('image/png'));
  }
  return out;
}`

// renderSingleJS renders one glyph per canvas — the fallback when a row OCRs to
// the wrong number of characters.
const renderSingleJS = `({chars, font}) => chars.map(ch => {
  const size = 64, scale = 3, pad = 12;
  const cv = document.createElement('canvas');
  cv.width = (size + pad * 2) * scale;
  cv.height = (size + pad * 2) * scale;
  const ctx = cv.getContext('2d');
  ctx.scale(scale, scale);
  ctx.font = ` + "`${size}px ${font}`" + `;
  ctx.textBaseline = 'middle';
  ctx.fillStyle = '#fff';
  ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.fillStyle = '#000';
  ctx.fillText(ch, pad, size + pad);
  return cv.toDataURL('image/png');
})`

// fontFamilyJS returns the computed font-family of the chapter container, which
// names the (encrypted) custom font.
const fontFamilyJS = `(sel) => {
    const el = document.querySelector(sel) || document.body;
    return getComputedStyle(el).fontFamily;
}`

var (
	bookIDPathRE = regexp.MustCompile(`/(?:book|info)/(\d+)`)
	bookIDNumRE  = regexp.MustCompile(`([0-9]{5,})`)
	wordCountRE  = regexp.MustCompile(`章节字数：(\d+)`)

	// The tooltip metadata Qidian hides in a chapter link's title attribute.
	titleLabelRE = regexp.MustCompile(`\s*(?:首发时间|章节字数|更新时间|最后更新|更新时间：|字数：)\s*`)
	titleDateRE  = regexp.MustCompile(`\s+\d{4}-\d{2}-\d{2}`)
	titleParenRE = regexp.MustCompile(`\s*（.*?）\s*$`)
)

// Rule is the qidian.com site rule.
type Rule struct{}

func init() { rule.Register(&Rule{}) }

var _ rule.Rule = (*Rule)(nil)

// -- rule.Rule ------------------------------------------------------------

func (r *Rule) Name() string { return "qidian" }

func (r *Rule) Domains() []string { return []string{"qidian.com"} }

// ConcurrencyLimit is 1: qidian rate-limits parallel chapter fetches hard.
func (r *Rule) ConcurrencyLimit() int { return 1 }

// StorageStatePath is where `novel login` saves the qidian session.
func (r *Rule) StorageStatePath() string {
	path, _ := config.StorageState("qidian")
	return path
}

// -- catalog -------------------------------------------------------------

// BookParse loads the rendered catalog page and returns metadata + chapters.
func (r *Rule) BookParse(url string, s *browser.Session) (*rule.Book, error) {
	id := r.bookID(url)
	catalogURL := r.catalogURL(url)
	doc, err := s.DomSoup(catalogURL, true)
	if err != nil {
		return nil, fmt.Errorf("qidian: failed to load %s: %w", catalogURL, err)
	}
	if doc == nil {
		return nil, fmt.Errorf("qidian: failed to load %s", catalogURL)
	}
	return &rule.Book{
		Site:     r.Name(),
		URL:      "https://book.qidian.com/info/" + id + "/",
		Title:    r.meta(doc, []metaCandidate{{"title", "h1"}, {"title", "h1.bookName"}, {"property", `meta[property="og:title"]`}}),
		Author:   r.meta(doc, []metaCandidate{{"name", ".author"}, {"name", ".author-name"}, {"property", `meta[name="author"]`}}),
		Intro:    r.meta(doc, []metaCandidate{{"class", ".book-intro-detail"}, {"class", "#book-intro-detail"}, {"name", `meta[name="description"]`}}),
		Chapters: r.parseChapters(doc),
	}, nil
}

// bookID pulls the numeric book id out of any qidian book/info URL.
func (r *Rule) bookID(url string) string {
	if m := bookIDPathRE.FindStringSubmatch(url); m != nil {
		return m[1]
	}
	if m := bookIDNumRE.FindStringSubmatch(url); m != nil {
		return m[1]
	}
	parts := strings.Split(strings.Trim(url, "/"), "/")
	return parts[len(parts)-1]
}

// catalogURL maps a book URL onto the catalog page for the host that was given.
func (r *Rule) catalogURL(url string) string {
	id := r.bookID(url)
	if strings.Contains(url, "www.qidian.com") {
		return "https://www.qidian.com/book/" + id + "/"
	}
	return "https://book.qidian.com/info/" + id + "/"
}

// metaCandidate is one (how-to-read, selector) pair tried by meta.
type metaCandidate struct {
	kind string // "property" reads the content attribute; anything else reads text
	sel  string
}

// meta returns the first non-empty value among the candidate selectors.
func (r *Rule) meta(doc *goquery.Document, candidates []metaCandidate) string {
	for _, c := range candidates {
		el := doc.Find(c.sel).First()
		if el.Length() == 0 {
			continue
		}
		var val string
		if c.kind == "property" {
			val, _ = el.Attr("content")
		} else {
			val = text.TextOf(el, " ")
		}
		val = strings.TrimSpace(val)
		if val != "" {
			return val
		}
	}
	return ""
}

// cleanChapterTitle drops the catalog tooltip metadata Qidian puts in the title
// attribute.
//
//	e.g. "第一章 绯红 首发时间：2018-04-01 12:28:26 章节字数：2425"
//	     -> "第一章 绯红"
func cleanChapterTitle(raw string) string {
	title := strings.TrimSpace(raw)
	// Cut at the first metadata label; also drop a trailing time/date run.
	title = titleLabelRE.Split(title, 2)[0]
	title = titleDateRE.Split(title, 2)[0]
	title = strings.TrimSpace(titleParenRE.ReplaceAllString(title, ""))
	if title == "" {
		return strings.TrimSpace(raw)
	}
	return title
}

// parseChapters ports the qidian rule's bookParse: www.qidian.com uses
// `.catalog-volume` + `li.chapter-item` (locked = `.chapter-locked`);
// book.qidian.com uses `#j-catalogWrap .volume` where a volume header
// containing "VIP" marks the whole volume VIP (free ones carry an
// `<em class=iconfont>`).
func (r *Rule) parseChapters(doc *goquery.Document) []rule.Chapter {
	var chapters []rule.Chapter
	volumes := doc.Find(".catalog-volume")
	if volumes.Length() == 0 {
		volumes = doc.Find("#j-catalogWrap .volume")
	}

	if volumes.Length() > 0 {
		sectionNumber := 0
		num := 0
		volumes.Each(func(_ int, vol *goquery.Selection) {
			header := vol.Find("h2.head, h2, .head").First()
			sectionName := ""
			if header.Length() > 0 {
				sectionName = text.TextOf(header, " ")
			}
			volumeVIP := strings.Contains(strings.ToUpper(sectionName), "VIP")
			if sectionName != "" {
				sectionNumber++
			}
			items := vol.Find("li.chapter-item")
			if items.Length() == 0 {
				items = vol.Find("li")
			}
			items.Each(func(_ int, li *goquery.Selection) {
				a := li.Find("a.chapter-name").First()
				if a.Length() == 0 {
					a = li.Find("a").First()
				}
				href, _ := a.Attr("href")
				if a.Length() == 0 || href == "" {
					return
				}
				locked := li.Find(".chapter-locked").Length() > 0
				freeVIP := li.Find("em.iconfont").Length() > 0
				isVIP := locked || (volumeVIP && !freeVIP)
				attrTitle, _ := a.Attr("title")
				rawTitle := attrTitle
				if rawTitle == "" {
					rawTitle = text.TextOf(a, " ")
				}
				wordCount := 0
				if m := wordCountRE.FindStringSubmatch(attrTitle); m != nil {
					wordCount, _ = strconv.Atoi(m[1])
				}
				num++
				chapters = append(chapters, rule.Chapter{
					Num:           num,
					Title:         cleanChapterTitle(rawTitle),
					URL:           r.abs(href),
					IsVIP:         isVIP,
					IsPaid:        paidFlag(locked, isVIP),
					SectionName:   sectionName,
					SectionNumber: sectionNumber,
					WordCount:     wordCount,
				})
			})
		})
	} else {
		num := 0
		doc.Find("li.chapter-item").Each(func(_ int, li *goquery.Selection) {
			a := li.Find("a.chapter-name").First()
			if a.Length() == 0 {
				a = li.Find("a").First()
			}
			href, _ := a.Attr("href")
			if a.Length() == 0 || href == "" {
				return
			}
			locked := li.Find(".chapter-locked, .iconfont.lock").Length() > 0
			rawTitle, _ := a.Attr("title")
			if rawTitle == "" {
				rawTitle = text.TextOf(a, " ")
			}
			num++
			chapters = append(chapters, rule.Chapter{
				Num:    num,
				Title:  cleanChapterTitle(rawTitle),
				URL:    r.abs(href),
				IsVIP:  locked,
				IsPaid: rule.BoolPtr(!locked),
			})
		})
	}

	sort.SliceStable(chapters, func(i, j int) bool { return chapters[i].Num < chapters[j].Num })
	return chapters
}

// paidFlag maps (locked, isVIP) onto the tri-state IsPaid field:
// locked -> false, VIP-and-not-locked -> nil (unknown), free -> true.
func paidFlag(locked, isVIP bool) *bool {
	switch {
	case locked:
		return rule.BoolPtr(false)
	case isVIP:
		return nil
	default:
		return rule.BoolPtr(true)
	}
}

// abs makes a catalog href absolute against qidian's book host.
func (r *Rule) abs(href string) string {
	switch {
	case strings.HasPrefix(href, "//"):
		return "https:" + href
	case strings.HasPrefix(href, "http"):
		return href
	default:
		return "https://book.qidian.com" + href
	}
}

// -- chapters ------------------------------------------------------------

// ChapterParse renders one chapter: paywall check, content extraction, and — for
// the encrypted font — PUA glyph decoding via the cached font map.
func (r *Rule) ChapterParse(ch rule.Chapter, s *browser.Session) (*rule.ChapterResult, error) {
	if err := s.Goto(ch.URL); err != nil {
		return nil, fmt.Errorf("qidian: goto %s: %w", ch.URL, err)
	}
	page, err := s.Page()
	if err != nil {
		return nil, fmt.Errorf("qidian: %w", err)
	}

	body, err := page.TextContent("body")
	if err != nil {
		return nil, fmt.Errorf("qidian: read body of %s: %w", ch.URL, err)
	}
	if r.locked(page, body) {
		return &rule.ChapterResult{Title: ch.Title, Locked: true}, nil
	}

	txt, selector := "", ""
	for _, sel := range contentSelectors {
		raw, err := page.TextContent(sel)
		if err != nil {
			continue
		}
		txt = strings.TrimSpace(raw)
		if txt != "" {
			selector = sel
			break
		}
	}

	title := ch.Title
	if raw, err := page.TextContent("h1.j_chapterName, .chapter-name, h1"); err == nil && raw != "" {
		title = strings.Split(strings.TrimSpace(raw), "\n")[0]
	}

	if txt == "" {
		return &rule.ChapterResult{Title: title, NeedsOCR: true}, nil
	}

	if text.PUARatio(txt) <= 0.05 {
		return &rule.ChapterResult{Title: title, Text: text.CleanText(txt)}, nil
	}

	// Encrypted font: decode PUA glyphs via the cached font map, then
	// rebuild the text.
	txt, err = r.decodePUA(s, txt, selector)
	if err != nil {
		return nil, err
	}
	if text.PUARatio(txt) > 0.05 {
		_, _ = r.screenshot(ch, s) // best-effort: the CLI OCRs these later
		return &rule.ChapterResult{Title: title, NeedsOCR: true}, nil
	}
	return &rule.ChapterResult{Title: title, Text: text.CleanText(txt)}, nil
}

// locked reports the paywall: a lock marker in the body text, or the VIP limit
// overlay being present.
func (r *Rule) locked(page playwright.Page, body string) bool {
	for _, m := range lockMarkers {
		if strings.Contains(body, m) {
			return true
		}
	}
	el, err := page.QuerySelector(".vip-limit-wrap")
	if err != nil || el == nil {
		return false
	}
	_ = el.Dispose()
	return true
}

// decodePUA rewrites every PUA character of txt through the font glyph map,
// OCRing only the glyphs that are not cached yet.
func (r *Rule) decodePUA(s *browser.Session, txt, selector string) (string, error) {
	puaChars := text.PUAChars(txt)
	font, err := elementFontFamily(s, selector)
	if err != nil {
		return "", err
	}
	cached := loadFontmap()
	mapping := make(map[string]string, len(puaChars))
	for _, c := range puaChars {
		if v, ok := cached[cacheKey(font, c)]; ok {
			mapping[c] = v
		}
	}
	var missing []string
	for _, c := range puaChars {
		if _, ok := mapping[c]; !ok {
			missing = append(missing, c)
		}
	}
	if len(missing) > 0 {
		fmt.Printf("    font: %d glyph(s) to OCR (%d row(s))\n", len(missing), len(missing)/perRow+1)
		built, err := buildGlyphMap(s, missing, selector)
		if err != nil {
			return "", err
		}
		for k, v := range built {
			mapping[k] = v
		}
	}
	return text.ReplaceMap(txt, mapping), nil
}

// screenshot keeps the encrypted-font rendering so `novel ocr` can recover it.
func (r *Rule) screenshot(ch rule.Chapter, s *browser.Session) (string, error) {
	page, err := s.Page()
	if err != nil {
		return "", fmt.Errorf("qidian: %w", err)
	}
	outDir := filepath.Join(config.OutputDir, fmt.Sprintf("qidian_%s_screenshots", urlSegment(ch.URL, 3)))
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		return "", fmt.Errorf("qidian: create %s: %w", outDir, err)
	}
	path := filepath.Join(outDir, urlSegment(strings.TrimRight(ch.URL, "/"), -1)+".png")
	if _, err := page.Screenshot(playwright.PageScreenshotOptions{
		Path:     playwright.String(path),
		FullPage: playwright.Bool(true),
	}); err != nil {
		return "", fmt.Errorf("qidian: screenshot %s: %w", ch.URL, err)
	}
	return path, nil
}

// urlSegment splits url on "/" and returns segment i (negative counts from the
// end). It mirrors the Python `url.split("/")[i]` indexing the port used.
func urlSegment(url string, i int) string {
	parts := strings.Split(url, "/")
	if i < 0 {
		i += len(parts)
	}
	if i < 0 || i >= len(parts) {
		return ""
	}
	return parts[i]
}

// -- font map / OCR ------------------------------------------------------

// cacheKey namespaces a glyph by the font it was rendered with.
func cacheKey(font, ch string) string { return font + "|" + ch }

// loadFontmap reads the glyph cache; a missing or corrupt file yields an empty map.
func loadFontmap() map[string]string {
	raw, err := os.ReadFile(glyphMapCache)
	if err != nil {
		return map[string]string{}
	}
	out := map[string]string{}
	if err := json.Unmarshal(raw, &out); err != nil {
		return map[string]string{}
	}
	return out
}

// saveFontmap persists the glyph cache, best-effort. json.Marshal keeps non-ASCII
// glyphs as UTF-8, like Python's ensure_ascii=False.
func saveFontmap(fontmap map[string]string) {
	if err := os.MkdirAll(filepath.Dir(glyphMapCache), 0o755); err != nil {
		return
	}
	raw, err := json.Marshal(fontmap)
	if err != nil {
		return
	}
	_ = os.WriteFile(glyphMapCache, raw, 0o644)
}

// elementFontFamily returns the computed font-family of the chapter container.
func elementFontFamily(s *browser.Session, selector string) (string, error) {
	var font string
	if err := s.Evaluate(fontFamilyJS, selector, &font); err != nil {
		return "", fmt.Errorf("qidian: read font family: %w", err)
	}
	return font, nil
}

// buildGlyphMap OCRs the page's encrypted font and returns {PUA char -> real char}.
func buildGlyphMap(s *browser.Session, chars []string, selector string) (map[string]string, error) {
	font, err := elementFontFamily(s, selector)
	if err != nil {
		return nil, err
	}
	var rows []string
	if err := s.Evaluate(withJSONArg(renderRowsJS), map[string]any{
		"chars":  chars,
		"font":   font,
		"perRow": perRow,
	}, &rows); err != nil {
		return nil, fmt.Errorf("qidian: render glyph rows: %w", err)
	}
	ocrTexts, err := ocrRows(rows)
	if err != nil {
		return nil, err
	}

	mapping := map[string]string{}
	var perCharNeeded []string
	for idx, t := range ocrTexts {
		start := idx * perRow
		if start > len(chars) {
			start = len(chars)
		}
		rowChars := chars[start:]
		if len(rowChars) > perRow {
			rowChars = rowChars[:perRow]
		}
		glyphs := []rune(t) // Python len() counts characters, not bytes
		if len(glyphs) == len(rowChars) {
			for i, g := range glyphs {
				mapping[rowChars[i]] = string(g)
			}
		} else {
			perCharNeeded = append(perCharNeeded, rowChars...)
		}
	}

	if len(perCharNeeded) > 0 { // rows OCR'd inconsistently -> one glyph at a time
		var singles []string
		if err := s.Evaluate(withJSONArg(renderSingleJS), map[string]any{
			"chars": perCharNeeded,
			"font":  font,
		}, &singles); err != nil {
			return nil, fmt.Errorf("qidian: render single glyphs: %w", err)
		}
		singleTexts, err := ocrRows(singles)
		if err != nil {
			return nil, err
		}
		for i, ch := range perCharNeeded {
			if i >= len(singleTexts) {
				break
			}
			if utf8.RuneCountInString(singleTexts[i]) == 1 {
				mapping[ch] = singleTexts[i]
			}
		}
	}

	fontmap := loadFontmap()
	for k, v := range mapping {
		fontmap[cacheKey(font, k)] = v
	}
	saveFontmap(fontmap)
	return mapping, nil
}

// ocrRows OCRs each data-URL image row and returns one text string per row.
func ocrRows(rows []string) ([]string, error) {
	client := ocr.New("")
	out := make([]string, 0, len(rows))
	for i, dataURL := range rows {
		t, err := ocrDataURL(client, dataURL, fmt.Sprintf("qidian_row_%d_*.png", i))
		if err != nil {
			return nil, fmt.Errorf("qidian: ocr glyph row %d: %w", i, err)
		}
		out = append(out, t)
	}
	return out, nil
}

// ocrDataURL writes one canvas data-URL to a temp PNG and hands it to the OCR
// sidecar, joined without separators (glyph rows must stay adjacent).
func ocrDataURL(client *ocr.Client, dataURL, pattern string) (string, error) {
	raw, err := decodeDataURL(dataURL)
	if err != nil {
		return "", err
	}
	f, err := os.CreateTemp("", pattern)
	if err != nil {
		return "", fmt.Errorf("create temp image: %w", err)
	}
	path := f.Name()
	defer os.Remove(path)
	if _, err := f.Write(raw); err != nil {
		_ = f.Close()
		return "", fmt.Errorf("write %s: %w", path, err)
	}
	if err := f.Close(); err != nil {
		return "", fmt.Errorf("close %s: %w", path, err)
	}
	return client.ImageText(path, "")
}

// decodeDataURL extracts the base64 payload of a canvas PNG data-URL.
func decodeDataURL(dataURL string) ([]byte, error) {
	i := strings.Index(dataURL, ",")
	if i < 0 {
		return nil, fmt.Errorf("malformed canvas data URL")
	}
	raw, err := base64.StdEncoding.DecodeString(dataURL[i+1:])
	if err != nil {
		return nil, fmt.Errorf("decode canvas data URL: %w", err)
	}
	return raw, nil
}

// withJSONArg adapts a verbatim `({a, b}) => ...` script to
// browser.Session.Evaluate, which marshals the Go argument to JSON and hands it
// to the page as a string: parse that string, then call the real arrow function.
func withJSONArg(inner string) string {
	return "(jsonArg) => (" + inner + ")(JSON.parse(jsonArg))"
}
