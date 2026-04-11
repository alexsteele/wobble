# TODO

## Node Operation

- Add a clean “run a node” workflow around `serve`.
- Add optional integrated mining while the node is serving.
- Add simple mining controls for testnet use: reward wallet, max transactions, difficulty bits, and mining cadence.
- Surface basic live node status clearly: best tip, height, mempool size, and configured peers.

## Remote Wallet Flow

- Keep `submit-payment-remote` as the main send path and make it easier to use end to end.
- Improve submit errors so users can distinguish funds, signature, policy, and connectivity failures.
- Make recipient aliases and wallet-based sending feel first-class in the remote flow.

## CLI Ergonomics

- Improve the CLI with structured subcommands and clearer output.
- Document a short manual testnet flow in the README: start nodes, submit a payment, mine it, and inspect balances.
- Reduce rough edges between local and remote command shapes where practical.

## Networking And Sync

- Move from one-shot sync triggers toward a more continuous background sync loop.
- Introduce basic peer-to-peer networking for block and transaction relay.
- Add block and transaction import/export formats for easier testing.

## State And Performance

- Add an owner-to-UTXO index so balance queries and coin selection do not scan the full active UTXO set.
- Replace the snapshot-per-block UTXO strategy with a more scalable reorg mechanism.

## Observability

- Expand structured logging so accepted tx/block events consistently include post-transition state such as best tip and mempool size.

## Testing

- Add property tests and adversarial fork/restart scenarios.

## Later

- Server mining options, parallelism.
