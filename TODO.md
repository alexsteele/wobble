# TODO

## Next

- Add wallet balances by public key and by wallet file.
- Replace raw public-key arguments with named contacts or saved recipient aliases.
- Add transaction fees and include them in coinbase rewards.
- Add better mempool conflict handling and eviction rules.
- Persist blocks and chain metadata separately from full node snapshots.
- Add block and transaction import/export formats for easier testing.
- Introduce basic peer-to-peer networking for block and transaction relay.
- Replace the snapshot-per-block UTXO strategy with a more scalable reorg mechanism.
- Add property tests and adversarial fork/restart scenarios.
- Improve the CLI with structured subcommands and clearer output.
