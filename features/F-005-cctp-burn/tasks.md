# F-005 tasks

- T-005-01 Config: the `cctp_burn` kind, its keys and load-time checks; the pre-burn reads and `cctp_config`.
- T-005-02 Store: sender verdicts, parts in flight (burn, message, nonce, expiry, attestation), relay intents.
  Postgres and SQLite migrations, plus the importer.
- T-005-03 Fake chain: messenger, transmitter, minter, attestation and re-attestation stub.
- T-005-04 Source leg: the on-chain allowance check, approval and burn intents, the burn receipt check, and acceptance
  at the detection height.
- T-005-05 Parts in flight in what a deposit is owed, in the reservation and in `REQ-SW-3`; the `burned` state.
- T-005-06 Sender check: two endpoints, one stored verdict, 7702; `contract_sender` and `sender_unchecked`.
- T-005-07 Attestation client: timing, 404 and empty answers, the 5-minute 429 pause, re-attestation.
- T-005-08 Mint lookup and the coordinate between `MessageReceived` logs; batched relays by others.
- T-005-09 Relayer (faucet index 1): the relay loop, relay intents, `usedNonces` and expiry before every resend, nonce
  reuse, and its gas through `ensure_gas`.
- T-005-10 `deposit_swept` 1.1.0: the fields, the top-level `sweep_tx_hash`, the contract test and notify delivery.
- T-005-11 `cctp_unminted`, then a read-only fork test against the deployed contracts; settle the unverified points
  in `plan.md`.
- T-005-12 Rollout gate: consumers confirm 1.1.0. Then a test deposit on Base in the test environment (the owner
  sends it), then production on the owner's yes.
