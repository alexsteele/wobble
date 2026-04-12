# TODO

## CLI Ergonomics

- Improve serve, status, pay, wallet flows.
- Easy recipient aliases
- Document the local two-node demo flow in the README and explain the generated logs/artifacts.
- Reduce rough edges between local and remote command shapes where practical.

## Mining

- Mining controls: max txns, difficulty bits, cadence, fees?
- Parallel mining

## Protocol

- Reduce hello chatter
- Persistent peer connections
- Move from one-shot sync triggers toward a more continuous background sync loop.
- Add block and transaction import/export formats for easier testing.

## State

- Owner-to-UTXO index for balance operations
- Incremental saves. No huge snapshots.

## Observability

- Post-transition state logging (tip/mempool size)
- Log file rotation

## Testing

- Add property tests and adversarial fork/restart scenarios.
- Keep tightening the local two-node demo script until it is a reliable executable smoke test.
- Add a `test-net` command that can provision wobble nodes across configured hosts over SSH, start them with generated configs, and drive random transactions and mining activity.
- Add a `test-net --local` mode that reuses the local test harness, spins up multiple local nodes, and fuzzes actions like payments, mining, restart, and reconnect.

## Later

- Server mining options, parallelism.
