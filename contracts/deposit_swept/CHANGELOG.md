# deposit_swept contract — changelog

evmhot owns this contract; consumers pin a version and the commit that published it. Additive changes (a new
optional field) bump the minor version; any change a strict consumer would reject bumps the major version.

## 1.1.0 — 2026-11-26 (not built)

Additive, for `cctp_burn` targets (`SPEC.md` 0.2.8, amendment 0008). Each `sweeps[]` entry MAY carry:

- `sweep_network`: the network its coordinate is on, when that differs from `chain`. A Base deposit burned over CCTP
  has `chain: "base"` and `sweep_network: "polygon"`. Absent means `chain`, so every 1.0.0 body is a valid 1.1.0 body.
- `burn_tx_hash`: the `depositForBurn` on `chain`. `sweep_tx_hash` and the coordinate are then the mint's
  `receiveMessage` on `sweep_network`, and `sweep_target` is the mint recipient.

The event is sent once every part is minted, never after the burn alone. Fixture: one Base deposit minted to the
Polygon reserve. Consumers key a sweep on `(sweep_network or chain, sweep_block_hash, sweep_log_index)`.

## 1.0.0 — 2026-11-09

The token `deposit_swept` as of `SPEC.md` 0.2.3 (amendments 0003 and 0004): the deposit's identity and sender, and
`sweeps[]`, one entry per sweep with its coordinate (`sweep_block_number`, `sweep_block_hash`, block-level
`sweep_log_index`), `sweep_tx_hash`, `sweep_amount` and `sweep_target`. Fixtures: one sweep; a deposit over 50,000 USDC
split in two.

## Planned, not built

- Fast CCTP transfers (a non-zero `max_fee`): the fee rule is Swaps' to set. 1.1.0 carries no fee field; the
  minted amount is `sweep_amount`.
