// Package rule defines the site-rule interface — the Go port of
// novel-downloader's BaseRuleClass.
//
// Each site is one Rule implementation with two methods:
//
//	BookParse(url, session)            -> *Book (metadata + full chapter list)
//	ChapterParse(chapter, session)     -> *ChapterResult (text, or a locked/OCR marker)
//
// Rules are registered at init time and dispatched by URL through For().
package rule

import (
	"fmt"
	"sort"
	"strings"
	"sync"

	"novel/internal/browser"
)

// Chapter is one catalog entry.
type Chapter struct {
	Num           int
	Title         string
	URL           string
	IsVIP         bool
	IsPaid        *bool // nil = unknown / decided at parse time
	SectionName   string
	SectionNumber int
	WordCount     int
}

// Book is a parsed catalog: metadata plus the full chapter list.
type Book struct {
	Site     string
	URL      string
	Title    string
	Author   string
	Intro    string
	Tags     []string
	Cover    string
	Chapters []Chapter
}

// VIPChapters returns the subset flagged VIP in the catalog.
func (b *Book) VIPChapters() []Chapter {
	out := make([]Chapter, 0, len(b.Chapters))
	for _, c := range b.Chapters {
		if c.IsVIP {
			out = append(out, c)
		}
	}
	return out
}

// ChapterResult is the outcome of parsing a single chapter.
type ChapterResult struct {
	Title    string
	Text     string
	Locked   bool
	NeedsOCR bool
	Images   []string
}

// Rule is implemented once per supported site.
type Rule interface {
	// Name is the site key used for storage-state paths and output filenames.
	Name() string
	// Domains are substrings matched against a URL to select this rule.
	Domains() []string
	// ConcurrencyLimit caps parallel chapter fetches (sites rate-limit).
	ConcurrencyLimit() int
	// StorageStatePath is where `novel login` saves this site's session.
	StorageStatePath() string

	BookParse(url string, s *browser.Session) (*Book, error)
	ChapterParse(ch Chapter, s *browser.Session) (*ChapterResult, error)
}

var (
	mu    sync.Mutex
	rules []Rule
)

// Register adds a rule to the registry. Called from each site's init().
func Register(r Rule) {
	mu.Lock()
	defer mu.Unlock()
	rules = append(rules, r)
}

// For returns the rule whose domains match url.
func For(url string) (Rule, error) {
	mu.Lock()
	defer mu.Unlock()
	for _, r := range rules {
		for _, d := range r.Domains() {
			if strings.Contains(url, d) {
				return r, nil
			}
		}
	}
	names := make([]string, 0, len(rules))
	for _, r := range rules {
		names = append(names, r.Name())
	}
	sort.Strings(names)
	return nil, fmt.Errorf("no rule for %q; known sites: %s", url, strings.Join(names, ", "))
}

// KnownSites lists the registered rule names, sorted.
func KnownSites() []string {
	mu.Lock()
	defer mu.Unlock()
	names := make([]string, 0, len(rules))
	for _, r := range rules {
		names = append(names, r.Name())
	}
	sort.Strings(names)
	return names
}

// BoolPtr is a helper for the tri-state IsPaid field.
func BoolPtr(b bool) *bool { return &b }