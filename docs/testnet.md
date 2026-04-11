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

Still missing for a convincing multi-node testnet:
- configured peer set per running server
- automatic relay of newly accepted transactions
- automatic relay of newly accepted or mined blocks
- an explicit multi-node integration test that exercises the whole path

Implemented in the first slice:
- configured peer set on `Server`
- best-effort transaction relay for newly accepted transactions
- best-effort relay hooks for newly accepted and newly mined blocks
- ignored integration test target at `tests/testnet_e2e.rs`

Current limitation:
- relay is still short-lived request/response TCP rather than persistent peer sessions
- the deterministic test harness must budget for duplicate gossip because nodes
  currently relay newly accepted objects back to configured peers without
  origin tracking or inventory suppression

## Implementation Plan

### 1. Best-Effort Peer Relay

Add a small peer list to `Server`.

Behavior:
- when a transaction is newly accepted, relay it to configured peers
- when a block is newly accepted from a peer, relay it to configured peers
- when a block is mined locally, relay it to configured peers

Rules:
- relay is best effort
- local acceptance must not fail just because a peer is unavailable
- duplicate objects should be treated as already known, not as fatal protocol errors

### 2. Separate Integration Test Target

Add an ignored integration test under `tests/testnet_e2e.rs`.

Why ignored:
- it uses real local TCP sockets
- it is slower and more orchestration-heavy than unit tests
- it should not run in the default `cargo test` unit path

Expected command:

```text
cargo test --test testnet_e2e -- --ignored
```

### 3. First E2E Scenario

Two nodes:
- node A: proposer-facing node
- node B: miner node

Connections:
- A knows peer B
- B knows peer A

Flow:
1. initialize node A and node B state
2. give node A spendable funds
3. start both servers on local TCP ports
4. submit a payment to node A
5. assert node B learns the transaction through relay
6. trigger mining on node B
7. assert node A learns the mined block through relay
8. assert both nodes share the same best tip
9. assert the mined block contains the submitted transaction

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

We consider this slice complete when:
- a separate ignored integration test proves the proposer-to-miner-to-peer path
- the path works over the real TCP protocol
- normal unit tests stay fast and unchanged
