# Protocol

authors: codex, alex

## Goal

Define a minimal peer-to-peer protocol for local multi-node testing.

This protocol is intentionally small:
- transport is plain TCP
- encoding is UTF-8 JSON
- framing is one JSON message per line
- peers are manually configured
- nodes relay full transactions and blocks

Non-goals for v1:
- authentication or encryption
- peer discovery
- binary efficiency
- compact blocks
- header-first sync
- mempool package relay
- DoS resistance

## Transport

Each peer connection is a long-lived TCP stream.

Messages are framed as newline-delimited JSON:
- each message is one JSON object
- each message ends with `\n`
- senders must not embed raw newlines outside normal JSON escaping
- receivers read one line, then parse one message

Why this shape:
- trivial to inspect with logs and terminal tools
- easy to implement with `serde_json`
- good enough for local development and small tests

Known limitation:
- newline-delimited JSON is not efficient for large payloads
- a binary or length-prefixed protocol may replace it later

## Envelope

Every wire message is a tagged enum serialized as JSON.

Conceptually:

```json
{ "type": "hello", "data": { ... } }
```

Rust should model this with a serde-tagged enum so parsing is explicit and
unknown message types fail cleanly.

## Node Identity

For now, a node is identified only by:
- a listen address
- an optional human-readable node name for logs

There is no persistent peer identity yet. Connections are trusted only as much
as the local test environment is trusted.

## Handshake

When a TCP connection is established, each side should send `hello`.

`hello` fields:
- `network`: string identifying the local network, for example `wobble-local`
- `version`: protocol version number
- `node_name`: optional display name
- `tip`: current best tip block hash, if known
- `height`: current best tip height, if known

Handshake rules:
- if `network` differs, close the connection
- if `version` is unsupported, close the connection
- after a valid `hello`, the peer may request state or relay objects

This is a compatibility check, not a security boundary.

## Messages

### `hello`

Purpose:
- confirm protocol compatibility
- share a small amount of current chain state

Fields:
- `network: String`
- `version: u32`
- `node_name: Option<String>`
- `tip: Option<BlockHash>`
- `height: Option<u64>`

### `get_tip`

Purpose:
- ask a peer for its current best tip summary

Fields:
- none

### `tip`

Purpose:
- answer `get_tip`
- optionally provide tip state after handshake

Fields:
- `tip: Option<BlockHash>`
- `height: Option<u64>`

### `announce_tx`

Purpose:
- relay a full transaction to a peer

Fields:
- `transaction: Transaction`

Receiver behavior:
- validate the transaction against the active chain and mempool policy
- if accepted, store it in the mempool
- if the serving node is snapshot-backed, persist the updated snapshot
- if newly accepted, relay it to other peers

Known limitation:
- this sends full transactions immediately instead of announcing hashes first

### `announce_block`

Purpose:
- relay a full block to a peer

Fields:
- `block: Block`

Receiver behavior:
- validate and index the block
- if it becomes the best tip, update active state
- if the serving node is snapshot-backed, persist the updated snapshot
- if newly accepted, relay it to other peers

Known limitation:
- this sends full blocks immediately instead of inventory plus fetch

### `get_block`

Purpose:
- request a specific block by hash

Fields:
- `block_hash: BlockHash`

### `block`

Purpose:
- answer `get_block`

Fields:
- `block: Option<Block>`

If the block is unknown, return `null` in the `block` field.

### `mine_pending`

Purpose:
- local testnet control message to mine a block from the server's current mempool

Fields:
- `reward: u64`
- `miner_public_key: Vec<u8>`
- `uniqueness: u32`
- `bits: u32`
- `max_transactions: usize`

Receiver behavior:
- build and mine a block from the current best tip and pending mempool contents
- accept the mined block into local chain state
- if the serving node is snapshot-backed, persist the updated snapshot
- return the mined block hash

Known limitation:
- this is a local control surface for testing, not a realistic public P2P message

## Initial Sync Behavior

The first version should keep sync logic simple.

Suggested flow:
1. connect to a peer
2. exchange `hello`
3. send `get_tip`
4. if the remote tip is unknown locally, request that tip with `get_block`
5. if the received block has an unknown parent, request the parent
6. repeat until the missing chain segment is filled

This is deliberately naive:
- it may fetch blocks one at a time
- it may duplicate requests across peers
- it does not separate headers from bodies

That is acceptable for local development.

## Relay Rules

For v1, relay only on first acceptance.

Transaction relay:
- do not relay a transaction already known locally
- do not relay a transaction rejected by mempool policy
- relay newly accepted transactions to connected peers except the sender

Block relay:
- do not relay a block already indexed locally
- do not relay a block rejected by consensus or chain rules
- relay newly accepted blocks to connected peers except the sender

## Error Handling

There is no explicit error message in v1.

If a peer sends invalid data:
- log the parse or validation failure
- ignore that message, or close the connection for malformed protocol traffic

If a peer is merely ahead:
- keep the connection open
- request missing blocks as needed

## Serialization Notes

Wire types should derive `Serialize` and `Deserialize`.

Preferred shapes:
- keep field names explicit and stable
- use serde-tagged enums for message dispatch
- reuse existing `Transaction`, `Block`, and hash types where practical

Potential follow-up:
- define dedicated wire wrappers if internal consensus structs diverge from the
  public protocol

## Gaps

The first protocol version intentionally leaves out:
- peer discovery
- peer addresses exchange
- mempool sync
- orphan transaction handling
- partial block download
- header-first synchronization
- request deduplication
- bandwidth limits
- authentication
- encryption
- rate limiting
- peer scoring or banning

## Implementation Plan

Minimum code shape:
- `src/net.rs`: connection handling and peer tasks
- `src/wire.rs`: serde message types for the protocol
- `src/main.rs`: simple commands to run a node and connect to peers

Suggested first demo:
1. start node A listening on a local TCP port
2. start node B listening on another port with A as a peer
3. submit a transaction to A
4. observe B receive and accept the transaction
5. mine a block on B
6. observe A receive and accept the block
