# TODO

## Protocol And Sync

- Keep long-lived peer sessions and reuse connected peers for sync and relay before opening fresh sockets.
- Reduce hello chatter further now that `announce_tip` and in-memory peer tip tracking are in place.
- Add explicit peer lifecycle state around connect, backoff, and shutdown.
- Move from one-shot sync triggers toward a steadier background sync loop.
- Add a simple peer inventory / announcement policy so we do less full-object gossip over time.
- Add block and transaction import/export formats for easier testing.

## Mining

- Finish the `Miner`/candidate flow so block assembly, cancellation, and solved-block submission are owned cleanly by `src/mining.rs`.
- Avoid unnecessary candidate rebuilds when mempool churn does not materially change the best work.
- Keep mining integrated with `Server`, but make the boundaries between server policy and miner execution clearer.

## State

- Owner-to-UTXO index for balance operations.
- Finish incremental saves so sync/reorg paths do not fall back to full node snapshots.
- Save chain/mempool updates incrementally instead of rewriting broad snapshots on hot paths.

## CLI Ergonomics

- Improve serve, status, pay, wallet flows.
- Easy recipient aliases.
- Reduce rough edges between local and remote command shapes where practical.

## Observability

- Add richer peer lifecycle logging around connect, disconnect, sync start, and sync backoff.
- Log file rotation.
- Keep `--log-stderr` focused on human-friendly local debugging while file logs remain the durable trace.

## Testing

- Add property tests and adversarial fork/restart scenarios.
- Add a `test-net` command that can provision wobble nodes across configured hosts over SSH, start them with generated configs, and drive random transactions and mining activity.
- Add a `test-net --local` mode that reuses the local test harness, spins up multiple local nodes, and fuzzes actions like payments, mining, restart, and reconnect.
