# TODO

## Async Runtime

- Wrap a real Tokio `TcpStream` with `PeerTransport`.
- Port one inbound peer connection path onto the async runtime while keeping the synchronous server as the fallback path.
- Teach the runtime coordinator to own live peer registrations instead of tests wiring them manually.
- Route persistence from `StateEffect::Persist` into the incremental SQLite save path.
- Hook the miner thread into `StartMiningJob` / `StopMining` instead of mining through the synchronous serve loop.
- Reduce hello chatter once long-lived async peer sessions are in place.

## Protocol And Sync

- Persistent peer connections
- Move from one-shot sync triggers toward a more continuous background sync loop.
- Add block and transaction import/export formats for easier testing.

## State

- Owner-to-UTXO index for balance operations
- Incremental saves. No huge snapshots.

## CLI Ergonomics

- Improve serve, status, pay, wallet flows.
- Easy recipient aliases
- Reduce rough edges between local and remote command shapes where practical.

## Observability

- Post-transition state logging (tip/mempool size)
- Log file rotation

## Testing

- Add property tests and adversarial fork/restart scenarios.
- Add a `test-net` command that can provision wobble nodes across configured hosts over SSH, start them with generated configs, and drive random transactions and mining activity.
- Add a `test-net --local` mode that reuses the local test harness, spins up multiple local nodes, and fuzzes actions like payments, mining, restart, and reconnect.
