// Package config holds .env loading, per-site cookie helpers, and output paths.
package config

import (
	"bufio"
	"os"
	"strings"
	"sync"

	"novel/internal/browser"
)

// OutputDir is where chapters, images, and sessions are written.
const OutputDir = "outputs"

// storageStates maps a site key to its Playwright storage-state file.
var storageStates = map[string]string{
	"qidian": OutputDir + "/qidian_storage.json",
	"sfacg":  OutputDir + "/sfacg_storage.json",
}

var (
	envOnce sync.Once
	cookieDomain = map[string]string{
		"qidian": ".qidian.com",
		"sfacg":  ".sfacg.com",
	}
)

// LoadEnv reads .env into the process environment (existing vars win).
func LoadEnv() {
	envOnce.Do(func() {
		f, err := os.Open(".env")
		if err != nil {
			return
		}
		defer f.Close()
		sc := bufio.NewScanner(f)
		for sc.Scan() {
			line := strings.TrimSpace(sc.Text())
			if line == "" || strings.HasPrefix(line, "#") || !strings.Contains(line, "=") {
				continue
			}
			k, v, _ := strings.Cut(line, "=")
			k, v = strings.TrimSpace(k), strings.TrimSpace(v)
			if _, ok := os.LookupEnv(k); !ok {
				_ = os.Setenv(k, v)
			}
		}
	})
}

// StorageState returns the storage-state file path for a site.
func StorageState(site string) (string, bool) {
	p, ok := storageStates[site]
	return p, ok
}

// StorageStateSites lists the sites with a storage-state path, sorted.
func StorageStateSites() []string {
	out := make([]string, 0, len(storageStates))
	for s := range storageStates {
		out = append(out, s)
	}
	// Only two sites; keep a deterministic order without importing sort elsewhere.
	for i := range out {
		for j := i + 1; j < len(out); j++ {
			if out[j] < out[i] {
				out[i], out[j] = out[j], out[i]
			}
		}
	}
	return out
}

// SiteCookies parses {SITE}_COOKIE from the environment into Playwright cookies,
// scoped to that site's domain.
func SiteCookies(site string) []browser.Cookie {
	LoadEnv()
	raw := strings.TrimSpace(os.Getenv(strings.ToUpper(site) + "_COOKIE"))
	if raw == "" {
		return nil
	}
	domain := cookieDomain[site]
	var out []browser.Cookie
	for _, part := range strings.Split(raw, ";") {
		part = strings.TrimSpace(part)
		if part == "" || !strings.Contains(part, "=") {
			continue
		}
		name, value, _ := strings.Cut(part, "=")
		out = append(out, browser.Cookie{
			Name:   strings.TrimSpace(name),
			Value:  strings.TrimSpace(value),
			Domain: domain,
			Path:   "/",
		})
	}
	return out
}

// HasSessionHint reports whether the user supplied any auth for a site.
func HasSessionHint(site string) bool {
	LoadEnv()
	up := strings.ToUpper(site)
	return strings.TrimSpace(os.Getenv(up+"_COOKIE")) != "" ||
		strings.TrimSpace(os.Getenv(up+"_CSRFTOKEN")) != ""
}