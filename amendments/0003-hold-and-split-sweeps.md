# Amendment 0003 — held tokens and split sweeps

**Status:** ruled. Applied to `SPEC.md` 0.2.2.
**Target:** `SPEC.md` 0.2.1
**Date:** 2026-11-09
**Rulings applied:** R9 (Polygon USDT0 and Base USDC go to bezent's reserve, and stay in their deposit addresses
until it admits them). The webhook shape for split sweeps is an engineering change, evmhot.
**Rationale in one line:** a token whose target can't take it yet must wait without being swept anywhere else, and a
deposit above a target's maximum is settled by several sweeps, each of which a consumer must be able to find.

## A1 — The `hold` kind

Revise `REQ-SW-6`: add `hold`. A held token is detected and notified, never swept; changing its target releases the
held deposits to the new target. This is how R9 runs until bezent admits USDT0 and Base USDC.

## A2 — Several sweeps per deposit

Revise `REQ-WH-4`: a token `deposit_swept` carries `sweeps`, one entry per sweep that settled the deposit, each with
the full sweep coordinate. A single `sweep_*` set could not describe a deposit split by `REQ-SW-7`. The top-level
`sweep_tx_hash` stays, as the first entry's, so existing clients keep working. The event is sent once, after the last
sweep succeeds.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | none |
| Revised | `REQ-SW-6`, `REQ-WH-4` |
| Withdrawn | none |

## What this costs, stated plainly

- Held deposits sit in hot-wallet addresses for as long as the hold lasts.
- A split deposit's `deposit_swept` arrives only after its last part; until then the consumer sees `deposit_detected`
  alone.
