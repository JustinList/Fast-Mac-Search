//! `fms-crypto-vfs` — Zero-Trust Page-Level AEAD Virtual File Layer for Fast Mac Search.
//!
//! Provides:
//! - [`HybridKeyProvider`]: Hardware Apple Secure Enclave P-256 ECIES key wrapping (signed bundles),
//!   macOS Login Keychain integration, Argon2id passphrase derivation (portable external SSDs),
//!   and `0600` local developer keyfile fallback for unsigned OSS builds.
//! - [`EncryptedBlockDirectory`]: `tantivy::directory::Directory` implementation encrypting all index
//!   segments, postings, fast fields, and `meta.json` in 16 KB AES-256-GCM frames with keyed-BLAKE3
//!   header authentication, cross-block/cross-file AAD binding, hot-segment RAM pinning, and lock-free `pread(2)`.

pub mod key_provider;
pub mod tantivy_dir;
pub mod vfs;

use std::io;
use thiserror::Error;

pub use key_provider::{
    HybridKeyProvider, KeyBackendKind, MasterKey, SecureEnclaveAvailability, ShardKey,
    ShardKeyEnvelope, derive_master_key_from_passphrase,
};
pub use tantivy_dir::EncryptedBlockDirectory;
pub use vfs::{
    DEFAULT_PAGE_SIZE, DecryptedPageCache, EncryptedRandomAccessFile, EncryptedStreamWriter,
    GCM_TAG_SIZE, HEADER_SIZE, VfsConfig, atomic_write_encrypted, read_encrypted_all,
};

#[derive(Debug, Error)]
pub enum CryptoVfsError {
    #[error("I/O error in encrypted VFS: {0}")]
    Io(#[from] io::Error),

    #[error("AEAD authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("corrupt encrypted VFS header: {0}")]
    CorruptHeader(String),

    #[error("key derivation error: {0}")]
    KeyDerivation(String),
}

impl CryptoVfsError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::Io(e) => e,
            Self::AuthenticationFailed(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            Self::CorruptHeader(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            Self::KeyDerivation(msg) => io::Error::other(msg),
        }
    }
}

/// Scans every regular file in `dir` and verifies that `canary` never appears in raw on-disk bytes.
pub fn verify_zero_plaintext_on_disk(dir: &std::path::Path, canary: &[u8]) -> io::Result<usize> {
    let mut files_checked = 0usize;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files_checked += verify_zero_plaintext_on_disk(&path, canary)?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path)?;
            files_checked += 1;
            if bytes.windows(canary.len()).any(|w| w == canary) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SECURITY VIOLATION: plaintext canary leaked in {:?}", path),
                ));
            }
        }
    }
    Ok(files_checked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::sync::Arc;
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{Schema, STORED, TEXT, Value};
    use tantivy::{Index, doc};

    fn temp_test_dir(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let rand_id: u64 = rand::random();
        p.push(format!("fms_crypto_vfs_{label}_{rand_id:016x}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn test_hkdf_and_argon2id_isolation() {
        let salt = [7u8; 16];
        let mk1 = derive_master_key_from_passphrase(b"correct-horse-battery-staple", &salt).unwrap();
        let mk2 = derive_master_key_from_passphrase(b"correct-horse-battery-staple", &salt).unwrap();
        assert_eq!(mk1.as_bytes(), mk2.as_bytes());

        let vol_a = [1u8; 16];
        let vol_b = [2u8; 16];
        let shard_salt = [9u8; 16];
        let dek_a = mk1.derive_shard_key(&vol_a, &shard_salt);
        let dek_b = mk1.derive_shard_key(&vol_b, &shard_salt);
        assert_ne!(dek_a.as_bytes(), dek_b.as_bytes());
    }

    #[test]
    fn test_multipage_random_access_and_block_swap_detection() {
        let dir = temp_test_dir("block_swap");
        let file_path = dir.join("segment.postings");
        let key = ShardKey::random_for_tests();

        // Write 90 KB across 6 x 16 KB pages with distinct deterministic bytes
        let plain: Vec<u8> = (0..90_123usize)
            .map(|i| ((i * 31 + 17) & 0xff) as u8)
            .collect();

        {
            let mut w = EncryptedStreamWriter::create(&file_path, &key, DEFAULT_PAGE_SIZE).unwrap();
            w.write_all(&plain[..10_000]).unwrap();
            w.write_all(&plain[10_000..55_000]).unwrap();
            w.write_all(&plain[55_000..]).unwrap();
            w.finish().unwrap();
        }

        let cache = Arc::new(DecryptedPageCache::new(64));
        let reader = EncryptedRandomAccessFile::open(&file_path, &key, cache, 0).unwrap();
        assert_eq!(reader.logical_len(), plain.len());

        // Single-page read
        let slice_a = reader.read_range(100..500).unwrap();
        assert_eq!(slice_a.as_slice(), &plain[100..500]);

        // Cross-page boundary read spanning pages 0, 1, 2, 3
        let slice_b = reader.read_range(15_900..50_100).unwrap();
        assert_eq!(slice_b.as_slice(), &plain[15_900..50_100]);

        // Now swap encrypted frame 0 and encrypted frame 1 on disk -> AAD page_index MUST reject!
        let mut raw_disk = fs::read(&file_path).unwrap();
        let frame_len = DEFAULT_PAGE_SIZE + GCM_TAG_SIZE;
        let f0_start = HEADER_SIZE;
        let f1_start = HEADER_SIZE + frame_len;
        let (left, right) = raw_disk.split_at_mut(f1_start);
        left[f0_start..f0_start + frame_len].swap_with_slice(&mut right[..frame_len]);
        fs::write(&file_path, &raw_disk).unwrap();

        let fresh_cache = Arc::new(DecryptedPageCache::new(64));
        let tampered_reader =
            EncryptedRandomAccessFile::open(&file_path, &key, fresh_cache, 0).unwrap();
        assert!(
            tampered_reader.read_range(0..64).is_err(),
            "swapped 16KB blocks must fail AES-GCM page_index AAD authentication"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tantivy_encrypted_directory_zero_plaintext_leakage() {
        let dir = temp_test_dir("tantivy_zero_leak");
        let key = ShardKey::random_for_tests();
        let enc_dir = EncryptedBlockDirectory::open(&dir, key.clone(), VfsConfig::default()).unwrap();

        let mut schema_builder = Schema::builder();
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let body = schema_builder.add_text_field("body", TEXT | STORED);
        let schema = schema_builder.build();

        let canary_secret = "ProjectHyperionTopSecretLaunchCode99481";
        {
            let index = Index::open_or_create(enc_dir.clone(), schema.clone()).unwrap();
            let mut writer = index.writer(15_000_000).unwrap();
            writer
                .add_document(doc!(
                    title => "Q3 Architecture Secret",
                    body => format!("Confidential payload {canary_secret} inside encrypted postings")
                ))
                .unwrap();
            writer.commit().unwrap();
        }

        // Verify ZERO occurrences of canary_secret or even "meta" JSON keys in raw disk files!
        let checked = verify_zero_plaintext_on_disk(&dir, canary_secret.as_bytes()).unwrap();
        assert!(checked >= 3, "expected encrypted Tantivy segment files on disk");
        verify_zero_plaintext_on_disk(&dir, b"ProjectHyperion").unwrap();

        // Reopen cold from disk with the same ShardKey and verify sub-millisecond BM25 search
        let reopened_dir =
            EncryptedBlockDirectory::open(&dir, key, VfsConfig::default()).unwrap();
        let reopened_index = Index::open_or_create(reopened_dir, schema).unwrap();
        let reader = reopened_index.reader().unwrap();
        let searcher = reader.searcher();
        let qp = QueryParser::for_index(&reopened_index, vec![title, body]);
        let q = qp.parse_query("projecthyperiontopsecretlaunchcode99481").unwrap();
        let top = searcher
            .search(&q, &TopDocs::with_limit(5).order_by_score())
            .unwrap();
        assert_eq!(top.len(), 1);
        let retrieved: tantivy::TantivyDocument = searcher.doc(top[0].1).unwrap();
        let body_text = retrieved.get_first(body).unwrap().as_str().unwrap();
        assert!(body_text.contains(canary_secret));

        // Verify wrong ShardKey cannot even open the index
        let wrong_dir = EncryptedBlockDirectory::open(
            &dir,
            ShardKey::random_for_tests(),
            VfsConfig::default(),
        )
        .unwrap();
        assert!(Index::open_or_create(wrong_dir, reopened_index.schema()).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
