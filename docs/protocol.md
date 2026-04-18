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

Peer communication uses plain TCP streams.

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

## Connection Semantics

The current protocol allows a peer session to carry multiple messages after the
initial handshake, but the implementation does not yet treat every connection
class the same way.

Connection classes:
- sync connections may stay open across a short request sequence such as `get_tip` followed by one or more `get_block` requests
- relay connections are currently best-effort one-shot connections
- admin connections are separate from public peer connections and are localhost-only

Why relay is still one-shot:
- the current server handles one inbound peer stream at a time
- leaving a relay socket open would keep the remote node inside that one stream handler
- that would delay later inbound connections until the relay socket closed

Planned direction:
- move to an async runtime that can manage multiple live peer sockets concurrently
- then allow a single long-lived peer connection to carry relay, sync, and request traffic safely

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

The `hello` message may also carry an advertised listener address so relay
logic can avoid bouncing an accepted object straight back to the announcing
peer. There is still no persistent cryptographic peer identity.

## Handshake

When a TCP connection is established, each side should send `hello`.

`hello` fields:
- `network`: string identifying the local network, for example `wobble-local`
- `version`: protocol version number
- `node_name`: optional display name
- `advertised_addr`: optional listener address this node wants peers to use
- `tip`: current best tip block hash, if known
- `height`: current best tip height, if known

Handshake rules:
- if `network` differs, close the connection
- if `version` is unsupported, close the connection
- after a valid `hello`, the peer may request state or relay objects
- `hello` must be the first message on a newly opened peer connection

This is a compatibility check, not a security boundary.

After handshake:
- a peer may send multiple protocol messages on the same connection
- the current protocol is ordered and implicit: there are no request IDs
- request/response flows therefore assume one in-flight request at a time on a given connection

## Messages

### `hello`

Purpose:
- confirm protocol compatibility
- share a small amount of current chain state

Fields:
- `network: String`
- `version: u32`
- `node_name: Option<String>`
- `advertised_addr: Option<String>`
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
- persist the updated live state in SQLite
- if newly accepted, relay it to other peers

Current session note:
- outbound relay currently uses a one-shot connection for each high-level announcement
- this is an implementation constraint, not a protocol requirement

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
- persist the updated live state in SQLite
- if newly accepted, relay it to other peers

Current session note:
- if a relayed block arrives before its parent, the receiver may fetch missing ancestors from the announcing peer
- that ancestor sync may reuse a single sync session for `get_tip` plus one or more `get_block` requests

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
- persist the updated live state in SQLite
- return the mined block hash

Known limitation:
- this is a local control surface for testing, not a realistic public P2P message

## Initial Sync Behavior

The first version should keep sync logic simple.

Suggested flow:
1. connect to a peer
2. exchange `hello`
3. use the advertised `tip` from `hello` when present
4. if the remote tip is unknown locally, request that tip with `get_block`
5. if the received block has an unknown parent, request the parent
6. repeat until the missing chain segment is filled

Current implementation notes:
- startup sync may still poll `get_tip` when the node does not yet have useful peer metadata
- hello-triggered and tip-triggered sync use the peer's advertised tip directly
- the sync walk may reuse one outbound session per selected peer
- the node requests only one peer at a time
- a successful sync closes that short-lived sync session when the walk completes

This is deliberately naive:
- it may fetch blocks one at a time
- it may duplicate requests across peers
- it does not separate headers from bodies

That is acceptable for local development.

## Next Sync Step

The next protocol improvement should make sync more push-driven and less dependent
on periodic polling.

Planned additions:
- add an explicit `announce_tip` message carrying `tip` and `height`
- let peers send `announce_tip` on an existing live connection whenever their best tip changes
- keep small per-peer sync state in memory, including advertised tip, advertised height, whether a sync is already in progress, and recent sync success or failure times

Planned receiver behavior:
- when a peer announces a better tip, update the stored peer metadata
- if the announced tip is unknown locally and that peer is ahead, schedule one follow-up sync attempt from that peer
- avoid starting duplicate sync attempts while one is already active for the same peer

Planned sync walk:
1. receive `announce_tip { tip, height }`
2. connect or reuse the live peer session
3. request the advertised tip block
4. if that block's parent is missing, keep walking backward until a known ancestor is found
5. apply the missing segment forward in order

Intended policy:
- push-based tip announcements should become the normal trigger for catch-up
- periodic configured-peer sync should remain only as a slow background repair path

Why this is the next step:
- it reduces repeated `hello` chatter
- it avoids blind polling when peers already know they advanced
- it gives the server a clearer boundary between peer metadata updates and actual sync work

## Relay Rules

For v1, relay only on first acceptance.

Transaction relay:
- do not relay a transaction already known locally
- do not relay a transaction rejected by mempool policy
- relay newly accepted transactions to connected peers except the sender
- prefer matching the sender by advertised listener address from `hello`
- fall back to `node_name` only when no advertised address is available

Block relay:
- do not relay a block already indexed locally
- do not relay a block rejected by consensus or chain rules
- relay newly accepted blocks to connected peers except the sender

Current implementation notes:
- sender suppression prefers the `advertised_addr` learned from `hello`
- if no advertised listener address is known, relay suppression may fall back to `node_name`
- relay remains best-effort; a failed outbound announcement does not roll back local acceptance

## Lifecycle

The server now exposes two distinct lifecycle controls:
- `disconnect()`: drop cached outbound peer sessions without stopping the node
- `stop()`: stop the server runtime and close its active outbound sessions during shutdown

These are runtime controls, not wire messages.

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
