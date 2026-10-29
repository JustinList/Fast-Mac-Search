//! `fms-l1-lexical` — Encrypted Full-Text BM25 & Search-As-You-Type Prefix Index.
//!
//! Wraps `tantivy` directly on top of [`fms_crypto_vfs::EncryptedBlockDirectory`], ensuring
//! zero plaintext terms, postings, fast fields, or document previews ever touch the disk.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use fms_crypto_vfs::{CryptoVfsError, EncryptedBlockDirectory, ShardKey, VfsConfig};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, PhrasePrefixQuery, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};
use thiserror::Error;

/// Maximum bytes read per text/source file during lexical & semantic content extraction (1 MiB).
pub const MAX_EXTRACT_BYTES: u64 = 1024 * 1024;
/// Maximum excerpt characters stored inside encrypted doc store for UI preview snippets.
const MAX_PREVIEW_CHARS: usize = 2048;

#[derive(Debug, Error)]
pub enum LexicalError {
    #[error("encrypted VFS error: {0}")]
    Vfs(#[from] CryptoVfsError),

    #[error("tantivy index error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
}

/// Search hit returned by the L1 Encrypted Tantivy BM25 + Prefix engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1Match {
    pub doc_id: u64,
    pub path: String,
    pub filename: String,
    pub bm25_score: f32,
    pub snippet: String,
}

#[derive(Clone, Copy)]
struct SchemaFields {
    doc_id: Field,
    path: Field,
    filename: Field,
    extension: Field,
    content_indexed: Field,
    preview_stored: Field,
    mtime: Field,
}

/// Encrypted Tantivy full-text index operating strictly inside `EncryptedBlockDirectory`.
/// Lazily acquires `IndexWriter` during ingestion batches and releases the writer lock upon `commit()`,
/// allowing concurrent zero-lock readers!
pub struct EncryptedLexicalIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<Option<IndexWriter>>,
    fields: SchemaFields,
}

impl EncryptedLexicalIndex {
    pub fn open_or_create(
        dir_path: impl AsRef<Path>,
        shard_key: ShardKey,
        vfs_config: VfsConfig,
    ) -> Result<Self, LexicalError> {
        let mut sb = Schema::builder();
        let doc_id = sb.add_u64_field("doc_id", INDEXED | STORED | FAST);
        let path = sb.add_text_field("path", STRING | STORED);
        let filename = sb.add_text_field("filename", TEXT | STORED);
        let extension = sb.add_text_field("extension", STRING | STORED);
        let content_indexed = sb.add_text_field("content", TEXT);
        let preview_stored = sb.add_text_field("preview", STORED);
        let mtime = sb.add_i64_field("mtime", FAST | STORED);
        let schema = sb.build();

        let enc_dir = EncryptedBlockDirectory::open(dir_path.as_ref(), shard_key, vfs_config)?;
        let index = Index::open_or_create(enc_dir, schema)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;

        Ok(Self {
            index,
            reader,
            writer: Mutex::new(None),
            fields: SchemaFields {
                doc_id,
                path,
                filename,
                extension,
                content_indexed,
                preview_stored,
                mtime,
            },
        })
    }

    /// Indexes or updates a document inside the encrypted Tantivy writer buffer.
    pub fn upsert_document(
        &self,
        doc_id: u64,
        path: &str,
        filename: &str,
        extension: &str,
        content: &str,
        mtime_secs: i64,
    ) -> Result<(), LexicalError> {
        let preview = truncate_utf8(content, MAX_PREVIEW_CHARS);
        let mut guard = self.writer.lock();
        if guard.is_none() {
            *guard = Some(self.index.writer_with_num_threads(2, 32_000_000)?);
        }
        let writer = guard.as_ref().unwrap();
        writer.delete_term(Term::from_field_text(self.fields.path, path));
        writer.add_document(doc!(
            self.fields.doc_id => doc_id,
            self.fields.path => path,
            self.fields.filename => filename,
            self.fields.extension => extension.to_ascii_lowercase(),
            self.fields.content_indexed => content,
            self.fields.preview_stored => preview,
            self.fields.mtime => mtime_secs,
        ))?;
        Ok(())
    }

    pub fn remove_by_path(&self, path: &str) -> Result<(), LexicalError> {
        let mut guard = self.writer.lock();
        if guard.is_none() {
            *guard = Some(self.index.writer_with_num_threads(2, 32_000_000)?);
        }
        guard
            .as_ref()
            .unwrap()
            .delete_term(Term::from_field_text(self.fields.path, path));
        Ok(())
    }

    /// Flushes and encrypts all staged Tantivy segments to disk, cleanly joins background
    /// merge threads so zero worker threads leak across commits, releases the writer lock,
    /// and reloads the in-memory reader.
    pub fn commit(&self) -> Result<(), LexicalError> {
        let taken_writer = {
            let mut guard = self.writer.lock();
            guard.take()
        };
        if let Some(mut writer) = taken_writer {
            writer.commit()?;
            writer.wait_merging_threads()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    /// Executes a sub-3ms BM25 + FST-prefix search-as-you-type query.
    pub fn search(&self, raw_query: &str, limit: usize) -> Result<(Vec<L1Match>, u64), LexicalError> {
        let started = Instant::now();
        let tokens = tokenize_query(raw_query);
        if tokens.is_empty() || limit == 0 {
            return Ok((Vec::new(), 0));
        }

        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(tokens.len() * 4);

        for (idx, tok) in tokens.iter().enumerate() {
            let is_last = idx + 1 == tokens.len();

            // Exact BM25 term match on filename (high boost) and content
            let fn_term = Term::from_field_text(self.fields.filename, tok);
            let ct_term = Term::from_field_text(self.fields.content_indexed, tok);

            let mut sub_should: Vec<(Occur, Box<dyn Query>)> = vec![
                (
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(fn_term.clone(), IndexRecordOption::WithFreqs)),
                        2.6,
                    )),
                ),
                (
                    Occur::Should,
                    Box::new(TermQuery::new(ct_term.clone(), IndexRecordOption::WithFreqs)),
                ),
            ];

            // Active keystroke prefix expansion via Tantivy FST PhrasePrefixQuery!
            if is_last && tok.len() >= 2 {
                let mut fn_prefix = PhrasePrefixQuery::new(vec![fn_term]);
                fn_prefix.set_max_expansions(32);
                let mut ct_prefix = PhrasePrefixQuery::new(vec![ct_term]);
                ct_prefix.set_max_expansions(32);

                sub_should.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(fn_prefix), 1.8)),
                ));
                sub_should.push((Occur::Should, Box::new(ct_prefix)));
            }

            let token_disjunction = BooleanQuery::new(sub_should);
            clauses.push((Occur::Must, Box::new(token_disjunction)));
        }

        let bool_query = BooleanQuery::new(clauses);
        let top_docs =
            searcher.search(&bool_query, &TopDocs::with_limit(limit).order_by_score())?;

        let mut out = Vec::with_capacity(top_docs.len());
        for (bm25_score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let doc_id = doc
                .get_first(self.fields.doc_id)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let path = doc
                .get_first(self.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let filename = doc
                .get_first(self.fields.filename)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let preview = doc
                .get_first(self.fields.preview_stored)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let snippet = build_compact_snippet(preview, &tokens);
            out.push(L1Match {
                doc_id,
                path,
                filename,
                bm25_score,
                snippet,
            });
        }

        let elapsed_us = started.elapsed().as_micros() as u64;
        Ok((out, elapsed_us))
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn tantivy_index(&self) -> &Index {
        &self.index
    }
}

/// Fast, zero-bloat UTF-8 text/code/config extractor that skips binary files immediately.
pub fn extract_text_from_file(path: &Path, known_size: u64) -> Option<String> {
    if known_size == 0 || known_size > MAX_EXTRACT_BYTES {
        return None;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_text_ext = matches!(
        ext.as_str(),
        "rs" | "swift"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "m"
            | "mm"
            | "py"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "go"
            | "java"
            | "kt"
            | "rb"
            | "php"
            | "sh"
            | "zsh"
            | "bash"
            | "fish"
            | "md"
            | "mdx"
            | "txt"
            | "rst"
            | "org"
            | "adoc"
            | "tex"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "jsonl"
            | "xml"
            | "plist"
            | "html"
            | "css"
            | "scss"
            | "sql"
            | "graphql"
            | "proto"
            | "csv"
            | "tsv"
            | "ini"
            | "cfg"
            | "conf"
            | "dockerfile"
            | "makefile"
    ) || matches!(
        path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "Makefile" | "Dockerfile" | "Cargo.lock" | "README" | "LICENSE" | ".gitignore"
    );

    if !is_text_ext {
        return None;
    }

    let file = File::open(path).ok()?;
    let cap = (known_size as usize).min(MAX_EXTRACT_BYTES as usize);
    let mut buf = Vec::with_capacity(cap);
    file.take(MAX_EXTRACT_BYTES).read_to_end(&mut buf).ok()?;

    // Reject binary blobs masquerading with text extensions by checking first 1024 bytes for NUL
    let probe_len = buf.len().min(1024);
    if buf[..probe_len].contains(&0) {
        return None;
    }

    String::from_utf8(buf).ok()
}

fn tokenize_query(q: &str) -> Vec<String> {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn truncate_utf8(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

fn build_compact_snippet(preview: &str, tokens: &[String]) -> String {
    if preview.is_empty() {
        return String::new();
    }
    let lower = preview.to_ascii_lowercase();
    let hit_pos = tokens
        .iter()
        .find_map(|t| lower.find(t.as_str()))
        .unwrap_or(0);

    let start_raw = hit_pos.saturating_sub(48);
    let mut start = start_raw;
    while start < preview.len() && !preview.is_char_boundary(start) {
        start += 1;
    }
    let end_raw = (start + 140).min(preview.len());
    let mut end = end_raw;
    while end > start && !preview.is_char_boundary(end) {
        end -= 1;
    }

    let cleaned = preview[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if start > 0 {
        format!("…{cleaned}…")
    } else if end < preview.len() {
        format!("{cleaned}…")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypted_lexical_prefix_as_you_type() {
        let mut dir = std::env::temp_dir();
        let r: u64 = 0xCAFE_BABE_1234;
        dir.push(format!("fms_l1_test_{r:x}"));
        let _ = std::fs::remove_dir_all(&dir);

        let key = ShardKey::random_for_tests();
        let idx = EncryptedLexicalIndex::open_or_create(&dir, key, VfsConfig::default()).unwrap();
        idx.upsert_document(
            42,
            "/Volumes/Crucial/notes/architecture.md",
            "architecture.md",
            "md",
            "Zero-trust Secure Enclave HKDF key derivation with sub-millisecond Tantivy BM25.",
            1_700_000_000,
        )
        .unwrap();
        idx.commit().unwrap();

        // Search-as-you-type partial prefix "encla"!
        let (hits, _us) = idx.search("zero encla", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, 42);
        assert!(hits[0].snippet.contains("Enclave"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
