# F-004 · Directed-deposit targets

**Implements:** `REQ-SW-6` (the `directed_deposit` kind) · `PLAN.md` DEC-6.

## Goal

A sweep into bezent's directed-deposit contract names its recipient on chain, in one transaction from the deposit
address.

## Scope

- Config: contract address and 32-byte recipient per network and token.
- The sweep calls `depositWithPermit` (falling back to the allowance it set) with the swept amount, within the
  contract's bounds; the `Transfer` before the contract's event is the sweep coordinate.

Waits on bezent's contract. The interface is taken from its released source, not from this plan.
