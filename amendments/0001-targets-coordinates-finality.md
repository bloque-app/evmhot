# Amendment 0001 — sweep targets, the sweep coordinate, finality and alerts

**Status:** ruled. Applied to `SPEC.md` 0.2.0.
**Target:** `SPEC.md` 0.1.0
**Date:** 2026-11-09
**Rulings applied:** by @pandres95 on the evmhot decision form, 2026-11-09: `d-usdc-sweep` (sweep the reserve token to a
reserve target), `d-gap` (scan the gap offline before moving a cursor), `d-rpc` (provider plan; alerts ship either
way). Also agreed with bezent's maintainers on 2026-11-08 and 2026-11-09 (the coordinate, the bounds, one token per
reserve target). Engineering changes evmhot owns are marked as such.
**Rationale in one line:** a consumer that verifies sweeps on chain needs to find each sweep by its coordinate, and must
never receive a token it does not admit or an amount outside its bounds; and a silent monitor must never happen again.

## A1 — Sweep target kinds

Add `REQ-SW-6`. A target is `transfer` (an address) or `directed_deposit` (a contract and a 32-byte recipient, one per
network and token). `directed_deposit` is specified now and built when its consumer's contract exists.

## A2 — Bounds per target

Add `REQ-SW-7`. A target may declare a minimum and a maximum per sweep. Below the minimum, the balance stays in the
deposit address; above the maximum, it is swept in several sweeps. A consumer refuses out-of-bounds arrivals, and
an arrival it refuses can only be released by hand.

## A3 — One token per reserve target

Add `REQ-SW-8`, `REQ-SW-9` and `INV-6`. A target that admits one token refuses configuration that routes another
token to it, at load time. Each sweep emits one `Transfer` into its target.

## A4 — The sweep coordinate

Add `REQ-SW-10`. Revise `REQ-WH-4`: `deposit_swept` adds `sweep_block_number`, `sweep_block_hash`, `sweep_log_index`
(the block-level log index of the `Transfer` into the target) and `sweep_amount`. A transaction hash alone is not a
coordinate.

## A5 — Sweep outcome

Add `REQ-SW-5` and `INV-5`. A deposit is marked swept only after a successful receipt. (Engineering change, evmhot.)

## A6 — Finality per network

Revise `REQ-DT-7` to describe 0.1.0, and add `REQ-DT-8`: each network chooses `finalized`, `safe` or newest-minus-offset
as its detection height, defaulting to `finalized`. (Engineering change, evmhot, agreed with bezent.)

## A7 — Lag and provider alerts

Add `REQ-OB-1` and `REQ-OB-2`. (Ruling `d-rpc`: the alert ships whatever provider is chosen.)

## A8 — Operator routes

Add `REQ-API-2`: moving a cursor requires the operator's token. Add `REQ-API-1`: a standalone evmhot keeps the `/evm`
paths and shapes. (Engineering change, evmhot.)

## A9 — The symbol-length filter

Withdraw the 0.1.0 behaviour that marked a token deposit swept without a sweep when the token's symbol was longer than
five characters. Admission is by contract address (`REQ-DT-2`), which already rejects spoofed tokens. (Engineering
change, evmhot.)

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SW-5` to `REQ-SW-10`, `REQ-DT-8`, `REQ-OB-1`, `REQ-OB-2`, `REQ-API-1`, `REQ-API-2`, `INV-5`, `INV-6` |
| Revised | `REQ-DT-7`, `REQ-WH-4` |
| Withdrawn | none |

## What this costs, stated plainly

- A balance below a target's minimum waits in its deposit address, possibly for a long time.
- Detection on `finalized` adds whatever the network's finality takes. On a network where it is long, the
  configuration chooses `safe` or an offset instead.
- Splitting large balances costs one sweep's gas per part.
