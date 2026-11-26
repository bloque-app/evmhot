# F-005 · CCTP burn targets

**Implements:** `REQ-SW-6` (the `cctp_burn` kind), `REQ-SW-16` to `REQ-SW-18`, `REQ-KY-5`, `REQ-WH-12`, `INV-5`
(widened), `INV-9` · amendment 0008 · `PLAN.md` DEC-6, DEC-7.

## Goal

A USDC deposit on Base reaches the reserve on Polygon in evmhot's own transactions: a burn from the deposit address,
then a mint that evmhot relays, or finds already relayed. `deposit_swept` reports the mint as the sweep.

## Scope

- **Config.** The `cctp_burn` kind and its keys, checked at load (`REQ-SW-16`). A once-per-process check reads both
  domains, the minter and the burn limit before the first burn.
- **Source leg.** A per-address approval intent, then `depositForBurn` intents, through the sweeper's existing intent
  machinery (`REQ-SW-13`). The burn receipt check matches `DepositForBurn` and the `Transfer` out of the address. The
  deposit state `burned` is new.
- **Sender check.** `eth_getCode` on two pool endpoints before each burn, with the 7702 designator accepted
  (`REQ-SW-17`).
- **Destination leg.** An attestation client (`GET /v2/messages/{source_domain}?transactionHash=`), a check of the
  used nonce, the relayer's relay intents with an explicit nonce, and the mint receipt check (`REQ-SW-18`). Finding an
  already relayed mint uses `MessageReceived` by nonce.
- **Relayer.** The faucet mnemonic at index 1. The faucet funds it on the destination through the same `ensure_gas`
  path a deposit address uses.
- **Webhook.** `sweep_network` and `burn_tx_hash` on each `sweeps[]` entry, contract 1.1.0. Sent once every part is
  minted, and to the target's notify URL too.
- **Alerts.** `contract_sender`, `cctp_unminted`, `cctp_config`.
- **Store.** Approval state per (address, token), burned deposits with burn hash, message and nonce, and relay intents.
  Migrations for Postgres and SQLite.

## How it is checked

- Unit and property tests on the fake chain. Extend it with a messenger, a transmitter and an attestation stub, so
  every path runs in CI: approve then burn; a burn that fails its check; sender code (none, 7702, contract, reads that
  disagree); attestation pending then complete; a relay that wins; a relay that loses to another relayer; a relay
  that is lost and resent; a mint short of the burn; `cctp_unminted`.
- The interfaces (function signatures, event layouts, `usedNonces`, the attestation response) are taken from Circle's
  verified V2 source and API documentation at the time of building, not from this plan.
- A fork test against Base and Polygon mainnet state, read-only, for the call encodings.
- Then a real test deposit of 1 USDC on Base, to the test environment. That needs the owner to send it (a reserved
  step on the decision form). Production follows on the owner's yes.

## Not in scope

Fast transfers and fees; the reverse leg (payouts to Base); any destination other than one recipient per target.
