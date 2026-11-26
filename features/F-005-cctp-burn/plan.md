# F-005 · CCTP burn targets

**Implements:** `REQ-SW-6` (the `cctp_burn` kind), `REQ-SW-16` to `REQ-SW-18`, `REQ-KY-5`, `REQ-WH-12`, `INV-9`, and
the parts of `REQ-SW-3`, `REQ-SW-13` to `REQ-SW-15`, `REQ-FC-1`, `REQ-WH-4`, `REQ-OB-4`, `INV-5`, `INV-7` and `INV-8`
marked (not built, amendment 0008) · `PLAN.md` DEC-6, DEC-7.

## Goal

A USDC deposit on Base reaches the reserve on Polygon in evmhot's own transactions: a burn from the deposit address,
then a mint that evmhot relays or finds already relayed. `deposit_swept` reports the mint as the sweep.

## Scope

- **Config.** The `cctp_burn` kind with its destination token, checked at load: `per_transfer` with a maximum, two
  RPC endpoints, and bounds within the reserve's. The pre-burn reads: domains, minter, burn limit, the destination's
  local token, and the minimum fee where it exists; `cctp_config`.
- **Source leg.** The allowance is read on chain before each burn. An approval intent, then `depositForBurn` intents,
  through the existing intent machinery (`REQ-SW-13`). The burn receipt check matches every `DepositForBurn` argument
  and the `Transfer` to the minter. A burn is accepted only at or below the detection height.
- **Parts in flight.** They count as paid in what a deposit is owed and in the `REQ-SW-14` reservation. `REQ-SW-3`
  skips such deposits. There is a `burned` state.
- **Sender check.** One verdict per deposit, from two endpoints, stored, with the 7702 designator accepted;
  `contract_sender` and `sender_unchecked`.
- **Attestation.** A client for `GET /v2/messages/{domain}?transactionHash=`: first poll after 10 minutes, then every
  minute. A 404 or empty answer is pending; a 429 pauses every request for 5 minutes, separate from the RPC pool.
  `POST /v2/reattest/{nonce}` before expiry.
- **Mint lookup.** The API's reported mint transaction first, then `usedNonces` plus `MessageReceived` by nonce in
  bounded ranges, then a relay. The coordinate is taken between `MessageReceived` logs, so batched relays by others
  resolve to the right log.
- **Relayer.** Faucet index 1. One relay loop per destination, with at most one open relay intent. The `usedNonces`
  and expiry reads come before every rebroadcast or re-sign. A nonce whose message someone else relayed is re-signed
  for the next relay, or as a zero-value self-transfer. The faucet funds the relayer through `ensure_gas`.
- **Webhook.** `sweep_network` and `burn_tx_hash` on each entry; the top-level `sweep_tx_hash` is the first burn.
  Contract 1.1.0, sent only once every part is minted, notify URL included.
- **Store.** Sender verdicts; parts in flight with burn, message, nonce, expiry and attestation; relay intents.
  Postgres and SQLite migrations, plus the importer.
- **Alert.** `cctp_unminted`, with Circle's `delayReason`.

## How it is checked

- **On the fake chain**, extended with a messenger, a transmitter, a minter and an attestation stub, so every path
  runs in CI:
  - approve then burn, with an allowance already set or a changed messenger;
  - a split deposit burned in parts, with no `balance_shortfall`;
  - a burn that fails its check, and a burn removed by a reorganisation and rebroadcast;
  - sender code: none, 7702, a contract, or reads that disagree;
  - attestation: pending, 404, 429, then complete;
  - relays: one that wins; one that loses to another relayer, singly and in a batch; one that is lost and resent; an
    expired message re-attested; a reverted relay;
  - a mint short of the burn, and `cctp_unminted`.
- **Interfaces.** Function signatures, event layouts, `usedNonces`, the attestation and re-attestation responses come
  from Circle's verified V2 source and API documentation at build time, not from this plan.
- **Unverified points, to check before production:**
  - whether standard messages carry a non-zero `expirationBlock`;
  - whether re-attestation applies to them;
  - whether the API answers 404 or an empty list for a transaction it hasn't indexed;
  - that USDC's mint emits `Transfer(0x0 → to)`.
- **A read-only fork test** against Base and Polygon mainnet, for the call encodings and the deployed contracts'
  events.
- **The rollout gate.** Every consumer of Base `deposit_swept` confirms it reads 1.1.0, and the runbook lists them.
  Then a real 1 USDC test deposit on Base to the test environment, which the owner sends (a reserved step on the
  decision form). Production follows on the owner's yes.

## Not in scope

Fast transfers and fees; the reverse leg (payouts to Base); any destination other than one recipient per target;
batching relays.
