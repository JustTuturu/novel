//! OCR-text cleaner — port of internal/cleaner/cleaner.go (which itself
//! ported src/novel/cleaner.py). Rust's regex crate has no lookarounds, so the
//! Go version's guard-capture rewrites are reused: match the surrounding
//! characters, rewrite with "$1$2".

use regex::Regex;
use std::sync::OnceLock;

const CJK: &str = "\u{3400}-\u{4dbf}\u{4e00}-\u{9fff}\u{f900}-\u{faff}";

fn chapter_title_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new("第[一二三四五六七八九十百千零〇两0-9]+章").unwrap())
}

fn footer_anchors() -> &'static Vec<Regex> {
    static V: OnceLock<Vec<Regex>> = OnceLock::new();
    V.get_or_init(|| {
        vec![
            Regex::new(r"[-·—]?作家说").unwrap(),
            Regex::new(r"上起点中文网支持我").unwrap(),
            Regex::new(r"下载\s*(?:App|APP)").unwrap(),
            Regex::new(r"扫一扫[，,]?手机接着读").unwrap(),
            Regex::new(r"目录\s*下一章").unwrap(),
        ]
    })
}

/// (pattern, replacement) pairs applied in order by normalise_lines.
fn normalisers() -> &'static Vec<(Regex, &'static str)> {
    static V: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    V.get_or_init(|| {
        let cjk = CJK;
        let mut guards: Vec<(String, &'static str)> = Vec::new();
        // Original: (?<![cjk])目录(?![cjk])  -> keep only the label off.
        for label in ["目录", "投票", "设置"] {
            guards.push((
                format!("([^{cjk}]){label}([^{cjk}])"),
                "$1$2",
            ));
        }
        // Original: (?<![cjk])夜间(?![cjk，。！？、])
        guards.push((format!("([^{cjk}])夜间([^cjk，。！？、])"), "$1$2"));
        let mut v: Vec<(Regex, &'static str)> = vec![
            (Regex::new(r"讨论热烈[^\n]*").unwrap(), ""),
            (Regex::new(r"[0-9]+字[0-9]{4}年[0-9]{2}月[0-9]{2}日[^\n]*").unwrap(), ""),
            (Regex::new(r"本作品由起点中文网[^\n]*").unwrap(), ""),
            (Regex::new(r"@版权所有[^\n]*").unwrap(), ""),
            (Regex::new(r"(?m)^书友[0-9]+[^\n]*").unwrap(), ""),
            (
                Regex::new(r"书详情|在书架|加书架|客户端|回书架|反馈|指南|举报|旧版").unwrap(),
                "",
            ),
        ];
        v.extend(guards.into_iter().map(|(p, r)| (Regex::new(&p).unwrap(), r)));
        v.extend([
            (Regex::new(r"(?m)^[:：三□✓√BCSQ②日]+\s*").unwrap(), ""),
            (
                Regex::new(&format!(r"([^{cjk}])\s+[□✓√BCSQ②]\s+([^{cjk}])")).unwrap(),
                "$1 $2",
            ),
            (
                Regex::new(&format!(
                    r"([{cjk}。！？…」])\s+[□✓√BCSQ②]\s*"
                ))
                .unwrap(),
                "$1 ",
            ),
            (
                Regex::new(r"[①-⑳⑴-⒇⒈-⒛㉑-㉟㊱-㊿]").unwrap(),
                "",
            ),
            (Regex::new(r"(?m)[0-9]{1,3}$").unwrap(), ""),
            (Regex::new(r"(?m)^[0-9]{1,3}\s*$").unwrap(), ""),
        ]);
        v
    })
}

fn collapse_spaces() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[ \t]{2,}").unwrap())
}

pub struct TextCleaner;

impl TextCleaner {
    pub fn clean(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut t = text.replace("\r\n", "\n").replace('\r', "\n");
        t = trim_to_chapter(&t);
        if let Some(i) = t.find("·作家说") {
            t.truncate(i);
        }
        t = trim_footer(&t);
        t = t
            .replace(['\u{feff}', '\u{200b}'], "")
            .replace(['\u{a0}', '\u{3000}', '\t'], " ");
        normalise_lines(&t)
    }
}

fn trim_to_chapter(text: &str) -> String {
    let Some(m) = chapter_title_re().find(text) else {
        return text.to_string();
    };
    let start = m.start();
    match text[..start].rfind('\n') {
        Some(i) => text[i + 1..].to_string(),
        None => text[start..].to_string(),
    }
}

fn trim_footer(text: &str) -> String {
    let positions: Vec<usize> = footer_anchors()
        .iter()
        .filter_map(|re| re.find(text).map(|m| m.start()))
        .collect();
    match positions.iter().min() {
        Some(&i) => text[..i].to_string(),
        None => text.to_string(),
    }
}

fn normalise_lines(text: &str) -> String {
    let mut t = text.to_string();
    for (re, rep) in normalisers() {
        t = re.replace_all(&t, *rep).into_owned();
    }
    let mut lines: Vec<String> = Vec::new();
    for line in t.split('\n') {
        let line = collapse_spaces().replace_all(line, " ").trim().to_string();
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines.join("\n").trim().to_string()
}
