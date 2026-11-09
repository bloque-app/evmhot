# F-002 · Fixes before any restart

**Implements:** amendment 0001 · `REQ-SW-5`, `REQ-SW-7` to `REQ-SW-10`, `REQ-WH-4`, `REQ-DT-8`, `REQ-OB-1`,
`REQ-OB-2`, `REQ-API-2`, `INV-5`, `INV-6` · `PLAN.md` DEC-4, DEC-5, DEC-6.

## Goal

Nothing that failed silently before can fail silently again, and a consumer that verifies sweeps on chain gets what
it needs.

## Scope

- Mark swept only on a successful receipt; a reverted sweep is a failure (`REQ-SW-5`).
- Record and report the sweep coordinate and amount (`REQ-SW-10`, `REQ-WH-4`); new nullable columns, so old rows stay valid.
- Detection height per network: `finalized`, `safe` or offset (`REQ-DT-8`).
- Target bounds, chunking above the maximum, holding below the minimum (`REQ-SW-7`); single-token targets checked at
  load (`REQ-SW-8`).
- Lag metric and alert; back-off and alert on rate-limit and quota answers (`REQ-OB-1`, `REQ-OB-2`).
- Steady-state detection by one ranged log query per poll; the native scan behind a per-network switch (DEC-4).
- Bearer token on `POST /evm/block_number` (`REQ-API-2`). Withdraw the symbol-length filter (amendment 0001, A9).

## Done when

Tests named for each requirement above pass, including a reverted sweep, a balance over the maximum, a balance under the
minimum, a misrouted token refused at load, and a provider answering 429.
