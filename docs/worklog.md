# Worklog

Derived from local git history.

| Date       | # Commits | Hours | Line Changes    |
| ---------- | --------: | ----: | --------------- |
| 2026-04-10 |        19 |   1.9 | +4,008 / -355   |

Current code size:

- `src/`: `2,977` lines

<!-- codex: Include sessions near midnight with the prior day. -->
<!-- codex: Commits hours apart are separate sessions. -->

## 2026-04-10 Notes

- Wrote the initial design doc and bootstrapped the Rust crate.
- Added core blockchain data types, deterministic hashing, and merkle helpers.
- Built UTXO validation, block consensus checks, and chain indexing.
- Added integrated node state with branch-aware UTXO snapshots and reorg handling.
- Added snapshot persistence for node state.
- Added a local CLI for initializing state, mining blocks, inspecting UTXOs, and queuing transactions.
- Added a mempool and block assembly from pending transactions.
- Replaced toy lock tags with Ed25519 signing and signature verification.
- Added wallet file storage and switched common CLI flows to wallet paths instead of raw secret keys.
