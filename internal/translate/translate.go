// Package translate converts Chinese novel text to Vietnamese, keeping
// character names as Pinyin. Go port of novel.translate.Translator.
package translate

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"regexp"
	"strings"
	"time"
)

// nameMap swaps known character names for stable Pinyin placeholders before
// sending text to Google Translate, then restores them afterwards.
var nameMap = []struct{ cn, pinyin string }{
	{"墨画", "Mo Hua"},
	{"安小富", "An Xiaofu"},
	{"安少爷", "An Xiaofu"},
	{"钱兴", "Qian Xing"},
	{"钱公子", "Qian Xing"},
	{"钱少爷", "Qian Xing"},
	{"瘦公子", "Qian Xing"},
	{"小胖子", "An Xiaofu"},
	{"钱家", "nha ho Qian"},
	{"安家", "nha An"},
}

// Translator translates via the free Google Translate endpoint.
type Translator struct {
	Src  string
	Dest string
	HTTP *http.Client
	Now  func() time.Time
}

// New returns a zh-cn -> vi translator.
func New() *Translator {
	return &Translator{
		Src:  "zh-cn",
		Dest: "vi",
		HTTP: &http.Client{Timeout: 15 * time.Second},
		Now:  time.Now,
	}
}

func (t *Translator) sleep(d time.Duration) {
	if t.Now != nil {
		time.Sleep(d)
	}
}

// gt calls the Google Translate single-shot endpoint and joins the result.
func (t *Translator) gt(text string) (string, error) {
	q := url.Values{}
	q.Set("q", text)
	q.Set("sl", t.Src)
	q.Set("tl", t.Dest)
	q.Set("dt", "t")
	q.Set("ie", "UTF-8")
	q.Set("oe", "UTF-8")
	q.Set("client", "gtx")
	endpoint := "https://translate.googleapis.com/translate_a/single?" + q.Encode()

	resp, err := t.HTTP.Get(endpoint)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", err
	}
	var parts [][]any
	if err := json.Unmarshal(body, &parts); err != nil {
		return "", err
	}
	if len(parts) == 0 {
		return "", fmt.Errorf("empty translation response")
	}
	var out strings.Builder
	for _, p := range parts[0] {
		if arr, ok := p.([]any); ok && len(arr) > 0 {
			if s, ok := arr[0].(string); ok {
				out.WriteString(s)
			}
		}
	}
	return out.String(), nil
}

// protectNames replaces Chinese names with §N{i}§ placeholders, returning the
// masked text and the reverse map.
func protectNames(text string) (string, map[string]string) {
	reverse := map[string]string{}
	for i, e := range nameMap {
		placeholder := fmt.Sprintf("\xa7N%d\xa7", i)
		reverse[placeholder] = e.pinyin
		text = strings.ReplaceAll(text, e.cn, placeholder)
	}
	return text, reverse
}

// restoreNames swaps placeholders back to Pinyin names.
func restoreNames(text string, reverse map[string]string) string {
	for placeholder, pinyin := range reverse {
		text = strings.ReplaceAll(text, placeholder, pinyin)
	}
	return text
}

// Paragraph translates a single line, protecting character names. On error it
// returns the original text.
func (t *Translator) Paragraph(para string) string {
	safe, reverse := protectNames(para)
	translated, err := t.gt(safe)
	if err != nil {
		fmt.Printf("  ERROR: %v\n", err)
		return para
	}
	return restoreNames(translated, reverse)
}

var wordCountRE = regexp.MustCompile(`\b\d+\x{3000}\x{3000}`)

// File translates input -> output. Headers ("# ...") and blank lines pass
// through; every other line is translated with a blank line between results.
func (t *Translator) File(inputPath, outputPath string) error {
	raw, err := os.ReadFile(inputPath)
	if err != nil {
		return err
	}
	text := wordCountRE.ReplaceAllString(string(raw), "")
	lines := strings.Split(text, "\n")
	total := len(lines)
	var out []string

	for i, line := range lines {
		if strings.TrimSpace(line) == "" {
			out = append(out, "")
			continue
		}
		if strings.HasPrefix(line, "# ") {
			out = append(out, line)
			continue
		}
		out = append(out, t.Paragraph(line))
		out = append(out, "")
		if (i+1)%5 == 0 || i == total-1 {
			fmt.Printf("  [%d/%d] lines done\n", i+1, total)
		}
		t.sleep(500 * time.Millisecond)
	}

	result := strings.TrimSpace(strings.Join(out, "\n"))
	if err := os.WriteFile(outputPath, []byte(result), 0o644); err != nil {
		return err
	}
	fmt.Printf("\nSaved to: %s\n", outputPath)
	return nil
}
