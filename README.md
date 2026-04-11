# wobble

wobble is a bitcoin style blockchain.

Developed with OpenAI codex

## Overview

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

## CLI

The current CLI is intentionally small:

```text
wobble init <snapshot>
wobble info <snapshot>
wobble utxos <snapshot>
wobble submit-transfer <snapshot> <txid> <vout> <amount> <uniqueness> <lock_tag>
wobble mine-coinbase <snapshot> <reward> <uniqueness> [bits]
wobble mine-pending <snapshot> <reward> <uniqueness> <max_transactions> [bits]
```

Example:

```text
wobble init /tmp/wobble.bin
wobble mine-coinbase /tmp/wobble.bin 50 0
wobble utxos /tmp/wobble.bin
wobble submit-transfer /tmp/wobble.bin <txid> 0 30 1 99
wobble mine-pending /tmp/wobble.bin 50 2 100
wobble info /tmp/wobble.bin
```
