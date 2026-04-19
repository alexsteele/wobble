# Worklog

<!-- codex: Use git log to update this table. -->
<!-- codex: Include sessions near midnight with the prior day. -->
<!-- codex: Commits hours apart are separate sessions. -->

Derived from local git history.

| Date       | # Commits | Hours | Line Changes     |
| ---------- | --------: | ----: | ---------------- |
| 2026-04-18 |         3 |   0.6 | +709 / -89       |
| 2026-04-12 |        21 |   5.8 | +4,002 / -510    |
| 2026-04-11 |        56 |   9.3 | +11,667 / -3,252 |
| 2026-04-10 |        44 |   4.2 | +7,924 / -487    |

Current code size:

- `src/`: `15,487` lines
- `tests/`: `895` lines
- `scripts/`: `688` lines

## 2026-04-18 Notes

- Replaced the split async/runtime wrappers with a simpler async-first `Server` flow so startup, listeners, and event dispatch read more directly top-to-bottom.
- Moved more protocol ownership back into `Server` and `Peer`, reducing leftover transitional abstractions from the earlier async scaffold.
- Added a miner-facing candidate flow in `src/mining.rs` so the server can submit work, cancel stale work, and accept solved blocks through the event loop.
- Added `announce_tip` handling and in-memory peer tip tracking so relays and sync triggers can avoid obvious duplicate work.
- Tightened the server around direct SQLite-backed state ownership again, keeping concurrency concerns in the async transport rather than in a second persistence runtime.

## 2026-04-12 Notes

- Tightened SQLite persistence around accepted blocks, mempool transactions, peer rows, and best-tip rebuild checks so state recovery is moving away from full snapshot rewrites.
- Added runtime-driven `disconnect()` and `stop()` controls and updated the TCP test harness to stop servers explicitly instead of relying on guessed connection counts.
- Stabilized the sync and testnet path around persisted peer tip metadata, better peer selection, and more reliable seeded-chain catch-up behavior.
- Introduced the first async runtime scaffold: `ServerRuntime`, `ServerHandle`, `StateTask`, and the spawned state loop that keeps `NodeState` ownership serialized behind channels.
- Added `StateEffect` routing through a lightweight `RuntimeCoordinator` so relay, persistence, disconnect, and mining actions have an explicit async handoff point.
- Added a transport-agnostic `PeerTask` plus a newline-delimited async `PeerTransport` adapter, with in-memory transport tests to validate the first live async peer path before porting TCP.

## 2026-04-11 Notes

- Split the local admin interface from the public peer protocol and moved status, balance, submit, and bootstrap onto the localhost admin socket.
- Added clearer CLI errors when local admin commands are used while `wobble serve` is not running.
- Switched `wallet-info` and `status` to offline-first reads from SQLite, with optional live server details layered on top.
- Added a read-only SQLite open path for inspection commands so they do not contend with a running server's writable handle.
- Fixed a server bug where one-shot admin requests could kill the serve loop after a non-blocking `WouldBlock`.
- Added short outbound peer IO timeouts so bootstrap sync remains best effort instead of hanging server startup.
- Added a local configurable testnet harness under `scripts/` that starts `/tmp` homes, bootstraps one side, waits for sync, optionally submits random payments, and captures logs toward a future `test-net` command.
- Simplified wallet-facing CLI shape around `wallet-info` and removed the separate address/balance split.
- Persisted peer metadata in SQLite, including advertised tip hash, advertised height, and hello timing, to support better sync decisions across restarts.
- Added an in-memory peer-state cache in the server so sync selection can use recently learned peer tip/height data without rereading SQLite on each pass.
- Tightened sync selection to prefer the highest advertised configured peer ahead of the local tip, with focused regression tests for both that path and the no-metadata fallback.

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
