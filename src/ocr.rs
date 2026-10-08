//! OCR client — the single entry point for every OCR caller.
//!
//! One engine: the `oar-ocr` PP-OCRv6 ONNX pipeline, running in-process, so
//! no Python and no sidecar service is involved.
//!
//! The contract that `src/qidian.rs` relies on: `image_text(path, join)`
//! returns the detected strings joined by `join`, **in reading order**.
//! `join = ""` keeps the glyphs of a row adjacent, and `src/qidian.rs` indexes
//! that string positionally, so both the ordering and the count of detections
//! are part of the contract.

use anyhow::{anyhow, bail, Context, Result};
use oar_ocr::domain::TextDetectionConfig;
use oar_ocr::processors::LimitType;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default model directory (relative to cwd).
pub const DEFAULT_NATIVE_MODEL_DIR: &str = "outputs/oar_models";

/// Model file names expected inside [`model_dir`].
const DET_MODEL: &str = "ppocrv6_det.onnx";
const REC_MODEL: &str = "ppocrv6_rec.onnx";
const DICT_FILE: &str = "ppocrv6_dict.txt";

pub struct Client {
    /// Built on first use and kept for the rest of the process run — loading
    /// the ONNX models is the expensive part, so it must not happen per image.
    ///
    /// `predict` takes `&self` and the pipeline is `Send + Sync`, so the engine
    /// stays in the struct and only the image crosses into the blocking thread.
    engine: Option<Arc<oar_ocr::oarocr::OAROCR>>,
}

impl Client {
    pub fn new() -> Self {
        Self { engine: None }
    }

    /// OCR one image file; `join` joins detected lines ("" = no separator).
    ///
    /// Returns the detections in reading order (top-to-bottom, then
    /// left-to-right) joined by `join`.
    pub async fn image_text(&mut self, path: &Path, join: &str) -> Result<String> {
        let path = path.to_path_buf();
        let join = join.to_string();
        if self.engine.is_none() {
            self.engine = Some(Arc::new(build_engine()?));
        }
        let engine = Arc::clone(
            self.engine
                .as_ref()
                .expect("engine was just built and never cleared"),
        );
        // ONNX Runtime inference is blocking, so it goes to a blocking thread.
        // The engine is shared by reference and stays alive in `self`, so the
        // models are loaded once per process, not once per image.
        let regions = tokio::task::spawn_blocking(move || detect(&engine, &path))
            .await
            .context("OCR worker panicked")??;
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
fn detect(engine: &oar_ocr::oarocr::OAROCR, path: &Path) -> Result<Vec<Detection>> {
    let image = oar_ocr::utils::load_image(path)
        .with_context(|| format!("loading image {}", path.display()))?;
    let results = engine
        .predict(vec![image])
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("OCR of {}", path.display()))?;
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

/// Load the models, or fail with the exact paths and how to get them.
///
/// Nothing is downloaded here: `scripts/fetch_ocr_models.sh` is the one
/// documented provisioning step, and a missing model must never silently
/// resolve to different weights.
fn build_engine() -> Result<oar_ocr::oarocr::OAROCR> {
    use oar_ocr::oarocr::OAROCRBuilder;

    let dir = model_dir();
    if !dir.is_dir() {
        bail!(
            "OCR model directory {} does not exist.\n\
             Create it and fetch the models first:\n    \
             OAR_MODEL_DIR={} ./scripts/fetch_ocr_models.sh\n\
             (expects {} , {} and {} — see README \"OCR\").",
            dir.display(),
            dir.display(),
            DET_MODEL,
            REC_MODEL,
            DICT_FILE
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
            "OCR models missing in {}:\n  {}\n\
             Fetch them (one explicit step, needs network):\n    \
             OAR_MODEL_DIR={} ./scripts/fetch_ocr_models.sh",
            dir.display(),
            missing.join("\n  "),
            dir.display()
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
                "loading OCR models from {} (det={}, rec={}, dict={})",
                dir.display(),
                DET_MODEL,
                REC_MODEL,
                DICT_FILE
            )
        })?;
    println!("OCR engine ready (oar-ocr, {}, {}).", DET_MODEL, REC_MODEL);
    Ok(engine)
}

/// `OAR_MODEL_DIR`, else `outputs/oar_models`.
pub fn model_dir() -> PathBuf {
    std::env::var_os("OAR_MODEL_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_NATIVE_MODEL_DIR))
}

/// ORDERING RULE — the one thing that must not drift, because `src/qidian.rs`
/// indexes the joined string positionally.
///
/// PaddleOCR-style engines return `rec_texts` in reading order, and we
/// re-establish it here explicitly rather than trusting the engine's internal
/// vector order:
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

/// DETECTION SETTINGS — pinned, verified against `paddlex` 3.7 running
/// `PP-OCRv6_medium_det` on the test screenshot (instrumented run):
///
/// * `limit_side_len = 64`, `limit_type = min` — paddlex resolves exactly these
///   for every PP-OCRv6 det model. The library's own default is
///   `min(960)`, which downscales a 1920x5511 page to a fraction of its size
///   and silently loses whole paragraphs (measured: 81 of 131 lines found);
///   with `min(64)` the engine finds ~131.
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

    fn det(x: f32, y: f32, _w: f32, h: f32, text: &str) -> Detection {
        Detection {
            min_x: x,
            min_y: y,
            max_y: y + h,
            text: text.to_string(),
        }
    }

    fn bbox(points: &[(f32, f32)]) -> oar_ocr::processors::BoundingBox {
        oar_ocr::processors::BoundingBox {
            points: points
                .iter()
                .map(|&(x, y)| oar_ocr::processors::Point { x, y })
                .collect(),
        }
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

    #[test]
    fn line_tolerance_is_half_the_median_height() {
        let dets = vec![det(0.0, 0.0, 1.0, 20.0, ""), det(0.0, 0.0, 1.0, 30.0, "")];
        assert_eq!(line_tolerance(&dets), 15.0);
    }

    #[test]
    fn line_tolerance_floors_at_min_pixels() {
        // Median height 8 -> half = 4, but the floor is 10.
        let dets = vec![det(0.0, 0.0, 1.0, 8.0, "")];
        assert_eq!(line_tolerance(&dets), MIN_LINE_TOLERANCE);
        assert_eq!(line_tolerance(&[]), MIN_LINE_TOLERANCE);
    }

    #[test]
    fn box_bounds_takes_polygon_extremes() {
        let b = bbox(&[(3.0, 1.0), (9.0, 2.0), (5.0, 7.0)]);
        assert_eq!(box_bounds(&b), (3.0, 1.0, 9.0, 7.0));
    }
}
