//! OCR client — the single entry point for every OCR caller.
//!
//! Two interchangeable backends, selected at runtime:
//!
//! * [`Backend::Sidecar`] (DEFAULT) talks to the Python sidecar
//!   (`sidecar/ocr_service.py`) over localhost HTTP. Unchanged behaviour.
//! * [`Backend::Native`] runs the `oar-ocr` PP-OCR ONNX pipeline in-process, so
//!   no Python and no sidecar process is needed.
//!
//! Both backends honour the same contract that `src/qidian.rs` relies on:
//! `image_text(path, join)` returns the detected strings joined by `join`,
//! **in reading order**. `join = ""` keeps the glyphs of a row adjacent, and
//! `src/qidian.rs` indexes that string positionally, so both the ordering and
//! the count of detections are part of the contract.

use anyhow::{anyhow, bail, Context, Result};
use oar_ocr::domain::TextDetectionConfig;
use oar_ocr::processors::LimitType;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_URL: &str = "http://127.0.0.1:8765";

/// Default model directory for the native backend (relative to cwd).
pub const DEFAULT_NATIVE_MODEL_DIR: &str = "outputs/oar_models";

/// Model file names expected inside [`native_model_dir`].
const DET_MODEL: &str = "ppocrv6_det.onnx";
const REC_MODEL: &str = "ppocrv6_rec.onnx";
const DICT_FILE: &str = "ppocrv6_dict.txt";

/// Which OCR engine does the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Python sidecar wrapping PaddleOCR. The default.
    Sidecar,
    /// In-process `oar-ocr` (ONNX Runtime).
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
    ///
    /// `predict` takes `&self` and the pipeline is `Send + Sync`, so the engine
    /// stays in the struct and only the image crosses into the blocking thread.
    native: Option<Arc<oar_ocr::oarocr::OAROCR>>,
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

    /// In-process OCR via `oar-ocr`.
    async fn native_image_text(&mut self, path: &Path, join: &str) -> Result<String> {
        let path = path.to_path_buf();
        let join = join.to_string();
        if self.native.is_none() {
            self.native = Some(Arc::new(build_native_engine()?));
        }
        let engine = Arc::clone(
            self.native
                .as_ref()
                .expect("native engine was just built and never cleared"),
        );
        // ONNX Runtime inference is blocking, so it goes to a blocking thread.
        // The engine is shared by reference and stays alive in `self`, so the
        // models are loaded once per process, not once per image.
        let regions = tokio::task::spawn_blocking(move || native_detect(&engine, &path))
            .await
            .context("native OCR worker panicked")??;
        Ok(join_regions(regions, &join))
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

/// One recognized text region, flattened for the ordering helpers.
#[derive(Clone)]
struct Detection {
    min_x: f32,
    min_y: f32,
    max_y: f32,
    text: String,
}

/// Run the oar-ocr pipeline over one image and flatten the detections.
fn native_detect(engine: &oar_ocr::oarocr::OAROCR, path: &Path) -> Result<Vec<Detection>> {
    let image = oar_ocr::utils::load_image(path)
        .with_context(|| format!("loading image {}", path.display()))?;
    let results = engine
        .predict(vec![image])
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("native OCR of {}", path.display()))?;
    let result = results
        .first()
        .ok_or_else(|| anyhow!("oar-ocr returned no result for {}", path.display()))?;
    Ok(result
        .text_regions
        .iter()
        .filter_map(|region| {
            let text = region.text.as_ref()?;
            let (min_x, min_y, _, max_y) = box_bounds(&region.bounding_box);
            Some(Detection {
                min_x,
                min_y,
                max_y,
                text: text.to_string(),
            })
        })
        .collect())
}

/// Load the native models, or fail with the exact paths and how to get them.
///
/// Nothing is downloaded here: `scripts/fetch_ocr_models.sh` is the one
/// documented provisioning step, and a missing model must never silently
/// resolve to a different engine's weights.
fn build_native_engine() -> Result<oar_ocr::oarocr::OAROCR> {
    use oar_ocr::oarocr::OAROCRBuilder;

    let dir = native_model_dir();
    if !dir.is_dir() {
        bail!(
            "native OCR model directory {} does not exist.\n\
             Create it and fetch the models first:\n    \
             OAR_MODEL_DIR={} ./scripts/fetch_ocr_models.sh\n\
             (expects {} , {} and {} — see README \"OCR backends\").\n\
             Or run with the default backend: --ocr-backend sidecar",
            dir.display(),
            dir.display(),
            DET_MODEL,
            REC_MODEL,
            DICT_FILE,
        );
    }

    let det = dir.join(DET_MODEL);
    let rec = dir.join(REC_MODEL);
    let dict = dir.join(DICT_FILE);
    let mut missing = Vec::new();
    for p in [&det, &rec, &dict] {
        if !p.is_file() {
            missing.push(p.display().to_string());
        }
    }
    if !missing.is_empty() {
        bail!(
            "native OCR models missing in {}:\n  {}\n\
             Fetch them (one explicit step, needs network):\n    \
             OAR_MODEL_DIR={} ./scripts/fetch_ocr_models.sh\n\
             Or run with the default backend: --ocr-backend sidecar",
            dir.display(),
            missing.join("\n  "),
            dir.display(),
        );
    }

    let engine = OAROCRBuilder::new(&det, &rec, &dict)
        .image_batch_size(1)
        .region_batch_size(4)
        .text_detection_config(PP_OCR_DET_CONFIG)
        .build()
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| {
            format!(
                "loading native OCR models from {} (det={}, rec={}, dict={})",
                dir.display(),
                DET_MODEL,
                REC_MODEL,
                DICT_FILE
            )
        })?;
    println!(
        "Native OCR backend ready (oar-ocr, {}, {}).",
        DET_MODEL, REC_MODEL
    );
    Ok(engine)
}

/// `OAR_MODEL_DIR`, else `outputs/oar_models`.
pub fn native_model_dir() -> PathBuf {
    std::env::var_os("OAR_MODEL_DIR")
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

/// DETECTION SETTINGS — tuned to match what the Python sidecar actually runs,
/// so `native` finds the same lines on the same image.
///
/// Verified against `paddlex` 3.7 running `PP-OCRv6_medium_det` on the test
/// screenshot (instrumented run):
///
/// * `limit_side_len = 64`, `limit_type = min` — paddlex resolves exactly these
///   for every PP-OCRv6 det model. The library's own default is
///   `min(960)`, which downscales a 1920x5511 page to a fraction of its size
///   and silently loses whole paragraphs (measured: 81 of 131 lines found);
///   with `min(64)` both engines find ~131.
/// * `score_threshold 0.2 / box_threshold 0.45 / unclip 1.4 / 3000 candidates`
///   — the `PostProcess` block of the model's own `inference.yml`.
/// * `max_side_len = None` keeps paddlex's 4000px cap, which the library also
///   defaults to.
const PP_OCR_DET_CONFIG: TextDetectionConfig = TextDetectionConfig {
    score_threshold: 0.2,
    box_threshold: 0.45,
    unclip_ratio: 1.4,
    max_candidates: 3000,
    limit_side_len: Some(64),
    limit_type: Some(LimitType::Min),
    max_side_len: None,
};

/// Join the detections of one image with `join`, in reading order.
fn join_regions(detections: Vec<Detection>, join: &str) -> String {
    reading_order(detections)
        .into_iter()
        .map(|d| d.text)
        .collect::<Vec<_>>()
        .join(join)
}

/// Put detections into reading order: top-to-bottom, then left-to-right.
fn reading_order(detections: Vec<Detection>) -> Vec<Detection> {
    let tolerance = line_tolerance(&detections);
    let keys: Vec<(f32, f32)> = detections
        .iter()
        .map(|d| (d.min_y, d.min_x))
        .collect();
    let mut idx: Vec<usize> = (0..detections.len()).collect();
    idx.sort_by(|&a, &b| {
        keys[a]
            .0
            .total_cmp(&keys[b].0)
            .then_with(|| keys[a].1.total_cmp(&keys[b].1))
    });

    let mut out = Vec::with_capacity(detections.len());
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
        out.extend(group.into_iter().map(|k| detections[k].clone()));
        i = j;
    }
    out
}

/// Half the median box height, floored at [`MIN_LINE_TOLERANCE`] pixels.
fn line_tolerance(detections: &[Detection]) -> f32 {
    let mut heights: Vec<f32> = detections
        .iter()
        .map(|d| (d.max_y - d.min_y).max(1.0))
        .collect();
    if heights.is_empty() {
        return MIN_LINE_TOLERANCE;
    }
    heights.sort_by(|a, b| a.total_cmp(b));
    let median = heights[heights.len() / 2];
    (median / 2.0).max(MIN_LINE_TOLERANCE)
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

/// (min_x, min_y, max_x, max_y) of a detection polygon.
fn box_bounds(bbox: &oar_ocr::processors::BoundingBox) -> (f32, f32, f32, f32) {
    let mut bounds = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &bbox.points {
        bounds.0 = bounds.0.min(p.x);
        bounds.1 = bounds.1.min(p.y);
        bounds.2 = bounds.2.max(p.x);
        bounds.3 = bounds.3.max(p.y);
    }
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(x: f32, y: f32, w: f32, h: f32, text: &str) -> Detection {
        Detection {
            min_x: x,
            min_y: y,
            max_y: y + h,
            text: text.to_string(),
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
        let detections = vec![
            det(200.0, 100.0, 30.0, 30.0, "c"),
            det(100.0, 10.0, 30.0, 30.0, "b"),
            det(10.0, 12.0, 30.0, 30.0, "a"),
            det(10.0, 100.0, 30.0, 30.0, "d"),
        ];
        // Row 1 (y≈10): a(10) b(100). Row 2 (y=100): d(10) c(200).
        let text = join_regions(detections, "");
        assert_eq!(text, "abdc");
    }

    #[test]
    fn join_with_newline_keeps_one_entry_per_detection() {
        let detections = vec![
            det(10.0, 10.0, 30.0, 30.0, "一"),
            det(10.0, 60.0, 30.0, 30.0, "二"),
        ];
        let text = join_regions(detections, "\n");
        assert_eq!(text, "一\n二");
    }

    #[test]
    fn natural_key_orders_digit_runs_numerically() {
        assert!(natural_key("img2.png") < natural_key("img10.png"));
    }

    }