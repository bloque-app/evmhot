# F-003 · Self-hosting

**Implements:** `PLAN.md` DEC-2, DEC-7, DEC-8, DEC-9.

## Goal

evmhot runs as its own production service, from its own image, with its own store, restarted by a runbook.

## Scope

- Private devops repository: image pins, deploy definitions, intake of new images, the cutover runbook.
- Infrastructure: the service, its task role, internal DNS and routing, the secret-store path.
- The store move to evmhot's own database, after a snapshot, with row counts checked.
- The runbook: the offline gap scan, the cursor move, the clients' base-URL change, the first test deposit. Each
  production step is approved on its own.

The offline gap scan exists: the `evmhot-gapscan` binary (see the README, "Gap scan"). It is a dry run by default,
reads balances at one block through Multicall3, fetches logs only for addresses that hold tokens, and reports any
balance the transfers do not explain. `--apply` stores the deposits and `--set-cursor` moves the cursor, as separate
steps, each approved on its own.

Details that name hosts, accounts or addresses stay in the private repository.
