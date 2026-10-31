# ⚡ Fast Mac Search (`fms`)

**Sub-millisecond, zero-trust encrypted search across macOS drives & every AI assistant on Apple Silicon.**

[![CI](https://github.com/JustinList/Fast-Mac-Search/actions/workflows/ci.yml/badge.svg)](https://github.com/JustinList/Fast-Mac-Search/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Platform: macOS 14+ (arm64)](https://img.shields.io/badge/platform-macOS%2014%2B%20(Apple%20Silicon)-black?logo=apple)](#install-the-mac-app-zero-cli)
[![Built with Rust & Swift 6](https://img.shields.io/badge/stack-Rust%20%2B%20Swift%206%20%2B%20MCP-orange)](#architecture)

`fast-mac-search` is an open-source replacement for macOS Spotlight (`mds` / `mdworker`) AND a unified **Cross-LLM Encrypted Memory + MCP Server** built in **Rust** with a native **Swift 6 / AppKit `NSPanel`** 120Hz ProMotion overlay. It bypasses Apple's lock-heavy metadata daemons to take direct control of XNU kernel syscalls (`getattrlistbulk(2)`), ARM NEON SIMD hardware (`std::arch::aarch64`), hardware-accelerated **16 KB page-level AES-256-GCM** encryption, and contextual **"System 1" predictive search**.

---

## Highlights

- **Sub-Millisecond Search-As-You-Type:** Splits search into a **3-Tier Parallel Keystroke Pipeline** where L0 fuzzy path hits and Ghost-Text predictions render in **< 0.5 ms** across 1,000,000+ paths.
- **Cross-LLM Encrypted Memory (`v0.2.0`):**
  - Automatically discovers and incrementally indexes local transcripts from **Claude Code** (`~/.claude/projects`), **Claude Desktop** (`~/Library/Application Support/Claude`), **OpenAI Codex** (`~/.codex/sessions`), **Gemini / Antigravity** (`~/.gemini/antigravity/brain`), and **LM Studio** (`~/.lmstudio/conversations`).
  - **1-Click ChatGPT & Claude Web Importer:** Parse `conversations.json` exports directly into AES-256-GCM encrypted memory—then safely delete the plaintext export file!
  - **Native Conversation Reader Window:** Filter by **`All | Files | AI`** (`⌘1` / `⌘2` / `⌘3`) or type `@claude`, `@codex`, `@gemini`, or `@chatgpt` in `⌥Space` and press `Return` to read highlighted transcripts, copy full Markdown threads, or copy CLI `--resume` commands.
- **Built-in Model Context Protocol (`fms mcp`) Server:** Give Claude Desktop, Claude Code, Codex, Cursor, and Antigravity `< 2 ms` tool access (`search_all`, `search_llm_chats`, `get_llm_conversation`, `search_status`) across all your drives and prior AI sessions!
- **External Drive First-Class Citizen:** Never litters external drives with `.Spotlight-V100` folders.
  - **Host-Cached Mode (Default):** Stores encrypted shards per hardware Volume UUID on your internal NVMe—search external SSDs even while unplugged (`OFFLINE · Connect <Drive>`).
  - **Portable Encrypted Mode (`.fastindex/`):** Optional self-contained encrypted index on the external SSD itself for instant zero-reindex mounting across Macs.
- **Zero-Trust Encryption at Rest + Wired RAM:** Every Tantivy posting list, term dictionary, vector graph, path arena, frecency graph, and LLM transcript cache is encrypted in **16 KB AES-256-GCM** authenticated frames on disk, while plaintext paths in RAM are wired via `mlock(2)` (never swapped to disk) and wiped via `zeroize`-on-drop.

---

## Install the Mac App (Zero CLI)

### Option A: Download Prebuilt `.dmg` (GitHub Releases)

1. Download **`Fast-Mac-Search-0.2.0-arm64.dmg`** from the [**Releases**](../../releases) page.
2. Open the `.dmg` and drag **Fast Mac Search.app** into **Applications**.
3. *(First launch on macOS Gatekeeper for ad-hoc signed OSS builds)* Clear the browser download quarantine flag once:
   ```bash
   xattr -cr "/Applications/Fast Mac Search.app"
   open "/Applications/Fast Mac Search.app"
   ```
4. Click the **`⚡`** icon in your macOS menu bar to pick drives/folders, import a ChatGPT/Claude `.json` export, or enable the Claude Desktop MCP server—then press **`⌥Space`** anywhere to search!

### Option B: Build `.app` + `.dmg` from Source (1 Command)

Requires macOS 14+ (Apple Silicon), Rust 1.85+, and Xcode Command Line Tools (`xcode-select --install`):

```bash
git clone https://github.com/JustinList/Fast-Mac-Search.git
cd Fast-Mac-Search
./scripts/build-macos-app.sh
open "dist/Fast-Mac-Search-0.2.0-arm64.dmg"
```

---

## Connect Your AI Agents via MCP (`fms mcp`)

`Fast Mac Search.app` ships with a built-in stdio JSON-RPC 2.0 **Model Context Protocol** server at `/Applications/Fast Mac Search.app/Contents/Helpers/fms` that talks directly to the warm `searchd` Unix Domain Socket in `~15 µs`.

- **Claude Desktop (1-Click):** Click **`⚡` → `Enable MCP Server for Claude Desktop…`** in the menu bar (or run `fms mcp-install`).
- **Claude Code CLI:**
  ```bash
  claude mcp add fast-mac-search -- "/Applications/Fast Mac Search.app/Contents/Helpers/fms" mcp
  ```
- **OpenAI Codex CLI:**
  ```bash
  codex mcp add fast-mac-search -- "/Applications/Fast Mac Search.app/Contents/Helpers/fms" mcp
  ```

### Exposed MCP Tools
| Tool | Purpose |
|---|---|
| `search_all` | Unified `< 2 ms` search across encrypted files + AI chats (`scope`: `"all" \| "files" \| "chats"`) |
| `search_llm_chats` | Search or list recent conversations filtered by `provider` (`"claude"`, `"codex"`, `"gemini"`, `"chatgpt"`, `"lmstudio"`) |
| `get_llm_conversation` | Retrieve the full clean user/assistant transcript of any session by `session_id` |
| `search_status` | Inspect mounted/offline volume shards and encrypted AI chat counts |

---

## Architecture

```text
Keystroke ("@claude keychain" or "q3 rev")
  ├─► [Stage 1 · <0.5 ms] System 1 Ghost-Text + Markov Frecency + L0 ARM NEON Arena
  ├─► [Stage 2 ·  1–3 ms] L1 Tantivy Prefix-BM25 over 16KB AES-256-GCM VFS
  └─► [Stage 3 ·  1–4 ms] L2 128-dim Semantic Embedder + f16 NEON Vector Graph
             │
             ▼
   Dynamic Query-Length-Weighted Reciprocal Rank Fusion (RRF) ──► AF_UNIX IPC ──► 120Hz NSPanel & MCP
```

| Crate / Client | Description |
|---|---|
| `crates/crypto-vfs` | Hybrid `KeyProvider` (Secure Enclave P-256 ECIES + Keychain + Argon2id + `0600` sealed keyfile), HKDF-SHA256 per-volume DEKs, 16 KB AES-256-GCM `EncryptedBlockDirectory` for Tantivy |
| `crates/fs-indexer` | Raw XNU `getattrlistbulk(2)` bulk crawler (128 KB kernel buffers), `.gitignore` smart filtering, hardware Volume UUID (`ATTR_VOL_UUID`), real-time `FSEvents` GCD stream with checkpoint resume |
| `crates/l0-arena` | Contiguous UTF-8 path arena (`mlock` + `zeroize`), 64-bit ASCII + Bigram Bloom bitmasks, 4×-unrolled `aarch64` NEON SIMD scanner, compact `FMSL0B02` binary snapshots, and `nucleo-matcher` Smith-Waterman fuzzy scorer |
| `crates/l1-lexical` | Encrypted Tantivy BM25 + `PhrasePrefixQuery` FST prefix search-as-you-type and fast UTF-8 source/document text extractor |
| `crates/l2-semantic` | Pluggable `SemanticEmbedder` (CoreML/ANE/MLX ready + built-in sub-0.2ms `MicroSemanticEmbedder`) and `f16`-quantized NSW + ARM NEON (`vfmaq_f32`) vector index |
| `crates/llm-memory` | Streaming cross-LLM parsers (Claude, Codex, Gemini/Antigravity, ChatGPT, LM Studio) + incremental `mtime` sync + AES-256-GCM 3-tier conversation memory |
| `crates/system1-predictor` | Contextual Markov Frecency Graph (`(Prefix, FrontmostApp, Workspace) -> File` with 72h exponential half-life) + speculative inline Ghost-Text Autocomplete |
| `crates/core-engine` | Multi-volume + LLM memory orchestrator, atomic keystroke generation cancellation, dynamic RRF ranker, and `0600` Unix Domain Socket (`AF_UNIX`) IPC server |
| `clients/macos-ui` | Standalone Swift 6 / AppKit `.app` (`SpotlightPanel`, `ConversationViewerWindowController`, `DriveManagerWindowController`, `DaemonSupervisor`, `⚡` pulsing menu bar telemetry) |
| `clients/cli` | `fms` terminal CLI (`search`, `llm sync/import/search/read`, `mcp`, `mcp-install`, `bench`) and `searchd` background daemon |

---

## Apple Silicon Benchmarks

Measured on Apple Silicon (M4, release profile with `.cargo/config.toml` ARMv8 Cryptography Extensions `+aes,+neon,+sha2`):

| Benchmark | Measured Result | Target |
|---|---|---|
| **L0 NEON SIMD + Smith-Waterman Fuzzy (`1,000,000` paths)** | **0.323 ms (`323 µs` p50)**, `mlock=true` (`102 MiB` RAM) | `< 0.50 ms` |
| **Full 3-Tier Parallel Search (`L0 + L1 + L2 + RRF`)** | **0.31 – 0.35 ms** warm (`L0: 13 µs`, `L1: 101 µs`, `L2: 32 µs`) | `< 4.00 ms` |
| **16 KB AES-256-GCM VFS (`pread(2)` + AEAD decrypt + verify)** | **1.68 GiB/s** end-to-end I/O (`0 ns` zero-copy on LRU page hit) | NVMe line rate |
| **Real External SSD Stress Test (`3,996,230` paths + `403,993` docs)** | **2.15 s** cold decrypt + `mlock` (`888 MB` encrypted binary shard) | Instant mount |

---

## Keyboard Shortcuts (`Fast Mac Search.app`)

| Shortcut | Action |
|---|---|
| **`⌥Space`** | Toggle the compact floating Spotlight bar from any app or fullscreen Space |
| **`⌘1` / `⌘2` / `⌘3`** | Switch scope between **`All`**, **`Files`**, and **`AI Chats`** |
| **`@claude` / `@codex` / `@gemini` / `@chatgpt`** | Inline prefix filter inside the search bar for a specific AI assistant |
| **Click outside / `Esc` / `✕`** | Instantly dismiss the search bar |
| **`Tab`** | Accept greyed-out System 1 Ghost-Text query autocomplete |
| **`↑` / `↓`** | Navigate ranked results |
| **`Return`** | Open file (or open highlighted **AI Conversation Reader** window) |
| **`⌘Return`** | Reveal selected file or raw `.jsonl` transcript in Finder |
| **`⌘Y`** | Toggle QuickLook preview (files) or AI Conversation Reader (chats) |
| **`⌘,`** | Open the visual **Drives & Encrypted Shards** manager |

---

## Optional Terminal CLI (`fms`)

```bash
# Sync all local Claude, Codex, Gemini/Antigravity, & LM Studio chats into encrypted memory
./target/release/fms llm sync

# Import a ChatGPT or Claude Web export (conversations.json) into encrypted memory
./target/release/fms llm import ~/Downloads/conversations.json

# Search across AI conversations (or filter with --provider claude|codex|gemini|chatgpt)
./target/release/fms llm search "keychain prompt" --provider claude

# Unified search across files + AI chats (--scope all|files|chats)
./target/release/fms search "encrypted vfs" --scope all

# Install MCP server into Claude Desktop
./target/release/fms mcp-install
```

---

## Zero-Trust Security Architecture

See [SECURITY.md](SECURITY.md) for full cryptographic details:
- **Per-Volume, LLM-Memory & Per-File Key Derivation:** `MasterKey -> HKDF-SHA256(namespace_or_volume_uuid, shard_salt) -> ShardKey -> HKDF-SHA256(file_id) -> FileKey`.
- **Authenticated 16 KB Pages:** AES-256-GCM frames with `(magic, file_id, page_index, plaintext_len)` bound into AAD + keyed `BLAKE3` 64-byte file header MAC.
- **Zero Swap Leakage:** Plaintext path & conversation title buffers in `L0PathArena` are wired with `mlock(2)` and scrubbed with `zeroize` on drop.

---

## Contributing & License

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md).

Licensed under the **GNU General Public License v3.0 (GPL-3.0)** — see [LICENSE](LICENSE) for full details.
