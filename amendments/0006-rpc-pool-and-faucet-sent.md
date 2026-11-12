# Amendment 0006 — an RPC pool, backoff on rate limits, and a top-up that was sent is sent

**Status:** ruled. Applied to `SPEC.md` 0.2.5.
**Target:** `SPEC.md` 0.2.4
**Date:** 2026-11-12
**Rationale in one line:** on free public RPCs (publicnode: 1,200 requests a minute per caller IP, shared by both
chains) one 429 threw away a whole 500-block chunk, and a faucet top-up that was broadcast was logged as failed.

## A1 — `rpc_urls`

A `[[chains]]` table MAY carry `rpc_urls = ["...", "..."]` beside `rpc_url`. The pool is `rpc_url` first, then
`rpc_urls`, without repeats; at least one of the two is required. With one URL the provider is built as before (any
scheme alloy accepts) and there is no failover, but A2's backoff and bisection rules apply to it too: that is a change
in behaviour for existing single-URL chains. With several, all are http(s), and `block_offset_from_head` MUST be at
least 10 or the config is refused at load (endpoints lag one another, see A2). `rpc_urls` is a known key (`REQ-CF-1`).

New `REQ-CF-2`: every read and write goes through the pool. A call starts at a rotating endpoint and moves to the next
one when the answer is a 429 or a rate-limit message, a 401, 402, 403 (publicnode's "Archive requests require a
personal token"), 404 or 413, a 5xx or 408, a transport error (a request that outlives 30 s, or a connection that
takes over 10 s, is one), a refusal of the method ("not available on this plan"), or a refusal of the range or size
("limited to 0 - 50 blocks range", "ranges over 10000 blocks", "up to a 10 block range", "Log response size
exceeded"). Each endpoint is tried once per call. A revert or any execution error, and a plain 400, are not failed
over: another endpoint would say the same. A message is a range refusal only if it says so in one of those phrases;
"exceeds" alone is not one ("transfer amount exceeds balance" is a revert), and "invalid block range" (from after
to) is a bad request. Range and size phrases are checked before rate-limit ones, so JSON-RPC code -32005 ("limit
exceeded") is a rate limit only when its message names no range or size (Infura sends "query returned more than 10000
results" under it). A missing method ("method not found", "not supported", "not available on this plan") fails over and
cools that endpoint for that method for 60 s.

- An endpoint that answered 429 cools down for 10 seconds, for every method. A 401, 402, 403, 404 or 413 cools it for
  every method for 60 seconds. A refusal of one method cools that endpoint for that method only, for 60 seconds. A
  range refusal, a 5xx or a transport error causes no cooldown (a range limit depends on the request). Cooled
  endpoints are tried last, not never.
- When every endpoint refuses, the last error comes back, so the caller can tell a rate limit from a bad range.
- `eth_sendRawTransaction` goes through the same path. Identical signed bytes are all that can reach two endpoints.

## A2 — Backoff instead of discarding work

New `REQ-DT-9`:

- A block fetch (catch-up chunk and per-block path) that fails with a 429, 401-404, 413, 5xx or transport error is
  retried with a wait of 0.5 s doubling to 8 s, up to 6 attempts, before the chunk fails. Blocks already fetched are
  kept.
- A `null` block at or below the head being processed is an error, never an empty block: it means the endpoint that
  answered lags the one that gave the head. It is retried the same way (the next attempt goes to the next endpoint)
  and, if it stays null, the chunk fails. The cursor never moves past a block that was not read.
- `eth_getLogs` failing with those errors waits the same way, not 50 ms.
- A range is bisected for a range or size error, after every endpoint of the pool has refused it, and for a 5xx,
  timeout or unreadable answer once the backoff budget is spent (free nodes answer oversized ranges that way). A range
  of one block is not split. A 429 never bisects.

Detection is unchanged: same deposits, same cursor, with or without limits.

Remaining risk, stated plainly: a lagging endpoint may still answer `eth_getLogs` with an empty list for blocks it does
not have yet, and that is indistinguishable from a quiet range. The offset of at least 10 blocks makes it unlikely;
the full answer is F-002b's two-node agreement.

## A3 — Faucet: sent means sent

New `REQ-FC-3`: once `send_transaction` succeeds the top-up counts as sent. The faucet records every top-up it sends,
confirmed or not, per address with its time. While a recorded top-up is in its window (2 minutes once confirmed, 10
minutes while its receipt is unread), a request to fund that address is answered "faucet top-up pending" and nothing
is sent; `ensure_gas` and the native sweep return that as a transient funding error and the sweep waits for a later
pass. A balance that still looks low inside the window (a node lagging the one that confirmed the top-up) is not a
reason to fund again. If the receipt wait errors or times out, the faucet logs a WARN with the transaction hash,
rebuilds its nonce cache, and returns the hash as funded but unconfirmed. If a send answers "already known" or "nonce
too low" (after a failover, another endpoint may have taken the transaction), the faucet logs a WARN and keeps the
record as unconfirmed instead of releasing it. So a pass never funds the same need twice,
and neither do the passes after it, until the window ends.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-CF-2`, `REQ-DT-9`, `REQ-FC-3` |
| Revised | `REQ-CF-1` (`rpc_urls` is a known key) |
| Withdrawn | none |

## What this costs, stated plainly

- A pool spreads load over endpoints but cannot beat a limit that is per caller IP across all of them.
- The faucet's record is in process: a restart inside the window can repeat one top-up.
- A second real need for gas at the same address inside the window waits for it to end (at most 2 minutes once the
  first top-up is confirmed).
- A chain with one URL gains no failover but does gain the backoff and bisection rules above.
- URLs in `rpc_urls` may carry API keys; the pool logs only host names, `main` still logs the first URL as before.
