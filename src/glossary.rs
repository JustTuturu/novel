//! Self-updating translation glossary — SQLite (rusqlite, bundled).
//!
//! WRITE-DEFAULT with zero manual curation: every translation call returns a
//! structured list of the proper nouns it handled, and each entry is upserted
//! into `outputs/glossary.db` keyed on `hanzi`, **first-writer-wins** — an
//! existing entry is never overwritten, so a name's translation pins on first
//! sight and stays consistent afterwards. Before each call, the top-k entries
//! relevant to the chunk (importance-weighted character overlap; embeddings
//! are NOT needed for v1) are prepended to the request.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::Path;
use std::sync::Mutex;

/// Where the glossary lives (relative to cwd).
pub const DB_PATH: &str = "outputs/glossary.db";

/// Maximum entries admitted from a single API response — the model is
/// best-effort, so a runaway list must not be able to bloat the DB.
const MAX_BATCH: usize = 64;

/// Proper nouns are short; anything longer than this is a sentence the model
/// misclassified, not a glossary term.
const MAX_HANZI_CHARS: usize = 12;

/// A stored glossary entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub hanzi: String,
    pub pinyin: String,
    pub role: String,
    pub importance: i64,
    pub first_seen: String,
}

/// A proper noun as reported by the model (best-effort, pre-sanitize).
#[derive(Debug, Clone, Deserialize)]
pub struct NewEntry {
    #[serde(default)]
    pub hanzi: String,
    #[serde(default)]
    pub pinyin: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub importance: i64,
}

pub struct Glossary {
    /// rusqlite is not `Sync`; the DB sees only short, serialized critical
    /// sections (one upsert batch / one retrieval), so a plain std mutex is
    /// the right guard — worker tasks just briefly block each other.
    conn: Mutex<Connection>,
}

impl Glossary {
    /// Opens (creating if needed) the glossary DB at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening glossary db {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS glossary (
               id          INTEGER PRIMARY KEY AUTOINCREMENT,
               hanzi       TEXT    NOT NULL UNIQUE,
               pinyin      TEXT    NOT NULL DEFAULT '',
               role        TEXT    NOT NULL DEFAULT '',
               importance  INTEGER NOT NULL DEFAULT 1,
               first_seen  TEXT    NOT NULL DEFAULT '',
               created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
               updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .context("creating glossary table")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// First-writer-wins upsert: inserts entries that don't exist yet and
    /// leaves existing ones (keyed on `hanzi`) untouched. Returns how many
    /// rows were created.
    pub fn admit(&self, entries: &[NewEntry], first_seen: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut created = 0;
        for e in sanitize(entries) {
            let n = conn
                .execute(
                    "INSERT INTO glossary (hanzi, pinyin, role, importance, first_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(hanzi) DO NOTHING",
                    rusqlite::params![e.hanzi, e.pinyin, e.role, e.importance, first_seen],
                )
                .context("glossary upsert")?;
            created += n;
        }
        Ok(created)
    }

    /// Top-k entries relevant to `chunk`: character overlap with the chunk,
    /// weighted by importance. Protagonist-level entries (importance 3) are
    /// always candidates even without an overlap in this chunk.
    ///
    /// score = 2 * min(overlap, 10) + importance, overlap > 0 required unless
    /// importance == 3.
    pub fn relevant(&self, chunk: &str, k: usize) -> Result<Vec<Entry>> {
        let all = self.all()?;
        let mut scored: Vec<(i64, Entry)> = all
            .into_iter()
            .filter_map(|e| {
                let overlap = chunk.matches(&e.hanzi).count();
                if overlap == 0 && e.importance < 3 {
                    return None;
                }
                let score = 2 * overlap.min(10) as i64 + e.importance;
                Some((score, e))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.importance.cmp(&a.1.importance))
                .then_with(|| a.1.hanzi.cmp(&b.1.hanzi))
        });
        Ok(scored.into_iter().map(|(_, e)| e).take(k).collect())
    }

    /// Every entry, highest importance first (insertion order within a tier).
    pub fn all(&self) -> Result<Vec<Entry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, hanzi, pinyin, role, importance, first_seen
                 FROM glossary
                 ORDER BY importance DESC, id ASC",
            )
            .context("preparing glossary query")?;
        let rows = stmt
            .query_map([], row_to_entry)
            .context("querying glossary")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("reading glossary rows")
    }

    /// Number of stored entries.
    pub fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM glossary", [], |r| {
            r.get(0)
        })?)
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: r.get(0)?,
        hanzi: r.get(1)?,
        pinyin: r.get(2)?,
        role: r.get(3)?,
        importance: r.get(4)?,
        first_seen: r.get(5)?,
    })
}

/// Drops unusable entries and clamps the rest: non-empty short `hanzi`,
/// importance pinned to 1..=3, batch capped at [`MAX_BATCH`], duplicates
/// within the batch collapsed (first wins).
fn sanitize(entries: &[NewEntry]) -> Vec<NewEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in entries {
        let hanzi = e.hanzi.trim().to_string();
        if hanzi.is_empty() || hanzi.chars().count() > MAX_HANZI_CHARS || !seen.insert(hanzi.clone())
        {
            continue;
        }
        out.push(NewEntry {
            hanzi,
            pinyin: e.pinyin.trim().to_string(),
            role: e.role.trim().to_string(),
            importance: e.importance.clamp(1, 3),
        });
        if out.len() >= MAX_BATCH {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "novel_glossary_test_{}_{}_{}.db",
            std::process::id(),
            tag,
            n
        ))
    }

    fn entry(hanzi: &str, pinyin: &str, importance: i64) -> NewEntry {
        NewEntry {
            hanzi: hanzi.into(),
            pinyin: pinyin.into(),
            role: String::new(),
            importance,
        }
    }

    #[test]
    fn sanitize_clamps_and_dedupes_and_drops_junk() {
        let raw = vec![
            entry("  墨画 ", "mo hoa", 9),     // trimmed + clamped to 3
            entry("", "x", 2),                 // empty hanzi dropped
            entry("一二三四五六七八九十十一", "y", 2), // 12 chars, kept
            entry("一二三四五六七八九十十一十二", "y", 2), // 14 chars dropped
            entry("墨画", "dup", 1),           // duplicate dropped (first wins)
        ];
        let out = sanitize(&raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].hanzi, "墨画");
        assert_eq!(out[0].importance, 3);
        assert_eq!(out[1].hanzi.chars().count(), 12);
    }

    #[test]
    fn admit_is_first_writer_wins() {
        let db = temp_db("fww");
        let g = Glossary::open(&db).unwrap();
        assert_eq!(
            g.admit(&[entry("安小富", "an tieu phu", 3)], "book#0001")
                .unwrap(),
            1
        );
        // Same hanzi, different everything -> ignored.
        assert_eq!(
            g.admit(&[entry("安小富", "different", 1)], "book#0002")
                .unwrap(),
            0
        );
        let all = g.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].pinyin, "an tieu phu");
        assert_eq!(all[0].importance, 3);
        assert_eq!(all[0].first_seen, "book#0001");
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn relevant_prefers_chunk_overlap_and_always_keeps_protagonists() {
        let db = temp_db("rel");
        let g = Glossary::open(&db).unwrap();
        g.admit(
            &[
                entry("王林", "vuong lam", 1),
                entry("墨画", "mo hoa", 1),
                entry("青云宗", "kinh van ton", 2),
                entry("主角甲", "chu giao giap", 3), // protagonist, no overlap
            ],
            "book#0001",
        )
        .unwrap();
        let hits = g.relevant("墨画走进青云宗的山门", 10).unwrap();
        let hanzi: Vec<&str> = hits.iter().map(|e| e.hanzi.as_str()).collect();
        assert!(hanzi.contains(&"墨画"));
        assert!(hanzi.contains(&"青云宗"));
        assert!(hanzi.contains(&"主角甲"), "protagonists always surface");
        assert!(!hanzi.contains(&"王林"));
        // Overlap first, protagonist after, and nothing else with k=2.
        let top2 = g.relevant("墨画走进青云宗的山门", 2).unwrap();
        assert_eq!(top2[0].hanzi, "青云宗");
        assert_eq!(top2[1].hanzi, "主角甲");
        std::fs::remove_file(&db).ok();
    }
}
