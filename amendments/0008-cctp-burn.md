# Amendment 0008 — sweep Base USDC over CCTP to the Polygon reserve

**Status:** proposed, applied to `SPEC.md` 0.2.8 as **(not built)**, on the default of the owner's decision form row
`d-cctp-evmhot` (26 Nov); the owner can overrule it there.
**Target:** `SPEC.md` 0.2.7
**Date:** 2026-11-26
**Rulings applied:** the owner's question "why isn't evmhot directly sweeping into the CCTP?" (26 Nov), Swaps' ask of
26 Nov to take the Base → Polygon leg, and the owner's answer to Swaps' `d-evm-one-key` ("one key, faucet funds it").
This replaces the plan of 9 Nov (Swaps holds Base USDC and bridges it; `q-base-forwarder`), which Swaps withdrew.
**Rationale in one line:** Base USDC only becomes reserve once it reaches Polygon, and evmhot already holds the key
that can burn it and the faucet that pays the gas, so one more hop through another party's wallet adds custody and
nothing else.

## A1 — the `cctp_burn` target kind

`REQ-SW-6` gains a fourth kind, `cctp_burn`: a sweep that burns the token on its network through Circle's CCTP V2 and
mints it to a recipient on another configured network (the **destination**). Its configuration:

| Key | Meaning |
|---|---|
| `token_messenger` | CCTP V2 `TokenMessengerV2` on this network |
| `message_transmitter` | CCTP V2 `MessageTransmitterV2` on the destination |
| `destination` | The destination network's name; MUST be a configured network |
| `source_domain`, `destination_domain` | CCTP domain ids |
| `mint_recipient` | The address the mint pays on the destination |
| `attestation_url` | Circle's attestation API base URL |
| `min_finality_threshold` | Default 2000 (standard transfer) |
| `max_fee` | In the token's minor units; default 0 |

At load: the destination is configured, the recipient is a non-zero address, and the network has at least two RPC
endpoints (`REQ-SW-17`). Before the first burn of a process, evmhot reads each side's `localDomain` and the
messenger's minter, and its per-message burn limit for the token. A domain that differs from the configuration, or a
target `max` (`REQ-SW-7`) above the burn limit, stops burns on that target and raises `cctp_config`.

A `cctp_burn` target MAY be `per_transfer` (`REQ-SW-15`), and the reserve's is: each burn covers one deposit, or one
part of it.

The mint's `Transfer` from the zero address to the recipient is the transfer into the target for `REQ-SW-9` and the
sweep coordinate (`REQ-SW-10`), on the destination network.

Only standard transfers with `max_fee` 0 are built. A fee rule for fast transfers is Swaps' to set later.

## A2 — the burn

New `REQ-SW-16`. A `cctp_burn` sweep is, from the deposit address:

1. once per address and token, an `approve` of the messenger for the largest amount (a sweep intent that settles no
   deposit);
2. per sweep, `depositForBurn(amount, destination_domain, mint_recipient, token, destination_caller = 0, max_fee,
   min_finality_threshold)`.

Both are sweep intents under `REQ-SW-13`: one open intent per address, an explicit nonce, never dropped once signed.
The faucet tops up gas for each, as for a transfer. A burn's receipt passes only if it holds the messenger's
`DepositForBurn` with this token, amount, depositor, recipient and domain, and the token's `Transfer` of that amount
out of the deposit address. Anything else is `sweep_review` (`REQ-SW-14`). A passed burn leaves its deposit
**burned**: no longer in the address, not yet swept.

`destination_caller` is zero, so anyone may relay the mint. The recipient is fixed in the burn, so a relay by someone
else cannot redirect it.

## A3 — the sender check before a burn

New `REQ-SW-17`. The reserve refunds a Base deposit to its sender's address on Polygon, which is only the same owner
for an account without code. Before each burn, evmhot reads the sender's code on two of the network's endpoints:

- both empty, or both a delegation designator (`0xef0100` and 20 bytes, EIP-7702): the check passes;
- both answer other code: the deposit goes to `needs_review` with alert `contract_sender`, and is held for an
  operator, as for `no_sender`;
- a read fails, or the two disagree: no burn this pass, and the next pass checks again. After an hour of failing or
  disagreeing reads, `contract_sender` is raised for that deposit.

## A4 — attestation and the mint

New `REQ-SW-18`. Each pass, for each burned deposit, evmhot asks the attestation API for the burn's message. When the
API answers `complete`, it has the message, its nonce and the attestation.

- If the destination's transmitter already marks the nonce used, someone has relayed it. evmhot finds the
  `MessageReceived` for that nonce on the destination and checks that mint's receipt.
- Otherwise it relays `receiveMessage(message, attestation)` on the destination from the **relayer**: the faucet
  mnemonic's address at index 1 (`REQ-KY-5`). The relay is a relay intent with the rules of `REQ-SW-13`: recorded
  before signing, an explicit nonce, the same resend, re-sign and review rules. A relay that reverts because the
  nonce is now used falls back to finding the mint.

A mint passes only if its receipt holds the token's `Transfer` from the zero address to the recipient for the burned
amount, less the fee the message says was executed (zero while `max_fee` is 0). It then records the sweep with that
coordinate and settles the deposit (`REQ-SW-5`).

A burned deposit with no passed mint 60 minutes after its burn raises `cctp_unminted`, then again hourly. Funds
between a burn and its mint are on neither network; only Circle's message holds them.

## A5 — keys

New `REQ-KY-5`. The relayer is the faucet mnemonic's address at index 1. It pays only for relays. The faucet tops it
up on the destination like a deposit address (`REQ-FC-1`), so its gas is never more than one top-up ahead. No new
secret is needed, and `REQ-KY-3` covers it.

## A6 — `deposit_swept` 1.1.0

New `REQ-WH-12`. A `cctp_burn` deposit's `deposit_swept` is sent once every part is minted. Its top-level fields
describe the deposit, on its own network. Each `sweeps[]` entry adds:

- `sweep_network`: the destination's name;
- `burn_tx_hash`: the part's `depositForBurn` transaction.

Its `sweep_*` coordinate, `sweep_tx_hash` and `sweep_target` are the mint's, on the destination. An entry without
`sweep_network` is on the deposit's own network, so every body sent today is unchanged. The contract becomes 1.1.0,
an additive change. A target's notify URL (`REQ-WH-10`) receives it the same way.

## A7 — store and invariants

The store gains: the per-address approval state; burned deposits with their burn and message; relay intents. `INV-5`
widens: a deposit swept by `cctp_burn` has a successful burn receipt and a successful mint receipt holding its
`Transfer`. New `INV-9`: every transaction mined from the relayer is a recorded relay intent.

## Out of scope

- The reverse, bUSDC paid out to a Base address. That is a payout from custody, outside `§1.3`. It stays with Swaps.
- Fast transfers and fees (A1).
- Any destination other than one recipient per target.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SW-16`, `REQ-SW-17`, `REQ-SW-18`, `REQ-KY-5`, `REQ-WH-12`, `INV-9` |
| Revised | `REQ-SW-6` (kind `cctp_burn`), `REQ-SW-9` and `REQ-SW-10` (the mint is the transfer into the target), `REQ-OB-4` (kinds `contract_sender`, `cctp_unminted`, `cctp_config`), `REQ-WH-11` (contract 1.1.0), `INV-5` |
| Withdrawn | none |

## What this costs, stated plainly

- Each deposit address gives Circle's messenger an unlimited allowance for the token. The messenger is an upgradeable
  proxy, so the allowance is only as safe as Circle's upgrade key. That is the same trust as holding USDC itself,
  which Circle can freeze.
- A new external dependency: Circle's attestation API. If it is down, burned deposits wait and `cctp_unminted` says
  so. Nothing is lost, since a message can be relayed late.
- Funds are in flight between a burn and its mint: about 15 to 20 minutes at standard finality.
- A new funded address, the relayer, that holds a little of the destination's native coin.
- Two transactions per address the first time (approve, burn), one after that.
- The test environment burns on mainnet into the same reserve. Each environment's swap service tells its mints apart
  by which evmhot reports them.
- The sender check reads two endpoints per burn, and it holds every deposit from a contract for a person.
