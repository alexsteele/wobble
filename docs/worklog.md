# Worklog

<!-- codex: Use git log to update this table. -->
<!-- codex: Include sessions near midnight with the prior day. -->
<!-- codex: Commits hours apart are separate sessions. -->

Derived from local git history.

| Date       | # Commits | Hours | Line Changes     |
| ---------- | --------: | ----: | ---------------- |
| 2026-04-11 |        53 |   8.5 | +11,038 / -2,462 |
| 2026-04-10 |        19 |   1.9 | +4,008 / -355    |

Current code size:

- `src/`: `9,552` lines
- `tests/`: `765` lines
- `scripts/`: `403` lines

## 2026-04-11 Notes

- Split the local admin interface from the public peer protocol and moved status, balance, submit, and bootstrap onto the localhost admin socket.
- Added clearer CLI errors when local admin commands are used while `wobble serve` is not running.
- Switched `wallet-info` and `status` to offline-first reads from SQLite, with optional live server details layered on top.
- Added a read-only SQLite open path for inspection commands so they do not contend with a running server's writable handle.
- Fixed a server bug where one-shot admin requests could kill the serve loop after a non-blocking `WouldBlock`.
- Added short outbound peer IO timeouts so bootstrap sync remains best effort instead of hanging server startup.
- Added a local two-node demo harness under `scripts/` that starts two homes, bootstraps one side, waits for sync, submits a payment, and captures logs toward a future `test-net` command.
- Simplified wallet-facing CLI shape around `wallet-info` and removed the separate address/balance split.

## 2026-04-10 Notes

- Wrote the initial design doc and bootstrapped the Rust crate.
- Added core blockchain data types, deterministic hashing, and merkle helpers.
- Built UTXO validation, block consensus checks, and chain indexing.
- Added integrated node state with branch-aware cached UTXO views and reorg handling.
- Added durable node-state persistence, later migrated to SQLite-backed storage.
- Added a local CLI for initializing state, mining blocks, inspecting UTXOs, and queuing transactions.
- Added a mempool and block assembly from pending transactions.
- Replaced toy lock tags with Ed25519 signing and signature verification.
- Added wallet file storage and switched common CLI flows to wallet paths instead of raw secret keys.
