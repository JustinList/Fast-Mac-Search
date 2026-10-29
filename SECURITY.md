# Security Policy & Cryptographic Architecture

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security or cryptographic vulnerabilities. Instead, use GitHub's **Report a vulnerability** private security advisory feature on this repository so a fix can be coordinated before disclosure.

## Cryptographic Guarantees & Threat Model

`fast-mac-search` is engineered so that **zero plaintext filenames, file contents, term dictionaries, posting lists, semantic vectors, or frecency history are ever written to persistent storage**.

### What Is Protected

1. **At-Rest Disk Confidentiality & Integrity (`crates/crypto-vfs`):**
   - Root 256-bit `MasterKey` derives isolated per-volume 256-bit `ShardKey`s via `HKDF-SHA256(MasterKey, shard_salt, "fms-shard-dek-v1::" || volume_uuid)`.
   - Every file within a shard derives a unique file subkey via `HKDF-SHA256(ShardKey, file_id)`.
   - Files are split into **16 KB AES-256-GCM** authenticated frames (`nonce = write_epoch_u32 || page_index_u64`).
   - Each frame's Associated Authenticated Data (`AAD`) binds `(magic, file_id, page_index, plain_len)` and the 64-byte file header is authenticated with keyed `BLAKE3`, preventing block-swapping, cross-file splicing, and truncation attacks.
2. **Swapfile Protection (`crates/l0-arena`):**
   - Plaintext UTF-8 path buffers in `L0PathArena` are wired into physical RAM with `mlock(2)` (within macOS `RLIMIT_MEMLOCK` limits) so macOS will not page plaintext file paths out to `/private/var/vm/swapfile*`, and wiped with `zeroize` prior to `munlock`/`Drop`.
3. **Local IPC Isolation (`crates/core-engine/src/ipc.rs`):**
   - The Unix Domain Socket (`~/Library/Caches/dev.fastmacsearch/searchd.sock`) and single-instance lockfile (`searchd.lock`) are restricted to `0600` (owner-only).

### Key Backend Tiers

- **Portable Passphrase Mode (`FMS_PASSPHRASE` / `.fastindex`):** Derives the `MasterKey` using `Argon2id` (`19 MiB` memory, `t=2`, `p=4`).
- **Signed Apple Developer Builds:** Supports hardware Apple Silicon Secure Enclave P-256 ECIES key wrapping (`kSecAttrTokenIDSecureEnclave`) and non-interactive Login Keychain storage.
- **Unsigned / Ad-Hoc Open-Source Builds (Default):** Uses a `0600` owner-only local random 256-bit keyfile (`~/Library/Application Support/FastMacSearch/keys/master.key`) to avoid repeated macOS `SecurityAgent` CDHash re-signing prompts.
