//! novel — download Chinese web novels from Qidian and SFACG.
//!
//! The Go implementation was the previous version; this Rust tree replaces it.
//! Site rules are ported from 404-novel-project/novel-downloader. OCR runs
//! in-process via oar-ocr (PP-OCRv6 ONNX models).

mod browser;
mod cleaner;
mod cli;
mod config;
mod glossary;
mod ocr;
mod rule;
mod text;
mod translate;

mod qidian;
mod sfacg;

use clap::Parser;

#[tokio::main]
async fn main() {
    qidian::register();
    sfacg::register();

    let cli = cli::Cli::parse();
    if let Err(e) = cli::run(cli).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
