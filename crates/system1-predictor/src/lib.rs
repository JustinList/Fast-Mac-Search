//! `fms-system1-predictor` — Fast "System 1" Behavioral Reflex & Speculative Autocomplete Engine.
//!
//! Operates in under `0.2 ms` per keystroke:
//! 1. **Contextual Markov Frecency Graph**: Maps `(keystroke_prefix, active_app, workspace)` ->
//!    exponentially decaying file affinity weights (`half_life = 72h`).
//! 2. **Query Intent Classifier**: Dynamically computes RRF fusion weights (`w_frecency`, `w_l0`, `w_l1`, `w_l2`)
//!    based on query length, path punctuation, code tokens, and natural-language phrasing.
//! 3. **Speculative Ghost-Text Autocomplete**: Predicts inline greyed-out completion suffixes (`Tab` to accept)
//!    from the local file vocabulary and Markov token transitions.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ahash::AHashMap;
use fms_crypto_vfs::{
    CryptoVfsError, DEFAULT_PAGE_SIZE, DecryptedPageCache, ShardKey, atomic_write_encrypted,
    read_encrypted_all,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Half-life for exponential recency decay: 72 hours (`259,200` seconds).
const FRECENCY_HALF_LIFE_SECS: f64 = 72.0 * 3600.0;

/// Ambient desktop context passed alongside each keystroke.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryContext {
    /// Frontmost macOS application bundle ID (`NSWorkspace.shared.frontmostApplication?.bundleIdentifier`).
    pub active_app_bundle_id: Option<String>,
    /// Active working directory or open repository root, if known.
    pub active_workspace_dir: Option<String>,
    /// Optional UNIX timestamp override (defaults to current wall-clock time).
    pub now_unix_secs: Option<i64>,
}

impl QueryContext {
    pub fn now_secs(&self) -> i64 {
        self.now_unix_secs.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(1_760_000_000)
        })
    }

    pub fn is_developer_app(&self) -> bool {
        let Some(bundle) = self.active_app_bundle_id.as_deref() else {
            return false;
        };
        let b = bundle.to_ascii_lowercase();
        b.contains("xcode")
            || b.contains("vscode")
            || b.contains("zed")
            || b.contains("cursor")
            || b.contains("terminal")
            || b.contains("iterm")
            || b.contains("ghostty")
            || b.contains("warp")
            || b.contains("jetbrains")
            || b.contains("neovide")
            || b.contains("antigravity")
    }
}

/// Inline greyed-out ghost-text completion suggestion (`Tab` to accept in UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostPrediction {
    /// Full completed query string (e.g. `"q3 revenue forecast"`).
    pub full_query: String,
    /// Only the suffix to render as ghost text after the user's cursor (e.g. `"venue forecast"`).
    pub ghost_suffix: String,
}

/// Dynamic Reciprocal Rank Fusion (RRF) weights produced by the System 1 Intent Classifier.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IntentWeights {
    pub frecency: f32,
    pub l0_path: f32,
    pub l1_lexical: f32,
    pub l2_semantic: f32,
    pub intent_label: &'static str,
}

/// Classifies query structure in `< 5 µs` to dynamically blend L0, L1, L2, and Frecency.
pub fn classify_query_intent(query: &str) -> IntentWeights {
    let trimmed = query.trim();
    let char_len = trimmed.chars().count();
    let word_count = trimmed.split_whitespace().count();
    let looks_like_path = trimmed.contains('/') || trimmed.contains('.') || trimmed.contains('_');

    if char_len <= 3 {
        // 1–3 keystrokes: ~90% L0 Path + Markov Frecency reflex
        IntentWeights {
            frecency: 0.45,
            l0_path: 0.45,
            l1_lexical: 0.08,
            l2_semantic: 0.02,
            intent_label: "reflex-prefix",
        }
    } else if looks_like_path && word_count == 1 {
        IntentWeights {
            frecency: 0.25,
            l0_path: 0.50,
            l1_lexical: 0.20,
            l2_semantic: 0.05,
            intent_label: "filename-exact",
        }
    } else if word_count >= 3 {
        // Conceptual / natural-language multi-word query: heavy L1 BM25 + L2 Semantic blend!
        IntentWeights {
            frecency: 0.15,
            l0_path: 0.20,
            l1_lexical: 0.35,
            l2_semantic: 0.30,
            intent_label: "semantic-content",
        }
    } else {
        // Balanced 2-token or medium single-word search
        IntentWeights {
            frecency: 0.25,
            l0_path: 0.35,
            l1_lexical: 0.25,
            l2_semantic: 0.15,
            intent_label: "hybrid-balanced",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrecencyEdge {
    score: f64,
    last_ts: i64,
}

impl FrecencyEdge {
    fn decayed(&self, now_ts: i64) -> f64 {
        let dt = (now_ts - self.last_ts).max(0) as f64;
        self.score * 0.5f64.powf(dt / FRECENCY_HALF_LIFE_SECS)
    }

    fn reinforce(&mut self, now_ts: i64, boost: f64) {
        self.score = self.decayed(now_ts) + boost;
        self.last_ts = now_ts;
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct System1Predictor {
    /// `(normalized_prefix, app_scope)` -> `path -> FrecencyEdge`
    prefix_edges: AHashMap<String, AHashMap<String, FrecencyEdge>>,
    /// Global file open frecency (`path -> FrecencyEdge`)
    global_file_frecency: AHashMap<String, FrecencyEdge>,
    /// Vocabulary token frequency (`"architecture" -> 42`)
    vocab_counts: AHashMap<String, u32>,
    /// Next-token Markov transitions (`"q3" -> {"revenue": 12, "roadmap": 4}`)
    markov_next: AHashMap<String, AHashMap<String, u32>>,
    /// Past full queries (`"q3 revenue forecast" -> 9`)
    past_queries: AHashMap<String, u32>,
}

impl System1Predictor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trains vocabulary and Markov bigrams on newly indexed filenames so speculative ghost-text
    /// works immediately on a fresh installation—before a single search has been typed!
    pub fn ingest_filename_vocabulary(&mut self, filename: &str) {
        let stem = filename
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(filename);

        let tokens: Vec<String> = split_identifier_tokens(stem);
        let mut prev: Option<String> = None;
        for tok in tokens {
            if tok.len() >= 2 && tok.len() <= 32 {
                *self.vocab_counts.entry(tok.clone()).or_insert(0) += 1;
                if let Some(p) = prev {
                    *self
                        .markov_next
                        .entry(p)
                        .or_default()
                        .entry(tok.clone())
                        .or_insert(0) += 1;
                }
                prev = Some(tok);
            }
        }
    }

    /// Records that the user typed `query` and opened `selected_path` while in `ctx`.
    pub fn record_selection(&mut self, query: &str, selected_path: &str, ctx: &QueryContext) {
        let now = ctx.now_secs();
        let norm_q = query.trim().to_ascii_lowercase();
        if !norm_q.is_empty() {
            *self.past_queries.entry(norm_q.clone()).or_insert(0) += 3;
        }

        self.global_file_frecency
            .entry(selected_path.to_string())
            .and_modify(|e| e.reinforce(now, 1.0))
            .or_insert(FrecencyEdge {
                score: 1.0,
                last_ts: now,
            });

        // Index all keystroke prefixes (1..=8 chars) both globally ("*") and scoped to active app!
        let app_key = ctx.active_app_bundle_id.as_deref().unwrap_or("*");
        let chars: Vec<char> = norm_q.chars().take(8).collect();
        for len in 1..=chars.len() {
            let pfx: String = chars[..len].iter().collect();

            let global_bucket = format!("*::{pfx}");
            self.prefix_edges
                .entry(global_bucket)
                .or_default()
                .entry(selected_path.to_string())
                .and_modify(|e| e.reinforce(now, 1.0))
                .or_insert(FrecencyEdge {
                    score: 1.0,
                    last_ts: now,
                });

            if app_key != "*" {
                let scoped_bucket = format!("{app_key}::{pfx}");
                self.prefix_edges
                    .entry(scoped_bucket)
                    .or_default()
                    .entry(selected_path.to_string())
                    .and_modify(|e| e.reinforce(now, 1.6))
                    .or_insert(FrecencyEdge {
                        score: 1.6,
                        last_ts: now,
                    });
            }
        }

        // Reinforce Markov token chain from the query itself
        let q_tokens = split_identifier_tokens(&norm_q);
        for pair in q_tokens.windows(2) {
            *self
                .markov_next
                .entry(pair[0].clone())
                .or_default()
                .entry(pair[1].clone())
                .or_insert(0) += 4;
        }
    }

    /// Queries the Contextual Markov Frecency Graph in `< 100 µs`, returning `(path, score)` pairs.
    pub fn rank_frecency(
        &self,
        query: &str,
        ctx: &QueryContext,
        limit: usize,
    ) -> Vec<(String, f32)> {
        let now = ctx.now_secs();
        let norm_q: String = query.trim().to_ascii_lowercase().chars().take(8).collect();
        if norm_q.is_empty() {
            return Vec::new();
        }

        let mut combined: AHashMap<&str, f64> = AHashMap::new();

        // 1. App-scoped Markov prefix edges (highest contextual signal!)
        if let Some(app) = ctx.active_app_bundle_id.as_deref() {
            let scoped_key = format!("{app}::{norm_q}");
            if let Some(edges) = self.prefix_edges.get(&scoped_key) {
                for (path, edge) in edges {
                    *combined.entry(path.as_str()).or_insert(0.0) += edge.decayed(now) * 1.75;
                }
            }
        }

        // 2. Global prefix edges
        let global_key = format!("*::{norm_q}");
        if let Some(edges) = self.prefix_edges.get(&global_key) {
            for (path, edge) in edges {
                *combined.entry(path.as_str()).or_insert(0.0) += edge.decayed(now);
            }
        }

        // 3. Ambient workspace & IDE affinity bonus on known frecency items
        for (path, score) in combined.iter_mut() {
            if let Some(ws) = ctx.active_workspace_dir.as_deref()
                && path.starts_with(ws)
            {
                *score *= 1.35;
            }
            if ctx.is_developer_app() && is_code_or_arch_doc(path) {
                *score *= 1.25;
            }
        }

        let mut ranked: Vec<(String, f32)> = combined
            .into_iter()
            .filter(|&(_, s)| s > 0.01)
            .map(|(p, s)| (p.to_string(), s as f32))
            .collect();
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);
        ranked
    }

    /// Computes an ambient context multiplier (`1.0 .. 1.6`) for any candidate path even prior to clicks
    /// (e.g., boosting repo architecture markdown files when an IDE is frontmost!).
    #[inline]
    pub fn ambient_context_multiplier(&self, path: &str, ctx: &QueryContext) -> f32 {
        let mut mult = 1.0f32;
        if let Some(ws) = ctx.active_workspace_dir.as_deref()
            && path.starts_with(ws)
        {
            mult *= 1.28;
        }
        if ctx.is_developer_app() && is_code_or_arch_doc(path) {
            mult *= 1.18;
        }
        if let Some(edge) = self.global_file_frecency.get(path) {
            let bonus = (edge.decayed(ctx.now_secs()) as f32 * 0.15).min(0.5);
            mult += bonus;
        }
        mult
    }

    /// Predicts greyed-out speculative ghost text (`Tab` to accept) in `< 0.2 ms`!
    pub fn predict_ghost_text(&self, raw_query: &str) -> Option<GhostPrediction> {
        if raw_query.is_empty() {
            return None;
        }
        let lower_input = raw_query.to_ascii_lowercase();
        let trimmed = lower_input.trim_start();
        if trimmed.len() < 2 {
            return None;
        }

        // Priority 1: Exact prefix match against high-confidence past queries
        if let Some((best_past, _)) = self
            .past_queries
            .iter()
            .filter(|(q, _)| q.len() > trimmed.len() && q.starts_with(trimmed))
            .max_by_key(|(_, count)| *count)
        {
            let suffix = &best_past[trimmed.len()..];
            return Some(GhostPrediction {
                full_query: format!("{raw_query}{suffix}"),
                ghost_suffix: suffix.to_string(),
            });
        }

        let ends_with_space = raw_query.ends_with(' ');
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        let last_tok = *tokens.last()?;

        if ends_with_space {
            // Predict the next Markov token after trailing space!
            let next_map = self.markov_next.get(last_tok)?;
            let (next_word, _) = next_map.iter().max_by_key(|(_, c)| *c)?;
            return Some(GhostPrediction {
                full_query: format!("{raw_query}{next_word}"),
                ghost_suffix: next_word.clone(),
            });
        }

        // Complete current partial token from indexed file vocabulary + optional next Markov token!
        let (completed_word, _) = self
            .vocab_counts
            .iter()
            .filter(|(w, _)| w.len() > last_tok.len() && w.starts_with(last_tok))
            .max_by_key(|(w, count)| (**count as usize) * 8 + (32usize.saturating_sub(w.len())))?;

        let mut suffix = completed_word[last_tok.len()..].to_string();

        // Speculatively append 1 high-confidence Markov continuation token if strongly linked
        if let Some(next_map) = self.markov_next.get(completed_word)
            && let Some((next_tok, &cnt)) = next_map.iter().max_by_key(|(_, c)| *c)
            && cnt >= 2
        {
            suffix.push(' ');
            suffix.push_str(next_tok);
        }

        Some(GhostPrediction {
            full_query: format!("{raw_query}{suffix}"),
            ghost_suffix: suffix,
        })
    }

    pub fn save_encrypted(&self, path: &Path, shard_key: &ShardKey) -> Result<(), CryptoVfsError> {
        let mut raw = serde_json::to_vec(self)
            .map_err(|e| CryptoVfsError::CorruptHeader(e.to_string()))?;
        let res = atomic_write_encrypted(path, shard_key, &raw, DEFAULT_PAGE_SIZE);
        raw.zeroize();
        res
    }

    pub fn load_encrypted(
        path: &Path,
        shard_key: &ShardKey,
        cache: Arc<DecryptedPageCache>,
    ) -> Result<Self, CryptoVfsError> {
        let mut raw = read_encrypted_all(path, shard_key, cache)?;
        let pred: Self = serde_json::from_slice(&raw)
            .map_err(|e| CryptoVfsError::CorruptHeader(format!("corrupt System1 state: {e}")))?;
        raw.zeroize();
        Ok(pred)
    }
}

fn is_code_or_arch_doc(path: &str) -> bool {
    path.ends_with(".md")
        || path.ends_with(".rs")
        || path.ends_with(".swift")
        || path.ends_with(".toml")
        || path.ends_with(".ts")
        || path.ends_with(".py")
}

/// Splits camelCase, snake_case, and kebab-case filenames into clean lowercase tokens.
fn split_identifier_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut curr = String::new();
    let mut prev_lower = false;

    for ch in s.chars() {
        if !ch.is_alphanumeric() {
            if !curr.is_empty() {
                out.push(std::mem::take(&mut curr));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !curr.is_empty() {
            out.push(std::mem::take(&mut curr));
        }
        curr.push(ch.to_ascii_lowercase());
        prev_lower = ch.is_lowercase();
    }
    if !curr.is_empty() {
        out.push(curr);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markov_frecency_and_ghost_autocomplete() {
        let mut s1 = System1Predictor::new();
        s1.ingest_filename_vocabulary("q3_revenue_forecast.md");
        s1.ingest_filename_vocabulary("q3_revenue_summary.md");
        s1.ingest_filename_vocabulary("EncryptedBlockDirectory.rs");

        // Ghost text predicts camelCase & snake_case vocabulary + Markov next word!
        let ghost = s1.predict_ghost_text("q3 rev").unwrap();
        assert_eq!(ghost.full_query, "q3 revenue");
        assert_eq!(ghost.ghost_suffix, "enue");

        let ide_ctx = QueryContext {
            active_app_bundle_id: Some("dev.zed.Zed".into()),
            active_workspace_dir: Some("/Users/dev/repo".into()),
            now_unix_secs: Some(1_760_000_000),
        };
        s1.record_selection("arch", "/Users/dev/repo/ARCHITECTURE.md", &ide_ctx);

        let ranked = s1.rank_frecency("ar", &ide_ctx, 5);
        assert_eq!(ranked[0].0, "/Users/dev/repo/ARCHITECTURE.md");
    }
}
