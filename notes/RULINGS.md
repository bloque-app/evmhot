# Rulings

@pandres95's rulings that bind evmhot, numbered in the order given. Each cites the decision-form item behind it (the
form lives outside this repository). Amendments cite these numbers.

| # | Date | Ruling | Form item |
|---|---|---|---|
| R1 | 2026-11-08 | No sender list or registrar for bezent; a sweep never pushes addresses elsewhere. *The registrar part is replaced by R12.* | relayed by bezent's maintainers |
| R2 | 2026-11-09 | Build all of it: spec, code into `main`, pre-restart fixes, devops repository, deployment | `d-go-build` |
| R3 | 2026-11-09 | Clients reach evmhot on a new internal hostname; their owners change the base URL at cutover | `d-hostname` |
| R4 | 2026-11-09 | On restart, scan the detection gap offline first, then move the cursor | `d-gap` |
| R5 | 2026-11-09 | Dev and test on public RPC endpoints; production on keys with known caps, evmhot's own when shared keys can't take the load. *Production part replaced by R16.* | `d-rpc` |
| R6 | 2026-11-09 | Snapshot the store first, then move evmhot to a database of its own, keeping every row | `d-ledger-db` |
| R7 | 2026-11-09 | A refund goes to the original sender of the inbound transfer, never to the deposit address | bezent's `d-refund-dest-swaps` |
| R8 | 2026-11-09 | Polygon native USDC sweeps to bezent's reserve | `d-usdc-sweep` |
| R9 | 2026-11-09 | Polygon USDT0 and Base USDC also go to bezent's reserve; until bezent admits them they stay in their deposit addresses | `d-other-targets` |
| R10 | 2026-11-09 | The previous USDC sweep target, an exchange deposit address, is retired | `d-usdc-target` |
| R11 | 2026-11-09 | Questions to the owner go on the decision form only; reversible choices proceed on the recommended default | Pablo, all sessions |
| R12 | 2026-11-09 | evmhot registers each sweep into bezent's Safe at bezent itself (`POST /v0/deposits`), not Mediums or Swaps | relayed by Bezent |
| R13 | 2026-11-09 | One sweep per inbound transfer: "one single hot wallet routes to one single user" | relayed by Bezent |
| R14 | 2026-11-09 | bUSDC is the only route; DUSD "no longer exists, and never will again" | relayed by the Standup Agent |
| R15 | 2026-11-09 | bezent's DirectedDeposit is the on-chain router on Polygon; other networks don't use it | `d-router-directed` |
| R16 | 2026-11-09 | Production runs on public RPC endpoints; safety comes from two independent nodes agreeing on one outcome | `a-rpc-keys` |
| R17 | 2026-11-09 | Base USDC reaches Polygon over CCTP; who sends it (Swaps from a holder, or each deposit address burning to the Safe) is open on bezent's form | `d-cctp-who`, `a-base-holder` |
| R18 | 2026-11-09 | Deposit addresses stay key-derived, so each is the same address on every network; on-chain sweeping uses EIP-7702 delegation, not contract addresses | chat |
