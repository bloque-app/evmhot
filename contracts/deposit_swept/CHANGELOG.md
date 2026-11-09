# deposit_swept contract — changelog

evmhot owns this contract; consumers pin a version and the commit that published it. Additive changes (a new
optional field) bump the minor version; any change a strict consumer would reject bumps the major version.

## 1.0.0 — 2026-11-09

The token `deposit_swept` as of `SPEC.md` 0.2.3 (amendments 0003 and 0004): the deposit's identity and sender, and
`sweeps[]`, one entry per sweep with its coordinate (`sweep_block_number`, `sweep_block_hash`, block-level
`sweep_log_index`), `sweep_tx_hash`, `sweep_amount` and `sweep_target`. Fixtures: one sweep; a deposit over 50,000 USDC
split in two.

## Planned, not built

- **1.1.0, Base USDC by CCTP (`cctp_burn`)**, if the owner rules for it: a Base deposit's `sweeps[]` entry carries the
  Polygon mint's coordinate (the `Transfer` from 0x0 to the Safe), plus new optional fields for the Base burn
  transaction hash and the network the deposit arrived on. `from_address` stays the Base sender. Field names are fixed
  in the amendment that builds it.
