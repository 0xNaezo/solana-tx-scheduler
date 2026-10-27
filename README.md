# solana-tx-scheduler

Measurement project for the Superteam Ukraine bounty
[Inside the Block: How Solana Validators Order Transactions](https://superteam.fun/earn/listing/inside-the-block-how-solana-validators-order-transactions).

## Task

Send our own transactions into the leader windows of specific Solana mainnet validators and determine, from the produced blocks, by what rule each validator orders transactions inside its block: what decides which transaction lands earlier, how strictly the rule holds, and whether there is a rule at all.

## Constraints

- Mainnet only, all measurements after the Alpenglow upgrade (Oct 21, 2026).
- At least 6 validators across at least 3 distinct client/fork versions.
- Total fee spend capped at ~0.05 SOL, enforced in code.
- Jito-Solana validators are in scope; Jito bundles and tips are not.
- Primary evidence comes from our own transactions.

## Deliverables

- One-command reproducible run via Docker Compose.
- Keys only via volume/env, never in the repo.
- Dry-run by default; real sending only behind an explicit flag with a spend cap.
- Raw data (signatures, slots) so every claim can be checked with `getBlock`.
- README explaining the design, what it accounts for, and what it does not separate.
- Report with the date of every measurement and a "what went wrong" section.
