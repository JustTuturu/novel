//! Text utilities — port of internal/text/text.go.

use regex::Regex;
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::sync::OnceLock;

fn pua_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new("[\u{e000}-\u{f8ff}]").unwrap())
}

fn single_cjk_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            "^[\u{3400}-\u{4dbf}\u{4e00}-\u{9fff}\u{f900}-\u{faff}\u{2018}\u{2019}\u{201c}\u{201d}\u{3000}\u{3001}\u{3002}\u{ff0c}\u{ff01}\u{ff1a}\u{ff1b}\u{ff08}\u{ff09}\u{2026}]$",
        )
        .unwrap()
    })
}

fn noise_patterns() -> &'static Vec<Regex> {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            Regex::new(r"[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]").unwrap(),
            Regex::new(r"(?m)^\d{1,3}\s*$").unwrap(),
            Regex::new(
                r"(?m)^(目录|书详情|加书架|投票|夜间|设置|客户端|在书架|反馈|指南|举报|旧版|回书架|书友圈)\s*$",
            )
            .unwrap(),
            Regex::new(r"本作品由起点中文网[^\n]*").unwrap(),
            Regex::new(r"@版权所有[^\n]*").unwrap(),
            Regex::new(r"·作家说[\s\S]*$").unwrap(),
            Regex::new(r"讨论热烈[^\n]*").unwrap(),
            Regex::new(r"(?m)^书友\d+[^\n]*").unwrap(),
            Regex::new(r"请下载(?:起点)?(?:中文网)?[^\n]*客户端[^\n]*").unwrap(),
            Regex::new(r"(?m)^.*本章字数：\d+字.*$").unwrap(),
            Regex::new(r"(?m)^.*更新时间：[\d\-\s:]+\d+.*$").unwrap(),
            Regex::new(r"(?m)^谷[\u{4e00}-\u{9fa5}]{0,1}$").unwrap(),
        ]
    })
}

fn collapse_blank_lines() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\n{3,}").unwrap())
}

/// Recover plaintext from attribute-based font obfuscation (parity with Go;
/// the live rules use text_content + glyph maps).
pub fn decrypt_html(html: &str) -> String {
    let doc = Html::parse_document(html);
    let root = doc.tree.root();
    decrypt_node(root)
}

fn decrypt_node(node: ego_tree::NodeRef<'_, scraper::Node>) -> String {
    match node.value() {
        scraper::Node::Text(t) => t.text.to_string(),
        scraper::Node::Element(el) => {
            if el.name().eq_ignore_ascii_case("br") {
                return "\n".to_string();
            }
            // A single-CJK attribute value is the real character; the visible
            // text is the font-mapped decoy and must be dropped.
            for (_name, value) in el.attrs() {
                if single_cjk_re().is_match(value) {
                    return value.to_string();
                }
            }
            node.children().map(decrypt_node).collect()
        }
        _ => node.children().map(decrypt_node).collect(),
    }
}

pub fn clean_text(text: &str, extra: &[&Regex]) -> String {
    let mut out = text.to_string();
    for re in extra.iter().copied().chain(noise_patterns().iter()) {
        out = re.replace_all(&out, "").into_owned();
    }
    out = collapse_blank_lines().replace_all(&out, "\n\n").into_owned();
    out.trim().to_string()
}

pub fn pua_ratio(text: &str) -> f64 {
    if text.is_empty() {
        return 1.0;
    }
    pua_re().find_iter(text).count() as f64 / text.chars().count() as f64
}

/// PUA codepoint range test.
pub fn is_pua(c: char) -> bool {
    let cp = c as u32;
    (0xE000..=0xF8FF).contains(&cp)
}

/// Sorted, unique PUA characters present in `text`.
pub fn pua_chars(text: &str) -> Vec<char> {
    let mut set: Vec<char> = Vec::new();
    for c in text.chars() {
        if is_pua(c) && !set.contains(&c) {
            set.push(c);
        }
    }
    set.sort_unstable();
    set
}

pub fn replace_map(text: &str, map: &HashMap<char, char>) -> String {
    text.chars().map(|c| *map.get(&c).unwrap_or(&c)).collect()
}

/// Text of the first match of `sel`, joined with `sep` (goquery's get_text).
pub fn text_of(doc: &Html, sel: &Selector, sep: &str) -> String {
    // ElementRef::text() yields the descendant text nodes, which is exactly
    // goquery's get_text(sep).
    doc.select(sel)
        .next()
        .map(|el| {
            el.text()
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(sep)
        })
        .unwrap_or_default()
}

pub fn text_of_first(doc: &Html, sel: &str, sep: &str) -> String {
    match Selector::parse(sel) {
        Ok(s) => text_of(doc, &s, sep),
        Err(_) => String::new(),
    }
}


/// Visible text of a subtree, block boundaries as newlines (get_text("\n")).
pub fn block_text(node: ego_tree::NodeRef<'_, scraper::Node>) -> String {
    const BLOCK: &[&str] = &[
        "p", "div", "br", "h1", "h2", "h3", "h4", "h5", "h6", "li", "tr", "section",
        "article", "blockquote", "pre",
    ];
    match node.value() {
        scraper::Node::Text(t) => t.text.to_string(),
        scraper::Node::Element(el) => {
            if el.name().eq_ignore_ascii_case("br") {
                return "\n".to_string();
            }
            let mut out: String = node.children().map(block_text).collect();
            if BLOCK.contains(&el.name().to_lowercase().as_str()) && !out.ends_with('\n') {
                out.push('\n');
            }
            out
        }
        _ => node.children().map(block_text).collect(),
    }
}

pub fn text_content(node: ego_tree::NodeRef<'_, scraper::Node>) -> String {
    match node.value() {
        scraper::Node::Text(t) => t.text.to_string(),
        _ => node.children().map(text_content).collect(),
    }
}
