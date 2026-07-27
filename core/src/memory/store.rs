//! SQLite-backed memory store: per-game profile card, session episodes,
//! and atomic facts with a Mem0-style ADD/UPDATE/NOOP gate and hybrid
//! (FTS5 BM25 ∪ embedding cosine) retrieval merged by reciprocal-rank fusion.

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use super::embed::{cosine, Embedder};

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub struct MemoryStore {
    conn: Connection,
    embedder: Box<dyn Embedder>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FactGateDecision {
    Add,
    Update(i64),
    Noop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrievedFact {
    pub id: i64,
    pub text: String,
    pub score: f64,
}

/// Gate thresholds (cosine similarity against the nearest existing fact).
const NOOP_SIMILARITY: f32 = 0.98;
const UPDATE_SIMILARITY: f32 = 0.82;
/// Reciprocal-rank-fusion constant (standard k=60).
const RRF_K: f64 = 60.0;

impl MemoryStore {
    pub fn open_in_memory(embedder: Box<dyn Embedder>) -> Result<Self, MemoryError> {
        Self::init(Connection::open_in_memory()?, embedder)
    }

    pub fn open(path: &str, embedder: Box<dyn Embedder>) -> Result<Self, MemoryError> {
        Self::init(Connection::open(path)?, embedder)
    }

    fn init(conn: Connection, embedder: Box<dyn Embedder>) -> Result<Self, MemoryError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS games(
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                exe TEXT,
                steam_appid INTEGER,
                wiki_base_url TEXT
            );
            CREATE TABLE IF NOT EXISTS profiles(
                game_id INTEGER PRIMARY KEY REFERENCES games(id),
                markdown TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS episodes(
                id INTEGER PRIMARY KEY,
                game_id INTEGER NOT NULL REFERENCES games(id),
                started_at INTEGER NOT NULL,
                ended_at INTEGER NOT NULL,
                summary TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS facts(
                id INTEGER PRIMARY KEY,
                game_id INTEGER NOT NULL REFERENCES games(id),
                kind TEXT NOT NULL DEFAULT 'note',
                text TEXT NOT NULL,
                embedding BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(text, fact_id UNINDEXED);
            "#,
        )?;
        Ok(Self { conn, embedder })
    }

    pub fn get_or_create_game(&self, name: &str, exe: Option<&str>) -> Result<i64, MemoryError> {
        if let Some(id) = self
            .conn
            .query_row("SELECT id FROM games WHERE name = ?1", params![name], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO games(name, exe) VALUES(?1, ?2)",
            params![name, exe],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_profile(&self, game_id: i64, markdown: &str, now: u64) -> Result<(), MemoryError> {
        self.conn.execute(
            "INSERT INTO profiles(game_id, markdown, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(game_id) DO UPDATE SET markdown = ?2, updated_at = ?3",
            params![game_id, markdown, now as i64],
        )?;
        Ok(())
    }

    pub fn get_profile(&self, game_id: i64) -> Result<Option<String>, MemoryError> {
        Ok(self
            .conn
            .query_row(
                "SELECT markdown FROM profiles WHERE game_id = ?1",
                params![game_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn add_episode(
        &self,
        game_id: i64,
        started_at: u64,
        ended_at: u64,
        summary: &str,
    ) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO episodes(game_id, started_at, ended_at, summary) VALUES(?1, ?2, ?3, ?4)",
            params![game_id, started_at as i64, ended_at as i64, summary],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn latest_episode(&self, game_id: i64) -> Result<Option<String>, MemoryError> {
        Ok(self
            .conn
            .query_row(
                "SELECT summary FROM episodes WHERE game_id = ?1 ORDER BY ended_at DESC LIMIT 1",
                params![game_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Mem0-style gate: compare a candidate fact against existing ones and
    /// decide ADD / UPDATE(nearest) / NOOP. (v1 upgrades this to an LLM gate;
    /// the deterministic version keeps duplicates out from day one.)
    pub fn gate_fact(&self, game_id: i64, text: &str) -> Result<FactGateDecision, MemoryError> {
        let candidate = self.embedder.embed(text);
        let mut stmt = self
            .conn
            .prepare("SELECT id, text, embedding FROM facts WHERE game_id = ?1")?;
        let rows = stmt.query_map(params![game_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut best: Option<(i64, f32)> = None;
        for row in rows {
            let (id, existing_text, blob) = row?;
            if existing_text.trim().eq_ignore_ascii_case(text.trim()) {
                return Ok(FactGateDecision::Noop);
            }
            let sim = cosine(&candidate, &blob_to_vec(&blob));
            if best.map(|(_, b)| sim > b).unwrap_or(true) {
                best = Some((id, sim));
            }
        }
        Ok(match best {
            Some((_, sim)) if sim >= NOOP_SIMILARITY => FactGateDecision::Noop,
            Some((id, sim)) if sim >= UPDATE_SIMILARITY => FactGateDecision::Update(id),
            _ => FactGateDecision::Add,
        })
    }

    /// Apply the gate and persist. Returns the decision that was applied.
    pub fn remember(
        &mut self,
        game_id: i64,
        text: &str,
        kind: &str,
        now: u64,
    ) -> Result<FactGateDecision, MemoryError> {
        let decision = self.gate_fact(game_id, text)?;
        let emb = vec_to_blob(&self.embedder.embed(text));
        match &decision {
            FactGateDecision::Add => {
                self.conn.execute(
                    "INSERT INTO facts(game_id, kind, text, embedding, created_at, updated_at)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
                    params![game_id, kind, text, emb, now as i64],
                )?;
                let id = self.conn.last_insert_rowid();
                self.conn.execute(
                    "INSERT INTO facts_fts(text, fact_id) VALUES(?1, ?2)",
                    params![text, id],
                )?;
            }
            FactGateDecision::Update(id) => {
                self.conn.execute(
                    "UPDATE facts SET text = ?1, embedding = ?2, updated_at = ?3 WHERE id = ?4",
                    params![text, emb, now as i64, id],
                )?;
                self.conn
                    .execute("DELETE FROM facts_fts WHERE fact_id = ?1", params![id])?;
                self.conn.execute(
                    "INSERT INTO facts_fts(text, fact_id) VALUES(?1, ?2)",
                    params![text, id],
                )?;
            }
            FactGateDecision::Noop => {}
        }
        Ok(decision)
    }

    pub fn fact_count(&self, game_id: i64) -> Result<i64, MemoryError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM facts WHERE game_id = ?1",
            params![game_id],
            |r| r.get(0),
        )?)
    }

    /// Hybrid retrieval: FTS5 BM25 ranking ∪ embedding cosine ranking,
    /// merged with reciprocal-rank fusion, scoped to one game.
    pub fn retrieve(
        &self,
        game_id: i64,
        query: &str,
        k: usize,
    ) -> Result<Vec<RetrievedFact>, MemoryError> {
        use std::collections::HashMap;
        let mut rrf: HashMap<i64, f64> = HashMap::new();

        // Keyword leg.
        let match_expr = fts_query(query);
        if !match_expr.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT f.id FROM facts_fts
                 JOIN facts f ON f.id = facts_fts.fact_id
                 WHERE facts_fts MATCH ?1 AND f.game_id = ?2
                 ORDER BY bm25(facts_fts) LIMIT 20",
            )?;
            let ids = stmt.query_map(params![match_expr, game_id], |r| r.get::<_, i64>(0))?;
            for (rank, id) in ids.enumerate() {
                *rrf.entry(id?).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }

        // Embedding leg (brute-force cosine — fine at desktop scale).
        let q = self.embedder.embed(query);
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding FROM facts WHERE game_id = ?1")?;
        let rows = stmt.query_map(params![game_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut scored: Vec<(i64, f32)> = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            scored.push((id, cosine(&q, &blob_to_vec(&blob))));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (id, sim)) in scored.iter().take(20).enumerate() {
            if *sim > 0.05 {
                *rrf.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }

        let mut merged: Vec<(i64, f64)> = rrf.into_iter().collect();
        merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(k);

        let mut out = Vec::with_capacity(merged.len());
        for (id, score) in merged {
            let text: String = self.conn.query_row(
                "SELECT text FROM facts WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            out.push(RetrievedFact { id, text, score });
        }
        Ok(out)
    }
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Sanitize free text into an FTS5 OR-query over its alphanumeric words.
fn fts_query(query: &str) -> String {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(|w| format!("\"{}\"", w.to_lowercase()))
        .collect();
    words.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::embed::HashEmbedder;

    fn store() -> MemoryStore {
        MemoryStore::open_in_memory(Box::new(HashEmbedder::default())).expect("open store")
    }

    #[test]
    fn fts5_is_available_in_bundled_sqlite() {
        // Store construction creates an fts5 virtual table; reaching here means it worked.
        let s = store();
        let g = s.get_or_create_game("Probe", None).unwrap();
        assert!(g > 0);
    }

    #[test]
    fn profile_roundtrip_and_game_identity() {
        let s = store();
        let g1 = s.get_or_create_game("Elden Ring", Some("eldenring.exe")).unwrap();
        let g2 = s.get_or_create_game("Elden Ring", None).unwrap();
        assert_eq!(g1, g2);
        s.set_profile(g1, "## Build\ndex katana", 100).unwrap();
        s.set_profile(g1, "## Build\ndex katana lvl 34", 200).unwrap();
        assert_eq!(s.get_profile(g1).unwrap().unwrap(), "## Build\ndex katana lvl 34");
        assert_eq!(s.get_profile(999).unwrap(), None);
    }

    #[test]
    fn fact_gate_add_update_noop() {
        let mut s = store();
        let g = s.get_or_create_game("Palworld", None).unwrap();

        // New fact → Add.
        let d = s.remember(g, "player's base is at the volcano foothills", "note", 1).unwrap();
        assert_eq!(d, FactGateDecision::Add);

        // Exact duplicate (case-insensitive) → Noop.
        let d = s.remember(g, "Player's base is at the volcano foothills", "note", 2).unwrap();
        assert_eq!(d, FactGateDecision::Noop);

        // Near-duplicate phrasing → Update of the existing fact, not a new row.
        let d = s
            .remember(g, "player's base is at the volcano foothills area", "note", 3)
            .unwrap();
        assert!(matches!(d, FactGateDecision::Update(_)), "got {d:?}");
        assert_eq!(s.fact_count(g).unwrap(), 1);

        // Unrelated fact → Add.
        let d = s.remember(g, "prefers not to hear story spoilers", "pref", 4).unwrap();
        assert_eq!(d, FactGateDecision::Add);
        assert_eq!(s.fact_count(g).unwrap(), 2);
    }

    #[test]
    fn hybrid_retrieval_finds_keyword_and_scopes_by_game() {
        let mut s = store();
        let pal = s.get_or_create_game("Palworld", None).unwrap();
        let er = s.get_or_create_game("Elden Ring", None).unwrap();
        s.remember(pal, "Anubis is the player's strongest fighting pal", "note", 1).unwrap();
        s.remember(pal, "base needs more coal for weapon production", "goal", 2).unwrap();
        s.remember(er, "died six times to Margit at the castle gate", "event", 3).unwrap();

        let hits = s.retrieve(pal, "which pal fights best", 3).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].text.contains("Anubis"), "top hit was {:?}", hits[0]);
        // Elden Ring facts must never leak into Palworld retrieval.
        assert!(hits.iter().all(|h| !h.text.contains("Margit")));
    }

    #[test]
    fn episodes_latest_wins() {
        let s = store();
        let g = s.get_or_create_game("Palworld", None).unwrap();
        s.add_episode(g, 0, 10, "built the first base").unwrap();
        s.add_episode(g, 20, 30, "caught Anubis, upgraded weapons").unwrap();
        assert_eq!(
            s.latest_episode(g).unwrap().unwrap(),
            "caught Anubis, upgraded weapons"
        );
    }
}
