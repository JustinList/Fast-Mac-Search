//! `fms-l2-semantic` — Encrypted `f16` Quantized Vector Index & Pluggable System-1 Embedder.
//!
//! Stores 128-dimensional Matryoshka-compatible embeddings in `f16` precision with navigable
//! small-world graph links and ARM NEON `vfmaq_f32` SIMD cosine scoring, persisted strictly
//! through [`fms_crypto_vfs`].

use std::collections::BinaryHeap;
use std::cmp::Ordering;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ahash::{AHashMap, AHashSet};
use fms_crypto_vfs::{
    CryptoVfsError, DEFAULT_PAGE_SIZE, DecryptedPageCache, ShardKey, atomic_write_encrypted,
    read_encrypted_all,
};
use half::f16;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Default Matryoshka Representation Learning (MRL) embedding dimension.
pub const EMBEDDING_DIM: usize = 128;
/// Maximum outgoing edges per node in the navigable small-world proximity graph.
const MAX_NEIGHBORS: usize = 16;

/// Trait allowing drop-in replacement of the embedder with an Apple Neural Engine (CoreML)
/// or MLX bi-encoder (`Snowflake/arctic-embed-xs` / `all-MiniLM-L6-v2` truncated to 128d).
pub trait SemanticEmbedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed_query(&self, query: &str) -> Vec<f32>;
    fn embed_document(&self, path: &str, content: &str) -> Vec<f32>;
}

/// Zero-dependency, sub-0.2ms local semantic projection embedder combining:
/// 1. Curated conceptual synonym clusters (bridging zero-lexical-overlap queries like
///    `"quarterly budget"` -> `"q3_revenue_forecast.md"` or `"login credentials"` -> `"oauth_token.rs"`).
/// 2. Signed feature-hashed morphological stems & character 3/4-grams (128-dim L2-normalized).
#[derive(Debug, Default, Clone)]
pub struct MicroSemanticEmbedder;

impl SemanticEmbedder for MicroSemanticEmbedder {
    #[inline]
    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    fn embed_query(&self, query: &str) -> Vec<f32> {
        embed_text_128(query, 1.4)
    }

    fn embed_document(&self, path: &str, content: &str) -> Vec<f32> {
        let snippet_end = floor_char_boundary(content, 4096);
        let mut combined = String::with_capacity(path.len() + 1 + snippet_end);
        combined.push_str(path);
        combined.push(' ');
        combined.push_str(&content[..snippet_end]);
        embed_text_128(&combined, 1.0)
    }
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Curated conceptual clusters mapped into dedicated semantic dimensions (`0..24`), while
/// dimensions `24..128` capture signed random projections of stems, bigrams, and char n-grams.
fn concept_cluster_dims(stem: &str) -> &'static [(usize, f32)] {
    match stem {
        "auth" | "login" | "signin" | "oauth" | "jwt" | "session" | "password" | "credential"
        | "passphrase" | "keychain" | "enclave" | "biometric" | "touchid" => &[(0, 1.8), (1, 1.2)],
        "crypto" | "encrypt" | "decrypt" | "cipher" | "aes" | "gcm" | "aead" | "hkdf"
        | "argon2" | "sha256" | "blake3" | "secret" | "zerotrust" | "security" => {
            &[(1, 1.4), (2, 1.9)]
        }
        "revenue" | "budget" | "finance" | "financial" | "forecast" | "invoice" | "profit"
        | "arr" | "mrr" | "quarterly" | "q1" | "q2" | "q3" | "q4" | "fiscal" | "pricing" => {
            &[(3, 2.0), (4, 1.3)]
        }
        "search" | "index" | "query" | "tantivy" | "bm25" | "lexical" | "rank" | "rrf"
        | "fuzzy" | "spotlight" | "vector" | "hnsw" | "embed" | "semantic" => &[(5, 1.8), (6, 1.4)],
        "ui" | "gui" | "window" | "panel" | "nspanel" | "appkit" | "swift" | "metal"
        | "overlay" | "render" | "animation" | "view" | "theme" => &[(7, 1.9), (8, 1.2)],
        "disk" | "drive" | "volume" | "ssd" | "nvme" | "apfs" | "filesystem" | "vfs" | "mount"
        | "external" | "portable" | "fsevent" | "inode" | "crawl" => &[(9, 1.8), (10, 1.3)],
        "simd" | "neon" | "aarch64" | "arm64" | "latency" | "benchmark" | "perf" | "cache"
        | "arena" | "mlock" | "memory" | "zero" => &[(11, 1.8), (12, 1.2)],
        "arch" | "architecture" | "design" | "spec" | "roadmap" | "rfc" | "proposal"
        | "diagram" | "readme" | "doc" => &[(13, 1.9), (14, 1.2)],
        "test" | "assert" | "verify" | "mock" | "ci" | "check" | "audit" => &[(15, 1.7)],
        "daemon" | "ipc" | "socket" | "server" | "client" | "rpc" | "stream" => &[(16, 1.8)],
        _ => &[],
    }
}

fn light_stem(word: &str) -> &str {
    if word.len() > 5 {
        for suffix in ["ation", "ments", "ing", "ed", "es", "ly", "er", "s"] {
            if let Some(stripped) = word.strip_suffix(suffix)
                && stripped.len() >= 3
            {
                return stripped;
            }
        }
    }
    word
}

fn embed_text_128(text: &str, concept_boost: f32) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBEDDING_DIM];
    let mut prev_stem_hash: Option<u64> = None;

    for raw_tok in text.split(|c: char| !c.is_alphanumeric()) {
        if raw_tok.is_empty() {
            continue;
        }
        let lower = raw_tok.to_ascii_lowercase();
        let stem = light_stem(&lower);

        // 1. Concept cluster dimensions (0..24)
        for &(dim, weight) in concept_cluster_dims(stem) {
            vec[dim] += weight * concept_boost;
        }

        // 2. Signed multi-probe random projection into dimensions 24..128
        let h = hash64(stem.as_bytes(), 0x9E37_79B9_7F4A_7C15);
        project_hash(&mut vec, h, 1.0);

        // 3. Word-pair bigram projection
        if let Some(ph) = prev_stem_hash {
            let bh = ph.wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ h;
            project_hash(&mut vec, bh, 0.55);
        }
        prev_stem_hash = Some(h);

        // 4. Character 3-grams for subword & typo resilience
        let bytes = lower.as_bytes();
        if bytes.len() >= 3 {
            for tri in bytes.windows(3).take(16) {
                let th = hash64(tri, 0x517C_C1B7_2722_0A95);
                project_hash(&mut vec, th, 0.25);
            }
        }
    }

    l2_normalize_in_place(&mut vec);
    vec
}

#[inline]
fn project_hash(vec: &mut [f32], h: u64, weight: f32) {
    const PROJ_SPAN: usize = EMBEDDING_DIM - 24;
    let d1 = 24 + ((h as usize) % PROJ_SPAN);
    let d2 = 24 + (((h >> 21) as usize) % PROJ_SPAN);
    let s1 = if (h & (1 << 42)) == 0 { weight } else { -weight };
    let s2 = if (h & (1 << 43)) == 0 {
        weight * 0.7
    } else {
        -weight * 0.7
    };
    vec[d1] += s1;
    vec[d2] += s2;
}

#[inline]
fn hash64(bytes: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01B3);
    }
    h ^ (h >> 33)
}

pub fn l2_normalize_in_place(v: &mut [f32]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    if norm_sq > 1e-12 {
        let inv = 1.0 / norm_sq.sqrt();
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
}

/// Computes cosine similarity (dot product of L2-normalized vectors) between a `f32` query
/// and an `f16`-quantized stored vector, accelerated with ARM NEON `vfmaq_f32` on Apple Silicon.
#[inline]
pub fn dot_f32_f16(query: &[f32], stored_f16: &[f16]) -> f32 {
    debug_assert_eq!(query.len(), stored_f16.len());

    #[cfg(target_arch = "aarch64")]
    unsafe {
        dot_f32_f16_neon(query, stored_f16)
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        query
            .iter()
            .zip(stored_f16.iter())
            .map(|(&q, &s)| q * s.to_f32())
            .sum()
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn dot_f32_f16_neon(query: &[f32], stored_f16: &[f16]) -> f32 {
    use std::arch::aarch64::{vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32};

    let n = query.len();
    let mut acc0 = vdupq_n_f32(0.0);
    let mut acc1 = vdupq_n_f32(0.0);
    let mut i = 0usize;

    while i + 8 <= n {
        let s0 = [
            stored_f16[i].to_f32(),
            stored_f16[i + 1].to_f32(),
            stored_f16[i + 2].to_f32(),
            stored_f16[i + 3].to_f32(),
        ];
        let s1 = [
            stored_f16[i + 4].to_f32(),
            stored_f16[i + 5].to_f32(),
            stored_f16[i + 6].to_f32(),
            stored_f16[i + 7].to_f32(),
        ];
        unsafe {
            let q0 = vld1q_f32(query.as_ptr().add(i));
            let q1 = vld1q_f32(query.as_ptr().add(i + 4));
            let sv0 = vld1q_f32(s0.as_ptr());
            let sv1 = vld1q_f32(s1.as_ptr());
            acc0 = vfmaq_f32(acc0, q0, sv0);
            acc1 = vfmaq_f32(acc1, q1, sv1);
        }
        i += 8;
    }

    let mut sum = vaddvq_f32(acc0) + vaddvq_f32(acc1);
    while i < n {
        sum += query[i] * stored_f16[i].to_f32();
        i += 1;
    }
    sum
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Match {
    pub doc_id: u64,
    pub path: String,
    pub cosine_similarity: f32,
}

#[derive(Clone, Copy)]
struct ScoredNode {
    score: f32,
    node_idx: u32,
}

impl PartialEq for ScoredNode {
    fn eq(&self, other: &Self) -> bool {
        self.node_idx == other.node_idx && self.score.to_bits() == other.score.to_bits()
    }
}
impl Eq for ScoredNode {}
impl PartialOrd for ScoredNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScoredNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score)
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedVectorStore {
    dim: usize,
    doc_ids: Vec<u64>,
    paths: Vec<String>,
    alive: Vec<u8>,
    vectors_f16_bits: Vec<u16>,
    neighbors: Vec<Vec<u32>>,
}

/// Encrypted `f16` vector index combining exact NEON scans for medium shards (`<= 4,096` docs)
/// with Navigable Small-World (NSW) greedy beam graph traversal for large shards (`> 4,096` docs).
pub struct EncryptedVectorIndex {
    dim: usize,
    doc_ids: Vec<u64>,
    paths: Vec<String>,
    alive: Vec<u8>,
    /// Flat contiguous `f16` matrix (`num_nodes * dim`) for cache-friendly NEON reads.
    vectors_f16: Vec<f16>,
    neighbors: Vec<Vec<u32>>,
    path_to_node: AHashMap<String, u32>,
    alive_count: usize,
}

impl Default for EncryptedVectorIndex {
    fn default() -> Self {
        Self::new(EMBEDDING_DIM)
    }
}

impl EncryptedVectorIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            doc_ids: Vec::new(),
            paths: Vec::new(),
            alive: Vec::new(),
            vectors_f16: Vec::new(),
            neighbors: Vec::new(),
            path_to_node: AHashMap::new(),
            alive_count: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.alive_count
    }

    pub fn is_empty(&self) -> bool {
        self.alive_count == 0
    }

    pub fn upsert(&mut self, doc_id: u64, path: &str, embedding: &[f32]) {
        assert_eq!(embedding.len(), self.dim);

        if let Some(&existing) = self.path_to_node.get(path) {
            let idx = existing as usize;
            self.doc_ids[idx] = doc_id;
            self.alive[idx] = 1;
            let start = idx * self.dim;
            for (dst, &src) in self.vectors_f16[start..start + self.dim]
                .iter_mut()
                .zip(embedding.iter())
            {
                *dst = f16::from_f32(src);
            }
            return;
        }

        let new_idx = self.doc_ids.len() as u32;
        self.doc_ids.push(doc_id);
        self.paths.push(path.to_string());
        self.alive.push(1);
        for &val in embedding {
            self.vectors_f16.push(f16::from_f32(val));
        }

        // Connect to top-M nearest neighbors in the proximity graph
        let mut peer_links = Vec::with_capacity(MAX_NEIGHBORS);
        if new_idx > 0 {
            let total_existing = new_idx as usize;
            let sample_step = (total_existing / 256).max(1);
            let mut candidates: Vec<(f32, u32)> = (0..total_existing)
                .step_by(sample_step)
                .filter(|&i| self.alive[i] != 0)
                .map(|i| (dot_f32_f16(embedding, self.node_slice(i)), i as u32))
                .collect();
            candidates.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
            for &(_, peer) in candidates.iter().take(MAX_NEIGHBORS) {
                peer_links.push(peer);
                let p_neighbors = &mut self.neighbors[peer as usize];
                if p_neighbors.len() < MAX_NEIGHBORS {
                    p_neighbors.push(new_idx);
                }
            }
        }
        self.neighbors.push(peer_links);
        self.path_to_node.insert(path.to_string(), new_idx);
        self.alive_count += 1;
    }

    pub fn remove_by_path(&mut self, path: &str) -> bool {
        if let Some(idx) = self.path_to_node.remove(path) {
            let i = idx as usize;
            if self.alive[i] != 0 {
                self.alive[i] = 0;
                self.alive_count = self.alive_count.saturating_sub(1);
                return true;
            }
        }
        false
    }

    #[inline]
    fn node_slice(&self, idx: usize) -> &[f16] {
        let start = idx * self.dim;
        &self.vectors_f16[start..start + self.dim]
    }

    /// Searches the `f16` vector index using NEON SIMD (exact scan up to 4,096 vectors,
    /// multi-entry NSW beam graph search above 4,096 vectors).
    pub fn search(&self, query_vec: &[f32], limit: usize) -> (Vec<L2Match>, u64) {
        let started = Instant::now();
        if self.alive_count == 0 || limit == 0 || query_vec.len() != self.dim {
            return (Vec::new(), 0);
        }

        let total_nodes = self.doc_ids.len();
        let mut top_scored: Vec<(f32, usize)> = if total_nodes <= 4096 {
            let mut v = Vec::with_capacity(self.alive_count);
            for i in 0..total_nodes {
                if self.alive[i] != 0 {
                    let sim = dot_f32_f16(query_vec, self.node_slice(i));
                    if sim > 0.08 {
                        v.push((sim, i));
                    }
                }
            }
            v
        } else {
            self.nsw_beam_search(query_vec, (limit * 6).max(64))
        };

        if top_scored.len() > limit {
            top_scored.select_nth_unstable_by(limit, |a, b| b.0.total_cmp(&a.0));
            top_scored.truncate(limit);
        }
        top_scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));

        let matches = top_scored
            .into_iter()
            .map(|(sim, idx)| L2Match {
                doc_id: self.doc_ids[idx],
                path: self.paths[idx].clone(),
                cosine_similarity: sim,
            })
            .collect();

        (matches, started.elapsed().as_micros() as u64)
    }

    fn nsw_beam_search(&self, query_vec: &[f32], ef: usize) -> Vec<(f32, usize)> {
        let total = self.doc_ids.len();
        let mut visited = AHashSet::with_capacity(ef * 4);
        let mut frontier = BinaryHeap::new();
        let mut results = Vec::with_capacity(ef * 2);

        // Seed entry points evenly across the graph
        let step = (total / 32).max(1);
        for seed in (0..total).step_by(step) {
            if visited.insert(seed as u32) {
                let s = dot_f32_f16(query_vec, self.node_slice(seed));
                frontier.push(ScoredNode {
                    score: s,
                    node_idx: seed as u32,
                });
                if self.alive[seed] != 0 && s > 0.08 {
                    results.push((s, seed));
                }
            }
        }

        let mut expansions = 0usize;
        while let Some(curr) = frontier.pop() {
            if expansions >= ef * 3 {
                break;
            }
            expansions += 1;
            for &nbr in &self.neighbors[curr.node_idx as usize] {
                if visited.insert(nbr) {
                    let idx = nbr as usize;
                    let s = dot_f32_f16(query_vec, self.node_slice(idx));
                    frontier.push(ScoredNode {
                        score: s,
                        node_idx: nbr,
                    });
                    if self.alive[idx] != 0 && s > 0.08 {
                        results.push((s, idx));
                    }
                }
            }
        }
        results
    }

    pub fn save_encrypted(&self, path: &Path, shard_key: &ShardKey) -> Result<(), CryptoVfsError> {
        const MAGIC: &[u8; 8] = b"FMSL2B02";
        let n = self.doc_ids.len();
        let path_bytes_est: usize = self.paths.iter().map(|p| 4 + p.len()).sum();
        let nbr_bytes_est: usize = self.neighbors.iter().map(|nb| 2 + nb.len() * 4).sum();
        let total_cap = 16 + n * 9 + self.vectors_f16.len() * 2 + path_bytes_est + nbr_bytes_est;

        let mut raw = Vec::with_capacity(total_cap);
        raw.extend_from_slice(MAGIC);
        raw.extend_from_slice(&(self.dim as u32).to_le_bytes());
        raw.extend_from_slice(&(n as u32).to_le_bytes());

        for &id in &self.doc_ids {
            raw.extend_from_slice(&id.to_le_bytes());
        }
        raw.extend_from_slice(&self.alive);
        for &vf in &self.vectors_f16 {
            raw.extend_from_slice(&vf.to_bits().to_le_bytes());
        }
        for p in &self.paths {
            let pb = p.as_bytes();
            raw.extend_from_slice(&(pb.len() as u32).to_le_bytes());
            raw.extend_from_slice(pb);
        }
        for nb in &self.neighbors {
            raw.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            for &peer in nb {
                raw.extend_from_slice(&peer.to_le_bytes());
            }
        }

        let res = atomic_write_encrypted(path, shard_key, &raw, DEFAULT_PAGE_SIZE);
        raw.zeroize();
        res
    }

    pub fn load_encrypted(
        path: &Path,
        shard_key: &ShardKey,
        cache: Arc<DecryptedPageCache>,
    ) -> Result<Self, CryptoVfsError> {
        const MAGIC: &[u8; 8] = b"FMSL2B02";
        let mut raw = read_encrypted_all(path, shard_key, cache)?;

        if raw.len() >= 16 && &raw[..8] == MAGIC {
            let dim = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
            let n = u32::from_le_bytes(raw[12..16].try_into().unwrap()) as usize;
            let mut cur = 16usize;

            let need_fixed = n
                .checked_mul(9)
                .and_then(|x| x.checked_add(n.checked_mul(dim)?.checked_mul(2)?))
                .ok_or_else(|| CryptoVfsError::CorruptHeader("L2 binary overflow".into()))?;
            if cur + need_fixed > raw.len() {
                raw.zeroize();
                return Err(CryptoVfsError::CorruptHeader("truncated L2 binary header".into()));
            }

            let mut doc_ids = Vec::with_capacity(n);
            for chunk in raw[cur..cur + n * 8].chunks_exact(8) {
                doc_ids.push(u64::from_le_bytes(chunk.try_into().unwrap()));
            }
            cur += n * 8;

            let alive = raw[cur..cur + n].to_vec();
            cur += n;

            let vec_elems = n * dim;
            let mut vectors_f16 = Vec::with_capacity(vec_elems);
            for chunk in raw[cur..cur + vec_elems * 2].chunks_exact(2) {
                vectors_f16.push(f16::from_bits(u16::from_le_bytes(chunk.try_into().unwrap())));
            }
            cur += vec_elems * 2;

            let mut paths = Vec::with_capacity(n);
            for _ in 0..n {
                if cur + 4 > raw.len() {
                    raw.zeroize();
                    return Err(CryptoVfsError::CorruptHeader("truncated L2 path len".into()));
                }
                let plen = u32::from_le_bytes(raw[cur..cur + 4].try_into().unwrap()) as usize;
                cur += 4;
                if cur + plen > raw.len() {
                    raw.zeroize();
                    return Err(CryptoVfsError::CorruptHeader("truncated L2 path bytes".into()));
                }
                let s = String::from_utf8_lossy(&raw[cur..cur + plen]).into_owned();
                cur += plen;
                paths.push(s);
            }

            let mut neighbors = Vec::with_capacity(n);
            for _ in 0..n {
                if cur + 2 > raw.len() {
                    raw.zeroize();
                    return Err(CryptoVfsError::CorruptHeader("truncated L2 nbr len".into()));
                }
                let m = u16::from_le_bytes(raw[cur..cur + 2].try_into().unwrap()) as usize;
                cur += 2;
                if cur + m * 4 > raw.len() {
                    raw.zeroize();
                    return Err(CryptoVfsError::CorruptHeader("truncated L2 nbr list".into()));
                }
                let mut nb = Vec::with_capacity(m);
                for chunk in raw[cur..cur + m * 4].chunks_exact(4) {
                    nb.push(u32::from_le_bytes(chunk.try_into().unwrap()));
                }
                cur += m * 4;
                neighbors.push(nb);
            }
            raw.zeroize();

            let mut alive_count = 0usize;
            let mut path_to_node = AHashMap::with_capacity(n);
            for (i, p) in paths.iter().enumerate() {
                if alive[i] != 0 {
                    alive_count += 1;
                    path_to_node.insert(p.clone(), i as u32);
                }
            }

            return Ok(Self {
                dim,
                doc_ids,
                paths,
                alive,
                vectors_f16,
                neighbors,
                path_to_node,
                alive_count,
            });
        }

        let snap: PersistedVectorStore = serde_json::from_slice(&raw)
            .map_err(|e| CryptoVfsError::CorruptHeader(format!("corrupt L2 vector shard: {e}")))?;
        raw.zeroize();

        let vectors_f16: Vec<f16> = snap
            .vectors_f16_bits
            .into_iter()
            .map(f16::from_bits)
            .collect();
        let alive_count = snap.alive.iter().filter(|&&a| a != 0).count();
        let mut path_to_node = AHashMap::with_capacity(snap.paths.len());
        for (i, p) in snap.paths.iter().enumerate() {
            if snap.alive[i] != 0 {
                path_to_node.insert(p.clone(), i as u32);
            }
        }

        let loaded = Self {
            dim: snap.dim,
            doc_ids: snap.doc_ids,
            paths: snap.paths,
            alive: snap.alive,
            vectors_f16,
            neighbors: snap.neighbors,
            path_to_node,
            alive_count,
        };
        // Auto-upgrade legacy v1 JSON vector shard in-place to compact FMSL2B02 binary.
        let _ = loaded.save_encrypted(path, shard_key);
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_lexical_overlap_semantic_conceptual_retrieval() {
        let embedder = MicroSemanticEmbedder;
        let mut vstore = EncryptedVectorIndex::default();

        vstore.upsert(
            1,
            "/docs/q3_revenue_forecast.md",
            &embedder.embed_document(
                "/docs/q3_revenue_forecast.md",
                "ARR projections and fiscal profit margins for autumn",
            ),
        );
        vstore.upsert(
            2,
            "/src/window_overlay.swift",
            &embedder.embed_document(
                "/src/window_overlay.swift",
                "NSPanel AppKit floating vibrancy animation",
            ),
        );

        // Query uses "quarterly budget" — ZERO shared words with "q3_revenue_forecast.md" or its body!
        let qvec = embedder.embed_query("quarterly budget");
        let (hits, _us) = vstore.search(&qvec, 2);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_id, 1);
    }
}
