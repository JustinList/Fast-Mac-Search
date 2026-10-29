use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Type of filesystem entry discovered during a `getattrlistbulk(2)` sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Metadata extracted in a single kernel transition via `getattrlistbulk(2)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawledEntry {
    pub path: PathBuf,
    pub inode: u64,
    pub mtime_secs: i64,
    pub size_bytes: u64,
    pub kind: EntryKind,
    /// When `true`, the path is included in the L0 filename arena for instant fuzzy lookup,
    /// but excluded from heavy L1/L2 full-text & vector indexing (e.g., vendor/build artifacts).
    pub l0_only: bool,
}

/// Configuration for the `getattrlistbulk(2)` filesystem crawler.
#[derive(Debug, Clone)]
pub struct CrawlConfig {
    /// Respect `.gitignore` and `.ignore` files encountered in directories.
    pub respect_gitignore: bool,
    /// Keep filenames from build/vendor folders (like `node_modules` or `target`) in L0 path arena
    /// while skipping L1/L2 content extraction. Defaults to `false` for maximum signal-to-noise.
    pub include_build_dirs_in_l0: bool,
    /// Include directory paths themselves as searchable items in L0.
    pub include_directories: bool,
    /// Optional cap on maximum entries crawled (useful for bounded previews/tests).
    pub max_entries: Option<usize>,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            include_build_dirs_in_l0: false,
            include_directories: true,
            max_entries: None,
        }
    }
}

/// Telemetry and TCC permission diagnostics from a `getattrlistbulk(2)` crawl.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlStats {
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub bytes_discovered: u64,
    pub elapsed_ms: f64,
    /// Directories that returned `EPERM`/`EACCES` (e.g. macOS TCC Full Disk Access needed).
    pub tcc_restricted_paths: Vec<PathBuf>,
}

/// System/OS junk directories that should never be indexed in any layer.
const SYSTEM_BLOCKLIST: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".Trashes",
    ".Trash",
    ".fastindex",
    ".DS_Store",
    "FastMacSearch",
    "dev.fastmacsearch",
];

/// Heavy artifact/vendor directories excluded from L1/L2 content indexing (and L0 unless configured).
const ARTIFACT_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".build",
    "DerivedData",
    "__pycache__",
    ".venv",
    "venv",
    ".next",
    ".turbo",
    ".cache",
];

#[inline]
pub fn is_system_junk_name(name: &str) -> bool {
    if name.starts_with("._") {
        // Skip macOS AppleDouble resource fork sidecars on external drives
        return true;
    }
    SYSTEM_BLOCKLIST.contains(&name)
}

#[inline]
pub fn is_artifact_dir_name(name: &str) -> bool {
    ARTIFACT_DIRS.contains(&name)
}

#[inline]
pub fn should_skip_internal_or_noisy_path(path: &Path) -> bool {
    let Some(s) = path.to_str() else {
        return false;
    };
    s.contains("/Library/Application Support/FastMacSearch")
        || s.contains("/Library/Caches")
        || s.contains("/Library/Logs")
        || s.contains("/Library/Saved Application State")
        || s.contains("/Library/Metadata")
        || s.contains("/Library/Developer/CoreSimulator")
        || s.contains("/.fastindex")
}

#[inline]
fn should_skip_macos_virtual_mount(path: &Path, root_is_slash: bool) -> bool {
    if root_is_slash && path == Path::new("/Volumes") {
        // External volumes under /Volumes are indexed independently via their own Volume UUID shard
        return true;
    }
    matches!(
        path.to_str(),
        Some(
            "/dev"
                | "/cores"
                | "/System/Volumes"
                | "/private/var/vm"
                | "/private/var/folders"
                | "/private/tmp"
        )
    )
}

/// Crawls `root` at raw kernel speed using `getattrlistbulk(2)` on macOS and parallel `rayon`
/// directory expansion across Apple Silicon performance cores.
pub fn crawl_volume_root(
    root: &Path,
    config: &CrawlConfig,
) -> io::Result<(Vec<CrawledEntry>, CrawlStats)> {
    crawl_volume_root_with_progress(root, config, &|_, _| {})
}

/// Crawls `root` with a live `(files_scanned, dirs_scanned)` progress callback invoked across BFS levels.
pub fn crawl_volume_root_with_progress<F>(
    root: &Path,
    config: &CrawlConfig,
    on_progress: &F,
) -> io::Result<(Vec<CrawledEntry>, CrawlStats)>
where
    F: Fn(u64, u64) + Send + Sync,
{
    let started = Instant::now();
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_is_slash = root == Path::new("/");

    let files_counter = AtomicU64::new(0);
    let dirs_counter = AtomicU64::new(0);
    let bytes_counter = AtomicU64::new(0);
    let restricted = Mutex::new(Vec::<PathBuf>::new());
    let collected = Mutex::new(Vec::<CrawledEntry>::with_capacity(16_384));

    let root_gitignore = if config.respect_gitignore {
        load_dir_gitignore(&root)
    } else {
        None
    };

    // Frontier-based parallel BFS using Rayon so hundreds of subdirectories execute
    // `getattrlistbulk(2)` concurrently without blowing thread stacks on deep trees.
    let mut frontier: Vec<(PathBuf, bool, Option<Gitignore>)> =
        vec![(root, false, root_gitignore)];

    while !frontier.is_empty() {
        if let Some(limit) = config.max_entries
            && collected.lock().len() >= limit
        {
            break;
        }

        let next_frontier: Vec<(PathBuf, bool, Option<Gitignore>)> = frontier
            .into_par_iter()
            .flat_map_iter(|(dir_path, inherited_l0_only, parent_gi)| {
                dirs_counter.fetch_add(1, Ordering::Relaxed);
                let active_gi = if config.respect_gitignore && !inherited_l0_only {
                    load_dir_gitignore(&dir_path).or(parent_gi)
                } else {
                    parent_gi
                };

                match read_dir_bulk(&dir_path) {
                    Ok(raw_entries) => {
                        let mut local_entries = Vec::with_capacity(raw_entries.len());
                        let mut child_dirs = Vec::new();

                        for mut item in raw_entries {
                            let file_name_str = item
                                .path
                                .file_name()
                                .and_then(OsStr::to_str)
                                .unwrap_or("");

                            if is_system_junk_name(file_name_str) {
                                continue;
                            }

                            let is_dir = item.kind == EntryKind::Directory;
                            if is_dir
                                && (should_skip_macos_virtual_mount(&item.path, root_is_slash)
                                    || should_skip_internal_or_noisy_path(&item.path))
                            {
                                continue;
                            }
                            let mut entry_l0_only = inherited_l0_only;

                            if is_dir && is_artifact_dir_name(file_name_str) {
                                if !config.include_build_dirs_in_l0 {
                                    continue;
                                }
                                entry_l0_only = true;
                            }

                            if !entry_l0_only
                                && let Some(ref gi) = active_gi
                                && gi.matched(&item.path, is_dir).is_ignore()
                            {
                                if config.include_build_dirs_in_l0 {
                                    entry_l0_only = true;
                                } else {
                                    continue;
                                }
                            }

                            item.l0_only = entry_l0_only;

                            match item.kind {
                                EntryKind::Directory => {
                                    child_dirs.push((
                                        item.path.clone(),
                                        entry_l0_only,
                                        active_gi.clone(),
                                    ));
                                    if config.include_directories {
                                        local_entries.push(item);
                                    }
                                }
                                EntryKind::File => {
                                    let fcount = files_counter.fetch_add(1, Ordering::Relaxed) + 1;
                                    bytes_counter.fetch_add(item.size_bytes, Ordering::Relaxed);
                                    if fcount.is_multiple_of(250) {
                                        on_progress(fcount, dirs_counter.load(Ordering::Relaxed));
                                    }
                                    local_entries.push(item);
                                }
                                EntryKind::Symlink | EntryKind::Other => {}
                            }
                        }

                        if !local_entries.is_empty() {
                            let mut guard = collected.lock();
                            if let Some(limit) = config.max_entries {
                                let rem = limit.saturating_sub(guard.len());
                                guard.extend(local_entries.into_iter().take(rem));
                            } else {
                                guard.extend(local_entries);
                            }
                        }

                        child_dirs
                    }
                    Err(err) => {
                        if matches!(
                            err.raw_os_error(),
                            Some(libc::EPERM) | Some(libc::EACCES)
                        ) {
                            restricted.lock().push(dir_path);
                        }
                        Vec::new()
                    }
                }
            })
            .collect();

        on_progress(
            files_counter.load(Ordering::Relaxed),
            dirs_counter.load(Ordering::Relaxed),
        );
        frontier = next_frontier;
    }

    let entries = collected.into_inner();
    let stats = CrawlStats {
        files_scanned: files_counter.load(Ordering::Relaxed),
        dirs_scanned: dirs_counter.load(Ordering::Relaxed),
        bytes_discovered: bytes_counter.load(Ordering::Relaxed),
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        tcc_restricted_paths: restricted.into_inner(),
    };
    Ok((entries, stats))
}

fn load_dir_gitignore(dir: &Path) -> Option<Gitignore> {
    let gi_path = dir.join(".gitignore");
    let ignore_path = dir.join(".ignore");
    if !gi_path.exists() && !ignore_path.exists() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(dir);
    if gi_path.exists() {
        let _ = builder.add(gi_path);
    }
    if ignore_path.exists() {
        let _ = builder.add(ignore_path);
    }
    builder.build().ok()
}

/// Performs a single-directory bulk attribute fetch using macOS `getattrlistbulk(2)`.
pub fn read_dir_bulk(dir: &Path) -> io::Result<Vec<CrawledEntry>> {
    #[cfg(target_os = "macos")]
    {
        macos_syscall::getattrlistbulk_dir(dir)
    }

    #[cfg(not(target_os = "macos"))]
    {
        posix_fallback_dir(dir)
    }
}

#[cfg(target_os = "macos")]
mod macos_syscall {
    use super::*;

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const FSOPT_NOFOLLOW: u64 = 0x0000_0001;

    const ATTR_CMN_NAME: u32 = 0x0000_0001;
    const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
    const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
    const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

    const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;

    /// 128 KB kernel buffer per `getattrlistbulk` batch (~1,500–2,500 entries per syscall!).
    const BULK_BUF_BYTES: usize = 128 * 1024;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AttributeSet {
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AttrReference {
        attr_dataoffset: i32,
        attr_length: u32,
    }

    unsafe extern "C" {
        fn getattrlistbulk(
            dirfd: libc::c_int,
            attr_list: *mut AttrList,
            attr_buf: *mut libc::c_void,
            attr_buf_size: libc::size_t,
            options: u64,
        ) -> libc::c_int;
    }

    struct FdGuard(libc::c_int);
    impl Drop for FdGuard {
        fn drop(&mut self) {
            if self.0 >= 0 {
                unsafe { libc::close(self.0) };
            }
        }
    }

    pub fn getattrlistbulk_dir(dir: &Path) -> io::Result<Vec<CrawledEntry>> {
        let c_dir = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in directory path"))?;

        let fd = unsafe {
            libc::open(
                c_dir.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let _fd_guard = FdGuard(fd);

        let mut attr_list = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS
                | ATTR_CMN_ERROR
                | ATTR_CMN_NAME
                | ATTR_CMN_OBJTYPE
                | ATTR_CMN_MODTIME
                | ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: ATTR_FILE_DATALENGTH,
            forkattr: 0,
        };

        // 8-byte aligned kernel buffer (`Vec<u64>`) as required by XNU getattrlistbulk
        let mut aligned_buf = vec![0u64; BULK_BUF_BYTES / 8];
        let buf_ptr = aligned_buf.as_mut_ptr().cast::<u8>();

        let mut results = Vec::new();

        loop {
            let count = unsafe {
                getattrlistbulk(
                    fd,
                    &mut attr_list,
                    buf_ptr.cast(),
                    BULK_BUF_BYTES,
                    FSOPT_NOFOLLOW,
                )
            };

            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                break;
            }

            let mut entry_ptr = buf_ptr as *const u8;
            let buf_end = unsafe { buf_ptr.add(BULK_BUF_BYTES) as *const u8 };

            for _ in 0..count {
                if unsafe { entry_ptr.add(24) } > buf_end {
                    break;
                }
                let entry_len = unsafe { ptr::read_unaligned(entry_ptr.cast::<u32>()) } as usize;
                if entry_len < 24 || unsafe { entry_ptr.add(entry_len) } > buf_end {
                    break;
                }

                let next_entry = unsafe { entry_ptr.add(entry_len) };
                let mut field = unsafe { entry_ptr.add(4) };

                // 1. ATTR_CMN_RETURNED_ATTRS (always first)
                let returned = unsafe { ptr::read_unaligned(field.cast::<AttributeSet>()) };
                field = unsafe { field.add(std::mem::size_of::<AttributeSet>()) };

                // 2. ATTR_CMN_ERROR (always immediately after RETURNED_ATTRS when requested)
                let mut entry_err = 0u32;
                if (returned.commonattr & ATTR_CMN_ERROR) != 0 {
                    entry_err = unsafe { ptr::read_unaligned(field.cast::<u32>()) };
                    field = unsafe { field.add(4) };
                }

                // 3. ATTR_CMN_NAME (0x0000_0001)
                let mut name_os: Option<&OsStr> = None;
                if (returned.commonattr & ATTR_CMN_NAME) != 0 {
                    let name_ref_base = field;
                    let attr_ref = unsafe { ptr::read_unaligned(field.cast::<AttrReference>()) };
                    field = unsafe { field.add(std::mem::size_of::<AttrReference>()) };

                    let str_ptr = unsafe { name_ref_base.offset(attr_ref.attr_dataoffset as isize) };
                    if str_ptr >= entry_ptr && str_ptr < next_entry {
                        let c_str = unsafe { CStr::from_ptr(str_ptr.cast()) };
                        name_os = Some(OsStr::from_bytes(c_str.to_bytes()));
                    }
                }

                if entry_err != 0 {
                    entry_ptr = next_entry;
                    continue;
                }

                // 4. ATTR_CMN_OBJTYPE (0x0000_0008)
                let mut obj_type = 0u32;
                if (returned.commonattr & ATTR_CMN_OBJTYPE) != 0 {
                    obj_type = unsafe { ptr::read_unaligned(field.cast::<u32>()) };
                    field = unsafe { field.add(4) };
                }

                // 5. ATTR_CMN_MODTIME (0x0000_0400) -> struct timespec (tv_sec: i64, tv_nsec: i64)
                let mut mtime_secs = 0i64;
                if (returned.commonattr & ATTR_CMN_MODTIME) != 0 {
                    let ts = unsafe { ptr::read_unaligned(field.cast::<libc::timespec>()) };
                    mtime_secs = ts.tv_sec;
                    field = unsafe { field.add(std::mem::size_of::<libc::timespec>()) };
                }

                // 6. ATTR_CMN_FILEID (0x0200_0000) -> u64 inode
                let mut inode = 0u64;
                if (returned.commonattr & ATTR_CMN_FILEID) != 0 {
                    inode = unsafe { ptr::read_unaligned(field.cast::<u64>()) };
                    field = unsafe { field.add(8) };
                }

                // 7. File attribute group: ATTR_FILE_DATALENGTH (0x0000_0200) -> off_t (i64)
                let mut size_bytes = 0u64;
                if (returned.fileattr & ATTR_FILE_DATALENGTH) != 0
                    && unsafe { field.add(8) } <= next_entry
                {
                    let raw_len = unsafe { ptr::read_unaligned(field.cast::<i64>()) };
                    if raw_len > 0 {
                        size_bytes = raw_len as u64;
                    }
                }

                if let Some(name) = name_os
                    && name != "."
                    && name != ".."
                {
                    let kind = match obj_type {
                        VREG => EntryKind::File,
                        VDIR => EntryKind::Directory,
                        VLNK => EntryKind::Symlink,
                        _ => EntryKind::Other,
                    };
                    results.push(CrawledEntry {
                        path: dir.join(name),
                        inode,
                        mtime_secs,
                        size_bytes,
                        kind,
                        l0_only: false,
                    });
                }

                entry_ptr = next_entry;
            }
        }

        Ok(results)
    }
}

#[cfg(not(target_os = "macos"))]
fn posix_fallback_dir(dir: &Path) -> io::Result<Vec<CrawledEntry>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let kind = if meta.is_file() {
            EntryKind::File
        } else if meta.is_dir() {
            EntryKind::Directory
        } else if meta.file_type().is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        out.push(CrawledEntry {
            path: entry.path(),
            inode: meta.ino(),
            mtime_secs: meta.mtime(),
            size_bytes: meta.len(),
            kind,
            l0_only: false,
        });
    }
    Ok(out)
}
