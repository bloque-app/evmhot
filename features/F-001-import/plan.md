# F-001 · Import the code that ran

**Implements:** `PLAN.md` DEC-1, DEC-2, DEC-3 · `SPEC.md` 0.2.0, every requirement not marked (not built), and
`REQ-API-1`, `REQ-RG-3`.

## Goal

This repository's `main` builds the service that ran in production, as a standalone binary with the same HTTP
surface its clients use, and CI publishes its image.

## Scope

- Replace `src/` with the crate that ran: multi-network config (`chains.toml`), Postgres store and migrations, the
  outbox worker, the batched catch-up, per-token sweep targets, and route logging at boot.
- A standalone server that serves `SPEC.md` §9.1 under `/evm` with the host service's exact shapes (health as
  `{status, enabled}`; `block_number` on the default network), and the admin routes of §9.2.
- The counter seed: on an existing store with no counter, seed `MAX(derivation_index) + 1`, never 0 (`REQ-RG-3`).
- Dockerfile, CI (fmt, clippy, tests against Postgres), image publishing to GHCR.
- Close the stale pull requests that led to the vendored copy as superseded.

## Out of scope

Every behaviour change (F-002). Deployment (F-003).

## Done when

- Tests named for `REQ-RG-1` to `REQ-RG-3`, `REQ-DT-1` to `REQ-DT-6`, `REQ-SW-1` to `REQ-SW-4`, `REQ-WH-1` to
  `REQ-WH-5` and `REQ-API-1` pass in CI.
- A test seeds the counter from a store that holds a hashed index near 2^31 and never allocates an index at or below it.
- The image builds and runs against a local Postgres with public RPCs.
