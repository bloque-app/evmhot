# F-005 tasks

- T-005-01 Config: the `cctp_burn` kind, its keys and load-time checks; the once-per-process on-chain check and
  `cctp_config`.
- T-005-02 Store: approval state, the `burned` deposit state with burn hash, message and nonce, and relay intents.
  Postgres and SQLite migrations, plus the importer.
- T-005-03 Fake chain: messenger, transmitter and attestation stub.
- T-005-04 Source leg: the approval intent, the burn intent and the burn receipt check.
- T-005-05 Sender check on two endpoints, with the 7702 designator; `contract_sender`.
- T-005-06 Attestation client, with backoff through the pool's rules.
- T-005-07 Relayer key (faucet index 1) and its gas through `ensure_gas` on the destination.
- T-005-08 Relay intents, the used-nonce check, finding a mint relayed by someone else, and the mint receipt check.
- T-005-09 `deposit_swept` 1.1.0: the `sweep_network` and `burn_tx_hash` fields, the contract test against the
  schema, and notify delivery.
- T-005-10 `cctp_unminted`, then a read-only fork test of the call encodings.
- T-005-11 Test deposit on Base in the test environment (the owner sends it), then production on the owner's yes.
