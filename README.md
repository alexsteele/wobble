# wobble

wobble is a bitcoin style blockchain.

Developed with OpenAI codex

status: experimental, early development

## Overview

<!-- HUMAN -->

A blockchain models a distributed ledger of transactions transferring coins
between participants. Validation rules prevent double spends, and proof-of-work
makes it costly to replace a chain with a conflicting one. This allows the
participants to agree on the current chain while being completely decentralized.

It consists of a sequence of blocks. Each block contains a header and a list of
transactions.

Transactions move coins through the blockchain by consuming coins and create new
ones that can be spent later on. The one exception is a coinbase transaction,
which creates a new coin for the miner.

Miners submit blocks to the chain with new transactions after satisfying a
proof-of-work. This involves repeatedly varying the header `nonce` and hashing
the header until the hash falls below a target. This process is expensive, hard
to fake, and easy to verify.

Miners send their new blocks to other nodes. Nodes append valid blocks to their
chain as long as they satisfy proof-of-work. The longest chain wins.

A UTXO is an unspent coint. The current set of UTXOs form the chain state.

https://bitcoin.org/bitcoin.pdf

See [design](docs/design.md).

<!-- END HUMAN -->

## Usage

```shell
Bitcoin-inspired blockchain prototype

Usage: wobble <COMMAND>

Commands:
  init            Initializes a node home with default state, wallet, aliases, and peers
  serve           Starts the local node server
  balance         Alias for `wallet balance`
  wallet          Reads or updates the local wallet stored under the node home
  transactions    Prints recent wallet transactions from the local sqlite index
  pay             Builds and queues a local payment from the node wallet
  inspect         Reads chain or peer state without mutating local data
  admin           Manages helper files like wallets and alias books
  debug           Runs lower-level development and test commands
  help            Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

Example:

```shell
wobble init                                            # create ~/.wobble with wallet, state, and config
wobble balance                                         # quick alias for wallet balance
wobble wallet info                                     # show this node's wallet keys and metadata
wobble wallet balance                                  # show wallet balances from local sqlite state
wobble wallet new-key miner                            # add a named keypair to the local wallet
wobble serve                                           # start the local node using ~/.wobble/config.json
wobble serve --mining                                  # start server with mining enabled
wobble pay <recipient_public_key> 30                   # queue a payment from the default wallet key
wobble pay <recipient_public_key> 30 --from savings    # queue a payment from a named wallet key
```

`wobble init` creates a default node home at `~/.wobble` with:
- `node.sqlite`
- `wallet.bin`
- `aliases.json`
- `peers.json`
- `config.json`

Node config example:

```json
{
  "listen_addr": "127.0.0.1:9001",
  "network": "wobble-local",
  "node_name": "miner",
  "mining": {
    "enabled": true,
    "reward_wallet": "wallet.bin",
    "interval_ms": 250,
    "max_transactions": 100,
    "bits": "0x207fffff"
  }
}
```

Notes:
- `reward_wallet` may be relative to the node home or an absolute path
- if `reward_wallet` is omitted, mining rewards go to the node home's default `wallet.bin`
- `wobble serve --mining` forces mining on for one run, and `wobble serve --no-mining` forces it off

Peer bootstrap file example:

```json
[
  { "addr": "127.0.0.1:9002", "node_name": "miner" },
  { "addr": "127.0.0.1:9003", "node_name": "observer" }
]
```

## Local Testnet

A configurable local harness lives at `scripts/local_testnet.py`.

Example:

```shell
python3 scripts/local_testnet.py --nodes 4 --payment-count 10 --payment-rate 0.5
```

This creates one temporary home per node under `/tmp`, starts one server per
home, wires the generated peers together, and can optionally drive random
confirmed payments. See [testnet](docs/testnet.md) for the current scope and
flags.

## Logging

The server uses structured `tracing` logs and writes them to daily log files
under `<home>/logs`. Pass `--log-stderr` to mirror them to stderr too. Use
`RUST_LOG` to control both sinks.

Examples:

```shell
cargo run -- serve
cargo run -- serve --log-stderr
RUST_LOG=wobble=debug cargo run -- serve
RUST_LOG=info cargo run -- serve --log-stderr
```

Notes:
- default logging falls back to `info` if `RUST_LOG` is not set
- `RUST_LOG=wobble=debug` is useful when following handshakes, relay, and sync decisions
- `serve` writes daily log files under `~/.wobble/logs`
- `--log-stderr` mirrors server logs to stderr so normal CLI output stays readable on stdout

## Build And Test

Common development commands:

```shell
cargo build
cargo test
cargo unit-test
cargo test --features e2e --test testnet_e2e
```

Notes:
- `cargo build` compiles the library and CLI binary
- `cargo test` runs the default full suite, including the E2E coverage
- `cargo unit-test` runs the fast library-only path without default features
- `cargo test --features e2e --test testnet_e2e` still runs just the E2E integration target when you want it in isolation
