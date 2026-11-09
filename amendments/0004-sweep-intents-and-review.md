# Amendment 0004 — sweep intents, review, one sweep per transfer

**Status:** ruled. Applied to `SPEC.md` 0.2.3.
**Target:** `SPEC.md` 0.2.2
**Date:** 2026-11-09
**Rulings applied:** R13 (one sweep per inbound transfer). The rest is an engineering change, evmhot, found by the
reviews of F-002a (PR #17).
**Rationale in one line:** a sweep that may already be on chain must be known before it is broadcast, and a deposit
whose settlement evmhot cannot prove must stop and wait for an operator, never be settled by guesswork.

## A1 — A sweep intent before every broadcast

New `REQ-SW-13`: before a sweep transaction is broadcast, evmhot signs it and records a sweep intent: the deposits it
covers, the target, the part, the amount, the nonce, the transaction hash and the signed bytes. At the start of every
pass, every pending intent on the network is resolved, whatever its deposits' status:

- a receipt for its hash settles it (success) or fails it (revert);
- with no receipt, while the address's nonce has not reached the intent's, the same signed bytes are broadcast again;
  after 10 passes the intent is dropped and the deposits are planned again (a new transaction at that nonce replaces
  the old one);
- with no receipt, once the nonce is used by another transaction, the deposits go to review (`A2`);
- an intent recorded but never signed is dropped.

New `INV-7`: no deposit is marked swept (by a receipt or by the zero-balance rule, `REQ-SW-3`) or released by an
operator while a sweep intent covering it is pending.

`REQ-SW-3` now applies only to a group with no recorded sweep and no intent other than failed or dropped ones.

## A2 — Deposits that need review

New `REQ-SW-14`: a token deposit enters `needs_review`, and is not swept again until an operator releases it, when:

- its sweep's receipt succeeded but holds no `Transfer` from the deposit address to the target, or holds one whose
  amount differs from the intent (`kind=sweep_review`);
- its sweep's nonce was used by another transaction (`kind=sweep_review`);
- for a `REQ-SW-12` or `REQ-SW-15` target, the deposit address holds less of the token than is still owed on it
  (`kind=balance_shortfall`);
- for such a target, the deposit has no recorded sender, as rows detected before 0.2.2 do (`kind=no_sender`).

While any deposit on an (address, token) pair is in `needs_review` or `failed`, or has a pending intent, nothing on
that pair is swept (`kind=pair_blocked`, naming the blocking deposit). A sweep therefore never moves tokens that
belong to a deposit waiting for a person.

## A3 — One sweep per inbound transfer

New `REQ-SW-15`: a target MAY declare `per_transfer`. Each inbound deposit is swept on its own, never with another,
even from the same sender. A sweep sends the lesser of what that deposit is still owed and the address's balance; a
balance below what the pair owes is `A2`'s shortfall. A deposit above the target's maximum is swept in parts, all for
that deposit. A `per_transfer` target whose minimum is above the token's detection minimum is refused at load, because
a deposit between the two could never be swept (`REQ-DT-4`); so is a target that declares both `per_transfer` and
`per_sender`. bezent's Safe is a `per_transfer` target (R13).

## A4 — Splits and dust

Revise `REQ-SW-7`: a split plans at most 10 parts per pass, reading the balance again each pass. When the last part
would be below the minimum, the pass sends one full part fewer, so the next pass can split the rest. A balance below
the minimum stays owed: the deposit is not settled until later deposits lift it.

## A5 — Operator release

Revise §9.2: `POST /admin/retry_sweeps` moves a deposit in `needs_review` or `failed` back to `detected`, in one
transaction. It refuses while an intent covering the deposit is pending, naming the transaction. A `review` intent
with a transaction hash goes back to pending, so its receipt is read again before anything is planned.

## A6 — Alerts

New `REQ-OB-4`: an alert is a log line that starts with `EVMHOT_ALERT kind=<kind> network=<network>` and carries the
deposit's identity and transaction where there are some. Kinds: `sweep_review`, `balance_shortfall`, `no_sender`,
`pair_blocked` and `dust`; `lag` and `provider` come with F-002b. `dust` and `pair_blocked` are raised once per
deposit and blocking state; later passes log quietly. Metric filters match the marker and the kind.

## A7 — Where the target is recorded

Revise `REQ-SW-10`: each sweep also records its target address, and each `sweeps` entry in `deposit_swept` carries it
as `sweep_target`.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SW-13`, `REQ-SW-14`, `REQ-SW-15`, `REQ-OB-4`, `INV-7` |
| Revised | `REQ-SW-3`, `REQ-SW-7`, `REQ-SW-10`, §9.2 |
| Withdrawn | none |

## What this costs, stated plainly

- A deposit in review, and every other deposit on its address and token, waits for a person. Until then the funds
  sit in the deposit address and no `deposit_swept` is sent.
- Every row detected before 0.2.2 that routes to bezent's Safe enters review on its first pass, because it has no
  sender. The importer copies senders where the source has them; rows from before 0.2.2 have none, so each needs an
  operator, or a sender backfill from the chain, which is not built.
- Two more tables (intents, alert markers) and one more write before each broadcast.
- The SQLite to Postgres importer refuses to run while an intent is pending.
