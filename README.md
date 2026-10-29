# ⚡ Fast Mac Search (`fms`)

**Sub-millisecond, zero-trust encrypted search-as-you-type for macOS & Apple Silicon.**

[![CI](https://github.com/JustinList/fast-mac-search/actions/workflows/ci.yml/badge.svg)](https://github.com/JustinList/fast-mac-search/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform: macOS 14+ (arm64)](https://img.shields.io/badge/platform-macOS%2014%2B%20(Apple%20Silicon)-black?logo=apple)](#install-the-mac-app-zero-cli)
[![Built with Rust & Swift 6](https://img.shields.io/badge/stack-Rust%20%2B%20Swift%206-orange)](#architecture)

`fast-mac-search` is an open-source replacement for macOS Spotlight (`mds` / `mdworker`) built in **Rust** with a native **Swift 6 / AppKit `NSPanel`** 120Hz ProMotion overlay. It bypasses Apple's lock-heavy metadata daemons to take direct control of XNU kernel syscalls (`getattrlistbulk(2)`), ARM NEON SIMD hardware (`std::arch::aarch64`), hardware-accelerated **16 KB page-level AES-256-GCM** encryption, and contextual **"System 1" predictive search**.

---

## Highlights

- **Sub-Millisecond Search-As-You-Type:** Splits search into a **3-Tier Parallel Keystroke Pipeline** where L0 fuzzy path hits and Ghost-Text predictions render in **< 0.5 ms** across 1,000,000 paths.
- **External Drive First-Class Citizen:** Never litters external drives with `.Spotlight-V100` folders.
  - **Host-Cached Mode (Default):** Stores encrypted shards per hardware Volume UUID on your internal NVMe—search external SSDs even while unplugged (`OFFLINE · Connect <Drive>`).
  - **Portable Encrypted Mode (`.fastindex/`):** Optional self-contained encrypted index on the external SSD itself for instant zero-reindex mounting across Macs.
- **Zero-Trust Encryption at Rest + Wired RAM:** Every Tantivy posting list, term dictionary, vector graph, path arena, and frecency graph is encrypted in **16 KB AES-256-GCM** authenticated frames on disk, while plaintext paths in RAM are wired via `mlock(2)` (never swapped to disk) and wiped via `zeroize`-on-drop.
- **Standalone Zero-CLI Mac App (`Fast Mac Search.app`):**
  - **1-Click Visual Drive & Folder Picker** right from the **`⚡` Menu Bar icon** or **Drives & Encrypted Shards Manager (`⌘,`)**.
  - **Pulsing Lightning Bolt + Live Hover Counter** showing scanned & encrypted file counts in real time during background indexing.
  - **Collapsible Auto-Dismissing Search Bar (`⌥Space`)**—compact `62pt` floating pill that expands dynamically on hits and vanishes immediately when clicking outside or pressing `Esc`.

---

## Install the Mac App (Zero CLI)

### Option A: Download Prebuilt `.dmg` (GitHub Releases)

1. Download **`Fast-Mac-Search-0.1.0-arm64.dmg`** from the [**Releases**](../../releases) page.
2. Open the `.dmg` and drag **Fast Mac Search.app** into **Applications**.
3. *(First launch on macOS Gatekeeper for ad-hoc signed OSS builds)* Clear the browser download quarantine flag once:
   ```bash
   xattr -cr "/Applications/Fast Mac Search.app"
   open "/Applications/Fast Mac Search.app"
   ```
4. Click the **`⚡`** icon in your macOS menu bar to pick any drive or folder to encrypt & index, then press **`⌥Space`** anywhere to search!

### Option B: Build `.app` + `.dmg` from Source (1 Command)

Requires macOS 14+ (Apple Silicon), Rust 1.85+, and Xcode Command Line Tools (`xcode-select --install`):

```bash
git clone https://github.com/JustinList/fast-mac-search.git
cd fast-mac-search
./scripts/build-macos-app.sh
open "dist/Fast-Mac-Search-0.1.0-arm64.dmg"
```

---

## Architecture

```text
Keystroke ("q3 rev")
  ├─► [Stage 1 · <0.5 ms] System 1 Ghost-Text + Markov Frecency + L0 ARM NEON Arena
  ├─► [Stage 2 ·  1–3 ms] L1 Tantivy Prefix-BM25 over 16KB AES-256-GCM VFS
  └─► [Stage 3 ·  1–4 ms] L2 128-dim Semantic Embedder + f16 NEON Vector Graph
             │
             ▼
   Dynamic Query-Length-Weighted Reciprocal Rank Fusion (RRF) ──► AF_UNIX IPC ──► 120Hz NSPanel
```

| Crate / Client | Description |
|---|---|
| `crates/crypto-vfs` | Hybrid `KeyProvider` (Secure Enclave P-256 ECIES + Keychain + Argon2id + `0600` sealed keyfile), HKDF-SHA256 per-volume DEKs, 16 KB AES-256-GCM `EncryptedBlockDirectory` for Tantivy |
| `crates/fs-indexer` | Raw XNU `getattrlistbulk(2)` bulk crawler (128 KB kernel buffers), `.gitignore` smart filtering, hardware Volume UUID (`ATTR_VOL_UUID`), real-time `FSEvents` GCD stream with checkpoint resume |
| `crates/l0-arena` | Contiguous UTF-8 path arena (`mlock` + `zeroize`), 64-bit ASCII + Bigram Bloom bitmasks, 4×-unrolled `aarch64` NEON SIMD scanner, compact `FMSL0B02` binary snapshots, and `nucleo-matcher` Smith-Waterman fuzzy scorer |
| `crates/l1-lexical` | Encrypted Tantivy BM25 + `PhrasePrefixQuery` FST prefix search-as-you-type and fast UTF-8 source/document text extractor |
| `crates/l2-semantic` | Pluggable `SemanticEmbedder` (CoreML/ANE/MLX ready + built-in sub-0.2ms `MicroSemanticEmbedder`) and `f16`-quantized NSW + ARM NEON (`vfmaq_f32`) vector index |
| `crates/system1-predictor` | Contextual Markov Frecency Graph (`(Prefix, FrontmostApp, Workspace) -> File` with 72h exponential half-life) + speculative inline Ghost-Text Autocomplete |
| `crates/core-engine` | Multi-volume shard orchestrator, atomic keystroke generation cancellation, dynamic RRF ranker, and `0600` Unix Domain Socket (`AF_UNIX`) IPC server |
| `clients/macos-ui` | Standalone Swift 6 / AppKit `.app` (`SpotlightPanel`, `DriveManagerWindowController`, `DaemonSupervisor`, `⚡` pulsing menu bar telemetry, `SMAppService` Launch-at-Login) |
| `clients/cli` | `fms` terminal CLI (human + `--json` streaming output for Raycast/Neovim/Alfred integrations) and `searchd` background daemon |

---

## Apple Silicon Benchmarks

Measured on Apple Silicon (M4, release profile with `.cargo/config.toml` ARMv8 Cryptography Extensions `+aes,+neon,+sha2`):

| Benchmark | Measured Result | Target |
|---|---|---|
| **L0 NEON SIMD + Smith-Waterman Fuzzy (`1,000,000` paths)** | **0.323 ms (`323 µs` p50)**, `mlock=true` (`102 MiB` RAM) | `< 0.50 ms` |
| **Full 3-Tier Parallel Search (`L0 + L1 + L2 + RRF`)** | **0.31 – 0.35 ms** warm (`L0: 13 µs`, `L1: 101 µs`, `L2: 32 µs`) | `< 4.00 ms` |
| **16 KB AES-256-GCM VFS (`pread(2)` + AEAD decrypt + verify)** | **1.68 GiB/s** end-to-end I/O (`0 ns` zero-copy on LRU page hit) | NVMe line rate |
| **Real External SSD Stress Test (`3,996,230` paths + `403,993` docs)** | **2.15 s** cold decrypt + `mlock` (`888 MB` encrypted binary shard) | Instant mount |

Run the hardware benchmark suite on your own Mac:

```bash
cargo run --release --bin fms -- bench --paths 1000000
```

---

## Keyboard Shortcuts (`Fast Mac Search.app`)

| Shortcut | Action |
|---|---|
| **`⌥Space`** | Toggle the compact floating Spotlight bar from any app or fullscreen Space |
| **Click outside / `Esc` / `✕`** | Instantly dismiss the search bar |
| **`Tab`** | Accept greyed-out System 1 Ghost-Text query autocomplete |
| **`↑` / `↓`** | Navigate ranked results |
| **`Return`** | Open selected file & reinforce System 1 Contextual Markov Frecency |
| **`⌘Return`** | Reveal selected file in Finder |
| **`⌘Y`** | Toggle native macOS QuickLook preview (`QLPreviewPanel`) |
| **`⌘,`** | Open the visual **Drives & Encrypted Shards** manager |
| **`⌘O`** | Add a specific folder or external drive via Finder picker |

---

## Optional Terminal CLI (`fms`)

Developers and power users can also embed or script the engine directly from the terminal:

```bash
# Build release binaries
cargo build --release

# Index a workspace into an encrypted Host-Cached shard
./target/release/fms index ~/Projects

# Create a Portable Encrypted Shard (.fastindex/) on an external SSD unlocked by passphrase
FMS_PASSPHRASE="your-strong-passphrase" ./target/release/fms index /Volumes/PortableSSD --portable

# Sub-millisecond terminal search (add --json for Raycast / Neovim / script pipelines)
./target/release/fms search "encrypted vfs"

# Inspect loaded volume shards, FSEvents checkpoints, and mlock status
./target/release/fms status
```

---

## Zero-Trust Security Architecture

See [SECURITY.md](SECURITY.md) for full cryptographic details:
- **Per-Volume & Per-File Key Derivation:** `MasterKey -> HKDF-SHA256(volume_uuid, shard_salt) -> ShardKey -> HKDF-SHA256(file_id) -> FileKey`.
- **Authenticated 16 KB Pages:** AES-256-GCM frames with `(magic, file_id, page_index, plaintext_len)` bound into AAD + keyed `BLAKE3` 64-byte file header MAC.
- **Zero Swap Leakage:** Plaintext path buffers in `L0PathArena` are wired with `mlock(2)` and scrubbed with `zeroize` on drop.

---

## Contributing & License

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md).

Dual-licensed under **MIT** ([LICENSE-MIT](LICENSE-MIT)) or **Apache-2.0** ([LICENSE-APACHE](LICENSE-APACHE)) at your option.
