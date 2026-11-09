# Amendment 0001 — sweep targets, the sweep coordinate, finality and alerts

**Status:** ruled. Applied to `SPEC.md` 0.2.0.
**Target:** `SPEC.md` 0.1.0
**Date:** 2026-11-09
**Rulings applied:** by @pandres95 on the evmhot decision form, 2026-11-09: `d-usdc-sweep` (sweep the reserve token to a
reserve target), `d-gap` (scan the gap offline before moving a cursor), `d-rpc` (provider plan; alerts ship either
way), and on bezent's form, 2026-11-09, `d-refund-dest-swaps` (a refund goes to the original sender). Rulings are
numbered in `notes/RULINGS.md`. Also agreed with bezent's maintainers on 2026-11-08 and 2026-11-09 (the coordinate, the bounds, one token per
reserve target). Engineering changes evmhot owns are marked as such.
**Rationale in one line:** a consumer that verifies sweeps on chain needs to find each sweep by its coordinate, and must
never receive a token it does not admit or an amount outside its bounds; and a monitor that stops must say so.

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

Add `REQ-API-2`: moving a cursor (`POST /evm/block_number`) requires the operator's token. Add `REQ-API-1`: a standalone evmhot keeps the `/evm`
paths and shapes. (Engineering change, evmhot.)

## A9 — The symbol-length filter

Withdraw `REQ-DT-10`: the 0.1.0 monitor dropped a token transfer whose symbol was longer than five characters, or
whose symbol could not be read. Admission is by contract address (`REQ-DT-2`), which already rejects spoofed tokens. (Engineering
change, evmhot.)

## A10 — The derivation counter's ceiling

Add `REQ-RG-5`: registration is refused at 2^31, never wrapped. (Engineering change, evmhot. Revised by amendment
0002: the ceiling is 2^32.)

## A11 — Atomic outbox

Add `REQ-WH-7`: the outbox write is in the same transaction as the state change it reports. (Engineering change,
evmhot.)

## A12 — Separate tokens

Add `REQ-WH-8`: the webhook bearer token and the operator token are different secrets, and the operator token never
leaves for a webhook URL. (Security fix, evmhot. In production the operator routes were not served, so the shared
token gave no operator access there.)

## A13 — Refunds to the sender

Add `REQ-SW-12` and `REQ-WH-9`, and revise `REQ-SW-1` to describe 0.1.0. A refund goes to the original sender of the
inbound transfer, never to the deposit address (ruling R7). So every deposit records its sender, both events carry it,
and a sweep into a target that needs a refund address never mixes senders. Until ruled otherwise, such a sweep covers
one inbound transfer; dust from one sender may accumulate to the minimum.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SW-5` to `REQ-SW-10`, `REQ-SW-12`, `REQ-DT-8`, `REQ-RG-5`, `REQ-WH-7` to `REQ-WH-9`, `REQ-OB-1`, `REQ-OB-2`, `REQ-API-1`, `REQ-API-2`, `INV-5`, `INV-6` |
| Revised | `REQ-DT-7`, `REQ-WH-4`, `REQ-SW-1`, `REQ-OB-3` |
| Withdrawn | `REQ-DT-10` |

## What this costs, stated plainly

- A balance below a target's minimum waits in its deposit address, possibly for a long time.
- Detection on `finalized` adds whatever the network's finality takes. On a network where it is long, the
  configuration chooses `safe` or an offset instead.
- Splitting large balances costs one sweep's gas per part, and so does sweeping each sender's transfers separately.
