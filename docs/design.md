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

Current message classes:
- `hello`
- `get_tip`
- `tip`
- `announce_tx`
- `announce_block`
- `get_block`
- `block`
- `mine_pending`

Behavior:
- peers connect over TCP
- peers exchange `hello` first
- nodes currently relay full transactions and blocks, not inventories
- receivers request unknown blocks with `get_block`
- blocks and transactions are validated before acceptance and relay

Non-goals for v1:
- peer discovery via DNS seeds
- compact blocks or inventory-first relay
- header-first sync optimization
- DoS-hardening

## Runtime Semantics

The runtime is split into two layers:

- Tokio owns inbound peer listeners, the localhost admin listener, and each live
  peer socket task
- one dedicated server event-loop thread owns `NodeState`, persistence, sync
  policy, relay policy, and mining decisions

Important implications:
- inbound peers are handled concurrently at the transport layer
- chain and mempool mutation still happen in one place, which keeps state
  transitions easy to reason about
- outbound peer traffic now also runs through async tasks, but the server talks
  to those peers through simple request/reply handles

Current outbound connection policy:
- sync reuses one connected outbound peer for `get_tip` plus any needed
  `get_block` requests
- relay currently reconnects per announcement because the protocol does not yet
  have an explicit ack or liveness signal for one-way relay messages
- broken outbound connections are dropped and re-established on demand

Lifecycle controls:
- `disconnect()` drops cached outbound peer connections without stopping the server
- `stop()` ends the serve loop and closes outbound peers during shutdown

This keeps the concurrency boundary at the edges while the core server logic
still reads like a single authoritative event loop.

## Mining

Mining is part of the server loop, not a separate miner service.

Current behavior:
- a node enables mining through its server config or `serve --mining`
- the server polls periodically for work
- it mines only when the mempool is non-empty
- it builds a coinbase that pays the configured reward wallet
- block reward is subsidy plus included transaction fees
- it searches nonce values until the block hash is below the target from `bits`
- accepted blocks go through the normal validation, persistence, logging, and relay path

Current settings:
- `enabled`
- `reward_wallet`
- `interval_ms`
- `max_transactions`
- `bits`

This is the intended shape for the project: one server owns networking, state,
and optional mining.

Difficulty is fixed and intentionally easy for local testing.

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

Near-term intent:
- keep consensus and state-transition code synchronous and deterministic
- move networking and peer-session management to an async runtime layer
- give one runtime task explicit ownership of state mutation, rather than
  letting many peer tasks mutate `NodeState` concurrently

## Async Runtime Outline

The next runtime design should separate:
- deterministic state mutation
- async socket I/O
- CPU-bound mining

### Runtime Roles

`ServerRuntime`
- owns the live async node process
- starts listeners, peer tasks, admin handling, timers, and shutdown
- routes commands and effects between tasks

`AsyncPeerServer`
- is the current server-layer bridge into the async runtime
- consumes a synchronous `Server` and moves its config/state into `StateTask`
- now supports a real async inbound-peer listener loop with clean stop/join
- now also supports dialing and managing outbound peers on the same async path
- now also serves the local admin protocol over the async runtime
- now can run the existing integrated mining policy by polling `MinePending` through the same state-task path
- now owns the live async peer transport map instead of returning raw transport ownership to callers
- updates runtime status from that live peer set so admin status reflects current peer count
- still excludes configured-peer sync and higher-level relay/session policy for now

`ServerHandle`
- small cloneable control surface for callers and tests
- supports lifecycle actions such as `stop()`

`StateTask`
- the single owner of `NodeState`
- receives commands from peers, admin clients, and the miner
- validates and applies all chain, mempool, and UTXO mutations
- performs direct SQLite saves after successful mutating commands when a store is configured
- emits effects such as replies, relay actions, persistence markers, and mining updates

`PeerTask`
- owns one live peer connection
- in the current scaffold, owns one transport-facing channel set
- forwards inbound wire messages into the state task
- emits outbound wire messages and coordinator-issued sends
- does not mutate `NodeState` directly

`PeerTransport`
- bridges one newline-delimited async stream to a peer task
- parses inbound `WireMessage` values from the stream
- writes outbound `WireMessage` values back to the stream
- is generic over the async stream so tests can use in-memory transports first
- now also has a thin Tokio `TcpStream` wrapper for the first real socket path

`PeerManager`
- tracks known peers and active peer-task channels
- maintains outbound connections and reconnect policy

Current scaffold note:
- `RuntimeCoordinator` now owns live peer registration and can spawn/register
  peer tasks and peer transports directly for inbound connections
- it now also has an async helper to accept one inbound Tokio TCP peer and
  serve it through the coordinator-owned peer path

`MinerThread`
- runs proof-of-work on a dedicated blocking thread
- receives mining jobs over a channel
- reports found blocks back to the state task

### Ownership Rule

Only the state task mutates `NodeState`.

Peer tasks:
- decode messages from sockets
- send commands to the state task
- write replies or outbound relays they are instructed to send

This keeps consensus and state transitions serialized even though networking is concurrent.

### Command Flow

Inbound request-response flow:
1. a peer task reads one wire message
2. it sends a `StateCommand` carrying the message plus a reply channel
3. the state task validates and applies any needed mutation
4. the state task returns a reply over that reply channel
5. the peer task writes the reply back to the socket

Asynchronous side effects:
1. the state task accepts an object or changes local state
2. it emits effects such as relay, persistence, or mining updates
3. the runtime routes those effects to peer tasks, the store, or the miner

### Suggested Runtime Types

`StateCommand`
- `InboundPeerMessage { peer_id, message, reply }`
- `AdminRequest { request, reply }`
- `MinerFoundBlock { job_id, block }`
- `PeerDisconnected { peer_id }`

`StateEffect`
- `Relay { peers, message }`
- `Persist`
- `StartMiningJob { job }`
- `StopMining`
- `DisconnectPeer { peer_id }`

`PeerCommand`
- `Send(WireMessage)`
- `Disconnect`

`MinerCommand`
- `StartJob(MiningJob)`
- `Stop`

`MinerEvent`
- `FoundBlock { job_id, block }`

Current scaffold note:
- direct request-specific replies are returned over per-command reply channels
- `StateEffect` is only for asynchronous follow-up work the runtime must route

### Mining Model

Mining should not run on the async executor.

Instead:
- the state task decides what the current mining job is
- the miner thread receives that job over a channel
- the miner thread searches nonces in a loop
- when the tip or mempool changes, the state task sends a replacement job
- when the miner finds a valid block, it sends a `MinerEvent::FoundBlock` back

The miner thread never mutates `NodeState` directly.

### Mining Cancellation

Mining jobs should be replaceable.

Simple first design:
- each job has a monotonically increasing `job_id`
- the miner thread checks for a newer job between nonce batches
- if a newer job arrives, the old job is abandoned

This is sufficient for local testnet mining.

### Persistence

SQLite access should remain coordinated by the state task.

Reason:
- accepted blocks, mempool changes, and peer metadata should still follow one
  authoritative mutation path
- this avoids many async tasks performing ad hoc concurrent writes

If blocking SQLite calls become a latency problem, the runtime can move them behind
`spawn_blocking` without changing ownership.

### Shutdown

`stop()` should:
- stop accepting new inbound connections
- tell peer tasks to disconnect
- tell the miner thread to stop
- let the state task finish any final persistence
- wait for all runtime tasks to exit cleanly

`disconnect()` should remain narrower:
- drop live outbound peer sessions without stopping the node

### Migration Plan

1. introduce the runtime types and channels without changing consensus logic
2. port transport and listeners to Tokio
3. move the current server loop into a state-owning async task
4. move mining to a dedicated thread with command/event channels
5. re-enable long-lived relay and sync sessions once multiple live peer tasks are supported

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
