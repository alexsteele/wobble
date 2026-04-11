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
wobble init <snapshot>
wobble info <snapshot>
wobble balance <snapshot> <public_key>
wobble utxos <snapshot>
wobble generate-key
wobble create-wallet <wallet_path>
wobble wallet-address <wallet_path>
wobble wallet-balance <snapshot> <wallet_path>
wobble submit-payment <snapshot> <sender_wallet> <recipient_public_key> <amount> <uniqueness>
wobble submit-transfer <snapshot> <txid> <vout> <amount> <sender_wallet> <recipient_public_key>
wobble mine-coinbase <snapshot> <reward> <miner_wallet> [uniqueness] [bits]
wobble mine-pending <snapshot> <reward> <miner_wallet> <uniqueness> <max_transactions> [bits]
```

Example:

```text
wobble init /tmp/wobble.bin
wobble create-wallet /tmp/miner.wallet
wobble create-wallet /tmp/recipient.wallet
wobble wallet-address /tmp/recipient.wallet
wobble mine-coinbase /tmp/wobble.bin 50 /tmp/miner.wallet 0
wobble wallet-balance /tmp/wobble.bin /tmp/miner.wallet
wobble utxos /tmp/wobble.bin
wobble submit-payment /tmp/wobble.bin /tmp/miner.wallet <recipient_public_key> 30 1
wobble mine-pending /tmp/wobble.bin 50 /tmp/miner.wallet 2 100
wobble wallet-balance /tmp/wobble.bin /tmp/miner.wallet
wobble wallet-balance /tmp/wobble.bin /tmp/recipient.wallet
wobble info /tmp/wobble.bin
```
