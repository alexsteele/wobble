# Testnet Plan

authors: codex, alex

## Goal

Demonstrate a real end-to-end testnet flow:
- one node accepts a submitted transaction
- that transaction reaches a miner node over the network
- the miner includes it in a block
- the block reaches another node over the network
- both nodes converge on the same best tip and balances

## Current State

Already implemented:
- SQLite-backed node state
- live TCP server
- handshake and basic request handling
- transaction announcement
- block announcement
- remote mining trigger
- configured peer set on `Server`
- optional peer bootstrap file for `wobble serve --peers_path <path>`
- best-effort transaction relay for newly accepted transactions
- best-effort relay hooks for newly accepted and newly mined blocks
- outbound client handshake helper used by remote CLI flows
- optional bootstrap sync from configured peers at server startup
- live hello-triggered sync when a peer advertises a tip this node does not know
- feature-gated integration test target at `tests/testnet_e2e.rs`
- end-to-end proposer -> miner -> proposer flow over real TCP
- restart/persistence proposer -> miner -> proposer flow over real TCP
- multi-hop proposer -> relay -> miner -> relay -> proposer flow over real TCP
- lagging-node catch-up from a live peer during bootstrap sync
- live lagging-node catch-up after a later peer hello advertisement

What the current E2E test proves:
- a proposer-facing node accepts a payment transaction
- the miner node learns that transaction through relay
- the miner mines the transaction into a block
- the proposer node learns the mined block through relay
- both nodes converge on the same tip and balances
- a restarted proposer reloads persisted mempool/chain state from SQLite and still converges after mining
- a relay node can forward a payment onward to a miner and forward the mined block back to the proposer
- a lagging node can start later, fetch a missed block from a configured peer, and converge before serving
- a running lagging node can learn about a newer tip from a later peer hello, sync it, and converge

Current limitation:
- relay is still short-lived request/response TCP rather than persistent peer sessions
- origin suppression now prefers the advertised listener address from `hello`
  and falls back to `node_name` only when that address is unavailable
- catch-up runs at bootstrap and on later peer hello, but not yet as a background sync loop

## Implementation Plan

### 1. Manual Testnet Flow

Document a concrete two-node CLI flow in the README.

Behavior:
- initialize two SQLite-backed nodes
- start both servers with peer bootstrap files
- submit a payment to one node
- mine on the other node
- inspect balances and tips from the CLI

Rules:
- keep the flow short and reproducible
- prefer commands that match the current real code path
- use it to expose rough edges in CLI ergonomics before larger networking work

### 2. Restart And Persistence E2E

Add a feature-gated scenario that covers restart behavior.

Flow:
1. submit a payment to a node
2. persist the node state
3. restart the process from SQLite
4. verify the mempool or best-tip state survives as expected
5. complete mining and convergence after restart

### 3. Multi-Hop Relay E2E

Add a three-node scenario:
- node A: proposer-facing node
- node B: relay node
- node C: miner node

Connections:
- A knows peer B
- B knows peers A and C
- C knows peer B

Flow:
1. submit a payment to node A
2. verify node B relays it onward to node C
3. mine on node C
4. verify the mined block returns through node B to node A
5. assert all three nodes converge on the same tip

Completed:
- proposer restart and SQLite-backed recovery are covered in `tests/testnet_e2e.rs`
- multi-hop relay through an intermediate node is covered in `tests/testnet_e2e.rs`

### 4. Catch-Up After Missed Blocks

Prove that a node that misses a block can reconnect and catch up with the
existing `get_tip` / `get_block` path.

Flow:
1. take one node offline
2. submit and mine while it is absent
3. reconnect it
4. fetch the missing block segment
5. assert it reaches the same best tip as the live nodes

Completed:
- a lagging node can bootstrap from a configured peer, pull a missed tip block,
  and converge in `tests/testnet_e2e.rs`
- a running lagging node can catch up later when a peer hello advertises a
  newer tip in `tests/testnet_e2e.rs`

## Non-Goals For This Slice

Do not add yet:
- background mining loops
- peer discovery
- retry queues
- inventory batching
- header-first synchronization
- peer scoring
- block download scheduling

## Exit Criteria

We consider the current slice complete when:
- a separate feature-gated integration test proves the proposer-to-miner-to-peer path
- the path works over the real TCP protocol
- normal unit tests stay fast and unchanged

We consider the next testnet slice complete when:
- the README shows a real manual two-node flow
- reconnect and catch-up after missed blocks is covered by an end-to-end test
