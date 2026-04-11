# Admin Interface

authors: codex, alex

## Goal

Define the local-only management interface for a running wobble node.

This interface is separate from the peer-to-peer protocol:
- the peer socket is for node-to-node communication
- the admin socket is for local management and operator actions

That separation matters because management endpoints should not be callable by
arbitrary peers on the network.

## Transport

The admin interface currently uses:
- plain TCP
- localhost binding, for example `127.0.0.1:9000`
- newline-delimited JSON

This is intentionally simple and easy to inspect. It is also a reasonable
foundation for both:
- the CLI
- a future local web portal

## Configuration

The node home config stores the admin listener address:

```json
{
  "listen_addr": "127.0.0.1:9001",
  "admin_addr": "127.0.0.1:9000",
  "network": "wobble-local",
  "node_name": "miner"
}
```

`listen_addr` is the public peer listener.

`admin_addr` is the local management listener and should normally stay bound to
localhost.

## Requests

Current admin requests:

- `get_status`
  Returns best tip, height, mempool size, and configured peer count.

- `get_balance`
  Returns the active-chain balance for one public key.

- `submit_transaction`
  Submits a signed transaction into the local node's mempool path.

- `bootstrap`
  Mines coinbase-only blocks to a supplied public key so a fresh testnet has
  spendable funds.

## Boundary

Rule of thumb:

- If the operation is part of node-to-node consensus or relay, it belongs on
  the peer protocol.
- If the operation is for local operator control, wallet UX, diagnostics, or
  management, it belongs on the admin interface.

Examples:

- peer protocol:
  `hello`, `get_tip`, `get_block`, `announce_tx`, `announce_block`

- admin interface:
  `status`, `wallet-balance`, local payment submission, bootstrap funding

## Future

Likely future admin additions:

- wallet address query
- peer list and peer health
- mining status and mining controls
- recent mempool transaction summaries
- a web UI layered on top of the same local admin socket
