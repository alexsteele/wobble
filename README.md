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

```text
wobble init <sqlite_path>
wobble info <sqlite_path>
wobble balance <sqlite_path> <public_key>
wobble utxos <sqlite_path>
wobble generate-key
wobble create-wallet <wallet_path>
wobble wallet-address <wallet_path>
wobble wallet-balance <sqlite_path> <wallet_path>
wobble create-alias-book <alias_book>
wobble alias-add <alias_book> <name> <public_key>
wobble alias-list <alias_book>
wobble serve <sqlite_path> <listen_addr> <network> [--node_name <name>] [--peers_path <path>]
wobble get-tip <peer_addr> <network> [--node_name <name>]
wobble submit-payment-remote <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> <uniqueness> <peer_addr> <network> [--node_name <name>]
wobble mine-pending-remote <reward> <miner_wallet> <uniqueness> <max_transactions> <peer_addr> <network> [--node_name <name>]
wobble submit-payment <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> <uniqueness>
wobble submit-transfer <sqlite_path> <txid> <vout> <amount> <sender_wallet> <recipient_public_key>
wobble mine-coinbase <sqlite_path> <reward> <miner_wallet> [uniqueness] [bits]
wobble mine-pending <sqlite_path> <reward> <miner_wallet> <uniqueness> <max_transactions> [bits]
```

Example:

```text
wobble init /tmp/wobble.sqlite
wobble create-wallet /tmp/miner.wallet
wobble create-wallet /tmp/recipient.wallet
wobble create-alias-book /tmp/recipients.aliases
wobble wallet-address /tmp/recipient.wallet
wobble alias-add /tmp/recipients.aliases recipient <recipient_public_key>
wobble mine-coinbase /tmp/wobble.sqlite 50 /tmp/miner.wallet 0
wobble wallet-balance /tmp/wobble.sqlite /tmp/miner.wallet
wobble utxos /tmp/wobble.sqlite
wobble submit-payment /tmp/wobble.sqlite /tmp/miner.wallet @/tmp/recipients.aliases:recipient 30 1
wobble mine-pending /tmp/wobble.sqlite 50 /tmp/miner.wallet 2 100
wobble wallet-balance /tmp/wobble.sqlite /tmp/miner.wallet
wobble wallet-balance /tmp/wobble.sqlite /tmp/recipient.wallet
wobble info /tmp/wobble.sqlite
```

Peer bootstrap file example:

```json
[
  { "addr": "127.0.0.1:9002", "node_name": "miner" },
  { "addr": "127.0.0.1:9003", "node_name": "observer" }
]
```

Example server startup with peers:

```text
wobble serve /tmp/wobble.sqlite 127.0.0.1:9001 wobble-local --node_name proposer --peers_path /tmp/peers.json
```

## Build And Test

Common development commands:

```text
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
