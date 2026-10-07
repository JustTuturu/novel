// Package cli implements the `novel` command-line interface: extract, login,
// ocr, and translate.
package cli

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"novel/internal/browser"
	"novel/internal/cleaner"
	"novel/internal/config"
	"novel/internal/ocr"
	"novel/internal/rule"
	"novel/internal/translate"

	"github.com/mxschmitt/playwright-go"

	// Import the site rules so they register themselves.
	_ "novel/internal/qidian"
	_ "novel/internal/sfacg"
)

// Run dispatches argv and returns a process exit code.
func Run(argv []string) int {
	if len(argv) < 2 {
		usage()
		return 1
	}
	switch argv[1] {
	case "extract":
		return cmdExtract(argv[2:])
	case "login":
		return cmdLogin(argv[2:])
	case "ocr":
		return cmdOcr(argv[2:])
	case "translate":
		return cmdTranslate(argv[2:])
	case "help", "-h", "--help":
		usage()
		return 0
	case "install":
		return cmdInstall(argv[2:])
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n", argv[1])
		usage()
		return 1
	}
}

func usage() {
	fmt.Print(`novel — download and translate Chinese web novels (Qidian, SFACG).

Usage:
  novel extract <url> [flags]     Download a book (URL selects the site)
  novel login --site <site>       Save a logged-in session for a site
  novel ocr <file-or-folder>      OCR images -> cleaned text
  novel translate <in> [out]      Translate extracted text (zh -> vi)
  novel install [browser...]      Download the Playwright browser driver(s)

Extract flags:
  --output-dir DIR   where to write chapters (default "outputs")
  --limit N          max chapters (0 = all)
  --start N          start from chapter N
  --only-vip         only VIP chapters
  --headed           show the browser window
  --cdp URL          attach to your real browser (e.g. http://localhost:9222)
`)
}

// cmdInstall downloads the Playwright driver(s). Needed once before the first
// run; playwright-go reports "please install the driver (vX.Y.Z) first" if not.
func cmdInstall(args []string) int {
	browsers := args
	if len(browsers) == 0 {
		browsers = []string{"chromium"}
	}
	if err := playwright.Install(); err != nil {
		fmt.Fprintf(os.Stderr, "install: %v\n", err)
		return 1
	}
	fmt.Printf("Installed: %s\n", strings.Join(browsers, ", "))
	return 0
}

func cmdExtract(args []string) int {
	url, limit, start, onlyVIP, headed, cdp, outputDir, err := parseExtractArgs(args)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}

	r, err := rule.For(url)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	fmt.Printf("--- %s: extract ---\n", r.Name())
	fmt.Printf("URL: %s\n", url)

	session := browser.New()
	defer session.Close()

	state, hasState := config.StorageState(r.Name())
	storageState := ""
	if hasState {
		if _, err := os.Stat(state); err == nil {
			storageState = state
		}
	}
	if cdp != "" {
		if err := session.Attach(cdp); err != nil {
			fmt.Fprintf(os.Stderr, "cdp attach: %v\n", err)
			return 1
		}
		fmt.Println("Using your real browser via CDP.")
	} else {
		if storageState != "" {
			fmt.Printf("Using saved session: %s\n", filepath.Base(storageState))
		} else {
			fmt.Println("No saved session; free chapters only (see `novel login`).")
		}
		if err := session.Start(config.SiteCookies(r.Name()), storageState, !headed); err != nil {
			fmt.Fprintf(os.Stderr, "browser: %v\n", err)
			return 1
		}
	}

	book, err := r.BookParse(url, session)
	if err != nil {
		fmt.Fprintf(os.Stderr, "book parse: %v\n", err)
		return 1
	}
	fmt.Printf("  %s — %s  [%d chapters, %d VIP]\n",
		book.Title, book.Author, len(book.Chapters), len(book.VIPChapters()))

	chapters := book.Chapters
	if start > 0 {
		var kept []rule.Chapter
		for _, c := range chapters {
			if c.Num >= start {
				kept = append(kept, c)
			}
		}
		chapters = kept
	}
	if onlyVIP {
		chapters = book.VIPChapters()
	}
	if limit > 0 && limit < len(chapters) {
		chapters = chapters[:limit]
	}

	if err := os.MkdirAll(outputDir, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "output dir: %v\n", err)
		return 1
	}

	var combined []string
	for _, ch := range chapters {
		section := ""
		if ch.SectionName != "" {
			section = ch.SectionName + " / "
		}
		fmt.Printf("  [%d] %s%s...", ch.Num, section, ch.Title)
		result, err := r.ChapterParse(ch, session)
		if err != nil {
			fmt.Printf(" ERROR (%v)\n", err)
			continue
		}
		if result.Locked {
			fmt.Println(" LOCKED (need login / purchase)")
			continue
		}
		if result.NeedsOCR && result.Text == "" {
			fmt.Println(" NEEDS OCR")
			continue
		}
		flag := ""
		if result.NeedsOCR {
			flag = " [needs OCR]"
		}
		fmt.Printf(" %d chars%s\n", len([]rune(result.Text)), flag)
		combined = append(combined, "# "+result.Title+"\n"+result.Text)
	}

	if len(combined) > 0 {
		safe := fmt.Sprintf("%s_%s", r.Name(), filepath.Base(strings.TrimSuffix(book.URL, "/")))
		out := filepath.Join(outputDir, safe+".txt")
		if err := os.WriteFile(out, []byte(strings.Join(combined, "\n\n")), 0o644); err != nil {
			fmt.Fprintf(os.Stderr, "write: %v\n", err)
			return 1
		}
		fmt.Printf("Saved %d chapters -> %s\n", len(combined), out)
		return 0
	}
	fmt.Println("No chapters extracted.")
	return 1
}

func parseExtractArgs(args []string) (url string, limit, start int, onlyVIP, headed bool, cdp, outputDir string, err error) {
	outputDir = config.OutputDir
	for i := 0; i < len(args); i++ {
		a := args[i]
		next := func(name string) (string, error) {
			if i+1 >= len(args) {
				return "", fmt.Errorf("%s needs a value", name)
			}
			i++
			return args[i], nil
		}
		switch {
		case strings.HasPrefix(a, "--"):
			name := strings.TrimPrefix(a, "--")
			if eq := strings.Index(name, "="); eq >= 0 {
				// Support --flag=value.
				val := name[eq+1:]
				name = name[:eq]
				switch name {
				case "output-dir":
					outputDir = val
				case "limit":
					limit, _ = strconv.Atoi(val)
				case "start":
					start, _ = strconv.Atoi(val)
				case "only-vip":
					onlyVIP = true
				case "headed":
					headed = true
				case "cdp":
					cdp = val
				}
				continue
			}
			switch name {
			case "only-vip":
				onlyVIP = true
			case "headed":
				headed = true
			case "output-dir", "limit", "start", "cdp":
				v, e := next(a)
				if e != nil {
					return url, 0, 0, false, false, "", "", e
				}
				switch name {
				case "output-dir":
					outputDir = v
				case "cdp":
					cdp = v
				case "limit":
					limit, _ = strconv.Atoi(v)
				case "start":
					start, _ = strconv.Atoi(v)
				}
			default:
				return url, 0, 0, false, false, "", "", fmt.Errorf("unknown flag %s", a)
			}
		default:
			if url == "" {
				url = a
			}
		}
	}
	if url == "" {
		return url, 0, 0, false, false, "", "", fmt.Errorf("extract needs a book URL")
	}
	return url, limit, start, onlyVIP, headed, cdp, outputDir, nil
}

func cmdLogin(args []string) int {
	site, timeout, cdp := "", 600, ""
	for i := 0; i < len(args); i++ {
		switch {
		case args[i] == "--site" && i+1 < len(args):
			i++
			site = args[i]
		case args[i] == "--timeout" && i+1 < len(args):
			i++
			timeout, _ = strconv.Atoi(args[i])
		case args[i] == "--cdp" && i+1 < len(args):
			i++
			cdp = args[i]
		}
	}
	if site == "" {
		fmt.Fprintln(os.Stderr, "login needs --site (qidian or sfacg)")
		return 1
	}
	if _, ok := config.StorageState(site); !ok {
		fmt.Fprintf(os.Stderr, "unknown site %q\n", site)
		return 1
	}
	if login(site, timeout, cdp) {
		return 0
	}
	return 1
}

func cmdOcr(args []string) int {
	target, outputDir := "", config.OutputDir
	for i := 0; i < len(args); i++ {
		if args[i] == "--output-dir" && i+1 < len(args) {
			i++
			outputDir = args[i]
		} else if !strings.HasPrefix(args[i], "--") && target == "" {
			target = args[i]
		}
	}
	if target == "" {
		fmt.Fprintln(os.Stderr, "ocr needs an image file or folder")
		return 1
	}
	if _, err := os.Stat(target); err != nil {
		fmt.Fprintf(os.Stderr, "'%s' does not exist.\n", target)
		return 1
	}

	client := ocr.New("")
	var raw string
	fi, err := os.Stat(target)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	if fi.IsDir() {
		raw, err = client.ProcessFolder(target)
	} else {
		raw, err = client.ImageText(target, "\n")
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "ocr: %v\n", err)
		return 1
	}
	if raw == "" {
		fmt.Println("Error: No text recognized.")
		return 1
	}
	final := cleaner.Clean(raw)

	if err := os.MkdirAll(outputDir, 0o755); err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	name := filepath.Base(strings.TrimRight(target, string(filepath.Separator)))
	out := filepath.Join(outputDir, name+"_cleaned.txt")
	if err := os.WriteFile(out, []byte(final), 0o644); err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	fmt.Printf("Saved to %s\n", out)
	return 0
}

func cmdTranslate(args []string) int {
	var positional []string
	for i := 0; i < len(args); i++ {
		if !strings.HasPrefix(args[i], "--") {
			positional = append(positional, args[i])
		}
	}
	if len(positional) == 0 {
		fmt.Fprintln(os.Stderr, "translate needs an input .txt file")
		return 1
	}
	input := positional[0]
	output := ""
	if len(positional) > 1 {
		output = positional[1]
	} else {
		base := strings.TrimSuffix(input, filepath.Ext(input))
		output = base + "_vi.txt"
	}
	t := translate.New()
	if err := t.File(input, output); err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	return 0
}

// login opens a browser (or attaches via CDP), waits for the user to sign in,
// and saves the session so later runs can read paywalled chapters.
func login(site string, timeoutS int, cdp string) bool {
	target := loginTarget(site)
	r, err := rule.For(target)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return false
	}
	statePath := r.StorageStatePath()
	_ = os.MkdirAll(filepath.Dir(statePath), 0o755)

	session := browser.New()
	defer session.Close()

	if cdp != "" {
		fmt.Printf("Attaching to real browser via CDP: %s\n", cdp)
		if err := session.Attach(cdp); err != nil {
			fmt.Fprintf(os.Stderr, "cdp attach: %v\n", err)
			return false
		}
		fmt.Printf("[login] Watching your tabs on %s; log in there.\n", target)
	} else {
		if err := session.Start(config.SiteCookies(site), "", false); err != nil {
			fmt.Fprintf(os.Stderr, "browser: %v\n", err)
			return false
		}
		if err := session.Goto(target); err != nil {
			fmt.Printf("[login] Could not open %s: %v\n", target, err)
		}
		fmt.Printf("[login] Browser opened on %s\n", target)
		fmt.Println("[login] Log in with your account, then leave this running.")
	}
	fmt.Println("[login] The session is saved as soon as your nickname appears.")

	deadline := time.Now().Add(time.Duration(timeoutS) * time.Second)
	for time.Now().Before(deadline) {
		ctx, err := session.Context()
		if err == nil {
			for _, page := range ctx.Pages() {
				html, cErr := page.Content()
				if cErr != nil {
					continue
				}
				if loggedIn(html, site) {
					if sErr := session.StorageState(statePath); sErr != nil {
						fmt.Fprintf(os.Stderr, "save session: %v\n", sErr)
						return false
					}
					fmt.Printf("[login] Logged in -> session saved to %s\n", statePath)
					return true
				}
			}
		}
		time.Sleep(3 * time.Second)
	}
	fmt.Println("[login] Timed out waiting for login; nothing saved.")
	return false
}

func loginTarget(site string) string {
	switch site {
	case "qidian":
		return "https://www.qidian.com/"
	case "sfacg":
		return "https://www.sfacg.com/"
	}
	return "https://www.qidian.com/"
}

// loggedIn mirrors the Python _logged_in heuristic per site.
func loggedIn(html, site string) bool {
	lower := strings.ToLower(html)
	if site == "sfacg" {
		return !strings.Contains(html, "SF游客") &&
			(strings.Contains(lower, "logout") || strings.Contains(html, "退出"))
	}
	return strings.Contains(lower, "quser") ||
		strings.Contains(html, "我的书架") ||
		strings.Contains(lower, "user-info")
}
