# evmhot — Implementation Plan

**Plan version:** 0.1.0 · **Implements:** `SPEC.md` 0.2.0

## 0. How to read this document

`SPEC.md` says what evmhot does; this plan says how it is built, run and deployed, and in what order. It MUST NOT
contradict the spec. Technology and network names appear here, not in the spec. Private details (hostnames, accounts,
addresses, credentials) live in the private devops repository, never here.

## 1. Shape of the system

- One Rust crate with two products: the `evmhot` **library** (the wallet service a host program embeds) and the
  `evmhot` **binary**, a self-contained service around it. From this iteration on, the service is the primary way evmhot
  runs; the library stays embeddable and is what the service is built from.
- `alloy` for chain access, `axum` for HTTP, Postgres for the store.
- One process serves every configured network. Each network runs a monitor task and a sweeper task. A webhook worker
  task delivers the outbox.
- Networks today: Polygon and Base. Admitted tokens per network come from configuration.
- Consumers today: Bloque's Mediums service (registers parties, receives webhooks, starts the downstream route on
  `deposit_swept`) and Bloque's Swaps service (`/evm/verify_transfer`). bezent, Bloque's bridge, receives Polygon
  USDC sweeps into its reserve and verifies them by coordinate.

## 2. Decisions

### DEC-1 Source of truth for the code

The code that ran in production was a copy of evmhot vendored into another Bloque service, with two further changes
(per-token sweep targets, and route logging at boot). That copy, not the old `main`, is the baseline. F-001 brings it
into this repository; the stale branches and pull requests that led to it are closed as superseded.

### DEC-10 Library and service

The crate keeps a library API (start the service, register, verify a transfer, read and move a cursor, retry) with no
HTTP in it. The binary adds the HTTP surface of `SPEC.md` §9, configuration loading, health and metrics, and graceful
shutdown. A host that embeds the library serves its own routes; the service's routes are not the library's concern.

### DEC-2 Store

Postgres. The schema is the one that ran (registrations, deposits, token deposits, sweep bookkeeping, outbox,
state). The derivation counter is seeded above the highest stored index (`REQ-RG-3`); F-001 makes the Postgres
backend's seed do so. evmhot moves to a database of its own, keeping every row (`REQ-ST-2`); the old one
is snapshotted first.

### DEC-3 HTTP surface

The standalone binary serves the `/evm/*` routes with the exact shapes the host service exposed (`REQ-API-1`), plus
the admin routes. Clients point at a new internal hostname; their configuration change is done by their owners at
cutover.

### DEC-4 Chain access and providers

HTTP polling only. Development and test use public RPC endpoints. Production uses provider keys with known caps;
each network has its own key, so one busy network cannot exhaust another's quota. Steady-state detection uses one
ranged log query per poll rather than per block, and the native-coin scan (one full block fetch per block) is a
per-network switch, because it dominates the provider load.

### DEC-5 Finality

Polygon detects on `finalized` (measured about 4 s behind the newest block). Base detects on `safe` (its
`finalized` trails by about 15 minutes), or newest-minus-offset where `safe` is unavailable (`REQ-DT-8`).

### DEC-6 Sweep targets

Configured per network and token in `chains.toml`. Polygon native USDC goes to bezent's reserve, a `transfer` target
that admits only that token, needs a refund address per sweep, and has bezent's per-transfer bounds (`REQ-SW-7`,
`REQ-SW-8`, `REQ-SW-12`). Polygon USDT0 is held in its deposit addresses until bezent admits it into the same reserve.
Base USDC is held until a bridge leg to Polygon exists (R6). `directed_deposit` (F-004) follows bezent's contract.

### DEC-7 Keys

Two mnemonics, read from the environment at start: deposit and faucet. In production they come from the secret
store through the deployment, never from files in the image. They are the same mnemonics that derived every existing
address (`REQ-KY-4`).

### DEC-8 Deployment

A container image built in CI and published to the GitHub container registry. It runs as its own service on Bloque's
container platform, defined in the private devops repository and Bloque's infrastructure repository. Restarting
production follows a runbook in the devops repository, one approved step at a time.

### DEC-9 The detection gap

Before a restart that finds a cursor far behind, the gap is scanned offline (token logs only, read-only), its
deposits are listed, and only then is the cursor moved to the present by an operator. Nothing replays into clients
unannounced.

## 3. Dependencies

| What | From | Needed by |
|---|---|---|
| Provider keys and caps for production | Bloque operations | Production restart |
| Secret-store path with the existing mnemonics | Bloque operations | F-003 |
| Clients' base-URL change | Mediums and Swaps owners | Cutover |
| bezent's directed-deposit contract | bezent | F-004 |

## 4. Features

| Feature | What | Requirements |
|---|---|---|
| F-001 import | The code that ran, as this repository's `main`: the crate, a standalone `/evm/*` server, the counter seed fix, Dockerfile, CI, image | `REQ-API-1`, `REQ-RG-3` and every requirement not marked (not built) |
| F-002 pre-restart fixes | Sweep outcome, sweep coordinate, senders and per-sender sweeps, finality per network, target bounds and admission, atomic outbox, separate tokens, counter ceiling, lag and provider alerts with back-off, ranged steady-state logs, operator route auth, provider health, withdraw the symbol filter | `REQ-SW-5`, `REQ-SW-7` to `REQ-SW-10`, `REQ-SW-12`, `REQ-WH-4`, `REQ-WH-7` to `REQ-WH-9`, `REQ-RG-5`, `REQ-DT-8`, `REQ-DT-10`, `REQ-OB-1` to `REQ-OB-3`, `REQ-API-2`, `INV-5`, `INV-6` |
| F-003 self-hosting | Image pins, deploy definitions and runbook in the devops repository; the service, task role, DNS and routing in the infrastructure repository; the store move | DEC-2, DEC-7, DEC-8, DEC-9 |
| F-004 directed deposit | `directed_deposit` targets | `REQ-SW-6` (second kind) |

## 5. Milestones

- **M-1 Spec-driven.** This plan, the spec and the features merged.
- **M-2 Code in main.** F-001 merged; CI builds and publishes the image.
- **M-3 Ready to restart.** F-002 merged; F-003's definitions reviewed.
- **M-4 Running.** Production restarted by the runbook; clients cut over.
- **M-5 Directed deposits.** F-004, when bezent's contract ships.

## 6. Risks

- **Losing the store** strands addresses (`REQ-ST-2`). Snapshot before every migration; verify row counts after.
- **Provider quota** silently stopping detection. Mitigated by DEC-4 and `REQ-OB-1`/`REQ-OB-2`.
- **Misrouted tokens** into a reserve that cannot refund them automatically. Mitigated by `REQ-SW-8` at load time.
- **A wrong counter seed** reallocating an index. Mitigated by `REQ-RG-3` and a test that seeds from a store with
  hashed indices.

## 7. What to do first

F-001, then F-002, then F-003. F-004 waits on bezent.
