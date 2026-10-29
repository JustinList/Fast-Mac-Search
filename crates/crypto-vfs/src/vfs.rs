use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use hkdf::Hkdf;
use lru::LruCache;
use parking_lot::Mutex;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use tantivy::directory::OwnedBytes;
use zeroize::Zeroize;

use crate::CryptoVfsError;
use crate::key_provider::ShardKey;

/// Default page size: 16 KB (matches Apple Silicon hardware MMU page size).
pub const DEFAULT_PAGE_SIZE: usize = 16 * 1024;
/// AES-256-GCM authentication tag size (128-bit).
pub const GCM_TAG_SIZE: usize = 16;
/// Fixed file header size on disk (64 bytes).
pub const HEADER_SIZE: usize = 64;

const HEADER_MAGIC: &[u8; 8] = b"FMSVFS01";
const PAGE_AAD_MAGIC: &[u8; 8] = b"FMSPGV01";
const FORMAT_VERSION: u16 = 1;

/// Configuration for the Page-Level AEAD Virtual File Layer.
#[derive(Debug, Clone)]
pub struct VfsConfig {
    /// Plaintext bytes per encrypted page frame (4096 or 16384).
    pub page_size: usize,
    /// Maximum number of decrypted pages retained in the shared LRU cache.
    pub cache_pages: usize,
    /// Files smaller than or equal to this byte threshold are decrypted once on open
    /// and pinned as zero-copy `OwnedBytes` (ideal for Tantivy FST term dicts & fast fields).
    pub hot_pin_max_bytes: usize,
}

impl Default for VfsConfig {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            cache_pages: 2048, // 32 MiB at 16 KB pages
            hot_pin_max_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FileHeader {
    pub page_size: u32,
    pub logical_len: u64,
    pub file_id: [u8; 16],
    pub write_epoch: [u8; 4],
}

impl FileHeader {
    fn encode(&self, header_mac_key: &[u8; 32]) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..8].copy_from_slice(HEADER_MAGIC);
        buf[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[10..12].copy_from_slice(&0u16.to_le_bytes());
        buf[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.logical_len.to_le_bytes());
        buf[24..40].copy_from_slice(&self.file_id);
        buf[40..44].copy_from_slice(&self.write_epoch);

        let mac = blake3::keyed_hash(header_mac_key, &buf[0..44]);
        buf[44..64].copy_from_slice(&mac.as_bytes()[0..20]);
        buf
    }

    fn decode(buf: &[u8; HEADER_SIZE], header_mac_key: &[u8; 32]) -> Result<Self, CryptoVfsError> {
        if &buf[0..8] != HEADER_MAGIC {
            return Err(CryptoVfsError::CorruptHeader(
                "invalid magic bytes (not an FMS encrypted file)".into(),
            ));
        }
        let expected_mac = blake3::keyed_hash(header_mac_key, &buf[0..44]);
        // Constant-time comparison via blake3 Hash equality semantics on full 20-byte tag:
        let mut diff = 0u8;
        for (a, b) in buf[44..64].iter().zip(&expected_mac.as_bytes()[0..20]) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return Err(CryptoVfsError::AuthenticationFailed(
                "header MAC verification failed (wrong key or tampered file length)".into(),
            ));
        }

        let version = u16::from_le_bytes([buf[8], buf[9]]);
        if version != FORMAT_VERSION {
            return Err(CryptoVfsError::CorruptHeader(format!(
                "unsupported VFS version {version}"
            )));
        }
        let page_size = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        if page_size < 512 || page_size > 1024 * 1024 {
            return Err(CryptoVfsError::CorruptHeader(format!(
                "invalid page size {page_size}"
            )));
        }
        let logical_len = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let mut file_id = [0u8; 16];
        file_id.copy_from_slice(&buf[24..40]);
        let mut write_epoch = [0u8; 4];
        write_epoch.copy_from_slice(&buf[40..44]);

        Ok(Self {
            page_size,
            logical_len,
            file_id,
            write_epoch,
        })
    }
}

fn derive_header_mac_key(shard_key: &ShardKey) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, shard_key.as_bytes());
    let mut out = [0u8; 32];
    hk.expand(b"fms-vfs-header-mac-v1", &mut out)
        .expect("valid HKDF length");
    out
}

fn derive_file_cipher(shard_key: &ShardKey, file_id: &[u8; 16]) -> Aes256Gcm {
    let hk = Hkdf::<Sha256>::new(Some(file_id), shard_key.as_bytes());
    let mut raw_key = [0u8; 32];
    hk.expand(b"fms-vfs-file-aead-v1", &mut raw_key)
        .expect("valid HKDF length");
    let cipher = Aes256Gcm::new_from_slice(&raw_key).expect("32-byte AES-256 key");
    raw_key.zeroize();
    cipher
}

#[inline]
fn make_page_nonce(write_epoch: &[u8; 4], page_index: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0..4].copy_from_slice(write_epoch);
    nonce[4..12].copy_from_slice(&page_index.to_le_bytes());
    nonce
}

#[inline]
fn make_page_aad(file_id: &[u8; 16], page_index: u64, plain_len: u32) -> [u8; 36] {
    let mut aad = [0u8; 36];
    aad[0..8].copy_from_slice(PAGE_AAD_MAGIC);
    aad[8..24].copy_from_slice(file_id);
    aad[24..32].copy_from_slice(&page_index.to_le_bytes());
    aad[32..36].copy_from_slice(&plain_len.to_le_bytes());
    aad
}

/// Shared decrypted 16KB page cache indexed by `(file_id_u128, page_index)`.
pub struct DecryptedPageCache {
    lru: Mutex<LruCache<(u128, u64), OwnedBytes>>,
}

impl DecryptedPageCache {
    pub fn new(capacity_pages: usize) -> Self {
        let cap = NonZeroUsize::new(capacity_pages.max(16)).unwrap();
        Self {
            lru: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn get(&self, key: &(u128, u64)) -> Option<OwnedBytes> {
        self.lru.lock().get(key).cloned()
    }

    pub fn put(&self, key: (u128, u64), page: OwnedBytes) {
        self.lru.lock().put(key, page);
    }
}

/// Streaming page-level AEAD writer that encrypts plaintext in `page_size` frames directly to disk,
/// writing the authenticated `FileHeader` upon `finish()`.
pub struct EncryptedStreamWriter {
    file: File,
    cipher: Aes256Gcm,
    header_mac_key: [u8; 32],
    file_id: [u8; 16],
    write_epoch: [u8; 4],
    page_size: usize,
    page_buf: Vec<u8>,
    current_page_index: u64,
    logical_len: u64,
    finished: bool,
}

impl EncryptedStreamWriter {
    pub fn create(
        path: &Path,
        shard_key: &ShardKey,
        page_size: usize,
    ) -> Result<Self, CryptoVfsError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)?;

        // Reserve 64-byte header space; finalized with authenticated logical_len on finish().
        file.write_all(&[0u8; HEADER_SIZE])?;

        let mut file_id = [0u8; 16];
        let mut write_epoch = [0u8; 4];
        OsRng.fill_bytes(&mut file_id);
        OsRng.fill_bytes(&mut write_epoch);

        let cipher = derive_file_cipher(shard_key, &file_id);
        let header_mac_key = derive_header_mac_key(shard_key);

        Ok(Self {
            file,
            cipher,
            header_mac_key,
            file_id,
            write_epoch,
            page_size,
            page_buf: Vec::with_capacity(page_size + GCM_TAG_SIZE),
            current_page_index: 0,
            logical_len: 0,
            finished: false,
        })
    }

    fn flush_full_or_tail_page(&mut self) -> io::Result<()> {
        if self.page_buf.is_empty() {
            return Ok(());
        }
        let plain_len = self.page_buf.len() as u32;
        let nonce_bytes = make_page_nonce(&self.write_epoch, self.current_page_index);
        let aad = make_page_aad(&self.file_id, self.current_page_index, plain_len);

        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce_bytes), &aad, &mut self.page_buf)
            .map_err(|_| io::Error::other("AES-256-GCM page encryption failed"))?;

        self.page_buf.extend_from_slice(tag.as_slice());
        self.file.write_all(&self.page_buf)?;
        self.page_buf.zeroize();
        self.page_buf.clear();
        self.current_page_index += 1;
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_full_or_tail_page()?;
        let header = FileHeader {
            page_size: self.page_size as u32,
            logical_len: self.logical_len,
            file_id: self.file_id,
            write_epoch: self.write_epoch,
        };
        let encoded = header.encode(&self.header_mac_key);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&encoded)?;
        self.file.sync_data()?;
        self.finished = true;
        Ok(())
    }
}

impl Write for EncryptedStreamWriter {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let total = buf.len();
        while !buf.is_empty() {
            let space = self.page_size - self.page_buf.len();
            let take = space.min(buf.len());
            self.page_buf.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            self.logical_len += take as u64;

            if self.page_buf.len() == self.page_size {
                self.flush_full_or_tail_page()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for EncryptedStreamWriter {
    fn drop(&mut self) {
        self.page_buf.zeroize();
        self.header_mac_key.zeroize();
    }
}

/// Random-access encrypted file reader backed by lock-free `pread(2)` and a zero-copy
/// decrypted 16KB LRU page cache (with automatic RAM pinning for small hot index segments).
pub struct EncryptedRandomAccessFile {
    path: PathBuf,
    file: File,
    header: FileHeader,
    file_id_u128: u128,
    cipher: Aes256Gcm,
    cache: Arc<DecryptedPageCache>,
    pinned_full: Option<OwnedBytes>,
}

impl std::fmt::Debug for EncryptedRandomAccessFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedRandomAccessFile")
            .field("path", &self.path)
            .field("logical_len", &self.header.logical_len)
            .field("page_size", &self.header.page_size)
            .field("pinned", &self.pinned_full.is_some())
            .finish()
    }
}

impl EncryptedRandomAccessFile {
    pub fn open(
        path: &Path,
        shard_key: &ShardKey,
        cache: Arc<DecryptedPageCache>,
        hot_pin_max_bytes: usize,
    ) -> Result<Self, CryptoVfsError> {
        let file = File::open(path)?;
        let mut hdr_buf = [0u8; HEADER_SIZE];
        file.read_exact_at(&mut hdr_buf, 0)?;

        let mut header_mac_key = derive_header_mac_key(shard_key);
        let header = FileHeader::decode(&hdr_buf, &header_mac_key)?;
        header_mac_key.zeroize();

        let cipher = derive_file_cipher(shard_key, &header.file_id);
        let file_id_u128 = u128::from_le_bytes(header.file_id);

        let mut reader = Self {
            path: path.to_path_buf(),
            file,
            header,
            file_id_u128,
            cipher,
            cache,
            pinned_full: None,
        };

        // Pin small hot files (e.g. Tantivy FST term dictionaries, meta.json, fast-field headers)
        let len = reader.logical_len();
        if len > 0 && len <= hot_pin_max_bytes {
            let full = reader.read_range_uncached_all()?;
            reader.pinned_full = Some(OwnedBytes::new(full));
        }

        Ok(reader)
    }

    #[inline]
    pub fn logical_len(&self) -> usize {
        self.header.logical_len as usize
    }

    /// Reads an arbitrary byte range `[range.start .. range.end)` and returns `OwnedBytes`.
    /// Single-page hits and pinned files are 100% zero-copy slices!
    pub fn read_range(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        let total_len = self.logical_len();
        if range.end > total_len || range.start > range.end {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "encrypted read out of bounds: {:?} > logical_len {}",
                    range, total_len
                ),
            ));
        }
        if range.is_empty() {
            return Ok(OwnedBytes::empty());
        }

        if let Some(pinned) = &self.pinned_full {
            return Ok(pinned.slice(range));
        }

        let page_size = self.header.page_size as usize;
        let first_page = range.start / page_size;
        let last_page = (range.end - 1) / page_size;

        // Fast path: range resides inside a single 16KB page -> zero-copy slice of cached OwnedBytes!
        if first_page == last_page {
            let page = self.get_or_decrypt_page(first_page as u64)?;
            let in_start = range.start % page_size;
            let in_end = in_start + range.len();
            return Ok(page.slice(in_start..in_end));
        }

        // Multi-page stitch
        let mut out = Vec::with_capacity(range.len());
        for page_idx in first_page..=last_page {
            let page = self.get_or_decrypt_page(page_idx as u64)?;
            let page_global_start = page_idx * page_size;
            let slice_start = range.start.saturating_sub(page_global_start);
            let slice_end = (range.end - page_global_start).min(page.len());
            out.extend_from_slice(&page.as_slice()[slice_start..slice_end]);
        }
        Ok(OwnedBytes::new(out))
    }

    fn get_or_decrypt_page(&self, page_index: u64) -> io::Result<OwnedBytes> {
        let cache_key = (self.file_id_u128, page_index);
        if let Some(hit) = self.cache.get(&cache_key) {
            return Ok(hit);
        }

        let page_size = self.header.page_size as u64;
        let plain_start = page_index * page_size;
        let remaining = self.header.logical_len.saturating_sub(plain_start);
        let expected_plain_len = remaining.min(page_size) as usize;

        let frame_size = page_size + GCM_TAG_SIZE as u64;
        let disk_offset = HEADER_SIZE as u64 + page_index * frame_size;
        let disk_read_len = expected_plain_len + GCM_TAG_SIZE;

        let mut frame = vec![0u8; disk_read_len];
        self.file.read_exact_at(&mut frame, disk_offset)?;

        let tag_offset = expected_plain_len;
        let tag = Tag::clone_from_slice(&frame[tag_offset..]);
        frame.truncate(expected_plain_len);

        let nonce = make_page_nonce(&self.header.write_epoch, page_index);
        let aad = make_page_aad(&self.header.file_id, page_index, expected_plain_len as u32);

        self.cipher
            .decrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut frame, &tag)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "AES-256-GCM authentication failed on {:?} page {page_index}",
                        self.path
                    ),
                )
            })?;

        let owned = OwnedBytes::new(frame);
        self.cache.put(cache_key, owned.clone());
        Ok(owned)
    }

    fn read_range_uncached_all(&self) -> Result<Vec<u8>, CryptoVfsError> {
        let total = self.logical_len();
        let page_size = self.header.page_size as usize;
        let num_pages = total.div_ceil(page_size);
        let mut full = Vec::with_capacity(total);
        for page_idx in 0..num_pages {
            let page = self.get_or_decrypt_page(page_idx as u64)?;
            full.extend_from_slice(page.as_slice());
        }
        Ok(full)
    }
}

/// Atomically encrypts and writes an arbitrary byte slice (used for `meta.json`, L0 arena snapshots,
/// L2 vector shards, and System 1 frecency graphs).
pub fn atomic_write_encrypted(
    path: &Path,
    shard_key: &ShardKey,
    plaintext: &[u8],
    page_size: usize,
) -> Result<(), CryptoVfsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut rand_suffix = [0u8; 6];
    OsRng.fill_bytes(&mut rand_suffix);
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("blob"),
        crate::key_provider::hex_encode(&rand_suffix)
    );
    let tmp_path = path.with_file_name(tmp_name);

    {
        let mut writer = EncryptedStreamWriter::create(&tmp_path, shard_key, page_size)?;
        writer.write_all(plaintext)?;
        writer.finish()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Reads and authenticates an entire encrypted blob created by `atomic_write_encrypted` or `EncryptedStreamWriter`.
pub fn read_encrypted_all(
    path: &Path,
    shard_key: &ShardKey,
    cache: Arc<DecryptedPageCache>,
) -> Result<Vec<u8>, CryptoVfsError> {
    let reader = EncryptedRandomAccessFile::open(path, shard_key, cache, 0)?;
    let bytes = reader.read_range(0..reader.logical_len())?;
    Ok(bytes.as_slice().to_vec())
}
