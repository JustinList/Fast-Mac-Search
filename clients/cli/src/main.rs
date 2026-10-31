mod mcp;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use fms_core_engine::ipc::{
    IpcRequest, IpcResponse, acquire_single_instance_lock, default_socket_path, send_ipc_request,
    serve_unix_socket,
};
use fms_core_engine::{EngineConfig, LlmSyncReport, SearchEngine, SearchScope};
use fms_crypto_vfs::{
    DEFAULT_PAGE_SIZE, DecryptedPageCache, EncryptedRandomAccessFile, EncryptedStreamWriter,
    ShardKey, verify_zero_plaintext_on_disk,
};
use fms_fs_indexer::ShardStorageMode;
use fms_l0_arena::L0PathArena;
use fms_system1_predictor::QueryContext;

#[derive(Parser)]
#[command(
    name = "fms",
    version,
    about = "Fast Mac Search — Sub-millisecond zero-trust Spotlight replacement for Apple Silicon"
)]
struct Cli {
    /// Custom encrypted state directory (defaults to ~/Library/Application Support/FastMacSearch)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    /// Custom Unix Domain Socket path for searchd IPC
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    ///Emit machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Crawl a directory or volume via getattrlistbulk(2) and build encrypted L0+L1+L2 shards
    Index {
        /// Directory or external volume mount point to index
        path: PathBuf,
        /// Store a portable encrypted `.fastindex/` shard directly on the target volume
        #[arg(long)]
        portable: bool,
    },

    /// Execute the 3-Tier System-1 + ARM NEON + Encrypted Tantivy + Semantic Vector search
    Search {
        /// Search query (partial prefixes, fuzzy filenames, @claude/@codex/@gemini tags, or concepts)
        query: String,
        /// Maximum number of ranked results
        #[arg(short = 'n', long, default_value_t = 12)]
        limit: usize,
        /// Filter scope: all, files, or chats
        #[arg(long, default_value = "all")]
        scope: String,
        /// Active frontmost app bundle ID for System 1 Markov frecency context
        #[arg(long)]
        app: Option<String>,
        /// Active workspace / git repository directory for System 1 affinity boost
        #[arg(long)]
        cwd: Option<String>,
        /// Run Stage 1 L0 SIMD + Frecency reflex only (<0.5ms)
        #[arg(long)]
        l0_only: bool,
    },

    /// Sync, import, search, and read encrypted cross-LLM conversations (Claude, Codex, Gemini, ChatGPT, LM Studio)
    Llm {
        #[command(subcommand)]
        action: LlmCommands,
    },

    /// Run the built-in Model Context Protocol (MCP) stdio server for Claude Desktop, Claude Code, Codex, Cursor, & Antigravity
    Mcp,

    /// Automatically register `fast-mac-search` as an MCP server in Claude Desktop and print setup snippets for other AI tools
    McpInstall,

    /// Record a user selection to train the System 1 Contextual Markov Frecency Graph
    Select {
        query: String,
        selected_path: String,
        #[arg(long)]
        app: Option<String>,
    },

    /// Mount an existing Portable Encrypted Shard (`.fastindex/`) from an external drive without re-indexing
    MountPortable {
        shard_dir: PathBuf,
    },

    /// Start the background `searchd` Unix Domain Socket daemon with real-time macOS FSEvents watching
    Daemon {
        /// Optional root directories to index on startup before serving IPC
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },

    /// Show loaded encrypted volume shards, hardware UUIDs, online/offline badges, and Secure Enclave status
    Status,

    /// Audit raw shard files on disk to verify zero plaintext leakage of a canary string
    VerifyZeroPlaintext {
        shard_dir: PathBuf,
        canary: String,
    },

    /// Run Apple Silicon hardware benchmarks (1,000,000-path ARM NEON SIMD scan + 16KB AES-256-GCM throughput)
    Bench {
        /// Number of synthetic paths to load into the L0 NEON SIMD arena
        #[arg(long, default_value_t = 1_000_000)]
        paths: usize,
    },
}

#[derive(Subcommand)]
enum LlmCommands {
    /// Incrementally discover and index all local Claude, Codex, Gemini/Antigravity, & LM Studio chats
    Sync,

    /// Import a ChatGPT or Claude Web `conversations.json` export into AES-256-GCM encrypted memory
    Import {
        /// Path to `conversations.json` export file
        path: PathBuf,
    },

    /// Search encrypted cross-LLM conversations (leave query empty to list recent chats)
    Search {
        /// Search query (optional; omit to list most recent AI chats)
        #[arg(default_value = "")]
        query: String,
        /// Optional provider filter: claude, codex, gemini, chatgpt, lmstudio
        #[arg(short = 'p', long)]
        provider: Option<String>,
        /// Maximum number of conversations to return
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },

    /// Print the clean decrypted dialogue transcript of an indexed LLM session
    Read {
        /// Session ID (e.g. `claude:...`, `codex:...`, `gemini:...`, `chatgpt:...`)
        session_id: String,
    },
}

fn build_config(home: Option<PathBuf>) -> EngineConfig {
    let mut cfg = EngineConfig::default();
    if let Some(h) = home {
        cfg.app_support_dir = h;
    }
    cfg
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let sock_path = cli.socket.clone().unwrap_or_else(default_socket_path);

    match cli.command {
        Commands::Index { path, portable } => {
            if sock_path.exists()
                && let Ok(IpcResponse::Indexed(rep)) =
                    send_ipc_request(&sock_path, &IpcRequest::IndexRoot { root: path.clone(), portable })
            {
                print_index_report(&rep, cli.json)?;
                return Ok(());
            }

            let engine = SearchEngine::open(build_config(cli.home))?;
            let mode = if portable {
                ShardStorageMode::Portable
            } else {
                ShardStorageMode::HostCached
            };
            let report = engine.index_root(&path, mode)?;
            print_index_report(&report, cli.json)?;
        }

        Commands::Search {
            query,
            limit,
            scope,
            app,
            cwd,
            l0_only,
        } => {
            let parsed_scope = SearchScope::from_tag(&scope);
            let context = QueryContext {
                active_app_bundle_id: app,
                active_workspace_dir: cwd.or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                }),
                now_unix_secs: None,
            };

            // Fast IPC path to warm `searchd` daemon (~15 µs over AF_UNIX) if running!
            let resp = if sock_path.exists()
                && let Ok(IpcResponse::Search(r)) = send_ipc_request(
                    &sock_path,
                    &IpcRequest::Query {
                        query: query.clone(),
                        limit,
                        context: context.clone(),
                        l0_only,
                        scope: parsed_scope,
                    },
                ) {
                r
            } else {
                let engine = SearchEngine::open(build_config(cli.home))?;
                engine.search_scoped(&query, limit, &context, l0_only, parsed_scope)
            };

            print_search_results(&resp, cli.json)?;
        }

        Commands::Llm { action } => match action {
            LlmCommands::Sync => {
                let rep = if sock_path.exists()
                    && let Ok(IpcResponse::LlmSynced(r)) =
                        send_ipc_request(&sock_path, &IpcRequest::SyncLlmChats)
                {
                    r
                } else {
                    let engine = SearchEngine::open(build_config(cli.home))?;
                    engine.sync_llm_chats()?
                };
                print_llm_sync_report(&rep, cli.json)?;
            }

            LlmCommands::Import { path } => {
                let abs = path.canonicalize().unwrap_or(path);
                let rep = if sock_path.exists() {
                    match send_ipc_request(&sock_path, &IpcRequest::ImportLlmExport { path: abs.clone() }) {
                        Ok(IpcResponse::LlmSynced(r)) => r,
                        Ok(IpcResponse::Error { message }) => anyhow::bail!("{message}"),
                        _ => {
                            let engine = SearchEngine::open(build_config(cli.home))?;
                            engine.import_llm_export(&abs)?
                        }
                    }
                } else {
                    let engine = SearchEngine::open(build_config(cli.home))?;
                    engine.import_llm_export(&abs)?
                };
                print_llm_sync_report(&rep, cli.json)?;
            }

            LlmCommands::Search {
                query,
                provider,
                limit,
            } => {
                let effective_q = match provider {
                    Some(p) if !p.trim().is_empty() => {
                        format!("@{} {}", p.trim().trim_start_matches('@'), query)
                    }
                    _ => query,
                };
                let ctx = QueryContext::default();
                let resp = if sock_path.exists()
                    && let Ok(IpcResponse::Search(r)) = send_ipc_request(
                        &sock_path,
                        &IpcRequest::Query {
                            query: effective_q.clone(),
                            limit,
                            context: ctx.clone(),
                            l0_only: false,
                            scope: SearchScope::Chats,
                        },
                    )
                {
                    r
                } else {
                    let engine = SearchEngine::open(build_config(cli.home))?;
                    engine.search_scoped(&effective_q, limit, &ctx, false, SearchScope::Chats)
                };
                print_search_results(&resp, cli.json)?;
            }

            LlmCommands::Read { session_id } => {
                let session = if sock_path.exists()
                    && let Ok(IpcResponse::LlmConversation { session }) = send_ipc_request(
                        &sock_path,
                        &IpcRequest::GetLlmConversation {
                            session_id: session_id.clone(),
                        },
                    )
                {
                    session
                } else {
                    let engine = SearchEngine::open(build_config(cli.home))?;
                    engine
                        .get_llm_session(&session_id)
                        .ok_or_else(|| anyhow::anyhow!("LLM session not found: {session_id}"))?
                };

                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&session)?);
                } else {
                    println!(
                        "✦ [{}] {} ({})",
                        session.provider.as_str(),
                        session.title,
                        session.session_id
                    );
                    if let Some(ref ws) = session.workspace {
                        println!("  Workspace : {ws}");
                    }
                    println!("  Source    : {}", session.source_path.display());
                    println!("────────────────────────────────────────────────────────────");
                    for (i, turn) in session.turns.iter().enumerate() {
                        let role = if turn.role == "user" {
                            "\x1b[1;36mUSER\x1b[0m"
                        } else {
                            "\x1b[1;33mASSISTANT\x1b[0m"
                        };
                        println!("\n[{role} #{}]\n{}", i + 1, turn.text);
                    }
                }
            }
        },

        Commands::Mcp => {
            mcp::run_mcp_stdio_server(cli.home, sock_path)?;
        }

        Commands::McpInstall => {
            let (fms_bin, claude_cfg) = mcp::install_mcp_into_claude_desktop()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "installed": true,
                        "fms_binary": fms_bin,
                        "claude_desktop_config": claude_cfg,
                    })
                );
            } else {
                println!("✓ Installed `fast-mac-search` MCP server into Claude Desktop:");
                println!("  Config : {}", claude_cfg.display());
                println!("  Binary : {}", fms_bin.display());
                println!("\nOptional 1-line setup for your other AI coding agents:");
                println!(
                    "  • Claude Code : claude mcp add fast-mac-search -- \"{}\" mcp",
                    fms_bin.display()
                );
                println!(
                    "  • OpenAI Codex: codex mcp add fast-mac-search -- \"{}\" mcp",
                    fms_bin.display()
                );
            }
        }

        Commands::Select {
            query,
            selected_path,
            app,
        } => {
            let context = QueryContext {
                active_app_bundle_id: app,
                active_workspace_dir: None,
                now_unix_secs: None,
            };
            if sock_path.exists() {
                let _ = send_ipc_request(
                    &sock_path,
                    &IpcRequest::RecordSelection {
                        query,
                        selected_path,
                        context,
                    },
                )?;
            } else {
                let engine = SearchEngine::open(build_config(cli.home))?;
                engine.record_selection(&query, &selected_path, &context);
            }
            println!("Recorded System 1 Markov frecency reinforcement.");
        }

        Commands::MountPortable { shard_dir } => {
            let engine = SearchEngine::open(build_config(cli.home))?;
            let summary = engine.mount_portable_shard(&shard_dir)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!(
                    "Mounted portable encrypted shard: {} ({} paths, uuid={})",
                    summary.volume_name, summary.l0_paths, summary.volume_uuid
                );
            }
        }

        Commands::Daemon { roots } => {
            let _instance_lock = match acquire_single_instance_lock(&sock_path) {
                Ok(guard) => guard,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    eprintln!("[searchd] Already running on {:?}; exiting.", sock_path);
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            let engine = Arc::new(SearchEngine::open(build_config(cli.home))?);
            for r in &roots {
                let rep = engine.index_root(r, ShardStorageMode::HostCached)?;
                eprintln!(
                    "[searchd] Indexed {:?} -> {} paths in {:.1} ms ({})",
                    r, rep.l0_paths_total, rep.total_ms, rep.key_backend
                );
            }
            let watchers = engine.start_realtime_watchers()?;
            eprintln!(
                "[searchd] Listening on {:?} (0600 zero-trust socket, {} FSEvents streams active)",
                sock_path, watchers
            );
            let shutdown = Arc::new(AtomicBool::new(false));
            serve_unix_socket(&sock_path, engine, shutdown)?;
        }

        Commands::Status => {
            let status = if sock_path.exists()
                && let Ok(IpcResponse::Status(s)) = send_ipc_request(&sock_path, &IpcRequest::Status)
            {
                s
            } else {
                let engine = SearchEngine::open(build_config(cli.home))?;
                engine.status()
            };

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Fast Mac Search Status");
                println!("  App Support Dir : {:?}", status.app_support_dir);
                println!("  Secure Enclave  : {}", status.secure_enclave_status);
                println!("  Loaded Shards   : {} ({} total L0 paths)", status.shard_count, status.total_paths);
                for s in &status.shards {
                    println!(
                        "  - {} [{}] uuid={} mode={:?} L0={} L1={} L2={} mlock={} key={}",
                        s.volume_name,
                        if s.is_online { "ONLINE" } else { "OFFLINE" },
                        s.volume_uuid,
                        s.mode,
                        s.l0_paths,
                        s.l1_docs,
                        s.l2_vectors,
                        s.mlocked,
                        s.key_backend
                    );
                }
                let prov_summary = if status.llm_providers.is_empty() {
                    "run `fms llm sync` to index local chats".to_string()
                } else {
                    status
                        .llm_providers
                        .iter()
                        .map(|p| format!("{}: {}", p.provider, p.count))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                println!(
                    "  AI Memory       : {} encrypted sessions ({})",
                    status.llm_sessions_total, prov_summary
                );
            }
        }

        Commands::VerifyZeroPlaintext { shard_dir, canary } => {
            let checked = verify_zero_plaintext_on_disk(&shard_dir, canary.as_bytes())?;
            println!(
                "PASS: Audited {checked} raw encrypted files in {:?} — 0 bytes of plaintext canary {:?} found.",
                shard_dir, canary
            );
        }

        Commands::Bench { paths } => {
            run_hardware_benchmarks(paths, cli.json)?;
        }
    }

    Ok(())
}

fn print_search_results(resp: &fms_core_engine::SearchResponse, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(resp)?);
    } else {
        println!(
            "⚡ {:.2} ms total (L0: {} µs | L1: {} µs | L2: {} µs) · intent={} · indexed={} items",
            resp.total_elapsed_us as f64 / 1000.0,
            resp.stage1_l0_us,
            resp.stage2_l1_us,
            resp.stage3_l2_us,
            resp.intent,
            resp.total_indexed_paths
        );
        if let Some(ref ghost) = resp.ghost_prediction {
            println!(
                "👻 Ghost autocomplete [Tab]: {}\x1b[90m{}\x1b[0m",
                resp.query, ghost.ghost_suffix
            );
        }
        for (idx, hit) in resp.hits.iter().enumerate() {
            let badge = if hit.is_llm_chat {
                format!(
                    "[✦ {}]",
                    hit.llm_provider.as_deref().unwrap_or(&hit.volume_name)
                )
            } else if hit.volume_online {
                format!("[{}]", hit.volume_name)
            } else {
                format!("[OFFLINE - Connect {}]", hit.volume_name)
            };
            let layers = hit.matched_layers.join("+");
            let loc = if hit.is_llm_chat {
                hit.llm_session_id.as_deref().unwrap_or(&hit.path)
            } else {
                &hit.path
            };
            println!(
                "{:>2}. {:<36} ({:.4} · {}) {} {}",
                idx + 1,
                hit.filename,
                hit.rrf_score,
                layers,
                badge,
                loc
            );
            if !hit.snippet.is_empty() {
                println!("      \x1b[90m{}\x1b[0m", hit.snippet);
            }
        }
    }
    Ok(())
}

fn print_llm_sync_report(rep: &LlmSyncReport, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(rep)?);
    } else {
        let breakdown = rep
            .providers
            .iter()
            .map(|p| format!("{}: {}", p.provider, p.count))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "✦ Encrypted Cross-LLM Memory Synced in {:.1} ms",
            rep.elapsed_ms
        );
        println!(
            "  Transcripts : {} discovered ({} parsed, {} unchanged cached)",
            rep.files_discovered, rep.files_parsed, rep.files_cached
        );
        println!(
            "  Total Chats : {} encrypted sessions ({})",
            rep.sessions_total, breakdown
        );
    }
    Ok(())
}

fn print_index_report(rep: &fms_core_engine::IndexReport, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(rep)?);
    } else {
        println!("Indexed Volume: {} ({})", rep.volume_name, rep.volume_uuid);
        println!("  Storage Mode  : {:?} -> {:?}", rep.storage_mode, rep.shard_dir);
        println!("  Key Backend   : {}", rep.key_backend);
        println!(
            "  Kernel Crawl  : {} files, {} dirs in {:.2} ms ({:.0} entries/sec)",
            rep.files_scanned,
            rep.dirs_scanned,
            rep.crawl_ms,
            ((rep.files_scanned + rep.dirs_scanned) as f64) / (rep.crawl_ms / 1000.0).max(0.0001)
        );
        println!(
            "  Encrypted Idx : {} L0 paths (mlock={}), {} L1+L2 content docs in {:.2} ms total",
            rep.l0_paths_total, rep.mlocked, rep.content_docs_indexed, rep.total_ms
        );
        if rep.tcc_restricted_count > 0 {
            println!(
                "  TCC Notice    : {} directories skipped (grant Full Disk Access in System Settings for restricted user folders)",
                rep.tcc_restricted_count
            );
        }
    }
    Ok(())
}

fn run_hardware_benchmarks(num_paths: usize, as_json: bool) -> Result<()> {
    // 1. Populate L0 Contiguous NEON Arena with `num_paths` realistic file paths
    let build_start = Instant::now();
    let mut arena = L0PathArena::with_capacity(num_paths);
    let dirs = [
        "Users/alex/workspace/rust-engine/crates/core/src",
        "Users/alex/workspace/ios-app/Features/Authentication",
        "Users/alex/Documents/Finance/2026/Quarterly",
        "Volumes/CrucialX9/MediaProjects/Shaders/Metal",
        "Library/Application Support/Developer/Configurations",
    ];
    let exts = ["rs", "swift", "md", "toml", "json", "metal", "ts", "py"];

    for i in 0..num_paths.saturating_sub(3) {
        let d = dirs[i % dirs.len()];
        let e = exts[i % exts.len()];
        let p = format!("/{d}/module_{}/handler_{i}.{e}", i % 512);
        arena.upsert(i as u64, &p, false);
    }
    arena.upsert(
        (num_paths - 3) as u64,
        "/Users/alex/Documents/Finance/2026/q3_revenue_forecast.md",
        false,
    );
    arena.upsert(
        (num_paths - 2) as u64,
        "/Users/alex/workspace/fast-mac-search/crates/crypto-vfs/src/encrypted_block_directory.rs",
        false,
    );
    arena.upsert(
        (num_paths - 1) as u64,
        "/Users/alex/workspace/fast-mac-search/ARCHITECTURE.md",
        false,
    );
    let mlocked = arena.lock_memory();
    let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;
    let ram_mb = arena.arena_bytes_used() as f64 / (1024.0 * 1024.0);

    // Warmup + median of 25 runs for L0 NEON SIMD + Smith-Waterman search
    let _ = arena.search("q3 rev", 10);
    let mut samples_us = Vec::with_capacity(25);
    let mut last_stats = Default::default();
    for _ in 0..25 {
        let (hits, st) = arena.search("q3 rev", 10);
        assert!(!hits.is_empty());
        samples_us.push(st.total_elapsed_us);
        last_stats = st;
    }
    samples_us.sort_unstable();
    let median_l0_us = samples_us[samples_us.len() / 2];

    // 2. Benchmark 16KB AES-256-GCM VFS encryption & random-access decryption throughput (64 MiB)
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("fms_vfs_bench_{}.bin", std::process::id()));
    let key = ShardKey::random_for_tests();
    let payload_size = 64 * 1024 * 1024usize; // 64 MiB
    let chunk = vec![0x5Au8; 1024 * 1024];

    let enc_start = Instant::now();
    {
        let mut w = EncryptedStreamWriter::create(&tmp, &key, DEFAULT_PAGE_SIZE)?;
        for _ in 0..64 {
            w.write_all(&chunk)?;
        }
        w.finish()?;
    }
    let enc_secs = enc_start.elapsed().as_secs_f64();
    let enc_gbps = (payload_size as f64 / (1024.0 * 1024.0 * 1024.0)) / enc_secs;

    let cache = Arc::new(DecryptedPageCache::new(64));
    let reader = EncryptedRandomAccessFile::open(&tmp, &key, cache, 0)?;
    let dec_start = Instant::now();
    let _full = reader.read_range(0..payload_size)?;
    let dec_secs = dec_start.elapsed().as_secs_f64();
    let dec_gbps = (payload_size as f64 / (1024.0 * 1024.0 * 1024.0)) / dec_secs;
    let _ = std::fs::remove_file(&tmp);

    if as_json {
        println!(
            "{}",
            serde_json::json!({
                "paths_indexed": arena.len(),
                "arena_ram_mib": ram_mb,
                "mlocked": mlocked,
                "arena_build_ms": build_ms,
                "l0_median_query_us": median_l0_us,
                "l0_simd_filter_us": last_stats.simd_elapsed_us,
                "l0_simd_survivors": last_stats.simd_survivors,
                "aes256_gcm_write_gb_per_s": enc_gbps,
                "aes256_gcm_read_gb_per_s": dec_gbps,
            })
        );
    } else {
        println!("Fast Mac Search — Apple Silicon Hardware Benchmark");
        println!("────────────────────────────────────────────────────────────");
        println!(
            "L0 Path Arena       : {} paths ({:.1} MiB RAM, mlock={}) built in {:.1} ms",
            arena.len(),
            ram_mb,
            mlocked,
            build_ms
        );
        println!(
            "L0 NEON + Fuzzy p50 : {} µs ({:.3} ms) | NEON filter={} µs ({} -> {} survivors)",
            median_l0_us,
            median_l0_us as f64 / 1000.0,
            last_stats.simd_elapsed_us,
            arena.len(),
            last_stats.simd_survivors
        );
        println!(
            "16KB AES-256-GCM VFS: {:.2} GiB/s encrypt+write | {:.2} GiB/s pread+decrypt",
            enc_gbps, dec_gbps
        );
    }
    Ok(())
}
