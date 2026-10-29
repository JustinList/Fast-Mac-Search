# Contributing to Fast Mac Search

Thank you for helping build a faster, private Spotlight replacement for macOS!

## Prerequisites

- **macOS 14.0+ (Sonoma or newer)** on **Apple Silicon (`arm64`)**
- **Rust 1.85+** (Rust 2024 edition)
- **Xcode Command Line Tools** (`xcode-select --install`, includes Swift 6)

## Local Development Workflow

```bash
# 1. Run all unit & zero-plaintext cryptographic integration tests
cargo test --workspace

# 2. Run the 1,000,000-path Apple Silicon NEON + AES-256-GCM benchmark suite
cargo run --release --bin fms -- bench --paths 1000000

# 3. Package the standalone 'Fast Mac Search.app' bundle & installer DMG
./scripts/build-macos-app.sh
```

## Engineering Guidelines

1. **Zero Plaintext on Disk:** Any new index structure persisted to disk must pass through `fms-crypto-vfs` (`EncryptedBlockDirectory`, `atomic_write_encrypted`, or `EncryptedStreamWriter`) and include a `verify_zero_plaintext_on_disk` test assertion.
2. **Latency Budgets:** Keep L0 arena changes under `< 0.5 ms` for `1,000,000` paths on M-series hardware, and keep UI `@MainActor` code free of blocking filesystem or socket syscalls.
3. **Zero `.Spotlight-V100` Litter:** Never write files onto external drives unless the user explicitly opts into **Portable Encrypted Mode** (`.fastindex`).

## License

By submitting a contribution to this repository, you agree that your contribution will be licensed under the **GNU General Public License v3.0 (GPL-3.0)** ([LICENSE](LICENSE)).
