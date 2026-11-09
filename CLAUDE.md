# CLAUDE.md

Operating instructions for coding agents (and a short statement of the rules for people) working in this repository.
Not documentation: `SPEC.md` is the source of truth. Read it before doing anything substantive.

## Name

The product is **evmhot**, always lowercase in prose (`docs/brand/`).

## What this project is

Bloque's EVM hot wallet for deposits: a deposit address per registration, detection of admitted tokens arriving at
those addresses, sweeps to configured targets, gas from a faucet, and webhooks to the registering service. Its role
ends at the sweep and the webhook: what a consumer does with a swept deposit is out of scope (`SPEC.md` §1.3).

## Where things are

`OVERVIEW.md` (plain digest) · `SPEC.md` (behaviour) · `amendments/` (every change) · `PLAN.md` (how, DEC/F/M) ·
`features/F-NNN-*/` (work) · `notes/RULINGS.md` (the owner's rulings, R1…) · `REFERENCES.md` (outside sources) ·
`STATE.md` (gitignored session state; private links go there, never in tracked files). The method is narthex's
(`REFERENCES.md`); its guard hook is enabled in `.claude/settings.json`.

## Spec-driven development

The chain is `SPEC.md` → `amendments/` → `PLAN.md` → `features/F-NNN-*/` → issue → PR. Changes flow **down**, never up.

- Behaviour is decided in `SPEC.md`. If code, plan and spec disagree, the spec wins.
- `PLAN.md` MUST NOT contradict `SPEC.md`. A technical fact that makes a requirement unsatisfiable is an amendment first.
- Never implement behaviour that is not specified. Never resolve an `OQ-*` by implementing one of its options.
- Work inside one feature directory at a time.

## Traceability

Identifier families are in `SPEC.md` §0. Never renumber; add. Tasks are `T-<feature>-<seq>`. Tests name the `REQ-*`,
`INV-*` or `AC-*` they verify.

## Vocabulary

| Say | Never say | Why |
|---|---|---|
| **deposit address** | user wallet, account wallet | One name; the API's `account_id` field is the deposit address |
| **sweep** / **sweep target** | treasury transfer, forward | A target can be a reserve or a contract, not only a treasury |
| **detection height** | head, tip | It is chosen per network (`REQ-DT-8`) |
| **sweep coordinate** | sweep id, tx hash | Block number, block hash and block-level log index (`REQ-SW-10`) |
| **network** | chain (in prose) | Config and payloads keep `chain` |

## Guardrails

1. **The repository is public.** Never commit hostnames, cloud account ids, secret-store paths, production addresses
   (targets, treasuries, reserves, faucets), database names, counts of production records or incident detail. They
   live in the private devops repository.
2. **Keys never leave the process** (`REQ-KY-3`, `INV-4`): not in logs, responses, the store, webhooks, test fixtures
   or error messages. Test mnemonics are the well-known development ones and say so.
3. **Never change a registered address or reuse a derivation index** (`INV-1`, `INV-3`). The counter starts above the
   highest stored index (`REQ-RG-3`).
4. **Never mark a deposit swept without a successful receipt** (`REQ-SW-5`), except the bounded zero-balance rule
   (`REQ-SW-3`).
5. **Never route a token to a target that does not admit it, or outside its bounds** (`REQ-SW-7`, `REQ-SW-8`).
   USDC.e is not USDC.
6. **Keep the `/evm` HTTP shapes** (`REQ-API-1`). A change to a request or response is an amendment.
7. **No sender lists and no pushing addresses to other systems** (`SPEC.md` §1.3).

## House rules (every agent brief carries these)

- **Commit identity and signing.** Plain `git commit -s`; never pass `-c user.*`, `--author`, set `GIT_AUTHOR_*` or
  `GIT_COMMITTER_*`, or use `--no-gpg-sign`. Use the identity and signing key the repository's or the user's git
  configuration already has. If signing fails, stop and ask; do not fall back to an unsigned commit.
- **DCO.** Every commit carries `Signed-off-by:` (`git commit -s`).
- **Builds.** `CARGO_TARGET_DIR` lives outside the checkout (for example `~/.cargo-target`). Remove any private target
  directory you create. Commit named paths only; never `git add -A`.
- **Scope.** Never edit `SPEC.md` or `PLAN.md` unless the task says so. On a spec gap, stop and propose an amendment.
- **Merging.** Only when the task says so, with every required check green on the exact head.
- **Tracking.** One GitHub issue per feature (title `F-NNN · name`). Every pull request names its issue.
- **Secrets.** Never commit a secret. Workflows reference secrets only as `${{ secrets.NAME }}`; tests pass on public
  fallbacks.
- **Production.** No production change and no fund movement without the owner's explicit approval for that step.
- **Questions.** Never ask the owner in chat or with a dialog. Questions go on the decision form; a reversible choice
  proceeds on the recommended default (R11).

## Decisions

Rulings are numbered in `notes/RULINGS.md` and applied through `amendments/`, citing the decision they apply; @pandres95 keeps the working decision form
outside this repository. Never apply an unruled decision, not even provisionally.

## Conventions

British English in `SPEC.md`, the plan and amendments. RFC 2119 keywords in normative text only. State trade-offs plainly.
