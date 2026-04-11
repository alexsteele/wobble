# TODO

## Next

- Add an owner-to-UTXO index so balance queries and coin selection do not scan the full active UTXO set.
- Add block and transaction import/export formats for easier testing.
- Introduce basic peer-to-peer networking for block and transaction relay.
- Replace the snapshot-per-block UTXO strategy with a more scalable reorg mechanism.
- Add property tests and adversarial fork/restart scenarios.
- Surface mempool policy rejections in the CLI with clearer submit error messages.
- Improve the CLI with structured subcommands and clearer output.
- Server mining options, parallelism
- Expand structured logging so accepted tx/block events consistently include post-transition state such as best tip and mempool size.
