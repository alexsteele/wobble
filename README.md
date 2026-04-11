# wobble

wobble is a bitcoin style blockchain.

Developed with OpenAI codex

## Overview

A blockchain is an ordered sequence of blocks. Each block contains a header and
a list of transactions. Miners submit blocks to the chain after satisfying
proof-of-work. Validation rules prevent double spends, and proof-of-work makes
it costly to replace a chain with a conflicting one.

Transactions move coins through the blockchain. Transactions consume coins and
create new ones that can be spent later on. The one exception is the coinbase
transaction, which creates a new coin for the miner.

Proof-of-work decides who gets to add the next block. Miners repeatedly vary
their block `nonce` and hash the header until it follows below a target. This is
expensive and hard to fake. But it is cheap to verify.

Miners submit new blocks to other nodes in the network. Nodes append valid
blocks to their chain as long as they satisfies proof-of-work. The longest chain
wins.

A UTXO is an unspent coint. The current set of UTXOs form the chain state.

https://bitcoin.org/bitcoin.pdf

See [design](docs/design.md).
