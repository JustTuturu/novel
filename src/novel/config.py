"""Shared config: .env loading, per-site auth, output paths."""

import os
import pathlib

OUTPUT_DIR = pathlib.Path("outputs")

# Playwright storage-state files (cookies from a completed login), per site.
STORAGE_STATES = {
    "qidian": OUTPUT_DIR / "qidian_storage.json",
    "sfacg": OUTPUT_DIR / "sfacg_storage.json",
}

_env_loaded = False


def load_env() -> None:
    """Load .env into os.environ (setdefault). Never a security boundary."""
    global _env_loaded
    if _env_loaded:
        return
    _env_loaded = True
    env_path = pathlib.Path(".env")
    if not env_path.exists():
        return
    for line in env_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        os.environ.setdefault(key.strip(), value.strip())


def _parse_cookie_string(raw: str, domain: str) -> list[dict]:
    pairs = []
    for part in raw.split(";"):
        part = part.strip()
        if not part or "=" not in part:
            continue
        name, _, value = part.partition("=")
        pairs.append({"name": name.strip(), "value": value.strip(),
                      "domain": domain, "path": "/"})
    return pairs


def _scoped(pairs: list[dict], domain: str) -> list[dict]:
    for p in pairs:
        p.update({"domain": domain, "path": "/"})
    return pairs


def site_cookies(site: str) -> list[dict]:
    """Cookies for a site from .env:
      QIDIAN_COOKIE / SFACG_COOKIE -> full raw Cookie header string
      QIDIAN_CSRFTOKEN             -> qidian's _csrfToken (paired w/ your login cookies)
    """
    load_env()
    raw = os.environ.get(f"{site.upper()}_COOKIE", "").strip()
    if raw:
        domain = ".qidian.com" if site == "qidian" else ".sfacg.com"
        return _parse_cookie_string(raw, domain)
    return []


def has_session_hint(site: str) -> bool:
    """Cheap check for whether the user has supplied any auth at all."""
    load_env()
    return bool(
        os.environ.get(f"{site.upper()}_COOKIE", "").strip()
        or os.environ.get(f"{site.upper()}_CSRFTOKEN", "").strip()
    )