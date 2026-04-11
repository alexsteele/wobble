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

## CLI

The current CLI is intentionally small:

```shell
High-level workflow:
wobble init [--home <dir>]
wobble serve <listen_addr> <network> [--home <dir>] [--node_name <name>] [--peers_path <path>] [--miner_wallet <path>] [--mining_interval_ms <ms>] [--mining_max_transactions <count>] [--mining_bits <bits>]
wobble submit-payment <recipient_public_key|@alias_book:name> <amount> [uniqueness] [--home <dir>]
wobble submit-payment-remote <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> [uniqueness] <peer_addr> <network> [--node_name <name>]
wobble wallet-address [--home <dir>]
wobble wallet-balance [--home <dir>]

Inspect state:
wobble info <sqlite_path>
wobble balance <sqlite_path> <public_key>
wobble utxos <sqlite_path>
wobble get-tip <peer_addr> <network> [--node_name <name>]

Mining and low-level transaction control:
wobble mine-coinbase <reward> [uniqueness] [bits] [--home <dir>]
wobble mine-coinbase <sqlite_path> <reward> <miner_wallet> [uniqueness] [bits]
wobble mine-pending <reward> <uniqueness> <max_transactions> [bits] [--home <dir>]
wobble mine-pending <sqlite_path> <reward> <miner_wallet> <uniqueness> <max_transactions> [bits]
wobble mine-pending-remote <reward> <miner_wallet> <uniqueness> <max_transactions> <peer_addr> <network> [--node_name <name>]
wobble submit-payment <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> [uniqueness]
wobble submit-transfer <sqlite_path> <txid> <vout> <amount> <sender_wallet> <recipient_public_key>

Setup and helpers:
wobble create-wallet <wallet_path>
wobble create-alias-book <alias_book>
wobble alias-add <alias_book> <name> <public_key>
wobble alias-list <alias_book>
wobble wallet-address <wallet_path>
wobble wallet-balance <sqlite_path> <wallet_path>
wobble generate-key
```

Example:

```shell
wobble init
wobble create-wallet /tmp/recipient.wallet
wobble wallet-address
wobble wallet-address /tmp/recipient.wallet
wobble create-alias-book /tmp/recipients.json
wobble alias-add /tmp/recipients.json recipient <recipient_public_key>
wobble mine-coinbase 50
wobble wallet-balance
wobble utxos ~/.wobble/node.sqlite
wobble submit-payment @/tmp/recipients.json:recipient 30
wobble mine-pending 50 2 100
wobble wallet-balance
wobble wallet-balance ~/.wobble/node.sqlite /tmp/recipient.wallet
wobble info ~/.wobble/node.sqlite
```

`wobble init` creates a default node home at `~/.wobble` with:
- `node.sqlite`
- `wallet.bin`
- `aliases.json`
- `peers.json`

Peer bootstrap file example:

```json
[
  { "addr": "127.0.0.1:9002", "node_name": "miner" },
  { "addr": "127.0.0.1:9003", "node_name": "observer" }
]
```

Example server startup with peers:

```shell
wobble serve \
  127.0.0.1:9001 \
  wobble-local \
  --node_name proposer \
  --home /tmp/proposer
```

Example server startup with integrated mining:

```shell
wobble serve \
  127.0.0.1:9002 \
  wobble-local \
  --home /tmp/miner \
  --node_name miner \
  --miner_wallet /tmp/miner/wallet.bin \
  --mining_interval_ms 250
```

## Logging

The server uses structured `tracing` logs on stderr.

Examples:

```shell
cargo run -- serve \
  127.0.0.1:9001 \
  wobble-local \
  --node_name proposer
RUST_LOG=wobble=debug cargo run -- serve \
  127.0.0.1:9001 \
  wobble-local \
  --node_name proposer
RUST_LOG=info cargo run -- serve \
  127.0.0.1:9001 \
  wobble-local \
  --node_name proposer
```

Notes:
- default logging falls back to `info` if `RUST_LOG` is not set
- `RUST_LOG=wobble=debug` is useful when following handshakes, relay, and sync decisions
- logs are emitted on stderr so normal CLI output stays readable on stdout

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
