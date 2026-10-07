//! OCR client — talks to the Python sidecar (sidecar/ocr_service.py), which
//! wraps the EasyOCR/PaddleOCR engine. Port of internal/ocr/client.go.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_URL: &str = "http://127.0.0.1:8765";

pub struct Client {
    pub base_url: String,
    http: reqwest::Client,
    child: Option<Child>,
}

impl Client {
    pub fn new(url: Option<&str>) -> Self {
        let base_url = url
            .map(|s| s.trim_end_matches('/').to_string())
            .or_else(|| std::env::var("NOVEL_OCR_URL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        Self {
            base_url,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
            child: None,
        }
    }

    pub async fn healthy(&self) -> bool {
        match self.http.get(format!("{}/health", self.base_url)).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// Spawn the sidecar once if nothing answers on the port.
    pub async fn ensure_up(&mut self) -> Result<()> {
        if self.healthy().await {
            return Ok(());
        }
        if self.child.is_some() {
            anyhow::bail!(
                "OCR sidecar at {} is not responding. Check .ocr_sidecar.log.",
                self.base_url
            );
        }
        let root = repo_root()?;
        let py = root.join(".venv/bin/python");
        let py = if py.exists() {
            py
        } else {
            PathBuf::from("python3")
        };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(".ocr_sidecar.log")
            .ok();
        let out = log
            .as_ref()
            .map(|l| Stdio::from(l.try_clone().unwrap()))
            .unwrap_or_else(|| Stdio::null());
        let err = log
            .as_ref()
            .map(|l| Stdio::from(l.try_clone().unwrap()))
            .unwrap_or_else(|| Stdio::null());
        let child = Command::new(&py)
            .arg(root.join("sidecar/ocr_service.py"))
            .current_dir(&root)
            .stdout(out)
            .stderr(err)
            .spawn()
            .with_context(|| format!("spawning sidecar with {}", py.display()))?;
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if self.healthy().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(anyhow!(
            "OCR sidecar did not come up at {} (log: .ocr_sidecar.log)",
            self.base_url
        ))
    }

    /// OCR one image file; `join` joins detected lines ("" = no separator).
    pub async fn image_text(&mut self, path: &Path, join: &str) -> Result<String> {
        self.ensure_up().await?;
        let body = serde_json::json!({
            "path": path.to_string_lossy(),
            "join": join,
        });
        let resp = self
            .http
            .post(format!("{}/ocr", self.base_url))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}/ocr", self.base_url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OCR sidecar returned {status}: {detail}"));
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// OCR every image in a folder, natural (numeric-aware) order.
    pub async fn process_folder(&mut self, dir: &Path) -> Result<Vec<(PathBuf, String)>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("reading folder {}", dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()),
                    Some(ref e)
                        if matches!(e.as_str(), "png" | "jpg" | "jpeg" | "bmp" | "tiff" | "gif" | "webp")
                )
            })
            .collect();
        files.sort_by_key(|p| natural_key(&p.to_string_lossy()));
        let mut out = Vec::new();
        for f in files {
            println!("Processing {}...", f.display());
            let text = self.image_text(&f, "\n").await?;
            out.push((f, text));
        }
        Ok(out)
    }
}

/// Split digit runs so "img2.png" sorts before "img10.png".
fn natural_key(s: &str) -> Vec<u64> {
    let mut keys = Vec::new();
    let mut cur = String::new();
    let mut in_digits = false;
    for c in s.chars() {
        let is_digit = c.is_ascii_digit();
        if is_digit != in_digits && !cur.is_empty() {
            keys.push(if in_digits { cur.parse().unwrap_or(0) } else { 0 });
            cur.clear();
        }
        in_digits = is_digit;
        cur.push(c);
    }
    if !cur.is_empty() {
        keys.push(if in_digits { cur.parse().unwrap_or(0) } else { 0 });
    }
    keys
}

/// Walk up from cwd looking for go.mod (the Go port's repoRoot analogue).
fn repo_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("sidecar/ocr_service.py").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(anyhow!(
                "cannot locate the novel repo root (no sidecar/ocr_service.py above {})",
                std::env::current_dir()?.display()
            ));
        }
    }
}
