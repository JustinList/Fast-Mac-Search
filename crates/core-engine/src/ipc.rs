use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use fms_fs_indexer::ShardStorageMode;
use fms_system1_predictor::QueryContext;
use serde::{Deserialize, Serialize};

use crate::{EngineStatus, IndexReport, SearchEngine, SearchResponse};

/// Default Unix Domain Socket path for `searchd` <-> macOS UI / CLI IPC.
pub fn default_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("FMS_SOCKET") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join("Library/Caches/dev.fastmacsearch")
        .join("searchd.sock")
}

/// Default application support directory for encrypted host-cached volume shards & master key envelopes.
pub fn default_app_support_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("FMS_HOME") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/FastMacSearch")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Query {
        query: String,
        limit: usize,
        #[serde(default)]
        context: QueryContext,
        #[serde(default)]
        l0_only: bool,
    },
    RecordSelection {
        query: String,
        selected_path: String,
        #[serde(default)]
        context: QueryContext,
    },
    IndexRoot {
        root: PathBuf,
        #[serde(default)]
        portable: bool,
    },
    IndexRootAsync {
        root: PathBuf,
        #[serde(default)]
        portable: bool,
    },
    RemoveShard {
        volume_uuid: String,
    },
    Status,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    Search(SearchResponse),
    Indexed(IndexReport),
    Status(EngineStatus),
    Ack { message: String },
    Error { message: String },
}

/// Acquires a non-blocking exclusive kernel `flock(2)` on `<socket>.lock` BEFORE opening shards
/// so at most one `searchd` process can ever run simultaneously for a given socket path.
pub fn acquire_single_instance_lock(socket_path: &Path) -> io::Result<File> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = socket_path.with_extension("lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("another searchd instance already holds {:?}", lock_path),
        ));
    }
    Ok(file)
}

/// Runs the multi-client Unix Domain Socket IPC server (`searchd`), restricting socket permissions
/// to `0600` (owner-only zero-trust access).
pub fn serve_unix_socket(
    socket_path: &Path,
    engine: Arc<SearchEngine>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }

    let listener = UnixListener::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;

    for incoming in listener.incoming() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match incoming {
            Ok(stream) => {
                let eng = Arc::clone(&engine);
                thread::spawn(move || {
                    let _ = handle_client_stream(stream, eng);
                });
            }
            Err(e) => {
                tracing::warn!("IPC accept error: {e}");
            }
        }
    }

    let _ = fs::remove_file(socket_path);
    Ok(())
}

fn handle_client_stream(stream: UnixStream, engine: Arc<SearchEngine>) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    for line_res in reader.lines() {
        let line = line_res?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<IpcRequest>(&line) {
            Ok(IpcRequest::Query {
                query,
                limit,
                context,
                l0_only,
            }) => IpcResponse::Search(engine.search(&query, limit.max(1), &context, l0_only)),
            Ok(IpcRequest::RecordSelection {
                query,
                selected_path,
                context,
            }) => {
                engine.record_selection(&query, &selected_path, &context);
                IpcResponse::Ack {
                    message: "recorded".into(),
                }
            }
            Ok(IpcRequest::IndexRoot { root, portable }) => {
                let mode = if portable {
                    ShardStorageMode::Portable
                } else {
                    ShardStorageMode::HostCached
                };
                match engine.index_root(&root, mode) {
                    Ok(report) => {
                        let _ = engine.start_realtime_watchers();
                        IpcResponse::Indexed(report)
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            Ok(IpcRequest::IndexRootAsync { root, portable }) => {
                let mode = if portable {
                    ShardStorageMode::Portable
                } else {
                    ShardStorageMode::HostCached
                };
                let engine_bg = Arc::clone(&engine);
                thread::Builder::new()
                    .name("fms-async-indexer".into())
                    .spawn(move || {
                        if let Ok(_rep) = engine_bg.index_root(&root, mode) {
                            let _ = engine_bg.start_realtime_watchers();
                        }
                    })?;
                IpcResponse::Ack {
                    message: "indexing_started".into(),
                }
            }
            Ok(IpcRequest::RemoveShard { volume_uuid }) => match engine.remove_shard(&volume_uuid) {
                Ok(removed) => IpcResponse::Ack {
                    message: if removed { "removed" } else { "not_found" }.into(),
                },
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            },
            Ok(IpcRequest::Status) => IpcResponse::Status(engine.status()),
            Ok(IpcRequest::Ping) => IpcResponse::Ack {
                message: "pong".into(),
            },
            Err(e) => IpcResponse::Error {
                message: format!("invalid IPC request: {e}"),
            },
        };

        let mut payload = serde_json::to_vec(&response)
            .unwrap_or_else(|_| br#"{"type":"error","message":"serialization error"}"#.to_vec());
        payload.push(b'\n');
        writer.write_all(&payload)?;
        writer.flush()?;
    }
    Ok(())
}

/// Sends a single synchronous IPC request over the Unix Domain Socket (`~15 µs` roundtrip).
pub fn send_ipc_request(socket_path: &Path, req: &IpcRequest) -> io::Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path)?;
    let mut raw = serde_json::to_vec(req).map_err(io::Error::other)?;
    raw.push(b'\n');
    stream.write_all(&raw)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line)?;
    serde_json::from_str::<IpcResponse>(&resp_line)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
