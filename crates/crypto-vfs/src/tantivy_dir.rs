use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use tantivy::HasLen;
use tantivy::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    AntiCallToken, Directory, FileHandle, OwnedBytes, TerminatingWrite, WatchCallback,
    WatchCallbackList, WatchHandle, WritePtr,
};

use crate::CryptoVfsError;
use crate::key_provider::ShardKey;
use crate::vfs::{
    DecryptedPageCache, EncryptedRandomAccessFile, EncryptedStreamWriter, VfsConfig,
    atomic_write_encrypted, read_encrypted_all,
};

struct EncryptedFileHandle {
    inner: EncryptedRandomAccessFile,
}

impl fmt::Debug for EncryptedFileHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl HasLen for EncryptedFileHandle {
    fn len(&self) -> usize {
        self.inner.logical_len()
    }
}

impl FileHandle for EncryptedFileHandle {
    fn read_bytes(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        self.inner.read_range(range)
    }
}

struct EncryptedTantivyWriter {
    rel_path: PathBuf,
    writer: EncryptedStreamWriter,
    dir_state: Arc<DirectoryState>,
    terminated: bool,
}

impl Write for EncryptedTantivyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl TerminatingWrite for EncryptedTantivyWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.writer.finish()?;
        self.terminated = true;
        self.dir_state.active_writers.write().remove(&self.rel_path);
        Ok(())
    }
}

impl Drop for EncryptedTantivyWriter {
    fn drop(&mut self) {
        self.dir_state.active_writers.write().remove(&self.rel_path);
    }
}

struct DirectoryState {
    root_dir: PathBuf,
    shard_key: ShardKey,
    config: VfsConfig,
    page_cache: Arc<DecryptedPageCache>,
    open_handles: RwLock<HashMap<PathBuf, Arc<dyn FileHandle>>>,
    active_writers: RwLock<HashSet<PathBuf>>,
    watch_router: WatchCallbackList,
}

/// Zero-trust `tantivy::directory::Directory` backed by 16KB AES-256-GCM encrypted pages,
/// keyed-BLAKE3 header authentication, hot-segment RAM pinning, and lock-free `pread(2)`.
#[derive(Clone)]
pub struct EncryptedBlockDirectory {
    state: Arc<DirectoryState>,
}

impl fmt::Debug for EncryptedBlockDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedBlockDirectory")
            .field("root_dir", &self.state.root_dir)
            .field("page_size", &self.state.config.page_size)
            .finish()
    }
}

impl EncryptedBlockDirectory {
    pub fn open(
        root_dir: impl Into<PathBuf>,
        shard_key: ShardKey,
        config: VfsConfig,
    ) -> Result<Self, CryptoVfsError> {
        let root_dir = root_dir.into();
        fs::create_dir_all(&root_dir)?;
        let page_cache = Arc::new(DecryptedPageCache::new(config.cache_pages));
        Ok(Self {
            state: Arc::new(DirectoryState {
                root_dir,
                shard_key,
                config,
                page_cache,
                open_handles: RwLock::new(HashMap::new()),
                active_writers: RwLock::new(HashSet::new()),
                watch_router: WatchCallbackList::default(),
            }),
        })
    }

    pub fn root_dir(&self) -> &Path {
        &self.state.root_dir
    }

    pub fn page_cache(&self) -> Arc<DecryptedPageCache> {
        Arc::clone(&self.state.page_cache)
    }

    fn resolve_path(&self, rel: &Path) -> PathBuf {
        self.state.root_dir.join(rel)
    }
}

impl Directory for EncryptedBlockDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        if let Some(cached) = self.state.open_handles.read().get(path) {
            return Ok(Arc::clone(cached));
        }

        let full_path = self.resolve_path(path);
        if !full_path.exists() {
            return Err(OpenReadError::FileDoesNotExist(path.to_path_buf()));
        }

        let inner = EncryptedRandomAccessFile::open(
            &full_path,
            &self.state.shard_key,
            Arc::clone(&self.state.page_cache),
            self.state.config.hot_pin_max_bytes,
        )
        .map_err(|e| OpenReadError::IoError {
            io_error: Arc::new(e.into_io_error()),
            filepath: path.to_path_buf(),
        })?;

        let handle: Arc<dyn FileHandle> = Arc::new(EncryptedFileHandle { inner });
        self.state
            .open_handles
            .write()
            .insert(path.to_path_buf(), Arc::clone(&handle));
        Ok(handle)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.state.open_handles.write().remove(path);
        let full_path = self.resolve_path(path);
        if !full_path.exists() {
            return Err(DeleteError::FileDoesNotExist(path.to_path_buf()));
        }
        fs::remove_file(&full_path).map_err(|e| DeleteError::IoError {
            io_error: Arc::new(e),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        if self.state.active_writers.read().contains(path) {
            return Ok(true);
        }
        Ok(self.resolve_path(path).exists())
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let rel = path.to_path_buf();
        let full_path = self.resolve_path(path);

        {
            let mut writers = self.state.active_writers.write();
            if full_path.exists() || !writers.insert(rel.clone()) {
                return Err(OpenWriteError::FileAlreadyExists(rel));
            }
        }
        self.state.open_handles.write().remove(path);

        let stream_writer = match EncryptedStreamWriter::create(
            &full_path,
            &self.state.shard_key,
            self.state.config.page_size,
        ) {
            Ok(w) => w,
            Err(e) => {
                self.state.active_writers.write().remove(&rel);
                return Err(OpenWriteError::IoError {
                    io_error: Arc::new(e.into_io_error()),
                    filepath: rel,
                });
            }
        };

        let tantivy_writer = EncryptedTantivyWriter {
            rel_path: rel,
            writer: stream_writer,
            dir_state: Arc::clone(&self.state),
            terminated: false,
        };
        Ok(BufWriter::new(Box::new(tantivy_writer)))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let full_path = self.resolve_path(path);
        if !full_path.exists() {
            return Err(OpenReadError::FileDoesNotExist(path.to_path_buf()));
        }
        read_encrypted_all(
            &full_path,
            &self.state.shard_key,
            Arc::clone(&self.state.page_cache),
        )
        .map_err(|e| OpenReadError::IoError {
            io_error: Arc::new(e.into_io_error()),
            filepath: path.to_path_buf(),
        })
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let full_path = self.resolve_path(path);
        atomic_write_encrypted(
            &full_path,
            &self.state.shard_key,
            data,
            self.state.config.page_size,
        )
        .map_err(CryptoVfsError::into_io_error)?;

        self.state.open_handles.write().remove(path);

        if path == Path::new("meta.json") {
            let _ = self.state.watch_router.broadcast();
        }
        Ok(())
    }

    fn sync_directory(&self) -> io::Result<()> {
        let dir = fs::File::open(&self.state.root_dir)?;
        dir.sync_all()
    }

    fn watch(&self, watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(self.state.watch_router.subscribe(watch_callback))
    }
}
