//! `fms-core-engine` — 3-Tier Parallel Real-Time Search & Encrypted Multi-Volume Shard Orchestrator.

pub mod ipc;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ahash::AHashMap;
use anyhow::{Context, Result};
use fms_crypto_vfs::{
    DEFAULT_PAGE_SIZE, DecryptedPageCache, HybridKeyProvider, KeyBackendKind, ShardKey, VfsConfig,
    atomic_write_encrypted, read_encrypted_all,
};
use fms_fs_indexer::{
    CrawlConfig, EntryKind, FsEventAction, FsEventsWatcher, ShardStorageMode, VolumeInfo,
    crawl_volume_root_with_progress, is_artifact_dir_name, is_system_junk_name,
    should_skip_internal_or_noisy_path,
};
use fms_l0_arena::L0PathArena;
use fms_l1_lexical::{EncryptedLexicalIndex, extract_text_from_file};
use fms_l2_semantic::{EncryptedVectorIndex, MicroSemanticEmbedder, SemanticEmbedder};
use fms_system1_predictor::{
    GhostPrediction, QueryContext, System1Predictor, classify_query_intent,
};
use parking_lot::RwLock;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const RRF_K: f32 = 60.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardManifest {
    pub volume: VolumeInfo,
    pub mode: ShardStorageMode,
    pub indexed_roots: Vec<PathBuf>,
    pub last_fsevent_id: u64,
    pub updated_unix_secs: u64,
}

pub struct VolumeShard {
    pub shard_dir: PathBuf,
    pub shard_key: ShardKey,
    pub key_backend: KeyBackendKind,
    pub manifest: RwLock<ShardManifest>,
    pub l0: RwLock<L0PathArena>,
    pub l1: EncryptedLexicalIndex,
    pub l2: RwLock<EncryptedVectorIndex>,
    pub system1: RwLock<System1Predictor>,
    pub cached_l0_len: AtomicUsize,
    pub cached_l2_len: AtomicUsize,
    pub cached_mlocked: AtomicBool,
}

impl VolumeShard {
    pub fn persist_snapshots(&self) -> Result<()> {
        let l0_path = self.shard_dir.join("l0_arena.fms");
        let l2_path = self.shard_dir.join("l2_vectors.fms");
        let s1_path = self.shard_dir.join("system1.fms");
        let mf_path = self.shard_dir.join("manifest.fms");

        self.l0.read().save_encrypted(&l0_path, &self.shard_key)?;
        self.l2.read().save_encrypted(&l2_path, &self.shard_key)?;
        self.system1
            .read()
            .save_encrypted(&s1_path, &self.shard_key)?;
        self.l1.commit()?;

        let mut mf_bytes = serde_json::to_vec(&*self.manifest.read())?;
        atomic_write_encrypted(&mf_path, &self.shard_key, &mf_bytes, DEFAULT_PAGE_SIZE)?;
        mf_bytes.zeroize();
        Ok(())
    }
}

/// Unified ranked search result returned to the CLI or native Swift `NSPanel` UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedHit {
    pub doc_id: u64,
    pub path: String,
    pub filename: String,
    pub rrf_score: f32,
    pub snippet: String,
    pub match_indices: Vec<u32>,
    pub matched_layers: Vec<String>,
    pub is_dir: bool,
    pub volume_name: String,
    pub volume_online: bool,
}

/// Complete telemetry + Ghost-Text + Ranked Results payload emitted on every keystroke.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub generation: u64,
    pub intent: String,
    pub ghost_prediction: Option<GhostPrediction>,
    pub stage1_l0_us: u64,
    pub stage2_l1_us: u64,
    pub stage3_l2_us: u64,
    pub total_elapsed_us: u64,
    pub total_indexed_paths: usize,
    pub hits: Vec<RankedHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub volume_name: String,
    pub volume_uuid: String,
    pub is_external: bool,
    pub storage_mode: ShardStorageMode,
    pub shard_dir: PathBuf,
    pub key_backend: String,
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub content_docs_indexed: u64,
    pub l0_paths_total: usize,
    pub mlocked: bool,
    pub crawl_ms: f64,
    pub total_ms: f64,
    pub tcc_restricted_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardStatusSummary {
    pub volume_name: String,
    pub volume_uuid: String,
    pub mount_point: PathBuf,
    pub is_external: bool,
    pub is_online: bool,
    pub mode: ShardStorageMode,
    pub shard_dir: PathBuf,
    pub key_backend: String,
    pub l0_paths: usize,
    pub l1_docs: u64,
    pub l2_vectors: usize,
    pub mlocked: bool,
    pub last_fsevent_id: u64,
    pub indexed_roots: Vec<PathBuf>,
}

/// Real-time indexing progress telemetry surfaced to the macOS Menu Bar pulsing lightning icon & hover tooltip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexingProgressSnapshot {
    pub is_indexing: bool,
    pub phase: String,
    pub active_target: String,
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub entries_ingested: u64,
    pub entries_total: u64,
}

#[derive(Default)]
struct LiveIndexingTelemetry {
    active_jobs: AtomicU64,
    is_indexing: AtomicBool,
    phase: RwLock<String>,
    active_target: RwLock<String>,
    files_scanned: AtomicU64,
    dirs_scanned: AtomicU64,
    entries_ingested: AtomicU64,
    entries_total: AtomicU64,
}

impl LiveIndexingTelemetry {
    fn snapshot(&self) -> IndexingProgressSnapshot {
        IndexingProgressSnapshot {
            is_indexing: self.is_indexing.load(Ordering::Relaxed),
            phase: self.phase.read().clone(),
            active_target: self.active_target.read().clone(),
            files_scanned: self.files_scanned.load(Ordering::Relaxed),
            dirs_scanned: self.dirs_scanned.load(Ordering::Relaxed),
            entries_ingested: self.entries_ingested.load(Ordering::Relaxed),
            entries_total: self.entries_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub app_support_dir: PathBuf,
    pub secure_enclave_status: String,
    pub shard_count: usize,
    pub total_paths: usize,
    pub shards: Vec<ShardStatusSummary>,
    #[serde(default)]
    pub indexing: IndexingProgressSnapshot,
}

pub struct EngineConfig {
    pub app_support_dir: PathBuf,
    pub passphrase: Option<Vec<u8>>,
    pub crawl_config: CrawlConfig,
    pub vfs_config: VfsConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let passphrase = std::env::var("FMS_PASSPHRASE").ok().map(|s| s.into_bytes());
        Self {
            app_support_dir: ipc::default_app_support_dir(),
            passphrase,
            crawl_config: CrawlConfig::default(),
            vfs_config: VfsConfig::default(),
        }
    }
}

/// Top-level Fast Mac Search Core Orchestrator (`searchd` / embedded library engine).
pub struct SearchEngine {
    config: EngineConfig,
    key_provider: HybridKeyProvider,
    cached_se_status: String,
    embedder: Arc<dyn SemanticEmbedder>,
    /// Map of `volume_uuid_string -> Arc<VolumeShard>`
    shards: RwLock<AHashMap<String, Arc<VolumeShard>>>,
    next_doc_id: AtomicU64,
    active_generation: AtomicU64,
    active_watchers: RwLock<AHashMap<String, (usize, FsEventsWatcher)>>,
    progress: LiveIndexingTelemetry,
}

impl SearchEngine {
    pub fn open(config: EngineConfig) -> Result<Self> {
        fs::create_dir_all(&config.app_support_dir)?;
        let key_dir = config.app_support_dir.join("keys");
        let key_provider = HybridKeyProvider::new(key_dir);
        let cached_se_status = format!("{:?}", key_provider.probe_secure_enclave());
        let engine = Self {
            config,
            key_provider,
            cached_se_status,
            embedder: Arc::new(MicroSemanticEmbedder),
            shards: RwLock::new(AHashMap::new()),
            next_doc_id: AtomicU64::new(1),
            active_generation: AtomicU64::new(0),
            active_watchers: RwLock::new(AHashMap::new()),
            progress: LiveIndexingTelemetry::default(),
        };
        engine.autoload_host_cached_shards()?;
        Ok(engine)
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn SemanticEmbedder>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Automatically loads all encrypted host-cached shards in `<app_support_dir>/shards/*`,
    /// including external drives that are currently unplugged (for instant offline search!).
    pub fn autoload_host_cached_shards(&self) -> Result<usize> {
        let shards_root = self.config.app_support_dir.join("shards");
        if !shards_root.exists() {
            return Ok(0);
        }
        let mut loaded = 0usize;
        for entry in fs::read_dir(&shards_root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("shard_envelope.json").exists() {
                if let Ok(shard) = self.open_existing_shard_dir(&path) {
                    let uuid_str = shard.manifest.read().volume.uuid_string.clone();
                    self.shards.write().insert(uuid_str, shard);
                    loaded += 1;
                }
            }
        }
        Ok(loaded)
    }

    /// Mounts a Portable Encrypted Shard (`<ExternalDrive>/.fastindex/`) without re-indexing!
    pub fn mount_portable_shard(&self, shard_dir: &Path) -> Result<ShardStatusSummary> {
        let shard = self.open_existing_shard_dir(shard_dir)?;
        let summary = shard_to_summary(&shard);
        let uuid_str = summary.volume_uuid.clone();
        self.shards.write().insert(uuid_str, shard);
        Ok(summary)
    }

    fn open_existing_shard_dir(&self, shard_dir: &Path) -> Result<Arc<VolumeShard>> {
        let env_raw = fs::read_to_string(shard_dir.join("shard_envelope.json"))?;
        let env: fms_crypto_vfs::ShardKeyEnvelope = serde_json::from_str(&env_raw)?;
        let vol_uuid = fms_crypto_vfs::key_provider::hex_decode_16(&env.volume_uuid_hex)?;

        let (shard_key, key_backend, _) = self.key_provider.resolve_or_init_shard_key(
            shard_dir,
            &vol_uuid,
            self.config.passphrase.as_deref(),
        )?;

        let cache = Arc::new(DecryptedPageCache::new(self.config.vfs_config.cache_pages));
        let mf_bytes =
            read_encrypted_all(&shard_dir.join("manifest.fms"), &shard_key, Arc::clone(&cache))?;
        let mut manifest: ShardManifest = serde_json::from_slice(&mf_bytes)?;
        manifest.volume.is_online = manifest.volume.mount_point.exists();

        let l0 = L0PathArena::load_encrypted(
            &shard_dir.join("l0_arena.fms"),
            &shard_key,
            Arc::clone(&cache),
        )?;
        let l1 = EncryptedLexicalIndex::open_or_create(
            shard_dir.join("l1_tantivy"),
            shard_key.clone(),
            self.config.vfs_config.clone(),
        )?;
        let l2 = EncryptedVectorIndex::load_encrypted(
            &shard_dir.join("l2_vectors.fms"),
            &shard_key,
            Arc::clone(&cache),
        )?;
        let system1 = System1Predictor::load_encrypted(
            &shard_dir.join("system1.fms"),
            &shard_key,
            cache,
        )
        .unwrap_or_default();

        let l0_len = l0.len();
        let l2_len = l2.len();
        let mlocked = l0.is_mlocked();
        let bump = (l0_len as u64) + 1000;
        self.next_doc_id.fetch_add(bump, Ordering::Relaxed);

        Ok(Arc::new(VolumeShard {
            shard_dir: shard_dir.to_path_buf(),
            shard_key,
            key_backend,
            manifest: RwLock::new(manifest),
            l0: RwLock::new(l0),
            l1,
            l2: RwLock::new(l2),
            system1: RwLock::new(system1),
            cached_l0_len: AtomicUsize::new(l0_len),
            cached_l2_len: AtomicUsize::new(l2_len),
            cached_mlocked: AtomicBool::new(mlocked),
        }))
    }

    fn get_or_create_shard(
        &self,
        root: &Path,
        mode: ShardStorageMode,
    ) -> Result<Arc<VolumeShard>> {
        let vol = VolumeInfo::for_path(root)?;
        if let Some(existing) = self.shards.read().get(&vol.uuid_string).cloned() {
            return Ok(existing);
        }

        let shard_dir = vol.shard_directory(mode, &self.config.app_support_dir);
        if shard_dir.join("manifest.fms").exists() {
            if let Ok(loaded) = self.open_existing_shard_dir(&shard_dir) {
                self.shards
                    .write()
                    .insert(vol.uuid_string.clone(), Arc::clone(&loaded));
                return Ok(loaded);
            } else if self.config.passphrase.is_none() {
                // Stale shard encrypted under a rotated/legacy key: reset cleanly before re-indexing.
                let _ = fs::remove_dir_all(&shard_dir);
            }
        }

        let (shard_key, key_backend, _) = self.key_provider.resolve_or_init_shard_key(
            &shard_dir,
            &vol.uuid,
            self.config.passphrase.as_deref(),
        )?;

        let l1 = EncryptedLexicalIndex::open_or_create(
            shard_dir.join("l1_tantivy"),
            shard_key.clone(),
            self.config.vfs_config.clone(),
        )?;

        let manifest = ShardManifest {
            volume: vol.clone(),
            mode,
            indexed_roots: Vec::new(),
            last_fsevent_id: FsEventsWatcher::current_system_event_id(),
            updated_unix_secs: unix_now(),
        };

        let shard = Arc::new(VolumeShard {
            shard_dir,
            shard_key,
            key_backend,
            manifest: RwLock::new(manifest),
            l0: RwLock::new(L0PathArena::default()),
            l1,
            l2: RwLock::new(EncryptedVectorIndex::default()),
            system1: RwLock::new(System1Predictor::new()),
            cached_l0_len: AtomicUsize::new(0),
            cached_l2_len: AtomicUsize::new(0),
            cached_mlocked: AtomicBool::new(false),
        });

        self.shards
            .write()
            .insert(vol.uuid_string, Arc::clone(&shard));
        Ok(shard)
    }

    /// Crawls `root` with `getattrlistbulk(2)`, extracts text & 128d semantic vectors in parallel
    /// across Apple Silicon cores, populates L0+L1+L2+System1, wires L0 via `mlock(2)`, and persists
    /// encrypted shards to disk.
    pub fn index_root(&self, root: &Path, mode: ShardStorageMode) -> Result<IndexReport> {
        let total_started = Instant::now();
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("cannot canonicalize {:?}", root))?;

        let shard = self.get_or_create_shard(&canonical_root, mode)?;
        let checkpoint_before = FsEventsWatcher::current_system_event_id();

        let target_label = canonical_root
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| shard.manifest.read().volume.name.clone());

        self.progress.active_jobs.fetch_add(1, Ordering::SeqCst);
        self.progress.is_indexing.store(true, Ordering::SeqCst);
        *self.progress.active_target.write() = target_label;
        *self.progress.phase.write() = "Scanning filesystem".into();
        self.progress.files_scanned.store(0, Ordering::Relaxed);
        self.progress.dirs_scanned.store(0, Ordering::Relaxed);
        self.progress.entries_ingested.store(0, Ordering::Relaxed);
        self.progress.entries_total.store(0, Ordering::Relaxed);

        struct IndexingJobReset<'a>(&'a LiveIndexingTelemetry);
        impl Drop for IndexingJobReset<'_> {
            fn drop(&mut self) {
                if self.0.active_jobs.fetch_sub(1, Ordering::SeqCst) == 1 {
                    self.0.is_indexing.store(false, Ordering::SeqCst);
                    *self.0.phase.write() = "Idle".into();
                }
            }
        }
        let _job_guard = IndexingJobReset(&self.progress);

        let (entries, crawl_stats) = crawl_volume_root_with_progress(
            &canonical_root,
            &self.config.crawl_config,
            &|files, dirs| {
                self.progress.files_scanned.store(files, Ordering::Relaxed);
                self.progress.dirs_scanned.store(dirs, Ordering::Relaxed);
            },
        )?;

        let total_entries = entries.len() as u64;
        self.progress
            .files_scanned
            .store(crawl_stats.files_scanned, Ordering::Relaxed);
        self.progress
            .dirs_scanned
            .store(crawl_stats.dirs_scanned, Ordering::Relaxed);
        self.progress
            .entries_total
            .store(total_entries, Ordering::Relaxed);
        *self.progress.phase.write() = "Encrypting & indexing".into();

        // Parallel content extraction + 128-dim semantic embedding generation via Rayon in streaming chunks
        // so L0 search remains sub-0.5ms responsive while large drives index in the background!
        struct PreparedDoc {
            doc_id: u64,
            path_str: String,
            filename: String,
            ext: String,
            mtime_secs: i64,
            is_dir: bool,
            text_and_vec: Option<(String, Vec<f32>)>,
        }

        let embedder = Arc::clone(&self.embedder);
        let mut content_docs_indexed = 0u64;
        let mut ingested_so_far = 0u64;

        for raw_chunk in entries.chunks(2048) {
            let prepared: Vec<PreparedDoc> = raw_chunk
                .par_iter()
                .map(|entry| {
                    let doc_id = self.next_doc_id.fetch_add(1, Ordering::Relaxed);
                    let path_str = entry.path.to_string_lossy().into_owned();
                    let filename = entry
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    let ext = entry
                        .path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_string();
                    let is_dir = entry.kind == EntryKind::Directory;

                    let text_and_vec = if !is_dir && !entry.l0_only {
                        extract_text_from_file(&entry.path, entry.size_bytes).map(|text| {
                            let vec = embedder.embed_document(&path_str, &text);
                            (text, vec)
                        })
                    } else {
                        None
                    };

                    PreparedDoc {
                        doc_id,
                        path_str,
                        filename,
                        ext,
                        mtime_secs: entry.mtime_secs,
                        is_dir,
                        text_and_vec,
                    }
                })
                .collect();

            let chunk_len = prepared.len() as u64;
            {
                let mut l0 = shard.l0.write();
                let mut l2 = shard.l2.write();
                let mut s1 = shard.system1.write();

                for item in prepared {
                    l0.upsert(item.doc_id, &item.path_str, item.is_dir);
                    s1.ingest_filename_vocabulary(&item.filename);

                    if let Some((text, vec)) = item.text_and_vec {
                        shard.l1.upsert_document(
                            item.doc_id,
                            &item.path_str,
                            &item.filename,
                            &item.ext,
                            &text,
                            item.mtime_secs,
                        )?;
                        l2.upsert(item.doc_id, &item.path_str, &vec);
                        content_docs_indexed += 1;
                    }
                }
                shard.cached_l0_len.store(l0.len(), Ordering::Relaxed);
                shard.cached_l2_len.store(l2.len(), Ordering::Relaxed);
            }

            ingested_so_far += chunk_len;
            self.progress
                .entries_ingested
                .store(ingested_so_far, Ordering::Relaxed);
        }

        let (mlocked, l0_paths_total) = {
            let mut l0 = shard.l0.write();
            let locked = l0.lock_memory();
            (locked, l0.len())
        };
        shard.cached_l0_len.store(l0_paths_total, Ordering::Relaxed);
        shard.cached_mlocked.store(mlocked, Ordering::Relaxed);

        *self.progress.phase.write() = "Sealing encrypted shard".into();

        {
            let mut mf = shard.manifest.write();
            if !mf.indexed_roots.contains(&canonical_root) {
                mf.indexed_roots.push(canonical_root);
            }
            mf.last_fsevent_id = checkpoint_before;
            mf.updated_unix_secs = unix_now();
            mf.volume.is_online = true;
        }

        shard.persist_snapshots()?;

        let mf_read = shard.manifest.read();
        Ok(IndexReport {
            volume_name: mf_read.volume.name.clone(),
            volume_uuid: mf_read.volume.uuid_string.clone(),
            is_external: mf_read.volume.is_external,
            storage_mode: mf_read.mode,
            shard_dir: shard.shard_dir.clone(),
            key_backend: shard.key_backend.to_string(),
            files_scanned: crawl_stats.files_scanned,
            dirs_scanned: crawl_stats.dirs_scanned,
            content_docs_indexed,
            l0_paths_total,
            mlocked,
            crawl_ms: crawl_stats.elapsed_ms,
            total_ms: total_started.elapsed().as_secs_f64() * 1000.0,
            tcc_restricted_count: crawl_stats.tcc_restricted_paths.len(),
        })
    }

    /// Starts real-time macOS `FSEvents` delta watching on all indexed roots across loaded shards,
    /// resuming from each shard's persisted `last_fsevent_id` checkpoint!
    pub fn start_realtime_watchers(self: &Arc<Self>) -> Result<usize> {
        let shards: Vec<Arc<VolumeShard>> = self.shards.read().values().cloned().collect();
        let mut started_count = 0usize;

        for shard in shards {
            let (vol_uuid, roots, since_id, online) = {
                let mf = shard.manifest.read();
                (
                    mf.volume.uuid_string.clone(),
                    mf.indexed_roots.clone(),
                    mf.last_fsevent_id,
                    mf.volume.mount_point.exists(),
                )
            };
            if roots.is_empty() || !online {
                continue;
            }
            if let Some((watched_root_count, _)) = self.active_watchers.read().get(&vol_uuid)
                && *watched_root_count == roots.len()
            {
                started_count += 1;
                continue;
            }

            let watcher = FsEventsWatcher::start(&roots, since_id, Duration::from_millis(60))?;
            let rx = watcher.receiver().clone();
            self.active_watchers
                .write()
                .insert(vol_uuid, (roots.len(), watcher));
            started_count += 1;

            let engine_ref = Arc::clone(self);
            let shard_ref = Arc::clone(&shard);
            thread::Builder::new()
                .name("fms-fsevents-ingester".into())
                .spawn(move || {
                    engine_ref.fsevents_ingest_loop(shard_ref, rx);
                })?;
        }
        Ok(started_count)
    }

    fn fsevents_ingest_loop(
        &self,
        shard: Arc<VolumeShard>,
        rx: crossbeam_channel::Receiver<fms_fs_indexer::FsDeltaEvent>,
    ) {
        let mut dirty_l1 = 0usize;
        let mut last_commit = Instant::now();
        let commit_cooldown = Duration::from_millis(1500);

        loop {
            let evt = if dirty_l1 > 0 {
                match rx.recv_timeout(commit_cooldown) {
                    Ok(e) => e,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        let _ = shard.l1.commit();
                        dirty_l1 = 0;
                        last_commit = Instant::now();
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(e) => e,
                    Err(_) => break,
                }
            };

            // Never self-index our own encrypted shard storage, IPC cache, or noisy system log/cache dirs!
            if should_skip_internal_or_noisy_path(&evt.path)
                || evt.path.starts_with(&self.config.app_support_dir)
                || evt.path.starts_with(&shard.shard_dir)
            {
                continue;
            }

            let name = evt
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if is_system_junk_name(name)
                || evt.path.components().any(|c| {
                    c.as_os_str()
                        .to_str()
                        .is_some_and(|s| is_artifact_dir_name(s) || is_system_junk_name(s))
                })
            {
                continue;
            }

            let path_str = evt.path.to_string_lossy().into_owned();
            match evt.action {
                FsEventAction::Remove => {
                    {
                        let mut l0 = shard.l0.write();
                        l0.remove_by_path(&path_str);
                        shard.cached_l0_len.store(l0.len(), Ordering::Relaxed);
                    }
                    let _ = shard.l1.remove_by_path(&path_str);
                    {
                        let mut l2 = shard.l2.write();
                        l2.remove_by_path(&path_str);
                        shard.cached_l2_len.store(l2.len(), Ordering::Relaxed);
                    }
                    dirty_l1 += 1;
                }
                FsEventAction::Upsert | FsEventAction::RescanSubtree => {
                    if let Ok(meta) = fs::symlink_metadata(&evt.path) {
                        let is_dir = meta.is_dir();
                        let doc_id = self.next_doc_id.fetch_add(1, Ordering::Relaxed);
                        {
                            let mut l0 = shard.l0.write();
                            l0.upsert(doc_id, &path_str, is_dir);
                            shard.cached_l0_len.store(l0.len(), Ordering::Relaxed);
                        }
                        shard.system1.write().ingest_filename_vocabulary(name);

                        if meta.is_file()
                            && let Some(text) = extract_text_from_file(&evt.path, meta.len())
                        {
                            let ext = evt
                                .path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("");
                            let mtime = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            let vec = self.embedder.embed_document(&path_str, &text);
                            let _ = shard
                                .l1
                                .upsert_document(doc_id, &path_str, name, ext, &text, mtime);
                            {
                                let mut l2 = shard.l2.write();
                                l2.upsert(doc_id, &path_str, &vec);
                                shard.cached_l2_len.store(l2.len(), Ordering::Relaxed);
                            }
                            dirty_l1 += 1;
                        }
                    }
                }
            }

            shard.manifest.write().last_fsevent_id = evt.event_id;

            // Coalesce encrypted Tantivy segment commits over 1.5s windows (or large 256-doc batches)
            if dirty_l1 >= 256 || (dirty_l1 > 0 && rx.is_empty() && last_commit.elapsed() >= commit_cooldown) {
                let _ = shard.l1.commit();
                dirty_l1 = 0;
                last_commit = Instant::now();
            }
        }
    }

    /// Executes the **3-Tier Parallel System-1 Keystroke Pipeline** with atomic generation
    /// cancellation and Dynamic Query-Length-Weighted Reciprocal Rank Fusion (RRF)!
    pub fn search(
        &self,
        raw_query: &str,
        limit: usize,
        ctx: &QueryContext,
        l0_only: bool,
    ) -> SearchResponse {
        let total_started = Instant::now();
        let my_gen = self.active_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let trimmed = raw_query.trim();
        let weights = classify_query_intent(trimmed);

        let shards: Vec<Arc<VolumeShard>> = self.shards.read().values().cloned().collect();
        let total_indexed_paths: usize = shards.iter().map(|s| s.l0.read().len()).sum();

        if trimmed.is_empty() || shards.is_empty() || limit == 0 {
            return SearchResponse {
                query: raw_query.to_string(),
                generation: my_gen,
                intent: weights.intent_label.to_string(),
                ghost_prediction: None,
                stage1_l0_us: 0,
                stage2_l1_us: 0,
                stage3_l2_us: 0,
                total_elapsed_us: total_started.elapsed().as_micros() as u64,
                total_indexed_paths,
                hits: Vec::new(),
            };
        }

        // =========================================================================
        // STAGE 1: System 1 Ghost-Text + Markov Frecency + L0 ARM NEON Arena (<0.5ms)
        // =========================================================================
        let stage1_start = Instant::now();
        let mut ghost_prediction: Option<GhostPrediction> = None;

        #[derive(Default)]
        struct CandidateAccum {
            doc_id: u64,
            path: String,
            filename: String,
            rrf_score: f32,
            snippet: String,
            match_indices: Vec<u32>,
            layers: Vec<String>,
            is_dir: bool,
            volume_name: String,
            volume_online: bool,
        }

        let mut merged: AHashMap<String, CandidateAccum> = AHashMap::with_capacity(limit * 8);
        let stage_k = (limit * 3).max(24);

        for shard in &shards {
            let (vol_name, vol_online) = {
                let mf = shard.manifest.read();
                (mf.volume.name.clone(), mf.volume.mount_point.exists())
            };

            let s1 = shard.system1.read();
            if ghost_prediction.is_none() {
                ghost_prediction = s1.predict_ghost_text(raw_query);
            }

            // 1A. Contextual Markov Frecency Reflex
            let frecency_hits = s1.rank_frecency(trimmed, ctx, stage_k);
            for (rank, (path, _raw_fscore)) in frecency_hits.into_iter().enumerate() {
                let rrf = weights.frecency / (RRF_K + (rank + 1) as f32);
                let filename = Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path)
                    .to_string();
                let entry = merged.entry(path.clone()).or_insert_with(|| CandidateAccum {
                    doc_id: 0,
                    path,
                    filename,
                    volume_name: vol_name.clone(),
                    volume_online: vol_online,
                    ..Default::default()
                });
                entry.rrf_score += rrf;
                push_unique_layer(&mut entry.layers, "Frecency");
            }
            drop(s1);

            // 1B. L0 ARM NEON SIMD Bitmask + Nucleo Smith-Waterman Fuzzy Path Arena
            let (l0_hits, _) = shard.l0.read().search(trimmed, stage_k);
            for (rank, m) in l0_hits.into_iter().enumerate() {
                let rrf = weights.l0_path / (RRF_K + (rank + 1) as f32);
                let entry = merged.entry(m.path.clone()).or_insert_with(|| CandidateAccum {
                    doc_id: m.doc_id,
                    path: m.path,
                    filename: m.filename,
                    is_dir: m.is_dir,
                    volume_name: vol_name.clone(),
                    volume_online: vol_online,
                    ..Default::default()
                });
                if entry.doc_id == 0 {
                    entry.doc_id = m.doc_id;
                }
                entry.is_dir = m.is_dir;
                entry.match_indices = m.match_indices;
                entry.rrf_score += rrf;
                push_unique_layer(&mut entry.layers, "L0");
            }
        }
        let stage1_l0_us = stage1_start.elapsed().as_micros() as u64;

        // =========================================================================
        // STAGE 2 & STAGE 3 (Parallel): L1 Encrypted Tantivy BM25 + L2 Semantic Vector
        // Aborted automatically if a newer keystroke incremented `active_generation`
        // or if query is a single character / explicit `l0_only` reflex query!
        // =========================================================================
        let mut stage2_l1_us = 0u64;
        let mut stage3_l2_us = 0u64;

        let run_deep_tiers = !l0_only
            && trimmed.chars().count() >= 2
            && self.active_generation.load(Ordering::Relaxed) == my_gen;

        if run_deep_tiers {
            let embedder = Arc::clone(&self.embedder);
            let run_l2 = trimmed.chars().count() >= 3;

            let ((l1_batches, l1_us), (l2_batches, l2_us)) = rayon::join(
                || {
                    let t = Instant::now();
                    let mut all = Vec::with_capacity(shards.len());
                    for shard in &shards {
                        if self.active_generation.load(Ordering::Relaxed) != my_gen {
                            break;
                        }
                        if let Ok((hits, _)) = shard.l1.search(trimmed, stage_k) {
                            let mf = shard.manifest.read();
                            all.push((
                                mf.volume.name.clone(),
                                mf.volume.mount_point.exists(),
                                hits,
                            ));
                        }
                    }
                    (all, t.elapsed().as_micros() as u64)
                },
                || {
                    let t = Instant::now();
                    if !run_l2 {
                        return (Vec::new(), 0u64);
                    }
                    let qvec = embedder.embed_query(trimmed);
                    let mut all = Vec::with_capacity(shards.len());
                    for shard in &shards {
                        if self.active_generation.load(Ordering::Relaxed) != my_gen {
                            break;
                        }
                        let (hits, _) = shard.l2.read().search(&qvec, stage_k);
                        let mf = shard.manifest.read();
                        all.push((
                            mf.volume.name.clone(),
                            mf.volume.mount_point.exists(),
                            hits,
                        ));
                    }
                    (all, t.elapsed().as_micros() as u64)
                },
            );

            stage2_l1_us = l1_us;
            stage3_l2_us = l2_us;

            for (vol_name, vol_online, hits) in l1_batches {
                for (rank, m) in hits.into_iter().enumerate() {
                    let rrf = weights.l1_lexical / (RRF_K + (rank + 1) as f32);
                    let entry = merged.entry(m.path.clone()).or_insert_with(|| CandidateAccum {
                        doc_id: m.doc_id,
                        path: m.path,
                        filename: m.filename,
                        volume_name: vol_name.clone(),
                        volume_online: vol_online,
                        ..Default::default()
                    });
                    if entry.snippet.is_empty() && !m.snippet.is_empty() {
                        entry.snippet = m.snippet;
                    }
                    entry.rrf_score += rrf;
                    push_unique_layer(&mut entry.layers, "L1");
                }
            }

            for (vol_name, vol_online, hits) in l2_batches {
                for (rank, m) in hits.into_iter().enumerate() {
                    let rrf = weights.l2_semantic / (RRF_K + (rank + 1) as f32);
                    let filename = Path::new(&m.path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&m.path)
                        .to_string();
                    let entry = merged.entry(m.path.clone()).or_insert_with(|| CandidateAccum {
                        doc_id: m.doc_id,
                        path: m.path,
                        filename,
                        volume_name: vol_name.clone(),
                        volume_online: vol_online,
                        ..Default::default()
                    });
                    entry.rrf_score += rrf;
                    push_unique_layer(&mut entry.layers, "L2");
                }
            }
        }

        // Apply ambient System 1 workspace & frontmost-app context multiplier!
        let first_s1 = shards.first().map(|s| s.system1.read());
        let mut final_hits: Vec<RankedHit> = merged
            .into_values()
            .map(|mut c| {
                if let Some(ref s1) = first_s1 {
                    c.rrf_score *= s1.ambient_context_multiplier(&c.path, ctx);
                }
                RankedHit {
                    doc_id: c.doc_id,
                    path: c.path,
                    filename: c.filename,
                    rrf_score: c.rrf_score,
                    snippet: c.snippet,
                    match_indices: c.match_indices,
                    matched_layers: c.layers,
                    is_dir: c.is_dir,
                    volume_name: c.volume_name,
                    volume_online: c.volume_online,
                }
            })
            .collect();
        drop(first_s1);

        final_hits.sort_unstable_by(|a, b| b.rrf_score.total_cmp(&a.rrf_score));
        final_hits.truncate(limit);

        SearchResponse {
            query: raw_query.to_string(),
            generation: my_gen,
            intent: weights.intent_label.to_string(),
            ghost_prediction,
            stage1_l0_us,
            stage2_l1_us,
            stage3_l2_us,
            total_elapsed_us: total_started.elapsed().as_micros() as u64,
            total_indexed_paths,
            hits: final_hits,
        }
    }

    /// Reinforces the System 1 Contextual Markov Frecency Graph when the user opens a search hit.
    pub fn record_selection(&self, query: &str, selected_path: &str, ctx: &QueryContext) {
        let shards: Vec<Arc<VolumeShard>> = self.shards.read().values().cloned().collect();
        for shard in shards {
            let belongs = {
                let mf = shard.manifest.read();
                selected_path.starts_with(mf.volume.mount_point.to_string_lossy().as_ref())
            };
            if belongs {
                shard
                    .system1
                    .write()
                    .record_selection(query, selected_path, ctx);
                let s1_path = shard.shard_dir.join("system1.fms");
                let _ = shard.system1.read().save_encrypted(&s1_path, &shard.shard_key);
                break;
            }
        }
    }

    /// Removes an indexed volume shard from memory and deletes its encrypted shard directory on disk.
    pub fn remove_shard(&self, volume_uuid: &str) -> Result<bool> {
        self.active_watchers.write().remove(volume_uuid);
        let removed = self.shards.write().remove(volume_uuid);
        if let Some(shard) = removed {
            if shard.shard_dir.exists() {
                let _ = fs::remove_dir_all(&shard.shard_dir);
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub fn status(&self) -> EngineStatus {
        let shards_guard = self.shards.read();
        let mut summaries: Vec<ShardStatusSummary> = shards_guard
            .values()
            .map(|s| shard_to_summary(s))
            .collect();
        summaries.sort_by(|a, b| a.volume_name.cmp(&b.volume_name));
        let total_paths = summaries.iter().map(|s| s.l0_paths).sum();

        EngineStatus {
            app_support_dir: self.config.app_support_dir.clone(),
            secure_enclave_status: self.cached_se_status.clone(),
            shard_count: summaries.len(),
            total_paths,
            shards: summaries,
            indexing: self.progress.snapshot(),
        }
    }
}

fn shard_to_summary(shard: &VolumeShard) -> ShardStatusSummary {
    let mf = shard.manifest.read();
    ShardStatusSummary {
        volume_name: mf.volume.name.clone(),
        volume_uuid: mf.volume.uuid_string.clone(),
        mount_point: mf.volume.mount_point.clone(),
        is_external: mf.volume.is_external,
        is_online: mf.volume.mount_point.exists(),
        mode: mf.mode,
        shard_dir: shard.shard_dir.clone(),
        key_backend: shard.key_backend.to_string(),
        l0_paths: shard.cached_l0_len.load(Ordering::Relaxed),
        l1_docs: shard.l1.num_docs(),
        l2_vectors: shard.cached_l2_len.load(Ordering::Relaxed),
        mlocked: shard.cached_mlocked.load(Ordering::Relaxed),
        last_fsevent_id: mf.last_fsevent_id,
        indexed_roots: mf.indexed_roots.clone(),
    }
}

#[inline]
fn push_unique_layer(layers: &mut Vec<String>, label: &str) {
    if !layers.iter().any(|l| l == label) {
        layers.push(label.to_string());
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fms_crypto_vfs::verify_zero_plaintext_on_disk;

    #[test]
    fn test_end_to_end_3tier_pipeline_and_zero_plaintext_shards() {
        let base = std::env::temp_dir().join(format!(
            "fms_core_e2e_{}_{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&base);
        let corpus = base.join("corpus");
        let home = base.join("app_support");
        fs::create_dir_all(corpus.join("docs")).unwrap();
        fs::create_dir_all(corpus.join("src")).unwrap();

        let canary = "UltraSecretCanaryToken88412";
        fs::write(
            corpus.join("docs/q3_revenue_forecast.md"),
            format!("# Q3 Revenue Forecast\nProjected ARR and quarterly budget margins. {canary}"),
        )
        .unwrap();
        fs::write(
            corpus.join("src/encrypted_vfs.rs"),
            "pub struct EncryptedBlockDirectory { /* 16KB AES-256-GCM */ }",
        )
        .unwrap();

        let cfg = EngineConfig {
            app_support_dir: home.clone(),
            passphrase: Some(b"test-portable-passphrase".to_vec()),
            crawl_config: CrawlConfig::default(),
            vfs_config: VfsConfig::default(),
        };

        let engine = SearchEngine::open(cfg).unwrap();
        let rep = engine
            .index_root(&corpus, ShardStorageMode::HostCached)
            .unwrap();
        assert!(rep.l0_paths_total >= 4);
        assert_eq!(rep.content_docs_indexed, 2);

        // Verify ZERO plaintext leakage of canary OR filenames inside raw encrypted shard files!
        verify_zero_plaintext_on_disk(&rep.shard_dir, canary.as_bytes()).unwrap();
        verify_zero_plaintext_on_disk(&rep.shard_dir, b"q3_revenue_forecast").unwrap();

        // Reopen cold from disk using the same passphrase and run 3-tier RRF queries!
        let reopened = SearchEngine::open(EngineConfig {
            app_support_dir: home,
            passphrase: Some(b"test-portable-passphrase".to_vec()),
            crawl_config: CrawlConfig::default(),
            vfs_config: VfsConfig::default(),
        })
        .unwrap();

        let ctx = QueryContext {
            active_app_bundle_id: Some("dev.zed.Zed".into()),
            active_workspace_dir: Some(corpus.to_string_lossy().into_owned()),
            now_unix_secs: None,
        };

        // 1. Exact lexical canary inside encrypted Tantivy L1
        let r1 = reopened.search(canary, 5, &ctx, false);
        assert!(!r1.hits.is_empty());
        assert!(r1.hits[0].filename.contains("q3_revenue_forecast.md"));
        assert!(r1.hits[0].matched_layers.contains(&"L1".to_string()));

        // 2. Semantic conceptual query ("quarterly budget") with ghost-text check on partial query
        let r2 = reopened.search("q3 rev", 5, &ctx, false);
        assert!(!r2.hits.is_empty());
        assert!(r2.ghost_prediction.is_some());

        let _ = fs::remove_dir_all(&base);
    }
}
