// Package ocr is the Go-side OCR client. Recognition itself runs in a small
// Python sidecar (PaddleOCR/EasyOCR have no Go runtime); this package speaks to
// that service over HTTP and can spawn it on demand.
package ocr

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

// DefaultURL is where the sidecar listens unless overridden.
const DefaultURL = "http://127.0.0.1:8765"

// Client talks to the OCR sidecar.
type Client struct {
	BaseURL string
	HTTP    *http.Client

	spawnOnce sync.Once
	spawned   bool
}

// New returns a client for url, defaulting to OCR_URL / DefaultURL.
func New(url string) *Client {
	if url == "" {
		url = os.Getenv("NOVEL_OCR_URL")
	}
	if url == "" {
		url = DefaultURL
	}
	return &Client{
		BaseURL: strings.TrimRight(url, "/"),
		HTTP:    &http.Client{Timeout: 180 * time.Second},
	}
}

type ocrRequest struct {
	Path string `json:"path"`
	Join string `json:"join"`
}

type ocrResponse struct {
	Text string `json:"text"`
	Error string `json:"error,omitempty"`
}

// ImageText recognizes one image file. join="" keeps the characters of a line
// adjacent (used for the qidian glyph-map rows).
func (c *Client) ImageText(path, join string) (string, error) {
	if err := c.ensureUp(); err != nil {
		return "", err
	}
	body, err := json.Marshal(ocrRequest{Path: path, Join: join})
	if err != nil {
		return "", err
	}
	resp, err := c.HTTP.Post(c.BaseURL+"/ocr", "application/json", bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", err
	}
	var out ocrResponse
	if err := json.Unmarshal(raw, &out); err != nil {
		return "", fmt.Errorf("decode ocr response: %w", err)
	}
	if out.Error != "" {
		return "", fmt.Errorf("ocr: %s", out.Error)
	}
	return out.Text, nil
}

var imageExts = map[string]bool{
	".png": true, ".jpg": true, ".jpeg": true, ".bmp": true,
	".tiff": true, ".gif": true, ".webp": true,
}

// ProcessFolder OCRs every image in a folder, natural-sorted by name.
func (c *Client) ProcessFolder(folder string) (string, error) {
	entries, err := os.ReadDir(folder)
	if err != nil {
		return "", err
	}
	var files []string
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		if imageExts[strings.ToLower(filepath.Ext(e.Name()))] {
			files = append(files, filepath.Join(folder, e.Name()))
		}
	}
	sort.Slice(files, func(i, j int) bool { return naturalLess(files[i], files[j]) })
	var parts []string
	for _, f := range files {
		t, err := c.ImageText(f, "\n")
		if err != nil {
			return "", err
		}
		if t != "" {
			parts = append(parts, t)
		}
	}
	return strings.Join(parts, "\n"), nil
}

// naturalLess sorts "2" before "10" by comparing digit runs numerically.
func naturalLess(a, b string) bool {
	ai, bi := 0, 0
	for ai < len(a) && bi < len(b) {
		ac, bc := a[ai], b[bi]
		if isDigit(ac) && isDigit(bc) {
			as, ae := ai, ai
			for ae < len(a) && isDigit(a[ae]) {
				ae++
			}
			bs, be := bi, bi
			for be < len(b) && isDigit(b[be]) {
				be++
			}
			an, aerr := parseInt(a[as:ae])
			bn, berr := parseInt(b[bs:be])
			if aerr == nil && berr == nil && an != bn {
				return an < bn
			}
			ai, bi = ae, be
			continue
		}
		if lower(ac) != lower(bc) {
			return lower(ac) < lower(bc)
		}
		ai++
		bi++
	}
	return len(a)-ai < len(b)-bi
}

func isDigit(b byte) bool { return b >= '0' && b <= '9' }
func lower(b byte) byte {
	if b >= 'A' && b <= 'Z' {
		return b + 32
	}
	return b
}

func parseInt(s string) (int64, error) {
	var n int64
	if s == "" {
		return 0, fmt.Errorf("empty")
	}
	for i := 0; i < len(s); i++ {
		if !isDigit(s[i]) {
			return 0, fmt.Errorf("not a number")
		}
		n = n*10 + int64(s[i]-'0')
	}
	return n, nil
}

// ensureUp pings the sidecar and, if it's down, tries to start it once.
func (c *Client) ensureUp() error {
	if c.Healthy() {
		return nil
	}
	c.spawnOnce.Do(func() {
		c.spawned = c.trySpawn()
	})
	if c.Healthy() {
		return nil
	}
	return fmt.Errorf("OCR sidecar unreachable at %s; start it with "+
		"`uv run python sidecar/ocr_service.py` (or set NOVEL_OCR_URL)", c.BaseURL)
}

// Healthy reports whether the sidecar answers on /health.
func (c *Client) Healthy() bool {
	req, err := http.NewRequest(http.MethodGet, c.BaseURL+"/health", nil)
	if err != nil {
		return false
	}
	resp, err := (&http.Client{Timeout: 2 * time.Second}).Do(req)
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, resp.Body)
	return resp.StatusCode == 200
}

func (c *Client) trySpawn() bool {
	repoRoot, err := repoRoot()
	if err != nil {
		return false
	}
	script := filepath.Join(repoRoot, "sidecar", "ocr_service.py")
	if _, err := os.Stat(script); err != nil {
		return false
	}
	// Prefer the project's uv-managed interpreter; fall back to python3.
	python := filepath.Join(repoRoot, ".venv", "bin", "python")
	if _, err := os.Stat(python); err != nil {
		python = "python3"
	}
	cmd := exec.Command(python, script, "--host", "127.0.0.1", "--port", portOf(c.BaseURL))
	cmd.Dir = repoRoot
	logFile, err := os.OpenFile(filepath.Join(repoRoot, ".ocr_sidecar.log"),
		os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err == nil {
		cmd.Stdout = logFile
		cmd.Stderr = logFile
		defer logFile.Close()
	}
	if err := cmd.Start(); err != nil {
		return false
	}
	go func() { _ = cmd.Wait() }()

	deadline := time.Now().Add(60 * time.Second)
	for time.Now().Before(deadline) {
		if c.Healthy() {
			return true
		}
		time.Sleep(time.Second)
	}
	return false
}

func portOf(baseURL string) string {
	_, port, err := net.SplitHostPort(strings.TrimPrefix(strings.TrimPrefix(baseURL, "http://"), "https://"))
	if err != nil || port == "" {
		return "8765"
	}
	return port
}

// repoRoot walks up from the executable's working directory looking for go.mod.
func repoRoot() (string, error) {
	dir, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for i := 0; i < 8; i++ {
		if _, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil {
			return dir, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			break
		}
		dir = parent
	}
	return "", fmt.Errorf("go.mod not found from cwd")
}