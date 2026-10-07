// Package sfacg implements the SFACG (www.sfacg.com) site rule — the Go port of
// src/novel/sfacg.py, itself a port of novel-downloader's sfacg rule.
//
// The key difference from Qidian: SFACG does not obfuscate its text, but its
// VIP chapters are delivered as a single long IMAGE (`.article-content >
// #vipImage`) that only loads with the right Referer + cookies. So VIP =
// download the image, keep it, and OCR it into text.
package sfacg

import (
	"fmt"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"strings"
	"sync"

	"github.com/PuerkitoBio/goquery"
	"golang.org/x/net/html"

	"novel/internal/browser"
	"novel/internal/config"
	"novel/internal/ocr"
	"novel/internal/rule"
	"novel/internal/text"
)

// siteName is the registry key, the storage-state key and the output prefix.
const siteName = "sfacg"

// badImageSizes are the payload sizes that mean the image request failed; the
// userscript rule retries on exactly these values.
var badImageSizes = map[int]bool{53658: true, 42356: true}

// publicChapterMarkers are page strings that prove a chapter is not readable.
var publicChapterMarkers = []string{"登录后阅读本章", "本章为VIP章节", "订阅本章", "请先登录"}

var (
	// novelIDRE pulls the novel id out of a /Novel/<id>/ URL.
	novelIDRE = regexp.MustCompile(`/Novel/(\d+)`)
	// shortIDRE pulls the novel id out of an older /s/<id>/ URL.
	shortIDRE = regexp.MustCompile(`/s/(\d+)`)
	// mainIndexRE strips the catalog suffix back to the novel's root URL.
	mainIndexRE = regexp.MustCompile(`/MainIndex/?$`)
	// tagCountRE drops the "(12)" popularity count from a catalog tag.
	tagCountRE = regexp.MustCompile(`\(\d+\)$`)
	// bookTagRE is the fallback for SFACG's leading 【书名】 tag.
	bookTagRE = regexp.MustCompile(`^【[^】]*】\s*`)
	// vipIconRE is the catalog's VIP flag on the first child element.
	vipIconRE = regexp.MustCompile(`icn_vip`)
)

// Rule is the SFACG site rule.
type Rule struct{}

func init() { rule.Register(&Rule{}) }

var _ rule.Rule = (*Rule)(nil)

// Name is the site key used for storage-state paths and output filenames.
func (r *Rule) Name() string { return siteName }

// Domains are substrings matched against a URL to select this rule.
func (r *Rule) Domains() []string { return []string{"sfacg.com", "sfacg.cn"} }

// ConcurrencyLimit caps parallel chapter fetches; SFACG rate-limits hard.
func (r *Rule) ConcurrencyLimit() int { return 1 }

// StorageStatePath is where `novel login` saves this site's session.
func (r *Rule) StorageStatePath() string {
	if p, ok := config.StorageState(siteName); ok {
		return p
	}
	return ""
}

// -- catalog ------------------------------------------------------------

// catalogURL normalizes any SFACG novel URL to its MainIndex catalog page.
//
// SFACG novels live on book.sfacg.com as /Novel/<id>/; the catalog is
// /Novel/<id>/MainIndex/ (older /s/<id>/MainIndex still appears in links).
func catalogURL(u string) string {
	for _, re := range []*regexp.Regexp{novelIDRE, shortIDRE} {
		if m := re.FindStringSubmatch(u); m != nil {
			return "https://book.sfacg.com/Novel/" + m[1] + "/MainIndex/"
		}
	}
	base := strings.TrimRight(strings.ReplaceAll(u, "/MainIndex", ""), "/")
	return base + "/MainIndex/"
}

// BookParse renders the catalog page and extracts the metadata plus every
// chapter entry.
func (r *Rule) BookParse(url string, s *browser.Session) (*rule.Book, error) {
	catalog := catalogURL(url)
	novelURL := strings.TrimRight(mainIndexRE.ReplaceAllString(catalog, ""), "/")

	soup, err := s.DomSoup(catalog, true)
	if err != nil {
		return nil, fmt.Errorf("failed to load %s: %w", catalog, err)
	}

	bookname := ""
	if el := firstOf(soup.Selection, "h1.story-title", "h1"); el != nil {
		bookname = text.TextOf(el, " ")
	}
	author := ""
	if el := soup.Find(".author-name").First(); el.Length() > 0 {
		author = text.TextOf(el, " ")
	}
	intro := ""
	if el := soup.Find(".introduce").First(); el.Length() > 0 {
		intro = text.TextOf(el, " ")
	}
	cover := ""
	if el := firstOf(soup.Selection, "#hasTicket div.pic img", "div.pic img"); el != nil {
		cover, _ = el.Attr("src")
	}

	links := soup.Find("ul.tag-list > li.tag > a")
	tags := make([]string, 0, links.Length())
	links.Each(func(_ int, a *goquery.Selection) {
		// Work on a copy: the icon glyph is not part of the tag name.
		label := a.Clone()
		label.Find("span.icn").Remove()
		name := tagCountRE.ReplaceAllString(text.TextOf(label, " "), "")
		tags = append(tags, strings.TrimSpace(name))
	})

	return &rule.Book{
		Site:     r.Name(),
		URL:      novelURL,
		Title:    bookname,
		Author:   author,
		Intro:    intro,
		Tags:     tags,
		Cover:    cover,
		Chapters: r.parseChapters(soup, bookname, loggedIn(soup)),
	}, nil
}

// loggedIn reads the top-right user bar: it shows the account name, or the
// literal "SF游客" for a guest session.
func loggedIn(soup *goquery.Document) bool {
	bar := soup.Find(".user-bar > .top-link > .normal-link").First()
	if bar.Length() == 0 {
		return false
	}
	return !strings.Contains(text.TextOf(bar, " "), "SF游客")
}

// parseChapters walks the catalog links. `.story-catalog` blocks are volumes
// (name in `.catalog-title`), `.catalog-list a` are chapters (name in the
// `title` attribute), and VIP chapters carry a leading `icn_vip` icon.
func (r *Rule) parseChapters(soup *goquery.Document, bookname string, loggedIn bool) []rule.Chapter {
	links := soup.Find(".catalog-list a")
	chapters := make([]rule.Chapter, 0, links.Length())
	var (
		sectionNumber  int
		sectionChapter int // kept for parity; the catalog exposes no such number
		num            int
		sectionName    string
	)
	links.Each(func(_ int, a *goquery.Selection) {
		href, _ := a.Attr("href")
		if href == "" {
			return
		}
		u := absoluteURL(href)

		title, _ := a.Attr("title")
		if title == "" {
			title = text.TextOf(a, " ")
		}
		title = stripBookTag(title, bookname)

		// Section name = the nearest .story-catalog ancestor's .catalog-title.
		newSection := ""
		if list := ancestorWithClass(a, "catalog-list"); list != nil {
			if container := ancestorWithClass(list, "story-catalog"); container != nil {
				if t := container.Find(".catalog-title").First(); t.Length() > 0 {
					newSection = stripBookTag(text.TextOf(t, " "), bookname)
				}
			}
		}
		if newSection != "" && newSection != sectionName {
			sectionName = newSection
			sectionNumber++
			sectionChapter = 0
		}
		sectionChapter++
		num++

		isVIP := hasVIPIcon(a)
		var paid *bool
		if !isVIP {
			// SFACG's catalog doesn't reveal entitlement for VIP chapters;
			// those are decided at parse time.
			paid = rule.BoolPtr(true)
		}
		chapters = append(chapters, rule.Chapter{
			Num:           num,
			Title:         title,
			URL:           u,
			IsVIP:         isVIP,
			IsPaid:        paid,
			SectionName:   sectionName,
			SectionNumber: sectionNumber,
		})
	})
	return chapters
}

// -- chapters -----------------------------------------------------------

// ChapterParse fetches one chapter page and turns it into text, an image to
// OCR, or a locked marker.
func (r *Rule) ChapterParse(ch rule.Chapter, s *browser.Session) (*rule.ChapterResult, error) {
	soup, err := s.Soup(ch.URL)
	if err != nil {
		// An unreadable page counts as locked, not as a hard failure.
		return &rule.ChapterResult{Title: ch.Title, Locked: true}, nil
	}
	body := text.TextOf(soup.Selection, " ")

	title := ch.Title
	if el := soup.Find("h1.article-title").First(); el.Length() > 0 {
		title = text.TextOf(el, " ")
	}

	// Paywall: .pay-section present = not entitled.
	if soup.Find(".pay-section").Length() > 0 {
		return &rule.ChapterResult{Title: title, Locked: true}, nil
	}

	if ch.IsVIP {
		img := firstOf(soup.Selection, ".article-content > #vipImage", "#vipImage")
		if img != nil {
			if src, _ := img.Attr("src"); src != "" {
				return r.vipChapter(ch, s, title, src)
			}
		}
	}

	content := soup.Find(".article-content").First()
	if content.Length() == 0 {
		for _, marker := range publicChapterMarkers {
			if strings.Contains(body, marker) {
				return &rule.ChapterResult{Title: title, Locked: true}, nil
			}
		}
		return &rule.ChapterResult{Title: title, NeedsOCR: true}, nil
	}

	imgs := content.Find("img[src]")
	images := make([]string, 0, imgs.Length())
	imgs.Each(func(_ int, img *goquery.Selection) {
		if src, ok := img.Attr("src"); ok && src != "" {
			images = append(images, src)
		}
	})

	return &rule.ChapterResult{
		Title:  title,
		Text:   text.CleanText(text.TextOf(content, "\n")),
		Images: images,
	}, nil
}

// vipChapter handles SFACG VIP = one long image. Fetch it (Referer + cookies),
// keep the image next to the outputs, and OCR it into text.
func (r *Rule) vipChapter(ch rule.Chapter, s *browser.Session, title, imgURL string) (*rule.ChapterResult, error) {
	outDir := filepath.Join(config.OutputDir, sessionNovelID(ch.URL)+"_images")
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		return nil, fmt.Errorf("create %s: %w", outDir, err)
	}
	ext := path.Ext(imgURL)
	if ext == "" {
		ext = ".gif"
	}
	imgPath := filepath.Join(outDir, chapterID(ch.URL)+ext)

	// The userscript retries the image up to 4 times and rejects the known
	// placeholder payloads by size.
	headers := map[string]string{"accept": "image/avif,image/webp,image/apng,image/*,*/*;q=0.8"}
	var raw []byte
	for i := 0; i < 4; i++ {
		body, err := s.FetchBytes(imgURL, ch.URL, headers)
		if err != nil || len(body) == 0 || badImageSizes[len(body)] {
			continue
		}
		raw = body
		break
	}
	if raw == nil {
		return &rule.ChapterResult{Title: title, Locked: true}, nil
	}
	if err := os.WriteFile(imgPath, raw, 0o644); err != nil {
		return nil, fmt.Errorf("write %s: %w", imgPath, err)
	}

	fmt.Printf("    VIP image %s (%d KB), OCRing...\n", filepath.Base(imgPath), len(raw)/1024)

	ocrText, err := ocrEngine().ImageText(imgPath, "\n")
	if err != nil {
		return nil, fmt.Errorf("ocr %s: %w", imgPath, err)
	}
	if strings.TrimSpace(ocrText) == "" {
		return &rule.ChapterResult{
			Title:    title,
			Text:     fmt.Sprintf("[VIP chapter saved as image: %s]", imgPath),
			Images:   []string{imgPath},
			NeedsOCR: true,
		}, nil
	}
	return &rule.ChapterResult{
		Title:  title,
		Text:   text.CleanText(ocrText),
		Images: []string{imgPath},
	}, nil
}

// -- helpers ------------------------------------------------------------

// absoluteURL expands a catalog link: protocol-relative links get https, and
// root-relative chapter paths (/Novel/..., /s/...) live on book.sfacg.com
// rather than www.sfacg.com.
func absoluteURL(href string) string {
	switch {
	case strings.HasPrefix(href, "//"):
		return "https:" + href
	case strings.HasPrefix(href, "/"):
		host := "www.sfacg.com"
		if strings.Contains(href, "/Novel/") || strings.Contains(href, "/s/") {
			host = "book.sfacg.com"
		}
		return "https://" + host + href
	}
	return href
}

// stripBookTag removes the 【书名】 prefix SFACG puts on every catalog title
// and volume name: the exact novel name first, any other bracketed tag after.
func stripBookTag(s, bookname string) string {
	s = strings.TrimSpace(s)
	if rest, ok := strings.CutPrefix(s, "【"+bookname+"】"); ok {
		return strings.TrimSpace(rest)
	}
	return strings.TrimSpace(bookTagRE.ReplaceAllString(s, ""))
}

// hasVIPIcon reports whether the anchor's first child element carries the
// icn_vip icon.
func hasVIPIcon(a *goquery.Selection) bool {
	if a.Length() == 0 {
		return false
	}
	first := firstElement(a.Nodes[0])
	if first == nil {
		return false
	}
	for _, attr := range first.Attr {
		if attr.Key == "class" && vipIconRE.MatchString(attr.Val) {
			return true
		}
	}
	return false
}

// firstElement is the first element in document order below n (n excluded),
// mirroring BeautifulSoup's find(True).
func firstElement(n *html.Node) *html.Node {
	for c := n.FirstChild; c != nil; c = c.NextSibling {
		if c.Type == html.ElementNode {
			return c
		}
		if found := firstElement(c); found != nil {
			return found
		}
	}
	return nil
}

// ancestorWithClass walks up from s to the nearest ancestor carrying class.
func ancestorWithClass(s *goquery.Selection, class string) *goquery.Selection {
	for p := s.Parents().First(); p.Length() > 0; p = p.Parents().First() {
		if p.HasClass(class) {
			return p
		}
	}
	return nil
}

// firstOf mirrors BeautifulSoup's `select_one(a) or select_one(b)`: the first
// selector that matches anything wins.
func firstOf(root *goquery.Selection, selectors ...string) *goquery.Selection {
	for _, sel := range selectors {
		if found := root.Find(sel).First(); found.Length() > 0 {
			return found
		}
	}
	return nil
}

// chapterID is the second-to-last path segment of a chapter URL, i.e. the id
// in .../MainIndex/<id>/...
func chapterID(u string) string {
	parts := strings.Split(strings.TrimRight(u, "/"), "/")
	if len(parts) < 2 {
		return ""
	}
	return parts[len(parts)-2]
}

// sessionNovelID turns `https://www.sfacg.com/s/12345/chapter/999` into
// `sfacg_12345`, the folder the VIP images of one novel are kept in.
func sessionNovelID(chapterURL string) string {
	if m := shortIDRE.FindStringSubmatch(chapterURL); m != nil {
		return siteName + "_" + m[1]
	}
	return siteName
}

// The OCR sidecar is spawned lazily (it boots an interpreter + torch), so the
// client is built once and only on the first VIP chapter that needs it.
var (
	ocrOnce   sync.Once
	ocrHandle *ocr.Client
)

func ocrEngine() *ocr.Client {
	ocrOnce.Do(func() { ocrHandle = ocr.New("") })
	return ocrHandle
}
