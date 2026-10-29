use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use clap::Parser;
use fms_core_engine::ipc::{acquire_single_instance_lock, default_socket_path, serve_unix_socket};
use fms_core_engine::{EngineConfig, SearchEngine};
use fms_fs_indexer::ShardStorageMode;

#[derive(Parser)]
#[command(
    name = "searchd",
    version,
    about = "Fast Mac Search background encrypted indexing & FSEvents IPC daemon"
)]
struct DaemonArgs {
    /// Custom encrypted state directory
    #[arg(long)]
    home: Option<PathBuf>,

    /// Custom Unix Domain Socket path
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Directories or volumes to crawl on startup
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = DaemonArgs::parse();
    let mut cfg = EngineConfig::default();
    if let Some(h) = args.home {
        cfg.app_support_dir = h;
    }

    let sock = args.socket.unwrap_or_else(default_socket_path);
    let _instance_lock = match acquire_single_instance_lock(&sock) {
        Ok(guard) => guard,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::info!("searchd already running for {:?}; exiting cleanly", sock);
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let engine = Arc::new(SearchEngine::open(cfg)?);

    for root in &args.roots {
        let rep = engine.index_root(root, ShardStorageMode::HostCached)?;
        tracing::info!(
            volume = %rep.volume_name,
            paths = rep.l0_paths_total,
            docs = rep.content_docs_indexed,
            ms = rep.total_ms,
            "Indexed root volume shard"
        );
    }

    let watchers = engine.start_realtime_watchers()?;
    tracing::info!(
        socket = ?sock,
        fsevents_streams = watchers,
        "searchd ready on 0600 Unix Domain Socket"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    serve_unix_socket(&sock, engine, shutdown)?;
    Ok(())
}
