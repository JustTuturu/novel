#!/usr/bin/env bash
# Fetch the ONNX models the OCR engine (oar-ocr) needs.
#
# The models are NOT bundled with the repo and are NOT downloaded at runtime:
# run this script once, then the engine reads them from the model directory.
#
#   ./scripts/fetch_ocr_models.sh
#   OAR_MODEL_DIR=/somewhere ./scripts/fetch_ocr_models.sh
#
# NOTE ON MODEL VERSION: the user asked for PP-OCRv7, but no PP-OCRv7 ONNX
# export exists anywhere yet (verified against the PaddlePaddle Hugging Face
# org, ModelScope and the PaddleOCR release list — see README "OCR").
# This script provisions the newest verifiable PaddleOCR ONNX pair,
# PP-OCRv6 *medium*, plus its character dictionary.
#
# Upstream: PaddleOCR PP-OCRv6 medium det/rec ONNX exports, mirrored by the
# oar-ocr project on ModelScope (owner `greatv`, repo `oar-ocr`, branch
# `master`). The same files are published by PaddlePaddle on Hugging Face as
# `PaddlePaddle/PP-OCRv6_{medium_det,medium_rec}_onnx` / `inference.onnx` with
# identical sizes; the ModelScope copies carry the hashes oar-ocr itself
# verifies, so they are the canonical source here.
set -euo pipefail

MODEL_DIR="${OAR_MODEL_DIR:-outputs/oar_models}"
BASE="https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath="

mkdir -p "$MODEL_DIR"

# name|remote path|expected sha256|expected size in bytes
FILES=(
  "ppocrv6_det.onnx|pp-ocrv6_medium_det.onnx|eb13b44b25bb36f89528b68720af8a61d9cf381176107f465db1757b65d086e1|62032837"
  "ppocrv6_rec.onnx|pp-ocrv6_medium_rec.onnx|9c09abf0957f7968c7586464b7397b84ad2387a0497a351af40e9acc71b673ba|76554979"
  "ppocrv6_dict.txt|ppocrv6_dict.txt|b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d|74947"
)

for entry in "${FILES[@]}"; do
  IFS='|' read -r name remote sha size <<<"$entry"
  dest="$MODEL_DIR/$name"

  if [[ -f "$dest" ]]; then
    have_sha=$(sha256sum "$dest" | cut -d' ' -f1)
    if [[ "$have_sha" == "$sha" ]]; then
      echo "ok       $dest (already present, sha256 matches)"
      continue
    fi
    echo "stale    $dest (sha256 $have_sha, expected $sha) — re-downloading"
  fi

  echo "fetching $remote -> $dest"
  curl --fail --location --silent --show-error -o "$dest.part" "${BASE}${remote}"
  mv "$dest.part" "$dest"

  have_size=$(stat -c %s "$dest" 2>/dev/null || stat -f %z "$dest")
  have_sha=$(sha256sum "$dest" | cut -d' ' -f1)
  if [[ "$have_size" != "$size" ]]; then
    echo "error    $dest: size $have_size, expected $size" >&2
    exit 1
  fi
  if [[ "$have_sha" != "$sha" ]]; then
    echo "error    $dest: sha256 $have_sha, expected $sha" >&2
    exit 1
  fi
  echo "verified $dest ($have_size bytes, sha256 $sha)"
done

cat <<EOF

Models ready in $MODEL_DIR
  ppocrv6_det.onnx    PP-OCRv6 medium text detection
  ppocrv6_rec.onnx    PP-OCRv6 medium text recognition
  ppocrv6_dict.txt    character dictionary (18708 entries)

Run:  novel ocr screenshots
EOF
