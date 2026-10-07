//! zh -> vi translation via the public Google Translate endpoint.
//! Port of internal/translate/translate.go (src/novel/translate.py).

use anyhow::{Context, Result};
use regex::Regex;
use std::sync::OnceLock;
use std::time::Duration;

/// Ordered so longer aliases are swapped first.
fn name_map() -> &'static [(&'static str, &'static str)] {
    &[
        ("安小富", "An Xiaofu"),
        ("安少爷", "An Xiaofu"),
        ("小胖子", "An Xiaofu"),
        ("钱少爷", "Qian Xing"),
        ("钱公子", "Qian Xing"),
        ("瘦公子", "Qian Xing"),
        ("钱兴", "Qian Xing"),
        ("墨画", "Mo Hua"),
        ("钱家", "nha ho Qian"),
        ("安家", "nha An"),
    ]
}

fn ideographic_space_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\b[0-9]+\u{3000}\u{3000}").unwrap())
}

pub struct Translator {
    pub src: String,
    pub dest: String,
    pub delay: Duration,
    http: reqwest::Client,
}

impl Translator {
    pub fn new() -> Self {
        Self {
            src: "zh-cn".to_string(),
            dest: "vi".to_string(),
            delay: Duration::from_millis(500),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn gt(&self, text: &str) -> Result<String> {
        let url = format!(
            "https://translate.googleapis.com/translate_a/single\
             ?q={}&sl={}&tl={}&dt=t&ie=UTF-8&oe=UTF-8&client=gtx",
            urlencode(text),
            self.src,
            self.dest
        );
        let v: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .context("translate request failed")?
            .json()
            .await
            .context("translate response was not JSON")?;
        let parts = v
            .get(0)
            .and_then(|p| p.as_array())
            .context("unexpected translate payload")?;
        let mut out = String::new();
        for p in parts {
            if let Some(s) = p.get(0).and_then(|s| s.as_str()) {
                out.push_str(s);
            }
        }
        Ok(out)
    }

    fn protect(&self, text: &str) -> (String, Vec<(String, String)>) {
        let mut out = text.to_string();
        let mut rev = Vec::new();
        for (i, (cn, pinyin)) in name_map().iter().enumerate() {
            let ph = format!("\u{a7}N{i}\u{a7}");
            out = out.replace(cn, &ph);
            rev.push((ph, pinyin.to_string()));
        }
        (out, rev)
    }

    fn restore(&self, text: &str, rev: &[(String, String)]) -> String {
        let mut out = text.to_string();
        for (ph, pinyin) in rev {
            out = out.replace(ph, pinyin);
        }
        out
    }

    pub async fn paragraph(&self, para: &str) -> String {
        let (safe, rev) = self.protect(para);
        match self.gt(&safe).await {
            Ok(t) => self.restore(&t, &rev),
            Err(e) => {
                println!("  ERROR: {e}");
                para.to_string()
            }
        }
    }

    pub async fn file(&self, input: &str, output: &str) -> Result<()> {
        let raw = std::fs::read_to_string(input)
            .with_context(|| format!("reading {input}"))?;
        let raw = ideographic_space_re().replace_all(&raw, "").to_string();
        let lines: Vec<&str> = raw.split('\n').collect();
        let total = lines.len();
        let mut out: Vec<String> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                out.push(String::new());
                continue;
            }
            if line.starts_with("# ") {
                out.push((*line).to_string());
                continue;
            }
            out.push(self.paragraph(line).await);
            out.push(String::new());
            if (i + 1) % 5 == 0 || i == total - 1 {
                println!("  [{}/{}] lines done", i + 1, total);
            }
            tokio::time::sleep(self.delay).await;
        }

        let result = out.join("\n").trim().to_string();
        std::fs::write(output, result)?;
        println!("\nSaved to: {output}");
        Ok(())
    }
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
