//! `fms-llm-memory` — Zero-Trust Encrypted Cross-LLM Conversation Index.
//!
//! Discovers and incrementally parses local AI transcripts across:
//! - **Claude Code & Claude Desktop** (`~/.claude/projects`, `~/Library/Application Support/Claude`)
//! - **OpenAI Codex** (`~/.codex/sessions`)
//! - **Gemini / Antigravity** (`~/.gemini/antigravity/brain`)
//! - **LM Studio** (`~/.lmstudio/conversations`)
//! - **ChatGPT & Claude Web JSON Exports** (`conversations.json` encrypted import)
//!
//! All titles, dialogue turns, BM25 postings, and 128d `f16` semantic vectors are stored strictly
//! inside 16 KB AES-256-GCM frames via [`fms_crypto_vfs`].

pub mod parsers;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, UNIX_EPOCH};

use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result};
use fms_crypto_vfs::{
    DEFAULT_PAGE_SIZE, DecryptedPageCache, HybridKeyProvider, KeyBackendKind, ShardKey, VfsConfig,
    atomic_write_encrypted, read_encrypted_all,
};
use fms_l0_arena::L0PathArena;
use fms_l1_lexical::EncryptedLexicalIndex;
use fms_l2_semantic::{EncryptedVectorIndex, SemanticEmbedder};
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub use parsers::{
    ClaudeSessionMeta, DiscoveredTranscript, LlmProvider, LlmSession, LlmTurn,
    discover_local_llm_files, parse_discovered_transcript, parse_export_or_lmstudio_json,
};

/// Deterministic 16-byte HKDF namespace ID isolating the LLM memory DEK from filesystem volume DEKs.
const LLM_MEMORY_NAMESPACE_UUID: [u8; 16] = *b"FMS_LLM_MEMORY01";
/// Base `doc_id` offset (`1 << 40`) so LLM chat `doc_id`s never collide with filesystem `doc_id`s.
const LLM_DOC_ID_BASE: u64 = 1u64 << 40;
/// Maximum searchable dialogue bytes indexed per conversation in L1 Tantivy & L2 vectors.
const MAX_INDEXED_DIALOGUE_BYTES: usize = 96 * 1024;
const RRF_K: f32 = 60.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProviderCount {
    pub provider: String,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmSyncReport {
    pub files_discovered: usize,
    pub files_parsed: usize,
    pub files_cached: usize,
    pub sessions_total: usize,
    pub providers: Vec<LlmProviderCount>,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSearchHit {
    pub doc_id: u64,
    pub session_id: String,
    pub provider: LlmProvider,
    pub title: String,
    pub workspace: Option<String>,
    pub model: Option<String>,
    pub updated_unix_secs: i64,
    pub source_path: PathBuf,
    pub snippet: String,
    pub rrf_score: f32,
    pub match_indices: Vec<u32>,
    pub matched_layers: Vec<String>,
}

#[derive(Debug, Default)]
pub struct LlmSearchOutput {
    pub hits: Vec<LlmSearchHit>,
    pub l0_us: u64,
    pub l1_us: u64,
    pub l2_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileSyncRecord {
    mtime_secs: i64,
    size_bytes: u64,
    session_ids: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedSyncState {
    files: AHashMap<PathBuf, FileSyncRecord>,
    imported_session_ids: AHashSet<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedSessionsBlob {
    next_offset: u64,
    doc_ids: AHashMap<String, u64>,
    l0_keys: AHashMap<String, String>,
    sessions: Vec<LlmSession>,
}

struct SessionRuntimeState {
    sessions: AHashMap<String, LlmSession>,
    session_to_doc_id: AHashMap<String, u64>,
    doc_id_to_session: AHashMap<u64, String>,
    session_to_l0_key: AHashMap<String, String>,
}

impl Default for SessionRuntimeState {
    fn default() -> Self {
        Self {
            sessions: AHashMap::new(),
            session_to_doc_id: AHashMap::new(),
            doc_id_to_session: AHashMap::new(),
            session_to_l0_key: AHashMap::new(),
        }
    }
}

/// Encrypted 3-tier conversation store & search index for all local + imported LLM chats.
pub struct EncryptedLlmMemory {
    llm_dir: PathBuf,
    imports_dir: PathBuf,
    shard_key: ShardKey,
    key_backend: KeyBackendKind,
    l0: RwLock<L0PathArena>,
    l1: EncryptedLexicalIndex,
    l2: RwLock<EncryptedVectorIndex>,
    state: RwLock<SessionRuntimeState>,
    sync_state: RwLock<PersistedSyncState>,
    sync_mutex: Mutex<()>,
    next_doc_seq: AtomicU64,
    cached_session_count: AtomicUsize,
}

impl EncryptedLlmMemory {
    pub fn open_or_create(
        app_support_dir: &Path,
        key_provider: &HybridKeyProvider,
        passphrase: Option<&[u8]>,
        vfs_config: VfsConfig,
    ) -> Result<Self> {
        let llm_dir = app_support_dir.join("llm_memory");
        let imports_dir = llm_dir.join("imports");
        fs::create_dir_all(&llm_dir)?;

        let (shard_key, key_backend, _) = key_provider.resolve_or_init_shard_key(
            &llm_dir,
            &LLM_MEMORY_NAMESPACE_UUID,
            passphrase,
        )?;

        let cache = Arc::new(DecryptedPageCache::new(vfs_config.cache_pages.max(64)));

        let l0_path = llm_dir.join("llm_l0.fms");
        let l2_path = llm_dir.join("llm_l2.fms");
        let sessions_path = llm_dir.join("llm_sessions.fms");
        let sync_path = llm_dir.join("llm_sync_state.fms");

        let l0 = if l0_path.exists() {
            L0PathArena::load_encrypted(&l0_path, &shard_key, Arc::clone(&cache))
                .unwrap_or_default()
        } else {
            L0PathArena::with_capacity(2048)
        };

        let l1 = EncryptedLexicalIndex::open_or_create(
            llm_dir.join("llm_l1_tantivy"),
            shard_key.clone(),
            vfs_config,
        )?;

        let l2 = if l2_path.exists() {
            EncryptedVectorIndex::load_encrypted(&l2_path, &shard_key, Arc::clone(&cache))
                .unwrap_or_default()
        } else {
            EncryptedVectorIndex::default()
        };

        let mut rt = SessionRuntimeState::default();
        let mut next_seq = 1u64;
        if sessions_path.exists()
            && let Ok(mut raw) = read_encrypted_all(&sessions_path, &shard_key, Arc::clone(&cache))
        {
            if let Ok(blob) = serde_json::from_slice::<PersistedSessionsBlob>(&raw) {
                next_seq = blob.next_offset.max(1);
                for s in blob.sessions {
                    let sid = s.session_id.clone();
                    let doc_id = blob.doc_ids.get(&sid).copied().unwrap_or_else(|| {
                        let id = LLM_DOC_ID_BASE + next_seq;
                        next_seq += 1;
                        id
                    });
                    let l0_k = blob
                        .l0_keys
                        .get(&sid)
                        .cloned()
                        .unwrap_or_else(|| make_l0_key(&s));
                    rt.session_to_doc_id.insert(sid.clone(), doc_id);
                    rt.doc_id_to_session.insert(doc_id, sid.clone());
                    rt.session_to_l0_key.insert(sid.clone(), l0_k);
                    rt.sessions.insert(sid, s);
                }
            }
            raw.zeroize();
        }

        let sync_state = if sync_path.exists()
            && let Ok(mut raw) = read_encrypted_all(&sync_path, &shard_key, cache)
        {
            let parsed = serde_json::from_slice::<PersistedSyncState>(&raw).unwrap_or_default();
            raw.zeroize();
            parsed
        } else {
            PersistedSyncState::default()
        };

        let count = rt.sessions.len();
        Ok(Self {
            llm_dir,
            imports_dir,
            shard_key,
            key_backend,
            l0: RwLock::new(l0),
            l1,
            l2: RwLock::new(l2),
            state: RwLock::new(rt),
            sync_state: RwLock::new(sync_state),
            sync_mutex: Mutex::new(()),
            next_doc_seq: AtomicU64::new(next_seq),
            cached_session_count: AtomicUsize::new(count),
        })
    }

    #[inline]
    pub fn llm_dir(&self) -> &Path {
        &self.llm_dir
    }

    #[inline]
    pub fn key_backend(&self) -> KeyBackendKind {
        self.key_backend
    }

    #[inline]
    pub fn session_count(&self) -> usize {
        self.cached_session_count.load(Ordering::Relaxed)
    }

    pub fn provider_counts(&self) -> Vec<LlmProviderCount> {
        let guard = self.state.read();
        compute_provider_counts(&guard.sessions)
    }

    pub fn get_session(&self, session_id: &str) -> Option<LlmSession> {
        self.state.read().sessions.get(session_id).cloned()
    }

    /// Incrementally discovers and syncs all local LLM transcripts under `home_dir`.
    /// Unchanged files (same `mtime` + byte size) are skipped in microseconds.
    pub fn sync_from_home(
        &self,
        home_dir: &Path,
        embedder: &dyn SemanticEmbedder,
    ) -> Result<LlmSyncReport> {
        let _guard = self.sync_mutex.lock();
        let started = Instant::now();

        let (discovered, claude_meta) = discover_local_llm_files(home_dir, &self.imports_dir);
        let files_discovered = discovered.len();

        let mut seen_paths = AHashSet::with_capacity(files_discovered);
        let mut to_parse: Vec<(DiscoveredTranscript, i64, u64)> = Vec::new();
        let mut files_cached = 0usize;

        {
            let sync_guard = self.sync_state.read();
            let rt_guard = self.state.read();

            for item in discovered {
                let p = item.path().to_path_buf();
                seen_paths.insert(p.clone());

                let (mtime, size) = file_stat(&p);
                if let Some(prev) = sync_guard.files.get(&p)
                    && prev.mtime_secs == mtime
                    && prev.size_bytes == size
                    && prev
                        .session_ids
                        .iter()
                        .all(|sid| rt_guard.sessions.contains_key(sid))
                {
                    files_cached += 1;
                } else {
                    to_parse.push((item, mtime, size));
                }
            }
        }

        let deleted_paths: Vec<PathBuf> = {
            let sync_guard = self.sync_state.read();
            sync_guard
                .files
                .keys()
                .filter(|p| !seen_paths.contains(*p))
                .cloned()
                .collect()
        };

        if to_parse.is_empty() && deleted_paths.is_empty() {
            let sessions_total = self.session_count();
            return Ok(LlmSyncReport {
                files_discovered,
                files_parsed: 0,
                files_cached,
                sessions_total,
                providers: self.provider_counts(),
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            });
        }

        let files_parsed = to_parse.len();

        // Parse changed/new transcripts in parallel across Apple Silicon cores + precompute embeddings!
        struct PreparedSession {
            session: LlmSession,
            searchable_body: String,
            embedding: Vec<f32>,
        }

        let parsed_batches: Vec<(PathBuf, i64, u64, Vec<PreparedSession>)> = to_parse
            .par_iter()
            .map(|(item, mtime, size)| {
                let raw_sessions = parse_discovered_transcript(item, &claude_meta);
                let prepared = raw_sessions
                    .into_iter()
                    .map(|session| {
                        let searchable_body = session.searchable_body(MAX_INDEXED_DIALOGUE_BYTES);
                        let embedding = embedder.embed_document(&session.title, &searchable_body);
                        PreparedSession {
                            session,
                            searchable_body,
                            embedding,
                        }
                    })
                    .collect();
                (item.path().to_path_buf(), *mtime, *size, prepared)
            })
            .collect();

        // Apply removals & upserts
        {
            let mut sync_w = self.sync_state.write();
            let mut rt_w = self.state.write();
            let mut l0_w = self.l0.write();
            let mut l2_w = self.l2.write();

            // Collect session IDs still referenced by untouched cached files or explicit imports
            let mut protected_ids: AHashSet<String> = sync_w.imported_session_ids.clone();
            let mut modified_or_deleted: AHashSet<PathBuf> =
                deleted_paths.iter().cloned().collect();
            for (p, _, _, _) in &parsed_batches {
                modified_or_deleted.insert(p.clone());
            }
            for (p, rec) in &sync_w.files {
                if !modified_or_deleted.contains(p) {
                    for sid in &rec.session_ids {
                        protected_ids.insert(sid.clone());
                    }
                }
            }

            let mut candidate_stale_ids: Vec<String> = Vec::new();
            for dp in &deleted_paths {
                if let Some(old_rec) = sync_w.files.remove(dp) {
                    candidate_stale_ids.extend(old_rec.session_ids);
                }
            }
            for (p, _, _, _) in &parsed_batches {
                if let Some(old_rec) = sync_w.files.get(p) {
                    candidate_stale_ids.extend(old_rec.session_ids.iter().cloned());
                }
            }

            // Track newly produced session IDs in this batch
            let mut new_batch_ids: AHashSet<String> = AHashSet::new();
            for (_, _, _, batch) in &parsed_batches {
                for ps in batch {
                    new_batch_ids.insert(ps.session.session_id.clone());
                }
            }

            for stale_id in candidate_stale_ids {
                if !protected_ids.contains(&stale_id) && !new_batch_ids.contains(&stale_id) {
                    remove_session_locked(&stale_id, &mut rt_w, &mut l0_w, &self.l1, &mut l2_w);
                }
            }

            // Upsert all newly parsed sessions (if duplicate session_id in batch, keep richer transcript)
            for (path, mtime, size, batch) in parsed_batches {
                let mut file_sids = Vec::with_capacity(batch.len());
                for ps in batch {
                    let sid = ps.session.session_id.clone();
                    file_sids.push(sid.clone());

                    let should_replace = match rt_w.sessions.get(&sid) {
                        Some(existing) if existing.source_path != path => {
                            ps.session.turns.len() >= existing.turns.len()
                        }
                        _ => true,
                    };
                    if should_replace {
                        upsert_session_locked(
                            ps.session,
                            &ps.searchable_body,
                            &ps.embedding,
                            &self.next_doc_seq,
                            &mut rt_w,
                            &mut l0_w,
                            &self.l1,
                            &mut l2_w,
                        )?;
                    }
                }
                sync_w.files.insert(
                    path,
                    FileSyncRecord {
                        mtime_secs: mtime,
                        size_bytes: size,
                        session_ids: file_sids,
                    },
                );
            }

            let _ = l0_w.lock_memory();
            self.cached_session_count
                .store(rt_w.sessions.len(), Ordering::Relaxed);
        }

        self.persist_all()?;

        let sessions_total = self.session_count();
        Ok(LlmSyncReport {
            files_discovered,
            files_parsed,
            files_cached,
            sessions_total,
            providers: self.provider_counts(),
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    /// Parses a ChatGPT or Claude Web `conversations.json` export file directly into AES-256-GCM
    /// encrypted storage without copying any plaintext `.json` file into `llm_memory`.
    pub fn import_export_file(
        &self,
        export_json_path: &Path,
        embedder: &dyn SemanticEmbedder,
    ) -> Result<LlmSyncReport> {
        let _guard = self.sync_mutex.lock();
        let started = Instant::now();

        let canonical = export_json_path
            .canonicalize()
            .with_context(|| format!("cannot open export file {:?}", export_json_path))?;

        let raw_sessions = parse_export_or_lmstudio_json(&canonical, None);
        if raw_sessions.is_empty() {
            anyhow::bail!(
                "No ChatGPT or Claude conversations found in {:?}",
                export_json_path
            );
        }

        let prepared: Vec<(LlmSession, String, Vec<f32>)> = raw_sessions
            .into_par_iter()
            .map(|s| {
                let body = s.searchable_body(MAX_INDEXED_DIALOGUE_BYTES);
                let vec = embedder.embed_document(&s.title, &body);
                (s, body, vec)
            })
            .collect();

        let imported_count = prepared.len();
        {
            let mut sync_w = self.sync_state.write();
            let mut rt_w = self.state.write();
            let mut l0_w = self.l0.write();
            let mut l2_w = self.l2.write();

            for (session, body, vec) in prepared {
                sync_w
                    .imported_session_ids
                    .insert(session.session_id.clone());
                upsert_session_locked(
                    session,
                    &body,
                    &vec,
                    &self.next_doc_seq,
                    &mut rt_w,
                    &mut l0_w,
                    &self.l1,
                    &mut l2_w,
                )?;
            }

            let _ = l0_w.lock_memory();
            self.cached_session_count
                .store(rt_w.sessions.len(), Ordering::Relaxed);
        }

        self.persist_all()?;

        Ok(LlmSyncReport {
            files_discovered: 1,
            files_parsed: imported_count,
            files_cached: 0,
            sessions_total: self.session_count(),
            providers: self.provider_counts(),
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    /// Executes 3-Tier (L0 NEON Fuzzy Title + L1 Encrypted Tantivy BM25 + L2 Semantic Vector)
    /// search across all encrypted LLM chats. If `raw_query` is empty, returns the most recent
    /// conversations (optionally filtered by `provider_filter`).
    pub fn search(
        &self,
        raw_query: &str,
        provider_filter: Option<LlmProvider>,
        limit: usize,
        l0_only: bool,
        embedder: &dyn SemanticEmbedder,
    ) -> LlmSearchOutput {
        if limit == 0 || self.session_count() == 0 {
            return LlmSearchOutput::default();
        }

        let trimmed = raw_query.trim();
        if trimmed.is_empty() {
            let l0_start = Instant::now();
            let rt = self.state.read();
            let mut recent: Vec<&LlmSession> = rt
                .sessions
                .values()
                .filter(|s| provider_filter.is_none_or(|pf| s.provider == pf))
                .collect();
            recent.sort_unstable_by(|a, b| b.updated_unix_secs.cmp(&a.updated_unix_secs));
            recent.truncate(limit);

            let hits = recent
                .into_iter()
                .enumerate()
                .map(|(rank, s)| {
                    let doc_id = rt.session_to_doc_id.get(&s.session_id).copied().unwrap_or(0);
                    LlmSearchHit {
                        doc_id,
                        session_id: s.session_id.clone(),
                        provider: s.provider,
                        title: s.title.clone(),
                        workspace: s.workspace.clone(),
                        model: s.model.clone(),
                        updated_unix_secs: s.updated_unix_secs,
                        source_path: s.source_path.clone(),
                        snippet: build_dialogue_snippet(s, ""),
                        rrf_score: 1.0 / (RRF_K + (rank + 1) as f32),
                        match_indices: Vec::new(),
                        matched_layers: vec!["Recent".into()],
                    }
                })
                .collect();

            return LlmSearchOutput {
                hits,
                l0_us: l0_start.elapsed().as_micros() as u64,
                l1_us: 0,
                l2_us: 0,
            };
        }

        let stage_k = (limit * 4).max(32);

        // Stage 1: L0 ARM NEON + Smith-Waterman on `[Provider] Title — Workspace`
        let t0 = Instant::now();
        let (l0_matches, _) = self.l0.read().search(trimmed, stage_k);
        let l0_us = t0.elapsed().as_micros() as u64;

        // Stage 2 & 3: Parallel L1 Encrypted Tantivy BM25 + L2 Semantic Vector
        let run_deep = !l0_only && trimmed.chars().count() >= 2;
        let ((l1_matches, l1_us), (l2_matches, l2_us)) = if run_deep {
            let run_l2 = trimmed.chars().count() >= 3;
            rayon::join(
                || self.l1.search(trimmed, stage_k).unwrap_or_default(),
                || {
                    if !run_l2 {
                        return (Vec::new(), 0u64);
                    }
                    let qvec = embedder.embed_query(trimmed);
                    self.l2.read().search(&qvec, stage_k)
                },
            )
        } else {
            ((Vec::new(), 0u64), (Vec::new(), 0u64))
        };

        #[derive(Default)]
        struct ChatAccum {
            rrf_score: f32,
            match_indices: Vec<u32>,
            layers: Vec<String>,
            l1_fallback_snippet: String,
        }

        let rt = self.state.read();
        let mut merged: AHashMap<String, ChatAccum> = AHashMap::with_capacity(limit * 4);

        let char_len = trimmed.chars().count();
        let (w_l0, w_l1, w_l2) = if char_len <= 3 {
            (1.15f32, 0.85f32, 0.35f32)
        } else if trimmed.contains(' ') {
            (0.90f32, 1.25f32, 0.95f32)
        } else {
            (1.05f32, 1.15f32, 0.70f32)
        };

        for (rank, m) in l0_matches.into_iter().enumerate() {
            if let Some(sid) = rt.doc_id_to_session.get(&m.doc_id) {
                let entry = merged.entry(sid.clone()).or_default();
                entry.rrf_score += w_l0 / (RRF_K + (rank + 1) as f32);
                entry.match_indices = m.match_indices;
                push_layer(&mut entry.layers, "L0");
            }
        }

        for (rank, m) in l1_matches.into_iter().enumerate() {
            let entry = merged.entry(m.path).or_default();
            entry.rrf_score += w_l1 / (RRF_K + (rank + 1) as f32);
            if entry.l1_fallback_snippet.is_empty() {
                entry.l1_fallback_snippet = m.snippet;
            }
            push_layer(&mut entry.layers, "L1");
        }

        for (rank, m) in l2_matches.into_iter().enumerate() {
            let entry = merged.entry(m.path).or_default();
            entry.rrf_score += w_l2 / (RRF_K + (rank + 1) as f32);
            push_layer(&mut entry.layers, "L2");
        }

        let mut hits: Vec<LlmSearchHit> = merged
            .into_iter()
            .filter_map(|(sid, acc)| {
                let session = rt.sessions.get(&sid)?;
                if let Some(pf) = provider_filter
                    && session.provider != pf
                {
                    return None;
                }
                let doc_id = rt.session_to_doc_id.get(&sid).copied().unwrap_or(0);
                let dialogue_snip = build_dialogue_snippet(session, trimmed);
                let snippet = if dialogue_snip.is_empty() {
                    acc.l1_fallback_snippet
                } else {
                    dialogue_snip
                };

                Some(LlmSearchHit {
                    doc_id,
                    session_id: sid,
                    provider: session.provider,
                    title: session.title.clone(),
                    workspace: session.workspace.clone(),
                    model: session.model.clone(),
                    updated_unix_secs: session.updated_unix_secs,
                    source_path: session.source_path.clone(),
                    snippet,
                    rrf_score: acc.rrf_score,
                    match_indices: acc.match_indices,
                    matched_layers: acc.layers,
                })
            })
            .collect();

        hits.sort_unstable_by(|a, b| {
            b.rrf_score
                .total_cmp(&a.rrf_score)
                .then_with(|| b.updated_unix_secs.cmp(&a.updated_unix_secs))
        });
        hits.truncate(limit);

        LlmSearchOutput {
            hits,
            l0_us,
            l1_us,
            l2_us,
        }
    }

    pub fn persist_all(&self) -> Result<()> {
        let l0_path = self.llm_dir.join("llm_l0.fms");
        let l2_path = self.llm_dir.join("llm_l2.fms");
        let sessions_path = self.llm_dir.join("llm_sessions.fms");
        let sync_path = self.llm_dir.join("llm_sync_state.fms");

        self.l0.read().save_encrypted(&l0_path, &self.shard_key)?;
        self.l2.read().save_encrypted(&l2_path, &self.shard_key)?;
        self.l1.commit()?;

        {
            let rt = self.state.read();
            let blob = PersistedSessionsBlob {
                next_offset: self.next_doc_seq.load(Ordering::Relaxed),
                doc_ids: rt.session_to_doc_id.clone(),
                l0_keys: rt.session_to_l0_key.clone(),
                sessions: rt.sessions.values().cloned().collect(),
            };
            let mut raw = serde_json::to_vec(&blob)?;
            let res = atomic_write_encrypted(&sessions_path, &self.shard_key, &raw, DEFAULT_PAGE_SIZE);
            raw.zeroize();
            res?;
        }

        {
            let sync = self.sync_state.read();
            let mut raw = serde_json::to_vec(&*sync)?;
            let res = atomic_write_encrypted(&sync_path, &self.shard_key, &raw, DEFAULT_PAGE_SIZE);
            raw.zeroize();
            res?;
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn upsert_session_locked(
    session: LlmSession,
    searchable_body: &str,
    embedding: &[f32],
    next_doc_seq: &AtomicU64,
    rt: &mut SessionRuntimeState,
    l0: &mut L0PathArena,
    l1: &EncryptedLexicalIndex,
    l2: &mut EncryptedVectorIndex,
) -> Result<()> {
    let sid = session.session_id.clone();
    let doc_id = match rt.session_to_doc_id.get(&sid).copied() {
        Some(existing_id) => existing_id,
        None => {
            let seq = next_doc_seq.fetch_add(1, Ordering::Relaxed);
            LLM_DOC_ID_BASE + seq
        }
    };

    let new_l0_key = make_l0_key(&session);
    if let Some(old_l0_key) = rt.session_to_l0_key.get(&sid)
        && old_l0_key != &new_l0_key
    {
        l0.remove_by_path(old_l0_key);
    }

    l0.upsert(doc_id, &new_l0_key, false);
    l1.upsert_document(
        doc_id,
        &sid,
        &session.title,
        session.provider.as_str(),
        searchable_body,
        session.updated_unix_secs,
    )?;
    l2.upsert(doc_id, &sid, embedding);

    rt.session_to_doc_id.insert(sid.clone(), doc_id);
    rt.doc_id_to_session.insert(doc_id, sid.clone());
    rt.session_to_l0_key.insert(sid.clone(), new_l0_key);
    rt.sessions.insert(sid, session);
    Ok(())
}

fn remove_session_locked(
    sid: &str,
    rt: &mut SessionRuntimeState,
    l0: &mut L0PathArena,
    l1: &EncryptedLexicalIndex,
    l2: &mut EncryptedVectorIndex,
) {
    if let Some(old_l0_key) = rt.session_to_l0_key.remove(sid) {
        l0.remove_by_path(&old_l0_key);
    }
    if let Some(doc_id) = rt.session_to_doc_id.remove(sid) {
        rt.doc_id_to_session.remove(&doc_id);
    }
    let _ = l1.remove_by_path(sid);
    l2.remove_by_path(sid);
    rt.sessions.remove(sid);
}

fn make_l0_key(session: &LlmSession) -> String {
    let safe_sid = session.session_id.replace('/', "_");
    let safe_label = session.display_label().replace('/', "∕");
    format!("llm://{safe_sid}/{safe_label}")
}

fn compute_provider_counts(sessions: &AHashMap<String, LlmSession>) -> Vec<LlmProviderCount> {
    let mut map: AHashMap<&'static str, usize> = AHashMap::new();
    for s in sessions.values() {
        *map.entry(s.provider.as_str()).or_insert(0) += 1;
    }
    let mut vec: Vec<LlmProviderCount> = map
        .into_iter()
        .map(|(p, c)| LlmProviderCount {
            provider: p.to_string(),
            count: c,
        })
        .collect();
    vec.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.provider.cmp(&b.provider)));
    vec
}

/// Extracts a clean, readable dialogue turn excerpt centered around the matched query terms.
pub fn build_dialogue_snippet(session: &LlmSession, raw_query: &str) -> String {
    let tokens: Vec<String> = raw_query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_ascii_lowercase())
        .collect();

    if tokens.is_empty() {
        // Fallback summary from the first assistant reply or opening user turn
        if let Some(turn) = session
            .turns
            .iter()
            .find(|t| t.role == "assistant")
            .or_else(|| session.turns.first())
        {
            let role_tag = if turn.role == "user" { "You" } else { "AI" };
            let compact = collapse_whitespace(&turn.text, 140);
            return format!("{role_tag}: {compact}");
        }
        return String::new();
    }

    let query_lower = raw_query.trim().to_ascii_lowercase();
    let mut best_turn: Option<(&LlmTurn, usize, usize)> = None; // (turn, match_score, byte_pos)

    for turn in &session.turns {
        let lower = turn.text.to_ascii_lowercase();
        let mut score = 0usize;
        let mut first_pos = None;

        if query_lower.len() >= 3
            && let Some(p) = lower.find(&query_lower)
        {
            score += 100;
            first_pos = Some(p);
        }
        for tok in &tokens {
            if let Some(p) = lower.find(tok.as_str()) {
                score += 10 + tok.len();
                if first_pos.is_none() {
                    first_pos = Some(p);
                }
            }
        }
        if score > 0 {
            let pos = first_pos.unwrap_or(0);
            if best_turn.is_none_or(|(_, best_s, _)| score > best_s) {
                best_turn = Some((turn, score, pos));
            }
        }
    }

    if let Some((turn, _, hit_pos)) = best_turn {
        let role_tag = if turn.role == "user" { "You" } else { "AI" };
        let text = &turn.text;
        let mut start = hit_pos.saturating_sub(42);
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + 148).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        let cleaned = text[start..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let prefix = if start > 0 { "…" } else { "" };
        let suffix = if end < text.len() { "…" } else { "" };
        format!("{role_tag}: {prefix}{cleaned}{suffix}")
    } else if let Some(turn) = session.turns.first() {
        let role_tag = if turn.role == "user" { "You" } else { "AI" };
        format!("{role_tag}: {}", collapse_whitespace(&turn.text, 140))
    } else {
        String::new()
    }
}

fn collapse_whitespace(s: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(max_bytes + 4);
    for word in s.split_whitespace() {
        if out.len() + word.len() + 1 > max_bytes {
            out.push('…');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn file_stat(path: &Path) -> (i64, u64) {
    match fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (mtime, m.len())
        }
        Err(_) => (0, 0),
    }
}

#[inline]
fn push_layer(layers: &mut Vec<String>, label: &str) {
    if !layers.iter().any(|l| l == label) {
        layers.push(label.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fms_crypto_vfs::verify_zero_plaintext_on_disk;
    use fms_l2_semantic::MicroSemanticEmbedder;

    #[test]
    fn test_cross_llm_sync_export_import_and_zero_plaintext_encryption() {
        let base = std::env::temp_dir().join(format!(
            "fms_llm_memory_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);

        let fake_home = base.join("home");
        let app_support = base.join("app_support");

        // 1. Mock Claude Code JSONL + Claude Desktop sidecar metadata
        let claude_proj = fake_home.join(".claude/projects/my-app");
        let claude_sidecar_dir =
            fake_home.join("Library/Application Support/Claude/claude-code-sessions/u1/o1");
        fs::create_dir_all(&claude_proj).unwrap();
        fs::create_dir_all(&claude_sidecar_dir).unwrap();

        let claude_canary = "ClaudeSecretAuthFixCanary7719";
        fs::write(
            claude_sidecar_dir.join("local_s1.json"),
            r#"{"sessionId":"local_s1","cliSessionId":"sess-abc-123","title":"Fix Keychain Spawn Loop","cwd":"/Users/dev/fast-mac-search","model":"claude-opus-4-6"}"#,
        )
        .unwrap();
        fs::write(
            claude_proj.join("sess-abc-123.jsonl"),
            r#"{"type":"user","sessionId":"sess-abc-123","cwd":"/Users/dev/fast-mac-search","message":{"role":"user","content":"Why does SecurityAgent keep prompting?"}}
{"type":"assistant","sessionId":"sess-abc-123","message":{"role":"assistant","content":[{"type":"text","text":"Use SecKeychainSetUserInteractionAllowed(0) and canary __CANARY__"}]}}
"#
            .replace("__CANARY__", claude_canary),
        )
        .unwrap();

        // 2. Mock OpenAI Codex JSONL
        let codex_dir = fake_home.join(".codex/sessions/2026/10/31");
        fs::create_dir_all(&codex_dir).unwrap();
        let codex_canary = "CodexNeonSimdBitmaskCanary9921";
        fs::write(
            codex_dir.join("rollout-codex-1.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"codex-uuid-1","cwd":"/Users/dev/engine"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Optimize ARM NEON bitmask filter"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"4x unrolled uint64x2_t scan with __CANARY__"}]}}
"#
            .replace("__CANARY__", codex_canary),
        )
        .unwrap();

        // 3. Mock Gemini / Antigravity transcript.jsonl
        let agy_dir = fake_home
            .join(".gemini/antigravity/brain/conv-gemini-42/.system_generated/logs");
        fs::create_dir_all(&agy_dir).unwrap();
        let gemini_canary = "GeminiZeroTrustVfsCanary5530";
        fs::write(
            agy_dir.join("transcript.jsonl"),
            r#"{"type":"USER_INPUT","created_at":"2026-10-31T08:00:00Z","content":"<USER_REQUEST>Design encrypted Tantivy block directory</USER_REQUEST>"}
{"type":"PLANNER_RESPONSE","created_at":"2026-10-31T08:00:05Z","content":"16KB AES-256-GCM frames with BLAKE3 header MAC __CANARY__"}
"#
            .replace("__CANARY__", gemini_canary),
        )
        .unwrap();

        // 4. Mock ChatGPT `conversations.json` export outside `app_support`
        let export_file = base.join("chatgpt_export.json");
        let chatgpt_canary = "ChatGptQuarterlyRevenueCanary3318";
        fs::write(
            &export_file,
            format!(
                r#"[{{
                    "id": "gpt-conv-99",
                    "title": "Q4 SaaS Pricing Strategy",
                    "update_time": 1761900000.0,
                    "mapping": {{
                        "n1": {{
                            "message": {{
                                "author": {{"role": "user"}},
                                "create_time": 1761900001.0,
                                "content": {{"parts": ["How should we structure enterprise tiers?"]}}
                            }}
                        }},
                        "n2": {{
                            "message": {{
                                "author": {{"role": "assistant"}},
                                "create_time": 1761900005.0,
                                "content": {{"parts": ["Anchor annual ARR tiers with {}"]}}
                            }}
                        }}
                    }}
                }}]"#,
                chatgpt_canary
            ),
        )
        .unwrap();

        let key_provider = HybridKeyProvider::new(app_support.join("keys"));
        let embedder = MicroSemanticEmbedder;

        let mem = EncryptedLlmMemory::open_or_create(
            &app_support,
            &key_provider,
            Some(b"unit-test-passphrase"),
            VfsConfig::default(),
        )
        .unwrap();

        let sync_rep = mem.sync_from_home(&fake_home, &embedder).unwrap();
        assert_eq!(sync_rep.sessions_total, 3);
        assert_eq!(sync_rep.files_parsed, 3);

        // Incremental sync with zero file changes must hit cache 100%
        let cached_rep = mem.sync_from_home(&fake_home, &embedder).unwrap();
        assert_eq!(cached_rep.files_parsed, 0);
        assert_eq!(cached_rep.files_cached, 3);

        // Import ChatGPT export and then DELETE the plaintext source file!
        let imp_rep = mem.import_export_file(&export_file, &embedder).unwrap();
        assert_eq!(imp_rep.sessions_total, 4);
        fs::remove_file(&export_file).unwrap();

        // Sync again: imported ChatGPT session must remain protected even after source export was deleted
        let post_del_rep = mem.sync_from_home(&fake_home, &embedder).unwrap();
        assert_eq!(post_del_rep.sessions_total, 4);

        // Verify ZERO bytes of plaintext canary or titles exist anywhere on disk in `llm_memory`!
        for secret in [
            claude_canary.as_bytes(),
            codex_canary.as_bytes(),
            gemini_canary.as_bytes(),
            chatgpt_canary.as_bytes(),
            b"Fix Keychain Spawn Loop",
            b"Q4 SaaS Pricing Strategy",
        ] {
            verify_zero_plaintext_on_disk(mem.llm_dir(), secret).unwrap();
        }

        // Reopen cold from disk and verify cross-LLM search + provider filtering + full conversation reader!
        drop(mem);
        let reopened = EncryptedLlmMemory::open_or_create(
            &app_support,
            &key_provider,
            Some(b"unit-test-passphrase"),
            VfsConfig::default(),
        )
        .unwrap();
        assert_eq!(reopened.session_count(), 4);

        let h_claude = reopened.search(claude_canary, None, 5, false, &embedder);
        assert_eq!(h_claude.hits.len(), 1);
        assert_eq!(h_claude.hits[0].provider, LlmProvider::Claude);
        assert_eq!(h_claude.hits[0].title, "Fix Keychain Spawn Loop");
        assert!(h_claude.hits[0].snippet.contains(claude_canary));

        let full_session = reopened.get_session(&h_claude.hits[0].session_id).unwrap();
        assert_eq!(full_session.turns.len(), 2);
        assert_eq!(
            full_session.workspace.as_deref(),
            Some("/Users/dev/fast-mac-search")
        );

        let h_codex_filtered = reopened.search(
            "NEON bitmask",
            Some(LlmProvider::Codex),
            5,
            false,
            &embedder,
        );
        assert_eq!(h_codex_filtered.hits.len(), 1);
        assert_eq!(h_codex_filtered.hits[0].provider, LlmProvider::Codex);

        let h_gpt = reopened.search(chatgpt_canary, Some(LlmProvider::ChatGpt), 5, false, &embedder);
        assert_eq!(h_gpt.hits.len(), 1);
        assert_eq!(h_gpt.hits[0].title, "Q4 SaaS Pricing Strategy");

        let _ = fs::remove_dir_all(&base);
    }
}
