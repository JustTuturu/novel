//! OCR client — the single entry point for every OCR caller.
//!
//! Two interchangeable backends, selected at runtime:
//!
//! * [`Backend::Sidecar`] (DEFAULT) talks to the Python sidecar
//!   (`sidecar/ocr_service.py`) over localhost HTTP. Unchanged behaviour.
//! * [`Backend::Native`] runs the `rapidocr-core` ONNX pipeline in-process, so
//!   no Python and no sidecar process is needed.
//!
//! Both backends honour the same contract that `src/qidian.rs` relies on:
//! `image_text(path, join)` returns the detected strings joined by `join`,
//! **in reading order**. `join = ""` keeps the glyphs of a row adjacent, and
//! `src/qidian.rs` indexes that string positionally, so both the ordering and
//! the count of detections are part of the contract.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_URL: &str = "http://127.0.0.1:8765";

/// Default model directory for the native backend (relative to cwd).
pub const DEFAULT_NATIVE_MODEL_DIR: &str = "outputs/rapidocr_models";

/// Which OCR engine does the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Python sidecar wrapping PaddleOCR. The default.
    Sidecar,
    /// In-process `rapidocr-core` (ONNX Runtime).
    Native,
}

impl Backend {
    /// Parses a backend name (`sidecar` | `native`), case-insensitively.
    ///
    /// Unknown values are a hard error: a silent fallback to the sidecar would
    /// hide a typo behind "it suddenly got slow" or "it needs Python again".
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sidecar" => Ok(Backend::Sidecar),
            "native" => Ok(Backend::Native),
            other => Err(anyhow!(
                "unknown OCR backend {other:?}; choose from sidecar, native"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Sidecar => "sidecar",
            Backend::Native => "native",
        }
    }
}

/// Backend precedence: explicit argument, else `NOVEL_OCR_BACKEND`, else
/// [`Backend::Sidecar`].
pub fn resolve_backend(explicit: Option<&str>) -> Result<Backend> {
    match explicit {
        Some(v) => Backend::parse(v),
        None => match std::env::var("NOVEL_OCR_BACKEND") {
            Ok(v) if !v.trim().is_empty() => Backend::parse(&v),
            _ => Ok(Backend::Sidecar),
        },
    }
}

pub struct Client {
    pub base_url: String,
    backend: Backend,
    http: reqwest::Client,
    child: Option<Child>,
    /// Built on first use by the native backend and kept for the rest of the
    /// process run — loading the ONNX models is the expensive part, so it must
    /// not happen per image.
    native: Option<rapidocr_core::RapidOcr>,
}

impl Client {
    /// Backed by the default backend (sidecar, or `NOVEL_OCR_BACKEND`).
    pub fn new(url: Option<&str>) -> Self {
        let backend = resolve_backend(None).unwrap_or(Backend::Sidecar);
        Self::with_backend(url, backend)
    }

    /// Backed by an explicitly chosen backend.
    pub fn with_backend(url: Option<&str>, backend: Backend) -> Self {
        let base_url = url
            .map(|s| s.trim_end_matches('/').to_string())
            .or_else(|| std::env::var("NOVEL_OCR_URL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        Self {
            base_url,
            backend,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
            child: None,
            native: None,
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub async fn healthy(&self) -> bool {
        if self.backend == Backend::Native {
            return true; // nothing to be up: it runs in-process
        }
        match self.http.get(format!("{}/health", self.base_url)).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// Spawn the sidecar once if nothing answers on the port.
    pub async fn ensure_up(&mut self) -> Result<()> {
        if self.backend == Backend::Native {
            return Ok(());
        }
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
    ///
    /// Returns the detections in reading order (top-to-bottom, then
    /// left-to-right) joined by `join` — same contract for both backends.
    pub async fn image_text(&mut self, path: &Path, join: &str) -> Result<String> {
        match self.backend {
            Backend::Sidecar => self.sidecar_image_text(path, join).await,
            Backend::Native => self.native_image_text(path, join).await,
        }
    }

    /// Sidecar request. Behaviour is byte-identical to before the native
    /// backend existed.
    async fn sidecar_image_text(&mut self, path: &Path, join: &str) -> Result<String> {
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

    /// In-process OCR via `rapidocr-core`.
    async fn native_image_text(&mut self, path: &Path, join: &str) -> Result<String> {
        let path = path.to_path_buf();
        let join = join.to_string();
        // The ONNX runner is blocking and stateful (`&mut self`), so it goes to
        // a blocking thread with ownership moved in and back out — that keeps
        // the loaded models alive for the next image instead of reloading.
        let engine = self.native.take();
        let (engine, result) = tokio::task::spawn_blocking(move || {
            let mut built = match engine {
                Some(e) => Ok(e),
                None => build_native_engine(),
            };
            // On init failure the error already carries the offline hint; keep it.
            let result = match built.as_mut() {
                Ok(e) => e
                    .run_path(&path)
                    .with_context(|| format!("native OCR of {}", path.display())),
                Err(e) => Err(anyhow!("{e:#}")),
            };
            (built, result)
        })
        .await
        .context("native OCR worker panicked")?;
        self.native = engine.ok();
        Ok(join_detections(result?, &join))
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

/// Download (first run only) and load the native models.
fn build_native_engine() -> Result<rapidocr_core::RapidOcr> {
    use rapidocr_core::config::PipelineConfig;
    use rapidocr_core::model::{model_set_by_name, ModelCache, ModelDownloadMode};

    let dir = native_model_dir();
    let pipeline = PipelineConfig::without_cls();
    let cache = ModelCache::new(&dir);

    let set_name = std::env::var("RAPIDOCR_MODEL_SET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| rapidocr_core::model::DEFAULT_MODEL_SET_NAME.to_string());
    let model_set = model_set_by_name(&set_name)
        .ok_or_else(|| anyhow!("unknown RapidOCR model set {set_name:?}"))?;

    if !cache.missing_assets_for_pipeline(model_set, pipeline).is_empty() {
        println!(
            "Preparing native OCR models in {} (first run downloads them; needs network)…",
            dir.display()
        );
    }
    cache
        .ensure_model_set_for_pipeline(model_set, pipeline, ModelDownloadMode::Missing)
        .with_context(|| format!("preparing native OCR models in {}", dir.display()))
        .map_err(|e| {
            // Most likely cause by far: no network on first run.
            anyhow!(
                "{e:#}\nnative OCR models unavailable; run once with network or use --ocr-backend sidecar"
            )
        })?;

    let cfg = cache.config_for(model_set).with_pipeline(pipeline);
    let engine = rapidocr_core::RapidOcr::from_config(cfg).context("loading native OCR models")?;
    println!("Native OCR backend ready ({}).", model_set.name);
    Ok(engine)
}

/// `RAPIDOCR_MODEL_DIR`, else `outputs/rapidocr_models`.
pub fn native_model_dir() -> PathBuf {
    std::env::var_os("RAPIDOCR_MODEL_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_NATIVE_MODEL_DIR))
}

/// ORDERING RULE — the one thing that must not drift from the sidecar.
///
/// PaddleOCR returns `rec_texts` in reading order, and `src/qidian.rs`
/// indexes the joined string positionally, so we re-establish reading order
/// here explicitly rather than trusting the engine's internal vector order:
///
/// 1. sort by the top edge (min y) of each detection box, top to bottom;
/// 2. group consecutive detections into a text line while they stay within
///    `LINE_TOLERANCE` (= half the median box height, at least 10px) of the
///    group's first box — a relative tolerance, so it works for both a
///    full-page screenshot and a single 30-glyph row;
/// 3. sort each line left to right by the box's left edge (min x).
///
/// Sorting is stable, so detections that tie keep engine order.
const MIN_LINE_TOLERANCE: f32 = 10.0;

/// Join the detections of one image with `join`, in reading order.
fn join_detections(output: rapidocr_core::types::OcrOutput, join: &str) -> String {
    reading_order(output.lines)
        .into_iter()
        .map(|l| l.text)
        .collect::<Vec<_>>()
        .join(join)
}

/// Put detections into reading order: top-to-bottom, then left-to-right.
fn reading_order(lines: Vec<rapidocr_core::types::OcrLine>) -> Vec<rapidocr_core::types::OcrLine> {
    let tolerance = line_tolerance(&lines);
    let keys: Vec<(f32, f32)> = lines
        .iter()
        .map(|l| {
            let (min_x, min_y, _, _) = box_bounds(&l.bbox);
            (min_y, min_x)
        })
        .collect();
    let mut idx: Vec<usize> = (0..lines.len()).collect();
    idx.sort_by(|&a, &b| {
        keys[a]
            .0
            .total_cmp(&keys[b].0)
            .then_with(|| keys[a].1.total_cmp(&keys[b].1))
    });

    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < idx.len() {
        let anchor_y = keys[idx[i]].0;
        let mut j = i + 1;
        while j < idx.len() && keys[idx[j]].0 - anchor_y <= tolerance {
            j += 1;
        }
        let mut group = idx[i..j].to_vec();
        group.sort_by(|&a, &b| {
            keys[a]
                .1
                .total_cmp(&keys[b].1)
                .then_with(|| keys[a].0.total_cmp(&keys[b].0))
        });
        out.extend(group.into_iter().map(|k| lines[k].clone()));
        i = j;
    }
    out
}

/// Half the median box height, floored at [`MIN_LINE_TOLERANCE`] pixels.
fn line_tolerance(lines: &[rapidocr_core::types::OcrLine]) -> f32 {
    let mut heights: Vec<f32> = lines
        .iter()
        .map(|l| {
            let (_, min_y, _, max_y) = box_bounds(&l.bbox);
            (max_y - min_y).max(1.0)
        })
        .collect();
    if heights.is_empty() {
        return MIN_LINE_TOLERANCE;
    }
    heights.sort_by(|a, b| a.total_cmp(b));
    let median = heights[heights.len() / 2];
    (median / 2.0).max(MIN_LINE_TOLERANCE)
}

/// (min_x, min_y, max_x, max_y) of a quad.
fn box_bounds(quad: &rapidocr_core::types::Quad) -> (f32, f32, f32, f32) {
    let mut bounds = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &quad.points {
        bounds.0 = bounds.0.min(p[0]);
        bounds.1 = bounds.1.min(p[1]);
        bounds.2 = bounds.2.max(p[0]);
        bounds.3 = bounds.3.max(p[1]);
    }
    bounds
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

#[cfg(test)]
mod tests {
    use super::*;
    use rapidocr_core::types::{OcrLine, Quad};

    fn line(x: f32, y: f32, w: f32, h: f32, text: &str) -> OcrLine {
        OcrLine {
            bbox: Quad::from_xyxy(x, y, x + w, y + h),
            text: text.to_string(),
            score: 0.9,
        }
    }

    #[test]
    fn backend_parse_accepts_both_names() {
        assert_eq!(Backend::parse("sidecar").unwrap(), Backend::Sidecar);
        assert_eq!(Backend::parse("native").unwrap(), Backend::Native);
        assert_eq!(Backend::parse("  NATIVE ").unwrap(), Backend::Native);
    }

    #[test]
    fn backend_parse_rejects_unknown_values() {
        let err = Backend::parse("paddle").unwrap_err().to_string();
        assert!(err.contains("sidecar, native"), "{err}");
    }

    #[test]
    fn resolve_backend_defaults_to_sidecar() {
        // No NOVEL_OCR_BACKEND set in the test process unless inherited.
        if std::env::var("NOVEL_OCR_BACKEND").is_err() {
            assert_eq!(resolve_backend(None).unwrap(), Backend::Sidecar);
        }
        assert_eq!(resolve_backend(Some("native")).unwrap(), Backend::Native);
    }

    #[test]
    fn reading_order_is_top_to_bottom_then_left_to_right() {
        // Deliberately shuffled: engine order is not reading order.
        let lines = vec![
            line(200.0, 100.0, 30.0, 30.0, "c"),
            line(100.0, 10.0, 30.0, 30.0, "b"),
            line(10.0, 12.0, 30.0, 30.0, "a"),
            line(10.0, 100.0, 30.0, 30.0, "d"),
        ];
        // Row 1 (y≈10): a(10) b(100). Row 2 (y=100): d(10) c(200).
        let text = join_detections(
            rapidocr_core::types::OcrOutput { lines },
            "",
        );
        assert_eq!(text, "abdc");
    }

    #[test]
    fn join_with_newline_keeps_one_entry_per_detection() {
        let lines = vec![
            line(10.0, 10.0, 30.0, 30.0, "一"),
            line(10.0, 60.0, 30.0, 30.0, "二"),
        ];
        let text = join_detections(
            rapidocr_core::types::OcrOutput { lines },
            "\n",
        );
        assert_eq!(text, "一\n二");
    }

    #[test]
    fn natural_key_orders_digit_runs_numerically() {
        assert!(natural_key("img2.png") < natural_key("img10.png"));
    }
}