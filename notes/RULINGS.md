# Rulings

@pandres95's rulings that bind evmhot, numbered in the order given. Each cites the decision-form item behind it (the
form lives outside this repository). Amendments cite these numbers.

| # | Date | Ruling | Form item |
|---|---|---|---|
| R1 | 2026-11-08 | No sender list or registrar for bezent; a sweep never pushes addresses elsewhere | relayed by bezent's maintainers |
| R2 | 2026-11-09 | Build all of it: spec, code into `main`, pre-restart fixes, devops repository, deployment | `d-go-build` |
| R3 | 2026-11-09 | Clients reach evmhot on a new internal hostname; their owners change the base URL at cutover | `d-hostname` |
| R4 | 2026-11-09 | On restart, scan the detection gap offline first, then move the cursor | `d-gap` |
| R5 | 2026-11-09 | Dev and test on public RPC endpoints; production on keys with known caps, evmhot's own when shared keys can't take the load | `d-rpc` |
| R6 | 2026-11-09 | Snapshot the store first, then move evmhot to a database of its own, keeping every row | `d-ledger-db` |
| R7 | 2026-11-09 | A refund goes to the original sender of the inbound transfer, never to the deposit address | bezent's `d-refund-dest-swaps` |
| R8 | 2026-11-09 | Polygon native USDC sweeps to bezent's reserve | `d-usdc-sweep` |
| R9 | 2026-11-09 | Polygon USDT0 and Base USDC also go to bezent's reserve; until bezent admits them they stay in their deposit addresses | `d-other-targets` |
| R10 | 2026-11-09 | The previous USDC sweep target, an exchange deposit address, is retired | `d-usdc-target` |
| R11 | 2026-11-09 | Questions to the owner go on the decision form only; reversible choices proceed on the recommended default | Pablo, all sessions |
