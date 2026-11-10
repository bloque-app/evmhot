# deposit_swept contract — changelog

evmhot owns this contract; consumers pin a version and the commit that published it. Additive changes (a new
optional field) bump the minor version; any change a strict consumer would reject bumps the major version.

## 1.0.0 — 2026-11-09

The token `deposit_swept` as of `SPEC.md` 0.2.3 (amendments 0003 and 0004): the deposit's identity and sender, and
`sweeps[]`, one entry per sweep with its coordinate (`sweep_block_number`, `sweep_block_hash`, block-level
`sweep_log_index`), `sweep_tx_hash`, `sweep_amount` and `sweep_target`. Fixtures: one sweep; a deposit over 50,000 USDC
split in two.

## Planned, not built

- Nothing for Base. Pablo chose a keyless forwarder contract on Base as the holder (Swaps' `d-base-option`, 9 Nov):
  evmhot sweeps Base USDC to it with a plain transfer, so a Base `deposit_swept` is this same 1.0.0 shape with
  `chain: "base"`, and Swaps registers the Polygon mint itself. The earlier `cctp_burn` fields are dropped.
