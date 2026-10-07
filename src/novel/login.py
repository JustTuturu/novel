"""Interactive login: open a real browser on the site, then save the session.

Both sites hand out paywalled text only to a logged-in browser, so this opens
a headed Chromium (or attaches to your real browser with --cdp), waits for you
to log in, and writes a Playwright storage-state file for later runs.
"""

import time

from .base import rule_for
from .browser import BrowserSession
from .config import site_cookies

LOGIN_TARGETS = {
    "qidian": "https://www.qidian.com/",
    "sfacg": "https://www.sfacg.com/",
}


def login(site: str, timeout_s: int = 600, cdp: str | None = None) -> bool:
    """Save a logged-in session for `site` to outputs/{site}_storage.json."""
    if site not in LOGIN_TARGETS:
        raise ValueError(f"Unknown site {site!r}; choose from {list(LOGIN_TARGETS)}")
    rule = rule_for(LOGIN_TARGETS[site])
    state_path = rule.storage_state_path()
    STATE_PARENT = state_path.parent
    STATE_PARENT.mkdir(parents=True, exist_ok=True)

    session = BrowserSession()
    if cdp:
        print(f"Attaching to real browser via CDP: {cdp}")
        session.attach(cdp)
        print(f"[login] Watching your tabs on {LOGIN_TARGETS[site]}; log in there.")
    else:
        session.start(site_cookies(site), headless=False)
        try:
            session.goto(LOGIN_TARGETS[site])
        except Exception as e:
            print(f"[login] Could not open {LOGIN_TARGETS[site]}: {e}")
        print(f"[login] Browser opened on {LOGIN_TARGETS[site]}")
        print("[login] Log in with your account, then leave this running.")
    print("[login] The session is saved as soon as your nickname appears.", flush=True)

    try:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            for page in session.context.pages:
                try:
                    if _logged_in(page.content(), site):
                        session.storage_state(state_path)
                        print(f"[login] Logged in -> session saved to {state_path}")
                        return True
                except Exception:
                    continue
            time.sleep(3)
        print("[login] Timed out waiting for login; nothing saved.")
        return False
    finally:
        session.close()


def _logged_in(html: str, site: str) -> bool:
    if site == "sfacg":
        return "SF游客" not in html and ("logout" in html.lower()
                                        or "退出" in html)
    # Qidian: an account menu only exists once signed in.
    return ("quser" in html.lower() or "我的书架" in html
            or "user-info" in html.lower())