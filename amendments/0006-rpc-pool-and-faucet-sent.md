# Amendment 0006 — an RPC pool, backoff on rate limits, and a top-up that was sent is sent

**Status:** ruled. Applied to `SPEC.md` 0.2.5.
**Target:** `SPEC.md` 0.2.4
**Date:** 2026-11-12
**Rationale in one line:** on free public RPCs (publicnode: 1,200 requests a minute per caller IP, shared by both
chains) one 429 threw away a whole 500-block chunk, and a faucet top-up that was broadcast was logged as failed.

## A1 — `rpc_urls`

A `[[chains]]` table MAY carry `rpc_urls = ["...", "..."]` beside `rpc_url`. The pool is `rpc_url` first, then
`rpc_urls`, without repeats; at least one of the two is required. With one URL nothing changes (any scheme alloy
accepts). With several, all are http(s). `rpc_urls` is a known key (`REQ-CF-1`).

New `REQ-CF-2`: every read and write goes through the pool. A call starts at a rotating endpoint and moves to the next
one when the answer is a 429, a rate-limit message, a 403 (publicnode's "Archive requests require a personal token"),
a 5xx, a transport error, a refusal of the method ("not available on this plan"), or a refusal of the range or size
("limited to 50 blocks", "ranges over 10000 blocks", "Log response size exceeded"). Each endpoint is tried once per
call. A revert or bad parameters are not failed over: another endpoint would say the same.

- An endpoint that answered 429 cools down for 10 seconds, for every method. A refusal of one method cools that
  endpoint for that method only, for 60 seconds. A range refusal, a 403 or a 5xx causes no cooldown (a range limit
  depends on the request). Cooled endpoints are tried last, not never.
- When every endpoint refuses, the last error comes back, so the caller can tell a rate limit from a bad range.
- `eth_sendRawTransaction` goes through the same path. Identical signed bytes are all that can reach two endpoints.

## A2 — Backoff instead of discarding work

New `REQ-DT-9`:

- A block fetch (catch-up chunk and per-block path) that fails with a 429, 403, 5xx or transport error is retried with
  a wait of 0.5 s doubling to 8 s, up to 6 attempts, before the chunk fails. Blocks already fetched are kept.
- `eth_getLogs` failing with those errors waits the same way, not 50 ms.
- A range is bisected only when the error is about the range or the response size, and only after every endpoint of
  the pool has refused it. A 429, 403 or 5xx never bisects.

Detection is unchanged: same deposits, same cursor, with or without limits.

## A3 — Faucet: sent means sent

New `REQ-FC-3`: once `send_transaction` succeeds the top-up counts as sent. If the receipt wait then errors or times
out, the faucet logs a WARN with the transaction hash, rebuilds its nonce cache, and returns the hash as funded but
unconfirmed. The sweeper re-reads the balance as it does after any top-up; if it is still short it waits for the next
pass (a transient funding error). The faucet remembers the unconfirmed top-up per address for 10 minutes and, while
its receipt cannot be read, answers a new request for that address with "faucet top-up pending" instead of sending
another. So a pass never funds the same need twice, and neither do the passes after it.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-CF-2`, `REQ-DT-9`, `REQ-FC-3` |
| Revised | `REQ-CF-1` (`rpc_urls` is a known key) |
| Withdrawn | none |

## What this costs, stated plainly

- A pool spreads load over endpoints but cannot beat a limit that is per caller IP across all of them.
- The faucet's "pending" memory is in process: after a restart a still-unconfirmed top-up can be funded again once.
- URLs in `rpc_urls` may carry API keys; the pool logs only host names, `main` still logs the first URL as before.
