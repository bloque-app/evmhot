# Amendment 0007 — alert when a faucet runs low

**Status:** ruled. Applied to `SPEC.md` 0.2.6.
**Target:** `SPEC.md` 0.2.5
**Date:** 2026-11-23
**Rulings applied:** the owner's queue item `q-faucet-alert` (decision form, 23 Nov), on the self-paced default
`d-self-paced`; promised to the owner and to Swaps on 14 Nov.
**Rationale in one line:** every sweep's gas comes from the faucet, so an empty faucet stops every sweep on its
network, and nothing said so before it happened.

## A1 — `faucet_low`

New `REQ-FC-4`: per network, evmhot reads the faucet's balance every 5 minutes, from start. When it is below the
network's floor, evmhot raises `EVMHOT_ALERT kind=faucet_low network=<network> deposit=- tx=-`, naming the faucet,
its balance, the floor and how many whole top-ups the balance still pays for. It raises it on the first low reading
and again every hour while the balance stays low. A reading at or above the floor after an alert is logged at INFO
and closes the spell, so the next drop alerts at once. A failed read is a WARN, not an alert: a provider that keeps
failing is `provider`'s (`REQ-OB-2`).

The floor is the chain table's `faucet_low_balance`, in wei, which defaults to 20 top-ups (`existential_deposit` × 20).
A floor of zero, or a value that is not an amount in wei, is refused at load. `faucet_low_balance` is a known key
(`REQ-CF-1`).

The check only reads. It never changes what the faucet funds, and a sweep's own "faucet has insufficient balance"
error is unchanged.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-FC-4` |
| Revised | `REQ-OB-4` (kind `faucet_low`), `REQ-CF-1` (`faucet_low_balance` is a known key) |
| Withdrawn | none |

## What this costs, stated plainly

- One balance read per network every 5 minutes, through the pool.
- The state is in process: a restart while the balance is low alerts again at once.
- The alert is a log line; it reaches a person only through whatever watches `EVMHOT_ALERT` lines.
- The default floor counts top-ups of `existential_deposit`; a network whose top-ups cost more needs its own
  `faucet_low_balance`.
