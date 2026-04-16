# TODO

## Async Runtime

- Clean up the remaining server/runtime naming now that async transport is the only path.
- Add explicit peer lifecycle state around connect, backoff, and shutdown.
- Reduce hello chatter further by preferring already-connected peers for sync and relay.

## Protocol And Sync

- Move from one-shot sync triggers toward a more continuous background sync loop.
- Prefer connected peers by advertised height before dialing a fresh peer.
- Add a simple peer inventory / announcement policy so we do less full-object gossip over time.
- Add block and transaction import/export formats for easier testing.

## State

- Owner-to-UTXO index for balance operations
- Finish incremental saves so sync/reorg paths do not fall back to full node snapshots.

## CLI Ergonomics

- Improve serve, status, pay, wallet flows.
- Easy recipient aliases
- Reduce rough edges between local and remote command shapes where practical.

## Observability

- Add richer peer lifecycle logging around connect, disconnect, sync start, and sync backoff.
- Log file rotation

## Testing

- Add property tests and adversarial fork/restart scenarios.
- Add a `test-net` command that can provision wobble nodes across configured hosts over SSH, start them with generated configs, and drive random transactions and mining activity.
- Add a `test-net --local` mode that reuses the local test harness, spins up multiple local nodes, and fuzzes actions like payments, mining, restart, and reconnect.
