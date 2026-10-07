// Package browser wraps playwright-go: a stealth Chromium launch, CDP attach to
// a real browser, authenticated HTTP fetch that shares the session cookies, and
// DOM helpers used by every site rule.
package browser

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/PuerkitoBio/goquery"
	"github.com/mxschmitt/playwright-go"
)

// Use real Brave when present (not the bundled Chromium) so the site can't
// fingerprint the automation binary. Override with NOVEL_BROWSER.
const braveDefault = "/opt/brave.com/brave-origin/brave"

// Binary resolves the browser executable: NOVEL_BROWSER, then real Brave if
// installed, then Playwright's own Chromium (empty string).
func Binary() string {
	if v := os.Getenv("NOVEL_BROWSER"); v != "" {
		return v
	}
	if _, err := os.Stat(braveDefault); err == nil {
		return braveDefault
	}
	return ""
}

// UA is a plain Chrome-family UA string.
const UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) " +
	"Chrome/126.0.0.0 Safari/537.36"

// stealthJS hides automation fingerprints the site's JS checks:
// navigator.webdriver, plugins, languages, hardware concurrency, headless UA,
// and the WebGL renderer string.
const stealthJS = `
(() => {
  const ua = navigator.userAgent.replace(/HeadlessChrom(e|ium)\/\d+/g, '');
  Object.defineProperty(navigator, 'userAgent', { get: () => ua, enumerable: true });
  Object.defineProperty(navigator, 'webdriver', { get: () => undefined, enumerable: true });
  Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => undefined, enumerable: true });
  const plugins = ['Chrome PDF Viewer','Chrome PDF','Skype for Business','Native Client','Google Docs','Google Drive','Google Hangouts','YouTube'];
  Object.defineProperty(navigator, 'plugins', { get: () => plugins, enumerable: true });
  if (Object.getOwnPropertyDescriptor(navigator, 'languages') === undefined) {
    Object.defineProperty(navigator, 'languages', { get: () => ['zh-CN','zh','en-US','en'], enumerable: true });
  }
  Object.defineProperty(navigator, 'hardwareConcurrency', { get: () => 8, enumerable: true });
  const screenValues = { width: 1920, height: 1080, availWidth: 1920, availHeight: 1040, colorDepth: 24, pixelDepth: 24 };
  for (const [k, v] of Object.entries(screenValues)) {
    try { Object.defineProperty(screen, k, { get: () => v, enumerable: true }); } catch (e) {}
  }
  window.chrome = window.chrome || { loadTimes: {}, runtime: {} };
  try {
    const gpu = { vendorId: 0x10de, renderer: 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)' };
    const gl = document.createElement('canvas').getContext('webgl');
    if (gl) {
      const info = gl.getExtension('WEBGL_debug_renderer_info');
      if (info) {
        Object.defineProperty(gl, 'getParameter', { value: (p) => p === info.UNMASKED_VENDOR_WEBGL ? gpu.vendorId : p === info.UNMASKED_RENDERER_WEBGL ? gpu.renderer : gl.getParameter(p), enumerable: true });
      }
    }
  } catch (e) {}
})();
`

// Session is a Chromium page — either launched by us or the user's real browser
// via CDP — plus an authenticated HTTP helper that shares its cookies.
type Session struct {
	Timeout time.Duration

	pw       *playwright.Playwright
	browser  playwright.Browser
	ctx      playwright.BrowserContext
	page     playwright.Page
	attached bool // CDP mode: Close() must never kill the user's browser
}

// Cookie is a Playwright-shaped cookie for Start.
type Cookie struct {
	Name   string `json:"name"`
	Value  string `json:"value"`
	Domain string `json:"domain"`
	Path   string `json:"path"`
}

// New returns an unstarted session with the default 45s timeout.
func New() *Session { return &Session{Timeout: 45 * time.Second} }

// Start launches a stealth Chromium. Pass cookies and/or a storage-state file.
func (s *Session) Start(cookies []Cookie, storageState string, headless bool) error {
	pw, err := playwright.Run()
	if err != nil {
		return fmt.Errorf("start playwright: %w", err)
	}
	s.pw = pw

	args := []string{"--disable-blink-features=AutomationControlled"}
	if headless {
		args = append(args, "--disable-gpu", "--no-sandbox", "--disable-dev-shm-usage")
	}
	browser, err := pw.Chromium.Launch(playwright.BrowserTypeLaunchOptions{
		Headless:         playwright.Bool(headless),
		ExecutablePath:   playwright.String(Binary()),
		Args:             args,
		IgnoreDefaultArgs: []string{
			"--enable-automation",
			"--disable-component-extensions-with-background-pages",
			"--use-mock-keychain",
			"--password-store=basic",
		},
	})
	if err != nil {
		_ = pw.Stop()
		s.pw = nil
		return fmt.Errorf("launch chromium: %w", err)
	}
	s.browser = browser

	opts := playwright.BrowserNewContextOptions{
		Viewport:    &playwright.Size{Width: 1920, Height: 1080},
		UserAgent:   playwright.String(UA),
		Locale:      playwright.String("zh-CN"),
		TimezoneId:  playwright.String("Asia/Shanghai"),
	}
	if storageState != "" {
		opts.StorageStatePath = playwright.String(storageState)
	}
	ctx, err := browser.NewContext(opts)
	if err != nil {
		s.Close()
		return fmt.Errorf("new context: %w", err)
	}
	s.ctx = ctx

	if len(cookies) > 0 && storageState == "" {
		_ = ctx.AddCookies(cookiesToPW(cookies))
	}
	if err := ctx.AddInitScript(playwright.Script{Content: playwright.String(stealthJS)}); err != nil {
		// Non-fatal: stealth is best-effort.
		_ = err
	}
	page, err := ctx.NewPage()
	if err != nil {
		s.Close()
		return fmt.Errorf("new page: %w", err)
	}
	s.page = page
	return nil
}

func cookiesToPW(cs []Cookie) []playwright.OptionalCookie {
	out := make([]playwright.OptionalCookie, 0, len(cs))
	for _, c := range cs {
		out = append(out, playwright.OptionalCookie{
			Name:   c.Name,
			Value:  c.Value,
			Domain: playwright.String(c.Domain),
			Path:   playwright.String(c.Path),
		})
	}
	return out
}

// Attach connects to an already-running real browser (e.g. Brave started with
// --remote-debugging-port). The site then sees a genuine browser, which matters
// because both rules refuse to render paywalled text to automated ones.
func (s *Session) Attach(cdpURL string) error {
	pw, err := playwright.Run()
	if err != nil {
		return fmt.Errorf("start playwright: %w", err)
	}
	s.pw = pw
	s.attached = true

	browser, err := pw.Chromium.ConnectOverCDP(cdpURL)
	if err != nil {
		_ = pw.Stop()
		s.pw, s.attached = nil, false
		return fmt.Errorf("connect over cdp %s: %w", cdpURL, err)
	}
	s.browser = browser

	contexts := browser.Contexts()
	if len(contexts) == 0 {
		ctx, err := browser.NewContext()
		if err != nil {
			s.Close()
			return fmt.Errorf("new context: %w", err)
		}
		s.ctx = ctx
	} else {
		s.ctx = contexts[0]
	}
	pages := s.ctx.Pages()
	if len(pages) == 0 {
		page, err := s.ctx.NewPage()
		if err != nil {
			s.Close()
			return fmt.Errorf("new page: %w", err)
		}
		s.page = page
	} else {
		s.page = pages[0]
	}
	return nil
}

// Attached reports whether this session is in CDP mode.
func (s *Session) Attached() bool { return s.attached }

// Close tears the session down. In CDP mode it only disconnects.
func (s *Session) Close() {
	if s.attached {
		if s.pw != nil {
			_ = s.pw.Stop()
		}
		s.browser, s.ctx, s.page, s.pw, s.attached = nil, nil, nil, nil, false
		return
	}
	if s.browser != nil {
		_ = s.browser.Close()
	}
	if s.pw != nil {
		_ = s.pw.Stop()
	}
	s.browser, s.ctx, s.page, s.pw = nil, nil, nil, nil
}

// Page exposes the underlying page. Fails if the session isn't started.
func (s *Session) Page() (playwright.Page, error) {
	if s.page == nil {
		return nil, errors.New("session not started: call Start or Attach first")
	}
	return s.page, nil
}

// Context exposes the browser context (cookie jar + request API).
func (s *Session) Context() (playwright.BrowserContext, error) {
	if s.ctx == nil {
		return nil, errors.New("session not started: call Start or Attach first")
	}
	return s.ctx, nil
}

// Cookies returns the current cookies as name/value pairs, optionally for one
// URL only.
func (s *Session) Cookies(url string) map[string]string {
	out := map[string]string{}
	if s.ctx == nil {
		return out
	}
	var items []playwright.Cookie
	var err error
	if url != "" {
		items, err = s.ctx.Cookies(url)
	} else {
		items, err = s.ctx.Cookies()
	}
	if err != nil {
		return out
	}
	for _, c := range items {
		out[c.Name] = c.Value
	}
	return out
}

// StorageState saves the session (cookies) to a Playwright storage-state file.
func (s *Session) StorageState(path string) error {
	if s.ctx == nil {
		return errors.New("session not started")
	}
	_, err := s.ctx.StorageState(playwright.BrowserContextStorageStateOptions{
		Path: playwright.String(path),
	})
	return err
}

// Fetch is an authenticated HTTP GET through the browser context, so it shares
// the session cookies and UA. It mirrors novel-downloader's ggetHtmlDOM
// (GM_xmlhttpRequest): same logged-in cookies, no page JS.
func (s *Session) Fetch(url, referer string, headers map[string]string, retries int) (string, error) {
	if s.ctx == nil {
		return "", errors.New("session not started")
	}
	opts := playwright.APIRequestContextGetOptions{
		Timeout: playwright.Float(20000),
	}
	hdrs := map[string]string{}
	for k, v := range headers {
		hdrs[k] = v
	}
	if referer != "" {
		hdrs["referer"] = referer
	}
	if len(hdrs) > 0 {
		opts.Headers = hdrs
	}
	var lastErr error
	for i := 0; i <= retries; i++ {
		resp, err := s.ctx.Request().Get(url, opts)
		if err != nil {
			lastErr = err
			continue
		}
		if !resp.Ok() {
			lastErr = fmt.Errorf("http %d", resp.Status())
			continue
		}
		body, err := resp.Text()
		if err != nil {
			lastErr = err
			continue
		}
		return body, nil
	}
	if lastErr == nil {
		lastErr = errors.New("request failed")
	}
	return "", lastErr
}

// FetchBytes is Fetch for binary payloads (SFACG's VIP image).
func (s *Session) FetchBytes(url, referer string, headers map[string]string) ([]byte, error) {
	if s.ctx == nil {
		return nil, errors.New("session not started")
	}
	opts := playwright.APIRequestContextGetOptions{
		Timeout: playwright.Float(30000),
	}
	hdrs := map[string]string{}
	for k, v := range headers {
		hdrs[k] = v
	}
	if referer != "" {
		hdrs["referer"] = referer
	}
	if len(hdrs) > 0 {
		opts.Headers = hdrs
	}
	resp, err := s.ctx.Request().Get(url, opts)
	if err != nil {
		return nil, err
	}
	if !resp.Ok() {
		return nil, fmt.Errorf("http %d", resp.Status())
	}
	return resp.Body()
}

// Goto navigates the page and waits for the DOM to be usable.
func (s *Session) Goto(url string) error {
	if s.page == nil {
		return errors.New("session not started")
	}
	if _, err := s.page.Goto(url, playwright.PageGotoOptions{
		WaitUntil: playwright.WaitUntilStateDomcontentloaded,
		Timeout:   playwright.Float(s.Timeout.Seconds() * 1000),
	}); err != nil {
		return err
	}
	if _, err := s.page.WaitForSelector("body", playwright.PageWaitForSelectorOptions{
		Timeout: playwright.Float(15000),
	}); err != nil {
		// Body absent is not fatal; the caller still reads whatever rendered.
		_ = err
	}
	return nil
}

// ScrollToBottom triggers lazy-loaded catalog lists.
func (s *Session) ScrollToBottom(maxSteps int) error {
	if s.page == nil {
		return errors.New("session not started")
	}
	_, err := s.page.Evaluate(
		`(maxSteps) => new Promise(resolve => {
            let last = 0, stuck = 0, n = 0;
            const step = () => {
                window.scrollBy(0, 600);
                const h = Math.max(document.body?.scrollHeight || 0,
                                   document.documentElement?.scrollHeight || 0);
                stuck = (h === last) ? stuck + 1 : 0;
                last = h; n++;
                if (n > maxSteps || stuck > 5 || window.scrollY + window.innerHeight >= h) resolve();
                else setTimeout(step, 120);
            };
            step();
        })`,
		maxSteps,
	)
	return err
}

// Content returns the page's current HTML.
func (s *Session) Content() (string, error) {
	if s.page == nil {
		return "", errors.New("session not started")
	}
	return s.page.Content()
}

// Soup fetches a URL over HTTP and parses it (no JS, uses session cookies).
func (s *Session) Soup(url string) (*goquery.Document, error) {
	html, err := s.Fetch(url, "", map[string]string{"accept-language": "zh-CN,zh;q=0.9"}, 2)
	if err != nil {
		return nil, err
	}
	doc, err := goquery.NewDocumentFromReader(strings.NewReader(html))
	if err != nil {
		return nil, err
	}
	return doc, nil
}

// DomSoup renders a URL with JS (for SPA/catalog pages) then parses the live DOM.
func (s *Session) DomSoup(url string, scroll bool) (*goquery.Document, error) {
	if err := s.Goto(url); err != nil {
		return nil, err
	}
	if scroll {
		_ = s.ScrollToBottom(60)
	}
	html, err := s.Content()
	if err != nil {
		return nil, err
	}
	return goquery.NewDocumentFromReader(strings.NewReader(html))
}

// Evaluate runs JS in the page and decodes a JSON-encoded return value.
//
// goquery-playwright's Evaluate returns interface{} with driver-specific types;
// passing JSON through both directions keeps every call site simple and typed.
func (s *Session) Evaluate(script string, arg any, out any) error {
	if s.page == nil {
		return errors.New("session not started")
	}
	var argJSON string
	if arg != nil {
		b, err := json.Marshal(arg)
		if err != nil {
			return fmt.Errorf("marshal js arg: %w", err)
		}
		argJSON = string(b)
	}
	raw, err := s.page.Evaluate("JSON.stringify(await ("+script+"))", argJSON)
	if err != nil {
		return fmt.Errorf("evaluate: %w", err)
	}
	str, ok := raw.(string)
	if !ok {
		// A non-string means the JS returned a non-JSON value.
		b, mErr := json.Marshal(raw)
		if mErr != nil {
			return fmt.Errorf("evaluate returned %T", raw)
		}
		str = string(b)
	}
	if out == nil {
		return nil
	}
	if err := json.Unmarshal([]byte(str), out); err != nil {
		return fmt.Errorf("decode js result %q: %w", truncate(str, 200), err)
	}
	return nil
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

// sleep is a tiny helper so rules can pace requests like the userscript did.
func sleep(d time.Duration) { time.Sleep(d) }

var _ = context.Background