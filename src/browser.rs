//! Browser session — a small hand-written raw Chrome DevTools Protocol client
//! (port of internal/browser/session.go).
//!
//! Chromiumoxide 0.9 hangs indefinitely on `Browser::new_page()` against Brave
//! 154 even though the CDP HTTP endpoint answers fine, so the browser layer is
//! written by hand here: we spawn the binary ourselves, parse the real
//! `DevTools listening on ws://…` line off its stderr and speak CDP over a
//! websocket. Everything the rest of the crate needs (goto / evaluate /
//! cookies / screenshot) is a plain command with a bounded timeout.
//!
//! Async (tokio) unlike the synchronous Go version. Stealth parity comes from
//! launch args + an init script injected on every new document.

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// Hard ceiling for every CDP command / event wait. Nothing may hang the CLI.
const CDP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long we wait for the browser to print its DevTools websocket URL.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
Chrome/126.0.0.0 Safari/537.36";

/// Browser binary: NOVEL_BROWSER → known Brave → "" (probe PATH instead).
pub fn binary() -> String {
    if let Ok(p) = std::env::var("NOVEL_BROWSER") {
        if !p.is_empty() {
            return p;
        }
    }
    const BRAVE: &str = "/opt/brave.com/brave-origin/brave";
    if Path::new(BRAVE).exists() {
        return BRAVE.to_string();
    }
    String::new()
}

/// Fallbacks tried (in order) when `binary()` is empty.
const CANDIDATE_BINARIES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave-browser",
];

/// Same payload as browser.py's STEALTH_JS.
const STEALTH_JS: &str = r#"
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
    const gpu = { vendorId: 0x10de, rendererString: 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)' };
    const gl = document.createElement('canvas').getContext('webgl');
    if (gl) {
      Object.defineProperty(gl, 'getParameter', { value: (p) => (p === 0x1F01 ? gpu.vendorId : (p === 0x1F00 ? gpu.rendererString : undefined)) });
    }
  } catch (e) {}
})();
"#;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub path: String,
}

/// Storage state file format (Playwright-compatible subset, so the existing
/// outputs/<site>_storage.json keeps working).
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct StorageState {
    #[serde(default)]
    pub cookies: Vec<Cookie>,
}

pub struct Fetched {
    pub text: String,
    pub bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// CDP transport: one websocket, many commands, flat sessions.
// ---------------------------------------------------------------------------

type EventKey = (String, String);

/// Request/response multiplexer over the browser websocket.
///
/// Commands get an incrementing `id` and a one-shot reply channel; a reader
/// task dispatches `{"id":…}` replies to the right waiter and fans `method`
/// events out to the subscribers registered for (sessionId, method).
struct Cdp {
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    waiters: Mutex<HashMap<u64, mpsc::UnboundedSender<Result<Value>>>>,
    events: Mutex<HashMap<EventKey, Vec<oneshot::Sender<Value>>>>,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .with_context(|| format!("cannot open a CDP websocket at {ws_url}"))?;
        let (mut sink, stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        let me = Arc::new(Self {
            out: out_tx,
            next_id: AtomicU64::new(1),
            waiters: Mutex::new(HashMap::new()),
            events: Mutex::new(HashMap::new()),
        });

        tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        tokio::spawn(reader_task(stream, me.clone()));
        Ok(me)
    }

    async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.waiters.lock().unwrap().insert(id, tx);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        if self.out.send(msg.to_string()).is_err() {
            self.waiters.lock().unwrap().remove(&id);
            return Err(anyhow!("CDP connection closed before sending {method}"));
        }
        match tokio::time::timeout(CDP_TIMEOUT, rx.recv()).await {
            Err(_) => {
                self.waiters.lock().unwrap().remove(&id);
                Err(anyhow!("CDP {method} timed out after {}s", CDP_TIMEOUT.as_secs()))
            }
            Ok(None) => {
                self.waiters.lock().unwrap().remove(&id);
                Err(anyhow!("CDP connection closed while waiting for {method}"))
            }
            Ok(Some(res)) => res,
        }
    }

    /// Register a one-shot listener for `(session_id, method)` *before*
    /// triggering the event, so nothing is missed.
    fn subscribe(&self, session: &str, method: &str) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        self.events
            .lock()
            .unwrap()
            .entry((session.to_string(), method.to_string()))
            .or_default()
            .push(tx);
        rx
    }

    fn prune_events(&self, key: &EventKey) {
        let mut ev = self.events.lock().unwrap();
        if let Some(v) = ev.get_mut(key) {
            v.retain(|s| !s.is_closed());
            if v.is_empty() {
                ev.remove(key);
            }
        }
    }

    fn on_event(&self, session: &str, method: &str, params: Value) {
        let key = (session.to_string(), method.to_string());
        let subs = self.events.lock().unwrap().remove(&key).unwrap_or_default();
        for s in subs {
            let _ = s.send(params.clone());
        }
    }

    /// Wake every pending command with an error (the socket died).
    fn fail_all(&self, why: &str) {
        for (_, tx) in self.waiters.lock().unwrap().drain() {
            let _ = tx.send(Err(anyhow!("{why}")));
        }
    }
}

async fn reader_task<S>(mut stream: S, cdp: Arc<Cdp>)
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut why = "CDP connection closed".to_string();
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                why = format!("CDP websocket error: {e}");
                break;
            }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
            let waiter = cdp.waiters.lock().unwrap().remove(&id);
            let Some(tx) = waiter else { continue };
            let reply = match v.get("error") {
                Some(err) => {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown CDP error");
                    Err(anyhow!("CDP error: {msg}"))
                }
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(reply);
        } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
            let session = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("");
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            cdp.on_event(session, method, params);
        }
    }
    cdp.fail_all(&why);
}

/// `Runtime.evaluate` result → plain value, with JS exceptions as anyhow errors.
fn eval_value(result: &Value) -> Result<Value> {
    if let Some(ex) = result.get("exceptionDetails") {
        let msg = ex
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| ex.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown exception");
        return Err(anyhow!("JS exception: {msg}"));
    }
    Ok(result
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// The browser-level CDP connection (cheap to clone; only an Arc inside).
#[derive(Clone)]
pub struct BrowserHandle {
    cdp: Arc<Cdp>,
}

/// One flat-attached page target.
#[derive(Clone)]
pub struct Page {
    cdp: Arc<Cdp>,
    session_id: String,
}

impl BrowserHandle {
    /// Create a new page target and flat-attach to it.
    async fn new_page(&self) -> Result<Page> {
        let created = self
            .cdp
            .call("Target.createTarget", json!({ "url": "about:blank" }), None)
            .await
            .context("failed to open page")?;
        let target_id = created
            .get("targetId")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))?
            .to_string();
        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await
            .with_context(|| format!("failed to attach to target {target_id}"))?;
        let session_id = attached
            .get("sessionId")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Target.attachToTarget returned no sessionId"))?
            .to_string();
        Ok(Page {
            cdp: self.cdp.clone(),
            session_id,
        })
    }

    /// All cookies the browser holds (storage scope), name → value.
    async fn cookies(&self) -> Result<Vec<Cookie>> {
        let res = self
            .cdp
            .call("Storage.getCookies", json!({}), None)
            .await
            .context("Storage.getCookies")?;
        let out = res
            .get("cookies")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|c| Cookie {
                        name: c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        value: c.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        domain: c.get("domain").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        path: c.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(out)
    }

    async fn set_cookies(&self, cs: &[Cookie]) -> Result<()> {
        if cs.is_empty() {
            return Ok(());
        }
        self.cdp
            .call("Storage.setCookies", json!({ "cookies": cookie_params(cs) }), None)
            .await
            .context("Storage.setCookies")?;
        Ok(())
    }
}

impl Page {
    async fn enable(&self) -> Result<()> {
        self.cdp.call("Page.enable", json!({}), Some(&self.session_id)).await?;
        self.cdp
            .call("Runtime.enable", json!({}), Some(&self.session_id))
            .await?;
        Ok(())
    }

    async fn add_init_script(&self, source: &str) -> Result<()> {
        self.cdp
            .call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": source }),
                Some(&self.session_id),
            )
            .await?;
        Ok(())
    }

    /// Navigate, then wait (bounded) for `Page.loadEventFired`.
    async fn navigate(&self, url: &str) -> Result<()> {
        let load = self.cdp.subscribe(&self.session_id, "Page.loadEventFired");
        let res = self
            .cdp
            .call("Page.navigate", json!({ "url": url }), Some(&self.session_id))
            .await
            .with_context(|| format!("goto {url}"))?;
        if let Some(err) = res.get("errorText").and_then(|e| e.as_str()) {
            cdp_fail(url, err);
        }
        // SPAs sometimes never fire loadEventFired; cap the wait so the CLI
        // still makes progress.
        if tokio::time::timeout(CDP_TIMEOUT, load).await.is_err() {
            self.cdp
                .prune_events(&(self.session_id.clone(), "Page.loadEventFired".into()));
        }
        Ok(())
    }

    async fn evaluate_raw(&self, expression: &str) -> Result<Value> {
        let res = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                }),
                Some(&self.session_id),
            )
            .await
            .with_context(|| format!("Runtime.evaluate({})", expression.chars().take(80).collect::<String>()))?;
        eval_value(&res)
    }

    async fn content(&self) -> Result<String> {
        match self.evaluate_raw("document.documentElement ? document.documentElement.outerHTML : null").await? {
            Value::String(s) => Ok(s),
            Value::Null => Err(anyhow!("empty page content (no documentElement)")),
            other => Ok(other.to_string()),
        }
    }

    /// innerText of the first match, `null` when the element is absent.
    async fn inner(&self, sel: &str, prop: &str) -> Result<Option<String>> {
        let sel_lit = serde_json::to_string(sel)?;
        let expr = format!(
            "(() => {{ const e = document.querySelector({sel_lit}); \
             return e === null ? null : e.{prop}; }})()"
        );
        Ok(match self.evaluate_raw(&expr).await? {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => Some(other.to_string()),
        })
    }

    async fn screenshot_png(&self) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let res = self
            .cdp
            .call(
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": false }),
                Some(&self.session_id),
            )
            .await
            .context("Page.captureScreenshot")?;
        let data = res
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("decoding screenshot base64")
    }
}

fn cdp_fail(url: &str, err: &str) {
    // Chromium reports navigation problems in the result, not as CDP errors.
    eprintln!("[browser] navigation to {url} reported: {err}");
}

// ---------------------------------------------------------------------------
// Page pool — many targets over the same multiplexed websocket (Phase 3)
// ---------------------------------------------------------------------------

tokio::task_local! {
    /// Set on the worker task while it runs a chapter on a page from the
    /// [`PagePool`]; `Session` routes its navigation/eval calls there instead
    /// of the main page. (The CDP transport is already sessionId-tagged, so
    /// any number of targets can interleave over the one websocket.)
    static TASK_PAGE: Arc<Page>;
}

/// A bounded set of live browser page targets over the SAME multiplexed CDP
/// websocket. `acquire` reserves one of `cap` slots (semaphore) and opens a
/// page target; the returned [`PooledPage`] is released back into the pool
/// when dropped, so later workers reuse the target instead of churning new
/// ones.
pub struct PagePool {
    browser: Option<BrowserHandle>,
    slots: Arc<tokio::sync::Semaphore>,
    idle: Mutex<Vec<Arc<Page>>>,
}

impl PagePool {
    pub fn new(browser: &BrowserHandle, cap: usize) -> Arc<Self> {
        Arc::new(Self {
            browser: Some(browser.clone()),
            slots: Arc::new(tokio::sync::Semaphore::new(cap.max(1))),
            idle: Mutex::new(Vec::new()),
        })
    }

    /// Reserve a slot and hand out a page target.
    pub async fn acquire(self: &Arc<Self>) -> Result<PooledPage> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("page pool closed"))?;
        // Pop the idle slot first so the mutex is released before any
        // `.await` below (holding a std guard across await can stall).
        let idle_page = self.idle.lock().unwrap().pop();
        let page = match idle_page {
            Some(p) => p,
            None => {
                let fresh = self
                    .browser
                    .as_ref()
                    .ok_or_else(|| anyhow!("no browser: call start()/attach() first"))?
                    .new_page()
                    .await
                    .context("page pool: opening a page")?;
                let fresh = Arc::new(fresh);
                fresh.enable().await.context("enabling Page/Runtime")?;
                // Stealth init is document-scoped, so each pooled page needs
                // its own copy of the script (already true of the main page).
                fresh.add_init_script(STEALTH_JS).await.ok();
                fresh
            }
        };
        Ok(PooledPage {
            pool: self.clone(),
            page,
            _permit: permit,
        })
    }
}

/// A checked-out page from a [`PagePool`] (RAII: dropping returns the page).
pub struct PooledPage {
    pool: Arc<PagePool>,
    page: Arc<Page>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl PooledPage {
    pub fn page(&self) -> &Arc<Page> {
        &self.page
    }
}

impl Drop for PooledPage {
    fn drop(&mut self) {
        self.pool.idle.lock().unwrap().push(self.page.clone());
    }
}

/// Run `fut` with every [`Session`] call that touches the page routed to
/// `pool_page` — the worker's own task executes the future, which is what the
/// tokio task-local requires.
pub async fn run_on_page<F, T>(pool_page: Arc<Page>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    TASK_PAGE.scope(pool_page, fut).await
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub struct Session {
    browser: Option<BrowserHandle>,
    page: Option<Page>,
    client: Client,
    /// Kept so `Drop` can kill the browser we launched (never for `--cdp`).
    child: Mutex<Option<tokio::process::Child>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(child) = self.child.get_mut().unwrap().as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl Session {
    /// The page this call should act on: the task-local pooled page when the
    /// caller is a worker (`run_on_page`), else the session's main page.
    fn active_page(&self) -> Result<Arc<Page>> {
        if let Ok(p) = TASK_PAGE.try_with(Arc::clone) {
            return Ok(p);
        }
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("no page: call start()/attach() first"))?
            .clone();
        Ok(Arc::new(page))
    }

    fn http_client() -> Result<Client> {
        Ok(Client::builder()
            .timeout(Duration::from_secs(20))
            .default_headers(default_headers(None))
            .build()?)
    }

    /// Launch our own browser (headless unless `headed`), then open a page with
    /// the stealth init script and any cookies/saved session applied.
    pub async fn start(
        cookies: &[Cookie],
        storage_state: Option<&str>,
        headed: bool,
    ) -> Result<Self> {
        let bin = resolve_binary()?;
        let (ws_url, child) = launch(&bin, headed).await?;

        let cdp = Cdp::connect(&ws_url).await?;
        let browser = BrowserHandle { cdp: cdp.clone() };
        let page = browser.new_page().await?;
        page.enable().await.context("enabling Page/Runtime")?;

        // Cookies: saved session first, then explicit cookies.
        if let Some(path) = storage_state {
            let saved: StorageState = serde_json::from_str(
                &std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
            )?;
            browser.set_cookies(&saved.cookies).await.ok();
        }
        if !cookies.is_empty() {
            browser.set_cookies(cookies).await.ok();
        }
        page.add_init_script(STEALTH_JS).await.ok();

        Ok(Self {
            browser: Some(browser),
            page: Some(page),
            client: Self::http_client()?,
            child: Mutex::new(Some(child)),
        })
    }

    /// Attach to an already-running browser over CDP (the `--cdp` path).
    /// Never kills that browser.
    pub async fn attach(cdp_url: &str) -> Result<Self> {
        let base = cdp_url.trim_end_matches('/');
        let ver: Value = reqwest::Client::new()
            .get(format!("{base}/json/version"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .with_context(|| format!("cannot reach a browser at {cdp_url}"))?
            .error_for_status()
            .with_context(|| format!("cannot reach a browser at {cdp_url}"))?
            .json()
            .await
            .context("/json/version is not JSON")?;
        let ws_url = ver
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("{cdp_url}/json/version has no webSocketDebuggerUrl"))?
            .to_string();

        let cdp = Cdp::connect(&ws_url)
            .await
            .with_context(|| format!("cannot reach a browser at {cdp_url}"))?;
        let browser = BrowserHandle { cdp: cdp.clone() };
        let page = browser
            .new_page()
            .await
            .context("failed to open page in the attached browser")?;
        page.enable().await.context("enabling Page/Runtime")?;
        page.add_init_script(STEALTH_JS).await.ok();

        Ok(Self {
            browser: Some(browser),
            page: Some(page),
            client: Self::http_client()?,
            child: Mutex::new(None),
        })
    }

    /// Pool of browser pages over the session's own (multiplexed) CDP
    /// websocket, for the concurrent chapter workers of `extract`.
    pub fn page_pool(&self, cap: usize) -> Result<Arc<PagePool>> {
        let b = self.browser()?;
        Ok(PagePool::new(b, cap))
    }

    pub fn browser(&self) -> Result<&BrowserHandle> {
        self.browser
            .as_ref()
            .ok_or_else(|| anyhow!("no browser: call start()/attach() first"))
    }

    /// All cookies currently in the browser (name → value), for the HTTP path.
    pub async fn cookie_map(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Some(b) = &self.browser {
            if let Ok(cs) = b.cookies().await {
                for c in cs {
                    out.insert(c.name, c.value);
                }
            }
        }
        out
    }

    /// Persist cookies as a storage-state file (Playwright-compatible subset,
    /// so existing outputs/<site>_storage.json files stay readable).
    pub async fn save_state(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cookies = Vec::new();
        if let Some(b) = &self.browser {
            if let Ok(cs) = b.cookies().await {
                cookies = cs;
            }
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(&StorageState { cookies })?,
        )?;
        Ok(())
    }

    // --- navigation / DOM ------------------------------------------------

    pub async fn goto(&self, url: &str) -> Result<()> {
        self.active_page()?.navigate(url).await?;
        // Give the SPA a moment, like the Python version's 1.5s settle.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(())
    }

    pub async fn content(&self) -> Result<String> {
        self.active_page()?.content().await
    }

    pub async fn soup(&self, url: &str) -> Result<Html> {
        Ok(Html::parse_document(&self.fetch(url, None, &[]).await?))
    }

    pub async fn dom_soup(&self, url: &str, scroll: bool) -> Result<Html> {
        self.goto(url).await?;
        if scroll {
            self.scroll_to_bottom(60).await?;
        }
        Ok(Html::parse_document(&self.content().await?))
    }

    pub async fn scroll_to_bottom(&self, max_steps: usize) -> Result<()> {
        let js = r#"async (maxSteps) => {{
                let total = 0, lastH = 0, stuck = 0;
                const step = 300;
                for (let i = 0; i < maxSteps; i++) {{
                    window.scrollBy(0, step);
                    total += step;
                    const h = Math.max(document.body?.scrollHeight || 0,
                                       document.documentElement?.scrollHeight || 0);
                    if (h === lastH) stuck++; else stuck = 0;
                    lastH = h;
                    if (total >= h || h === 0 || stuck > 5) break;
                    await new Promise(r => setTimeout(r, 120));
                }}
                return true;
            }}"#
        .to_string();
        self.eval(
            &js,
            Some(serde_json::json!(max_steps)),
            &mut serde_json::Value::Null,
        )
        .await?;
        Ok(())
    }

    /// Evaluate a JS arrow function. `arg` is passed as a single JSON object.
    /// Mirrors session.go's Evaluate wrapper (JSON in, JSON out).
    pub async fn eval<T: serde::de::DeserializeOwned>(
        &self,
        script: &str,
        arg: Option<serde_json::Value>,
        out: &mut T,
    ) -> Result<()> {
        let wrapped = format!(
            "async (arg) => JSON.stringify(await ({script})(arg === undefined ? undefined : arg))"
        );
        // The arrow must be *invoked* in the same expression, otherwise V8
        // parses `async (arg) => …` as a statement and only hands us the
        // function object back.
        let decl = match arg {
            Some(v) => format!("({wrapped})({v})"),
            None => format!("({wrapped})(undefined)"),
        };
        let raw = self.active_page()?.evaluate_raw(decl.as_str()).await?;
        // A JSON string result needs one more decode pass.
        let text = match raw {
            Value::String(s) => s,
            other => other.to_string(),
        };
        if text == "null" {
            return Ok(());
        }
        *out = serde_json::from_str(&text).map_err(|e| {
            anyhow!(
                "eval decode failed: {e} (raw: {}…)",
                text.chars().take(120).collect::<String>()
            )
        })?;
        Ok(())
    }

    /// textContent of the first selector that matches.
    pub async fn text_of(&self, sel: &str) -> Result<String> {
        match self.active_page()?.inner(sel, "innerText").await? {
            Some(t) => Ok(t.trim().to_string()),
            None => Err(anyhow!("no element for {sel}")),
        }
    }

    pub async fn screenshot(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let png = self.active_page()?.screenshot_png().await?;
        std::fs::write(path, png)?;
        Ok(())
    }

    // --- plain HTTP (mirrors ggetHtmlDOM) -------------------------------

    pub async fn fetch(
        &self,
        url: &str,
        referer: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Result<String> {
        Ok(self.fetch_bytes(url, referer, extra).await?.text)
    }

    /// HTTP fetch carrying the live browser cookies. Binary payloads (SFACG
    /// VIP images) must not be decoded as text.
    pub async fn fetch_bytes(
        &self,
        url: &str,
        referer: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Result<Fetched> {
        let mut req = self.client.get(url);
        if let Some(r) = referer {
            req = req.header(REFERER, r);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let jar = self.cookie_map().await;
        if !jar.is_empty() {
            let mut s = String::new();
            for (k, v) in jar {
                s.push_str(&format!("{k}={v}; "));
            }
            req = req.header("cookie", s.trim_end_matches("; "));
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("HTTP {status} for {url}"));
        }
        let bytes = resp.bytes().await?;
        Ok(Fetched {
            text: String::from_utf8_lossy(&bytes).to_string(),
            bytes: bytes.to_vec(),
        })
    }
}

// ---------------------------------------------------------------------------
// Launch helpers
// ---------------------------------------------------------------------------

/// NOVEL_BROWSER → known Brave → PATH probe. Errors when nothing is found.
fn resolve_binary() -> Result<String> {
    let b = binary();
    if !b.is_empty() {
        return Ok(b);
    }
    for cand in CANDIDATE_BINARIES {
        if let Some(found) = which(cand) {
            return Ok(found);
        }
    }
    Err(anyhow!(
        "no browser binary found (set NOVEL_BROWSER; looked for {})",
        CANDIDATE_BINARIES.join(", ")
    ))
}

/// Minimal `which`: scan PATH for an executable file.
fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand.to_string_lossy().to_string());
        }
    }
    None
}

/// Spawn the browser and return (browser websocket URL, child).
///
/// A dedicated `--user-data-dir` is mandatory: without it Brave attaches to an
/// already-running instance and never opens its own debug port.
async fn launch(bin: &str, headed: bool) -> Result<(String, tokio::process::Child)> {
    // Pick a free port ourselves so we can also poll /json/version as a fallback.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").context("reserving a debug port")?;
        l.local_addr().context("local_addr")?.port()
    };

    let profile = std::env::temp_dir().join(format!(
        "novel-profile-{}-{}",
        std::process::id(),
        chrono_now()
    ));
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("creating profile dir {}", profile.display()))?;

    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-sandbox")
        .arg("--disable-dev-shm-usage")
        .arg("--window-size=1920,1080")
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--lang=zh-CN")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-timer-throttling")
        .arg("--disable-renderer-backgrounding");
    if !headed {
        cmd.arg("--headless=new");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to launch {bin}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("no stderr pipe on {bin}"))?;

    // Read stderr on a task: grab the real ws URL, keep draining afterwards so
    // the pipe never fills up and stalls the browser.
    let (tx, rx) = oneshot::channel::<String>();
    let tail = Arc::new(Mutex::new(String::new()));
    let tail_task = tail.clone();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut tx = Some(tx);
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("DevTools listening on") {
                let mut found = None;
                if let Some(rest) = line.split("DevTools listening on").nth(1) {
                    let url = rest.trim().trim_end_matches('.');
                    if url.starts_with("ws://") || url.starts_with("wss://") {
                        found = Some(url.to_string());
                    }
                }
                match (tx.take(), found) {
                    (Some(slot), Some(url)) => {
                        let _ = slot.send(url);
                    }
                    (slot, _) => tx = slot,
                }
            } else {
                let mut t = tail_task.lock().unwrap();
                t.push_str(&line);
                t.push('\n');
                if t.len() > 8192 {
                    let cut = t.len() - 4096;
                    *t = t[cut..].to_string();
                }
            }
        }
    });

    let from_stderr = match tokio::time::timeout(LAUNCH_TIMEOUT, rx).await {
        Ok(Ok(url)) => Ok(url),
        Ok(Err(_)) => Err(anyhow!("{bin} exited before printing its DevTools URL")),
        Err(_) => Err(anyhow!("{bin} never printed 'DevTools listening on …'")),
    };

    let ws_url = match from_stderr {
        Ok(url) => url,
        Err(e) => {
            // Fallback: the port we reserved answers /json/version anyway.
            let polled = poll_ws_url(port).await;
            let err_text = tail.lock().unwrap().clone();
            let _ = child.start_kill();
            if polled.is_none() {
                return Err(anyhow!(
                    "{e}\nbrowser stderr:\n{}",
                    if err_text.trim().is_empty() { "(empty)" } else { &err_text }
                ));
            }
            polled.expect("checked above")
        }
    };
    Ok((ws_url, child))
}

async fn poll_ws_url(port: u16) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let client = reqwest::Client::new();
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(url) = v.get("webSocketDebuggerUrl").and_then(|u| u.as_str()) {
                    return Some(url.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn default_headers(referer: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(USER_AGENT, HeaderValue::from_static(UA));
    h.insert(
        "accept-language",
        HeaderValue::from_static("zh-CN,zh;q=0.9"),
    );
    if let Some(r) = referer {
        if let Ok(v) = HeaderValue::from_str(r) {
            h.insert(REFERER, v);
        }
    }
    h
}

fn chrono_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// CDP `CookieParam` objects; empty domain/path are left to the browser.
fn cookie_params(cs: &[Cookie]) -> Vec<Value> {
    cs.iter()
        .map(|c| {
            let mut m = serde_json::Map::new();
            m.insert("name".into(), json!(c.name));
            m.insert("value".into(), json!(c.value));
            if !c.domain.is_empty() {
                m.insert("domain".into(), json!(c.domain));
            }
            if !c.path.is_empty() {
                m.insert("path".into(), json!(c.path));
            }
            Value::Object(m)
        })
        .collect()
}

/// Compile a CSS selector, panicking on bad syntax at development time.
pub fn selector(css: &str) -> Selector {
    Selector::parse(css).unwrap_or_else(|e| panic!("bad selector {css:?}: {e}"))
}