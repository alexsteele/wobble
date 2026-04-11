# Design

authors: codex, alex

## Goal

Build a Bitcoin-inspired blockchain in Rust.

Target properties:
- permissionless-style protocol shape
- proof-of-work block production
- UTXO-based validation
- heaviest-chain fork choice
- deterministic local state replay

Non-goals for v1:
- production security
- P2P compatibility with Bitcoin
- scripting beyond basic transfers
- wallets, mining pools, fees market optimization

## System Model

Each node maintains:
- a block store
- a UTXO set
- a mempool
- a peer set
- a current best chain tip

Nodes:
- accept and validate transactions
- gossip transactions and blocks
- mine candidate blocks from mempool contents
- switch to a better chain when a fork with more cumulative work arrives

Consensus is Nakamoto-style:
- miners extend the chain by finding a block hash below a target
- honest nodes follow the valid chain with the most cumulative work
- finality is probabilistic, not immediate

## Core Data Types

`Transaction`
- `version`
- `inputs: Vec<TxIn>`
- `outputs: Vec<TxOut>`
- `lock_time`

`TxIn`
- `prev_txid`
- `prev_vout`
- `unlocking_data`

`TxOut`
- `value`
- `locking_data`

`BlockHeader`
- `version`
- `prev_blockhash`
- `merkle_root`
- `time`
- `bits`
- `nonce`

`Block`
- `header`
- `txs: Vec<Transaction>`

`ChainIndex`
- block hash to metadata
- height
- cumulative work
- parent pointer

## Terms

- a UTXO is an unspent transaction output: a `TxOut` that exists in chain state and has not yet been consumed by any valid `TxIn`
- a `TxOut` creates spendable value and defines the condition to spend it later
- a `TxIn` consumes a prior `TxOut` by referencing `(prev_txid, prev_vout)` and providing matching `unlocking_data`
- inputs destroy old UTXOs; outputs create new UTXOs
- partial spends are modeled by creating a change output, not by splitting an input in place
- a coinbase transaction is the special first transaction in a block that creates new coins and collects block fees

## Validation Rules

Transaction validation:
- structure is well-formed
- referenced outputs exist
- referenced outputs are unspent
- unlocking data satisfies locking data
- total input value >= total output value
- no duplicate spends within the transaction

Block validation:
- header hash satisfies target from `bits`
- `prev_blockhash` is known unless block is genesis
- merkle root matches body
- first transaction is coinbase
- exactly one coinbase
- every non-coinbase transaction is valid against the chain view
- block subsidy is valid for the current height

Chain validation:
- every block links to a valid parent
- cumulative work is monotonic
- UTXO transitions are deterministic

## Fork Choice

Use cumulative work, not height.

Rules:
- track all valid side branches
- prefer the valid tip with greatest cumulative work
- reorganize when a side branch overtakes the current best chain
- on reorg, roll back UTXO changes to the fork point, then apply the new branch

## Networking

Initial protocol is custom and minimal.

Message classes:
- `version`
- `peers`
- `inv`
- `get_data`
- `tx`
- `block`

Behavior:
- peers connect over TCP
- nodes announce object hashes with `inv`
- receivers request unknown objects with `get_data`
- blocks and transactions are validated before acceptance and relay

Non-goals for v1:
- peer discovery via DNS seeds
- compact blocks
- header-first sync optimization
- DoS-hardening

## Mining

Mining loop:
- select transactions from mempool
- build coinbase
- compute merkle root
- iterate nonce and extra nonce until hash meets target
- broadcast found block

Difficulty:
- v1 may use a fixed target
- next step is periodic retargeting based on observed block interval

## Storage

Persist:
- raw blocks by hash
- chain metadata
- current best tip
- UTXO set
- mempool contents

Requirements:
- restart without replaying from genesis in the common case
- recover best chain, UTXO state, and mempool safely after crash

Likely shape:
- SQLite-backed block store and indexed metadata
- SQLite-backed active UTXO state
- SQLite-backed mempool state

## Rust Architecture

Planned crates or modules:
- `types`: transactions, blocks, hashes, serialization
- `crypto`: hashing, merkle trees, signatures
- `consensus`: validation, subsidy, work, fork choice
- `state`: UTXO set, reorg application
- `mempool`: transaction admission and eviction
- `net`: peer protocol and sync
- `miner`: block assembly and PoW loop
- `store`: persistence layer
- `node`: process orchestration

Runtime:
- `tokio` for networking and task orchestration
- explicit state machine boundaries over implicit shared mutation

## Execution Plan

Milestone 1:
- block and transaction types
- hashing and merkle roots
- block store
- in-memory UTXO validation

Milestone 2:
- chain index
- cumulative work
- side branches
- reorg-safe state application

Milestone 3:
- mempool
- TCP peer protocol
- block and transaction relay

Milestone 4:
- mining loop
- coinbase handling
- fixed difficulty local testnet

Milestone 5:
- persistent UTXO state
- restart and resync behavior
- retargeting and observability

## Design Constraints

Keep v1 simple:
- deterministic serialization
- no scripting VM
- no wallet UX
- no attempt at mainnet economics

The objective is a correct small system that demonstrates:
- replicated append-only history
- adversarial fork handling
- proof-of-work leader election
- deterministic state transitions
