# Amendment 0005 — a second delivery of deposit_swept, and its published contract

**Status:** ruled. Applied to `SPEC.md` 0.2.4.
**Target:** `SPEC.md` 0.2.3
**Date:** 2026-11-09
**Rulings applied:** R12 (on each sweep into bezent's Safe, evmhot calls the swap service, which registers it at
bezent; evmhot never calls bezent), and Swaps' `d-evmhot-auth` (a bearer token held in Vault for both services).
**Rationale in one line:** swap needs each Safe sweep's coordinate and the original sender, which `deposit_swept`
already carries, so evmhot delivers that same event to swap instead of inventing a second one.

## A1 — Notify URL per target

New `REQ-WH-10`: a sweep target MAY declare `notify_url` and `notify_token_env`, the name of the environment variable
holding its bearer token. Each token `deposit_swept` for a deposit settled into that target is also delivered to
`notify_url`:

- the same body as the registration's webhook;
- its own outbox row, keyed `(id, "deposit_swept:notify")`, written in the same transaction (`REQ-WH-7`);
- `Authorization: Bearer <token>` with the target's token, never the webhook token;
- the delivery rules of `REQ-WH-5`: at least once, retried, leased; retried for at least two hours (swap may be
  redeployed meanwhile), then `EVMHOT_ALERT kind=notify_failed`, and `POST /admin/retry_webhooks` with event
  `deposit_swept:notify` resends it.

evmhot refuses to start when a target has a `notify_url` and its token variable is empty. A failing notify delivery
never holds up the registration's webhook, nor the other way round.

In production the Safe target notifies `{swap}/api/swap/_internal/busdc/deposit-swept` with a JWT of scope
`busdc.intake` (`EVMHOT_SWAP_NOTIFY_TOKEN`). swap answers 2xx even when it parks a sweep it can't act on.

## A2 — The published contract

New `REQ-WH-11`: the token `deposit_swept` is published in `contracts/deposit_swept/` as a JSON Schema with fixtures
and a changelog, versioned. A test builds the payload with evmhot's own code and checks it against the schema.
Consumers ignore unknown fields; an additive change bumps the minor version, any other the major. Version 1.0.0
covers Polygon and Base alike.

## A3 — Unknown configuration keys

New `REQ-CF-1`: a `[[chains]]` table with a key evmhot does not know is refused at load. The production template had
carried `detection` and `scan_native`, which this version silently ignored.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-WH-10`, `REQ-WH-11`, `REQ-CF-1` |
| Revised | none |
| Withdrawn | none |

## What this costs, stated plainly

- One more delivery per Safe sweep, and one more token in evmhot's secret.
- swap trusts nothing in the body for money: it re-reads the receipt over its own node before registering at bezent.
