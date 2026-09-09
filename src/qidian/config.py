"""Shared config: .env loading, QQ auth cookies, output paths."""

import os
import pathlib

OUTPUT_DIR = pathlib.Path("outputs")

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


def _scoped(pairs: list[dict]) -> list[dict]:
    for p in pairs:
        p.update({"domain": ".qq.com", "path": "/"})
    return pairs


def qq_auth_cookies() -> list[dict]:
    """Build Playwright cookie list from env. [] when no ywguid/ywkey."""
    load_env()
    raw = os.environ.get("QQ_COOKIE")
    if raw:
        pairs = []
        for part in raw.split(";"):
            part = part.strip()
            if not part or "=" not in part:
                continue
            name, _, value = part.partition("=")
            pairs.append({"name": name.strip(), "value": value.strip()})
        return _scoped(pairs)

    pairs = []
    if os.environ.get("QQ_YWGUID"):
        pairs.append({"name": "ywguid", "value": os.environ["QQ_YWGUID"]})
    if os.environ.get("QQ_YWKEY"):
        pairs.append({"name": "ywkey", "value": os.environ["QQ_YWKEY"]})
    if os.environ.get("QQ_YWSESSION"):
        pairs.append({"name": "ywsession", "value": os.environ["QQ_YWSESSION"]})
    return _scoped(pairs)