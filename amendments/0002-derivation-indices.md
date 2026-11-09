# Amendment 0002 — derivation indices above 2^31

**Status:** ruled (engineering correction, evmhot). Applied to `SPEC.md` 0.2.1.
**Target:** `SPEC.md` 0.2.0
**Date:** 2026-11-09
**Rationale in one line:** the store already holds indices above 2^31, so a ceiling at 2^31 would refuse every new
registration; the wallet library derives those indices deterministically as hardened children.

## A1 — Index range

Revise `REQ-KY-1`: the index is a raw 32-bit child number, and one at or above 2^31 is a hardened child at that
number (the library parses `m/44'/60'/0'/0/<n>` with `n` as a `u32`). Revise `REQ-RG-5`: the ceiling is 2^32, not 2^31.

## A2 — `register`'s response

Correct §9.1: the host omitted `funding_tx` rather than sending null.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | none |
| Revised | `REQ-KY-1`, `REQ-RG-5` |
| Withdrawn | none |

## What this costs, stated plainly

Nothing new: this describes what already ran. Hardened children at those numbers stay the only way to re-derive those
addresses, so `REQ-KY-4` (never change the mnemonic) and `REQ-ST-2` (keep the store) matter as before.
