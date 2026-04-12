# Testnet

authors: codex, alex

## Current State

Wobble now has a working local testnet flow over the real TCP protocol.

Implemented:
- proposer to miner transaction relay
- mined block relay back to peers
- restart and SQLite-backed recovery
- multi-hop relay through an intermediate node
- bootstrap sync from configured peers at startup
- later catch-up when a peer advertises a newer tip
- configurable local harness at `scripts/local_testnet.py`

The main end-to-end coverage lives in `tests/testnet_e2e.rs`.

## Local Harness

`scripts/local_testnet.py` creates one temporary node home per local node under
`/tmp`, starts one `wobble serve` process per home, wires the generated
`peers.json` files together, and can optionally submit random confirmed
payments.

By default it also includes the local user's existing `~/.wobble` listener as a
seed peer when that config exists.

Example:

```shell
python3 scripts/local_testnet.py --nodes 4 --payment-count 10 --payment-rate 0.5
```

Useful flags:
- `--nodes N` chooses how many temporary homes and servers to start
- `--mining-node I` selects which temporary node runs integrated mining
- `--bootstrap-blocks N` chooses how many initial funding blocks to mine on node 0
- `--seed-peer HOST:PORT` overrides the default peer taken from `~/.wobble/config.json`
- `--payment-count N` enables random confirmed payments after bootstrap
- `--payment-rate R` spaces those payments to roughly `R` payments per second
- `--payment-amount N` sets the amount sent by each random payment

## Known Gaps

Not implemented yet:
- persistent long-lived peer sessions
- background sync loops
- peer discovery and peer exchange
- peer scoring or ban policy
- header-first block sync
