# Amendment 0004 — sweep intents, review, one sweep per transfer

**Status:** ruled. Applied to `SPEC.md` 0.2.3, including the sweep-sending redesign (PR #17): the never-drop,
same-nonce rule and the reservation rule.
**Target:** `SPEC.md` 0.2.2
**Date:** 2026-11-09
**Rulings applied:** R13 (one sweep per inbound transfer). The rest is an engineering change, evmhot, found by the
reviews of F-002a (PR #17).
**Rationale in one line:** a sweep that may already be on chain must be known before it is broadcast, and a deposit
whose settlement evmhot cannot prove must stop and wait for an operator, never be settled by guesswork.

## A1 — One signed transaction per deposit address, and a sweep intent before every broadcast

New `REQ-SW-13`: an address has at most one signed, unresolved sweep transaction at a time, across every token and
the native coin, because nonces belong to the address. Every sweep, the native coin included, goes through a sweep
intent:

1. Before anything is signed, evmhot records the intent: the deposits it covers, the target, the part and the amount.
   It then signs the transaction locally with an explicit nonce (the address's pending nonce, and above the nonce of
   any intent of the address that landed), stores the nonce, the transaction hash and the signed bytes on the intent,
   and broadcasts. The store refuses a second open intent for an address. A broadcast that fails leaves the intent open
   with its bytes. No receipt is awaited inside a pass: the pass for an address ends at its first send, and the receipt
   is read by a later pass.
2. At the start of every pass the address's open intent is resolved, whatever its deposits' status, before anything
   else is planned for the address. **An intent keeps its nonce for good: once signed it is never dropped.**
   - A receipt for one of its hashes settles it (success) or fails it (revert; REQ-SW-4's failure count applies). At
     most one of its transactions can land: they share the nonce.
   - With no receipt, while the address's mined nonce has not passed the intent's, the newest signed bytes are
     broadcast again. If that transaction is underpriced (its max fee is below the current estimate) and the intent
     has waited 10 passes, evmhot signs another transaction at the same nonce with both fee fields raised by at least
     10% (or to the current estimate, whichever is higher), stores its hash next to the earlier ones, and broadcasts
     it. A transaction held for other reasons is only rebroadcast.
   - With no receipt for any stored hash and the nonce used for 64 blocks, the nonce was used by something outside
     evmhot, or a receipt is late: the deposits go to review (`A2`) and the intent stays watched. For 5,000 blocks from that
     verdict every pass reads its receipts, each intent on its own so one bad hash never stops the network; a late
     receipt that matches records the sweep and releases the deposits, with that address and token's
     `balance_shortfall` deposits, in one transaction (`kind=review_self_resolved`). An operator retry is refused
     meanwhile, and afterwards re-reads every stored hash before releasing anything. The verdict counts blocks, not passes, because a load-balanced
     provider can serve the nonce from a node ahead of the one serving receipts.
   - A transaction unmined with its nonce free raises `kind=sweep_stuck` once after 30 minutes.
   - A native sweep estimates its gas. A failed estimate or a revert counts toward `REQ-SW-4`'s 5 failures; the fifth
     sends the native deposits to review (`kind=native_sweep_failed`).
   - Only an intent with no signed transaction at all (the process stopped before signing) is dropped: nothing exists
     to wait for.

New `INV-7`: no deposit is marked swept (by a receipt or by the zero-balance rule, `REQ-SW-3`) or released by an
operator while a sweep intent covering it is pending. A deposit is settled only by its recorded sweeps, once nothing
is left owed.

`REQ-SW-3` applies only to a group with no recorded sweep and no intent other than a failed, dropped or
closed-by-operator one.

## A2 — Deposits that need review

New `REQ-SW-14`: a token or native deposit enters `needs_review`, and is not swept again until an operator releases it,
when:

- its sweep's receipt succeeded but holds no `Transfer` from the deposit address to the target, or holds one whose
  amount differs from the intent (`kind=sweep_review`);
- its sweep's nonce was used by another transaction (`kind=sweep_review`);
- for a `REQ-SW-12` or `REQ-SW-15` target, the deposit address holds less of the token than is still owed on it, that
  is, than the live deposits owe plus what deposits in review or failed on the pair are still owed, which is reserved
  (`kind=balance_shortfall`). A sweep is never capped to the balance;
- for such a target, the deposit has no recorded sender, as rows detected before 0.2.2 do (`kind=no_sender`).

For a target that sweeps the whole balance, any deposit on the (address, token) pair in `needs_review` or `failed`
blocks the pair, because its tokens cannot be told apart from the rest (`kind=pair_blocked`, naming the blocking
deposit). A sweep therefore never moves tokens that belong to a deposit waiting for a person. For `REQ-SW-12` and
`REQ-SW-15` targets the amounts of such deposits are reserved instead, so the pair does not stay blocked: a deposit with
no sender, or one waiting for a person, does not hold up the others. A sweep still in flight is not a block: the
address simply sends nothing else until it resolves.

## A3 — One sweep per inbound transfer

New `REQ-SW-15`: a target MAY declare `per_transfer`. Each inbound deposit is swept on its own, never with another,
even from the same sender. A sweep sends the lesser of what that deposit is still owed and the address's balance; a
balance below what the pair owes is `A2`'s shortfall. A deposit above the target's maximum is swept in parts, all for
that deposit. A `per_transfer` target whose minimum is above the token's detection minimum is refused at load, because
a deposit between the two could never be swept (`REQ-DT-4`); so is a target that declares both `per_transfer` and
`per_sender`. bezent's Safe is a `per_transfer` target (R13). A deposit above the target's maximum is split evenly: each part is
`ceil(remaining / ceil(remaining / max))`, recomputed after each part. Its parts are then at least `(max + 1) / 2`, so
a `per_transfer` target whose minimum and maximum satisfy `2 * min > max + 1` is refused at load, because a part could
fall below the minimum.

## A4 — Splits and dust

Revise `REQ-SW-7`: a split sends one part per pass for an address (`REQ-SW-13`), reading the balance again each pass,
so a long split of one token holds back the address's other tokens and its native sweep until it ends. When the last
part would be below the minimum, the part is cut so the tail is exactly the minimum, if both fit; otherwise the tail is
dust. A balance below the minimum stays owed: the deposit is not settled until later deposits lift it (`kind=dust`,
raised once).

## A5 — Operator release

Revise §9.2: `POST /admin/retry_sweeps` moves a deposit in `needs_review` or `failed` back to `detected`, in one
transaction, for token and native deposits. It refuses while an intent covering the deposit is pending, naming the
transaction. It **closes** the deposit's `review` intents (`closed_by_operator`); it never re-opens one. From then on
what is owed is computed from the recorded sweeps alone. If the reviewed sweep had in fact moved the funds and no sweep
is recorded, the address is empty and `REQ-SW-3` settles the deposit after its 10 passes with no `deposit_swept`;
retrying such a review is the operator's word that nothing more is owed. That settlement raises
`kind=settled_without_sweep` once, so it is never silent.

## A6 — Alerts

New `REQ-OB-4`: an alert is a log line that starts with `EVMHOT_ALERT kind=<kind> network=<network>` and carries the
deposit's identity and transaction where there are some. Kinds: `sweep_review`, `balance_shortfall`, `no_sender`,
`pair_blocked` (review or failed blockers only), `dust`, `settled_without_sweep`, `native_sweep_failed`,
`review_self_resolved` and `sweep_stuck`; `lag` and `provider` come with F-002b. `dust` and `pair_blocked` are raised once per
deposit and blocking state; later passes log quietly. Metric filters match the marker and the kind.

## A7 — Where the target is recorded

Revise `REQ-SW-10`: each sweep also records its target address, and each `sweeps` entry in `deposit_swept` carries it
as `sweep_target`.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SW-13`, `REQ-SW-14`, `REQ-SW-15`, `REQ-OB-4`, `INV-7`, `INV-8` |
| Revised | `REQ-SW-3`, `REQ-SW-4`, `REQ-SW-7`, `REQ-SW-10`, §9.2 |
| Withdrawn | none |

## What this costs, stated plainly

- A deposit in review waits for a person; on a whole-balance target so does every other deposit on its address and
  token. Until then the funds
  sit in the deposit address and no `deposit_swept` is sent.
- A row detected before 0.2.2 that routes to bezent's Safe has no sender and enters review. On 2026-11-09 production
  held none: every Polygon native USDC deposit in the store was already swept.
- Two more tables (intents, alert markers) and one more write before each broadcast. Sweeps wait for the earlier one on
  the same address: a long split, or a held transaction, delays the address's other tokens and its native sweep.
- A transaction held for a lower fee is repriced at its nonce; both versions can be mined, only one can land.
- The SQLite to Postgres importer refuses to run while an intent is pending.
