# solana-tx-scheduler

No validator I measured orders transactions by fee alone. On mainnet after Alpenglow, across 25 validators in 9 client groups, the higher fee usually wins between transactions sent within about 20 ms of each other, and the earlier-sent one wins when they are 150 ms or more apart. Five validators pass the pre-registered test for this rule, fee inside short batches and arrival order beyond, with batch lengths of 30 to 150 ms. Validators behind the Harmonic proxy lean towards arrival order. [Report §0](report/REPORT.md#0-verdict) has one row per validator.

A controlled experiment for the Superteam Ukraine bounty
[Inside the Block: How Solana Validators Order Transactions](https://superteam.fun/earn/listing/inside-the-block-how-solana-validators-order-transactions).

Block order of other people's transactions cannot separate fee priority inside short arrival batches from fee priority alone, or noise from a weak rule, because their arrival times are unknown. So I send my own batches, with a known fee ladder, send order and send gap, straight to the TPU of chosen mainnet leaders, one batch per leader window (a leader's 4 consecutive slots, about 0.85 s), and read their positions back from the produced blocks.

Client groups are the gossip `clientId`: JitoLabs is Jito-Solana, AgaveBam and FireBAM run behind a Jito BAM sequencer, HarmonicAgave and HarmonicFiredancer behind a Harmonic builder proxy, and Rakurai and Raiku are their own forks.

## Results

The mainnet run went from 2026-11-04 16:10 to 2026-11-05 06:38 UTC. It fired 652 windows at 25 validators (26 validator×client rows, because gossip reported `LA1NEzry` as AgaveBam for one window). Of the 4,858 transactions sent, 4,779 (98%) landed in a block of the targeted leader, for 0.026 SOL in fees. Another 1,662 sends were refused at the QUIC layer, by BAM nodes and one Rakurai node; windows where fewer than two sends got through were repeated.

- Five validators get the label "priority within ~T ms batches": fee decides among transactions sent within T ms of each other, arrival order beyond. T is 30 to 150 ms, on one each of Agave, Firedancer, FireBAM, Rakurai and HarmonicAgave. The FireBAM one, `LA1NEz`, had only 7 usable windows of the planned 20, because the BAM node refused 38 of its 45.
- On eleven validators the order is not random: the order on which fee and arrival agree held in 86–100% of pairs. Fee wins 67–86% of conflicts between transactions sent 5–20 ms apart and arrival wins 78–100% at 150 ms or more, so neither wins overall; none passes the batched test. Four more lean towards fee, and four towards arrival.
- Of the six validators behind the Harmonic proxy, one is FIFO (borderline), three lean towards arrival, one is mixed, and one prioritises by fee within ~75 ms batches.

I expected most validators to put the higher fee first, as the Agave, Firedancer and BAM code does. The strongest fee preference I measured, 74% of conflicts on `7PdKhp`, is below the 80% that the priority label needs. Of the 15 labelled rows with a prediction I wrote from the client code, 3 match it.

Incidents during the run (the spend-cap stop and resume, a host poweroff, one window after the deadline from which nothing landed, a memory fix in `collect`) and errata to the report are in [`data/NOTES.md`](data/NOTES.md) and report §9.

| what | where |
|---|---|
| Mainnet report, generated from `data/` and never edited by hand | [`report/REPORT.md`](report/REPORT.md), [`report/summary.json`](report/summary.json) |
| Raw data: `landed.jsonl` (signature, slot, index), `windows.jsonl` (send times), `ledger.jsonl`, `anchors.jsonl`; cluster snapshots | [`data/raw/`](data/raw/), [`data/snapshots/`](data/snapshots/) |
| Two calibration runs on a local Agave validator | [`report/localnet/`](report/localnet/REPORT.md), [`report/localnet2/`](report/localnet2/REPORT.md) |
| Power analysis behind the sample size | [`report/power.md`](report/power.md) |
| Hypotheses, thresholds, predictions, stopping rule | [`PREREGISTRATION.md`](PREREGISTRATION.md) |
| Client and validator research | [`research/validators.md`](research/validators.md) |

## Bounty requirements

| requirement | where |
|---|---|
| Leader schedule and leader types | [Design](#design), [Predictions](#predictions-from-the-code) |
| Path from the sender to the executing node | [Design](#design) |
| Interaction between our own transactions, send order | [report §3](report/REPORT.md#3-what-changes-the-order-send-gap-slot-locks-send-order) |
| Competition inside the block | [report §6](report/REPORT.md#6-corroboration-from-other-users-transactions-costs-0-sol) |
| Noise versus rule | [Analysis](#analysis), [Calibration](#calibration) |
| Mainnet only, after Alpenglow | `run` refuses mainnet epochs before 1064 |
| Jito bundles and tips excluded | our transactions pay no tip; report §6 drops Jito-tip transactions |
| At least 6 validators and 3 clients | `plan` refuses fewer; the run had 25 in 9 groups |
| Spend limit of about 0.05 SOL, keys only by volume or environment | [Spend limit and keys](#spend-limit-and-keys) |
| One command with Docker Compose, dry run by default | [Running the experiment](#running-the-experiment) |
| Raw data checkable with `getBlock` | [Check it yourself](#check-it-yourself) |
| Date of every measurement | [report Appendix B](report/REPORT.md#appendix-b-every-live-measurement) |
| What went wrong | [report §9](report/REPORT.md#9-what-went-wrong), [`data/NOTES.md`](data/NOTES.md) |
| What the design does not separate | [below](#what-the-design-does-not-separate) |

## Check it yourself

No SOL and no keys are needed:

```bash
scripts/check.sh
```

It builds the binary and, in about two minutes, checks that:

1. every report regenerates byte for byte from the committed raw data;
2. every calibration signature sits at its recorded slot and index in the archived blocks;
3. so do the mainnet signatures, in 20 blocks re-fetched from `RPC_URL`, or in the whole archive when `ARCHIVE_URL` is set;
4. the analysis code matches the hashes frozen in the pre-registration, and the anchored files still hash to their on-chain memos.

The full `getBlock` archive (1.1 GB) is the release asset [`blocks.tar`](https://github.com/0xNaezo/solana-tx-scheduler/releases/tag/v1.0):

```bash
ARCHIVE_URL=https://github.com/0xNaezo/solana-tx-scheduler/releases/download/v1.0/blocks.tar scripts/check.sh
```

It is packed with:

```bash
(cd data/blocks && tar -cf ../../blocks.tar *.json.zst)
```

With Docker only: `docker compose run --rm scheduler --data-dir data/localnet verify --offline --full`.

Memo transactions anchor `PREREGISTRATION.md`, before the first live window, and `data/blocks/MANIFEST.sha256`, after the run (report Appendix A). The pre-registration also fixes the hashes of the analysis code. Every row in the report carries signatures and slots, so per-transaction numbers can be checked with `getBlock`.

CI runs fmt, clippy and the tests, fails if any report or `report/power.md` regenerates differently, and runs `scripts/e2e-localnet.sh`, which must stop on a 120,000-lamport cap after exactly one window, find every sent transaction, pass `verify --full`, record the anchor, and leave every payer empty after `sweep`.

## Design

Each measurement transaction holds three ComputeBudget instructions and nothing else: `SetComputeUnitLimit(1000)`, `SetComputeUnitPrice(p)` and `SetLoadedAccountsDataSizeLimit`, whose value is a nonce inside one 32 KiB page, so each transaction is unique at the same cost. It uses about 450 CU and cannot fail. All transactions in a window have the same shape and the same cost-model cost, so `cu_price`, fee per CU and Agave's reward/cost all order them the same way ([`src/tx.rs`](src/tx.rs)). Rank `i` (0–9) pays `cu_price = 70,000·(i+1)` µlamports/CU, at most 5,700 lamports per transaction with the base fee.

Each window gets one cell of a 2 × 3 × 2 design, shuffled with the plan's seed and cycled per validator:

| factor | levels | why |
|---|---|---|
| send order | reverse (cheapest first), random | Under reverse, FIFO and priority predict opposite orders. |
| pause between sends | 0, 10, 50 ms | Gaps from 0 to 450 ms inside a batch; where fee stops winning is the batch length. |
| fee payer | one shared, one per transaction | A shared payer write-locks one account in every pair, so the scheduler must serialize them; distinct payers may run on parallel threads. Distinct payers are reassigned to ranks in every window, so no address-dependent effect can line up with fee. |

`run` reads each target's leader windows from `getLeaderSchedule` in 4-slot rotation units. It looks the leader up in gossip, connects to its advertised `tpuQuic` as an unstaked client, and fires 2 slots before the window by a slot clock that polls `getSlot`. Each transaction goes on its own stream, in the planned order and pauses, with its send time recorded in microseconds. No RPC `sendTransaction` is used: an RPC node forwards on its own schedule, possibly to several leaders, which would destroy the timing the design controls. One connection per leader and 10 streams per window stay far below Agave 4.4's unstaked limits ([8 connections per IP, 400 TPS per peer][quic], [8 new connections per minute][throttle]).

`collect` finds each transaction with `getSignatureStatuses` and fetches `getBlock` for every slot of the window. A position counts only in a block produced by the targeted leader. Raw responses are kept compressed and hashed; each block is reduced to one row per transaction for the foreign-transaction analysis.

The client is the gossip `clientId`, stored with every window: a BAM validator that falls back to plain Jito mid-run becomes a separate row. The `version` string cannot tell the forks apart. The report also shows the route to each leader: `direct`, `bam-node` (port 5010 on a foreign IP), `harmonic-proxy`, or `direct (jito-relayer port)` (port 11228). For BAM and Harmonic the order measured is the sequencer's, which is what decides their blocks.

## Analysis

The unit is a pair of our transactions that landed in the same slot. Pairs split across slots are decided by arrival time and reported apart.

- D: among disagree pairs (higher fee sent later) sent at least 5 ms apart, the share where the higher fee landed first. 1 is priority, 0 is send order. Pairs sent closer reach the leader in an order the sender does not control, so they are shown in the gap table but not in D.
- A: among agree pairs, the share where the common order held. About 0.5 means the order is noise.
- Pair accuracy of three models: FIFO, priority, and priority among transactions sent within B ms of each other with send order beyond (B from 5 to 200 ms). The batch length T is a maximum-likelihood fit over the gaps. The design has no gaps between about 0.2 and 10 ms, so a fit below 10 ms is printed as "≤10 ms".

p-values come from 10,000 permutations of landing order within each slot, Holm-corrected over two tests per validator across the run. Confidence intervals come from 2,000 bootstrap resamples of windows, because pairs inside a window are not independent. The label follows [`PREREGISTRATION.md`](PREREGISTRATION.md):

| condition | label |
|---|---|
| fewer than 30 disagree pairs or 5 windows | insufficient data |
| p_D < 0.05 and D ≥ 0.8 | priority |
| p_D < 0.05 and D ≤ 0.2 | FIFO |
| p_D < 0.05, otherwise | mixed, priority- or arrival-leaning |
| only p_A < 0.05 | mixed/balanced |
| neither | no rule detected |

A rule becomes "priority within ~T ms batches" when the batching model beats both pure models by at least 5 points of pair accuracy and the 2.5% bootstrap quantile of that margin is above 0. Without the second condition, FIFO with arrival jitter was relabelled as batching in about 15% of simulated runs; with it, about 5%.

The report also gives D by send gap, D in the window's first slot against later slots, labels per payer level, survivorship bounds on D, and a placebo test: does landing order follow the first 8 bytes of the signature? No scheduler orders by that, so a significant placebo marks a pipeline artifact. As supporting evidence at no cost, it computes Goodman–Kruskal γ between Agave priority and position for the hundreds of other users' transactions in the same blocks, with Jito-tip transactions excluded.

## Calibration

Synthetic schedulers ([`report/power.md`](report/power.md)) go through the same classifier, with 1 ms of arrival jitter, 80% landing, 213 ms slots at a random phase, and Holm over 46 tests with every other validator null. At 20 windows per validator it names priority in 98–100% of runs, FIFO in 93–95%, and priority within 20 ms or 50 ms batches in 97–100% with the batch length within ×1.5, and it never labels noise as a rule. That is why the plan uses 20 windows per validator. Rules set right on a threshold (D 0.80 or 0.22) get the strict label only in 33–47% of runs and "mixed" otherwise.

The real positive control is a local Agave validator. `scripts/e2e-localnet.sh` runs the same live pipeline on it: 24 windows, 240 transactions, all landed. It was run twice on 2026-11-03, before and after the review changes, and both runs are committed:

| | run 1, [`localnet`](report/localnet/REPORT.md) | run 2, [`localnet2`](report/localnet2/REPORT.md) |
|---|---|---|
| D, pairs ≥5 ms apart | 0.00 | 0.02 |
| D, pairs 0–5 ms apart | 0.75 [0.55, 0.93] | 0.66 [0.42, 0.87] |
| pair accuracy: FIFO / priority / batches of 5 ms | 80% / 46% / 93% | 77% / 42% / 86% |
| 2.5% quantile of the batching margin | 0.013 | −0.040 |
| label | priority within batches of ≤10 ms | FIFO |
| shared payer / distinct payers | batches / FIFO | FIFO / FIFO |
| placebo p | 0.44 | 0.31 |

The two runs agree on the mechanism and differ in which side of the pre-registered margin it falls. Transactions sent 10 ms or more apart land in send order (D 0.00 and 0.02). In pause-0 windows with a shared payer, fee decides (4 of 4 windows perfectly in run 1, 3 of 4 in run 2). In pause-0 windows with distinct payers, fee wins about half the time. That fits Agave's code: the greedy scheduler takes the buffer in priority order ([greedy_scheduler.rs][greedy]) and the buffer fills in 10 ms receive passes ([receive_and_buffer.rs][recv]). Transactions that must be serialized come out in fee order, while independent ones are spread over threads whose output interleaves. On mainnet, an Agave label of FIFO or of short batches would therefore reflect the same scheduler, and the per-payer labels show which. Under the pre-registered predictions, run 2's FIFO would count as a miss for Agave. Both runs are kept.

The test validator runs Agave 3.1.10, the version CI installs. It has the same greedy scheduler and 10 ms receive timeout, but holds packets before its leader slot for about one slot (tick-based), where 4.4 holds below 2 slots. It is also always the leader, so only 7% of run 2's transactions landed in a window's first slot, and the pre-slot buffer is barely exercised. On mainnet, vanilla Agave validators act as a known-rule control, and the placebo is computed for every validator.

## Predictions from the code

Written before any mainnet data; versions checked on 2026-11-03 (Agave v4.4.0, jito-solana v4.4.0-jito, Firedancer v26.10.5). The report marks each label as matching or not.

| client | mechanism | predicted label |
|---|---|---|
| Agave | Greedy scheduler, the only one since 4.1 ([banking_stage.rs][banking]), priority `reward·10⁶/(cost+1)` with half the base fee burnt ([transaction_priority.rs][prio]). Packets that arrive before the leader slot are held until the bank exists ([decision_maker.rs][decision]); later ones come in 10 ms passes. | priority, or priority within batches |
| JitoLabs | Same scheduler for normal transactions; bundles run in their own stage ([bundle_stage.rs][bundle]). | as Agave |
| Firedancer | `pack` keeps pending transactions in a treap by reward/compute ([fd_pack.c][pack]). The default `schedule_strategy = "balanced"` paces block filling ([default.toml][fdconfig]), which leaves more pending transactions to choose from. | priority, or priority within batches |
| AgaveBam, FireBAM | The BAM node runs auction rounds of about 20–50 ms ([BAM changelog][bamlog]), where transactions "compete on a shared priority score" ([maker plugin][bammaker]); the validator executes that order ([bam_receive_and_buffer.rs][bam-rx]). | priority within batches of 10–100 ms |
| HarmonicAgave, HarmonicFiredancer | The operator picks a builder strategy: 50 ms batch auctions, FIFO, or a proprietary one ([Harmonic][harmonic]). | none: each is consistent |
| Rakurai | Proprietary scheduler, described as heuristics that select high-value transactions ([Rakurai docs][rakurai]). | none |

Prior work: Chorus One's [The State of Schedulers on Solana][chorus] (Umberto Natale, 2026-03-16) measured, before Alpenglow, a normalized footrule distance between block order and fee order over all transactions in a block: about 0.66 for Agave, 0.6 for BAM and Harmonic, 0.40–0.45 for Firedancer. Without arrival times, that distance cannot tell a FIFO scheduler from a priority one fed in short arrival batches. This experiment fixes arrival and fee by design and measures after Alpenglow.

## What the design does not separate

For `bam-node` and `harmonic-proxy` routes the measurement covers the sequencer and the validator together. The send time of a packet is known, its arrival time at the leader is not, which is why D starts at 5 ms. The slot clock trails the leader by one or two slots, so the pre-slot buffer and streaming are not told apart per transaction; D in the first slot against later slots shows the combined effect.

Our transactions have identical cost, so the data cannot tell which fee formula a scheduler uses. `getBlock` gives one flat order without execution threads or entry boundaries; the payer contrast only shows whether locks change it. Order is measured among the transactions that landed, and landing rates and bounds on D with the lost ones counted either way are printed next to every label. Each node reports its own `clientId`, so the binary behind it is not verified, and a strategy change during the run would blur into a "mixed" label.

## Running the experiment

```bash
docker compose up
```

This is a rehearsal that sends nothing. It plans, waits for 24 real leader windows, builds and signs every batch, opens QUIC to each leader, fetches those blocks and writes a report to `data/rehearsal/report/` (git-ignored), leaving the committed `data/` and `report/` alone. The RPC comes from `RPC_URL`, else `HELIUS_API` in `.env`, else the public endpoint, which rate-limits `getBlock` too hard for a full run. If your uid:gid is not 1000:1000, prefix commands with `HOST_UID=$(id -u) HOST_GID=$(id -g)`.

The live run:

1. Put a dedicated wallet at `keys/id.json` (mounted read-only, git-ignored, never copied into the image) with about 0.07 SOL. The ten derived fee payers hold up to about 0.035 SOL at peak, including their rent-exempt reserve, and `sweep` returns it. `run --live` refuses a wallet that holds more than 0.1 SOL.
2. Anchor the pre-registration: `docker compose run --rm scheduler anchor --file PREREGISTRATION.md --label prereg --live`.
3. Start it with a fee cap and a deadline. The run was started with this command and the pre-registered deadline of 2026-11-05 06:00 UTC; after the 0.03 SOL cap stopped it, it was restarted with `MAX_SPEND_LAMPORTS=50000000` (NOTES, 2026-11-04):

```bash
LIVE=1 MAX_SPEND_LAMPORTS=30000000 MAX_MINUTES=$(( ($(date -ud '2026-11-05 06:00' +%s) - $(date +%s)) / 60 )) docker compose up
```

The run's plan was 25 validators × 20 windows × 10 transactions. It took about 14.5 hours, mostly waiting for the windows of small validators and for repeats of refused windows. Ctrl+C is safe: `run` writes the window in flight and exits. The next `LIVE=1` start resumes the quotas with the same `plan.json` (`REPLAN=1` makes a new one). When the run ends, the pipeline prints the closing commands: anchor the block manifest, sweep the payers, regenerate the report, commit, and publish the archive.

Without Docker, `cargo build --release` (Rust 1.97.1, pinned in `rust-toolchain.toml`) and call `target/release/solana-tx-scheduler <command>`. Every command that spends is a dry run unless given `--live`.

| command | what it does |
|---|---|
| `plan` | Snapshots the cluster into `data/snapshots/`, groups nodes by gossip `clientId`, picks validators, and writes `data/plan.json` with the design seed. |
| `run [--live --max-spend-lamports N]` | Sends one batch per leader window over QUIC; writes `data/raw/windows.jsonl` and `ledger.jsonl`. |
| `collect` | Finds where each transaction landed and archives `getBlock` for every slot of every window (`data/blocks/<slot>.json.zst`, `MANIFEST.sha256`). |
| `analyze [--out DIR]` | Regenerates the report from the data directory only. Writes `report/` for `data`, else `<data-dir>/report`. |
| `verify [--offline] [--full] [--sample N]` | Checks every recorded (signature, slot, index) against the RPC or the archive, and the archive against `MANIFEST.sha256`; `--full` also re-derives every block row, `--sample N` checks N evenly spaced blocks. |
| `fingerprint --validator ID` | Read-only ordering card for any validator, from its recent blocks. |
| `simulate` | Runs the classifier on synthetic schedulers and writes `report/power.md`. |
| `anchor --file F [--live]` | Writes `sha256(F)` on-chain in a memo. |
| `sweep [--live]` | Returns the derived payers' lamports to the wallet. |
| `scripts/e2e-localnet.sh` | The full live pipeline on a throwaway `solana-test-validator`; `E2E_OUT=DIR` keeps its data, which is how `data/localnet*` were made. |

## Spend limit and keys

Before each send, `run` reserves the worst-case fee in `data/raw/ledger.jsonl` and fsyncs it ([`src/budget.rs`](src/budget.rs)). The cap is `min(--max-spend-lamports, 0.05 SOL)` across all runs, and the 0.05 SOL is compiled in. Payer top-ups, sweeps and anchors go through the ledger too. After every window, `run` checks the on-chain balance of the wallet and all derived payers, and stops if they lost more than the budget had left when it started, whatever the ledger says. `run --live` refuses a wallet holding more than `--max-wallet-lamports` (0.1 SOL), so a bug can lose at most that balance.

Report §8 reconciles the ledger against `meta.fee` of the landed transactions and against on-chain balances. On localnet all three matched to the lamport. On mainnet the wallets lost 22,223 lamports less than the fees paid, and I have not found why (NOTES, 2026-11-05). Keys are read only from `keys/` (mounted read-only) or the `KEYPAIR` path, and `.gitignore` and `.dockerignore` exclude them. An API key in the RPC URL is redacted from every log line and data file.

## Layout

```
src/
  tx.rs        measurement transaction, fee ladder, derived payers
  budget.rs    three-layer spend limit
  quic.rs      QUIC client to the leader's TPU
  rpc.rs       slot clock, leader windows, gossip
  blocks.rs    block archive, block rows, foreign-transaction gamma
  stats.rs     pairs, D/A/tau, permutations, bootstrap, Holm, classifier, synthetic schedulers
  commands/    one file per subcommand
scripts/       check.sh, pipeline.sh (Docker entrypoint), e2e-localnet.sh
data/          mainnet raw data, data/localnet* calibration runs, NOTES.md
report/        generated reports
research/      client and validator research
```

The code was written from 2026-10-27 to 2026-11-04 in a scratch repo and squashed before publishing, so the design changes made before the anchor are described in the 2026-11-03 entries of `data/NOTES.md`.

MIT licensed.

[prio]: https://github.com/anza-xyz/agave/blob/v4.4.0/core/src/transaction_priority.rs#L32-L66
[banking]: https://github.com/anza-xyz/agave/blob/v4.4.0/core/src/banking_stage.rs#L587-L591
[greedy]: https://github.com/anza-xyz/agave/blob/v4.4.0/core/src/banking_stage/transaction_scheduler/greedy_scheduler.rs
[recv]: https://github.com/anza-xyz/agave/blob/v4.4.0/core/src/banking_stage/transaction_scheduler/receive_and_buffer.rs#L329-L330
[decision]: https://github.com/anza-xyz/agave/blob/v4.4.0/core/src/banking_stage/decision_maker.rs#L73-L101
[quic]: https://github.com/anza-xyz/agave/blob/v4.4.0/streamer/src/quic.rs#L41-L56
[throttle]: https://github.com/anza-xyz/agave/blob/v4.4.0/streamer/src/nonblocking/stream_throttle.rs#L17-L68
[bundle]: https://github.com/jito-foundation/jito-solana/blob/v4.4.0-jito/core/src/bundle_stage.rs
[bam-rx]: https://github.com/jito-foundation/jito-solana/blob/v4.4.0-jito/core/src/banking_stage/transaction_scheduler/bam_receive_and_buffer.rs#L148
[pack]: https://github.com/firedancer-io/firedancer/blob/v26.10.5/src/disco/pack/fd_pack.c#L226-L248
[fdconfig]: https://github.com/firedancer-io/firedancer/blob/v26.10.5/src/app/fdctl/config/default.toml
[bammaker]: https://bam.dev/docs/bam/maker-plugin/how-it-works/
[bamlog]: https://bam.dev/blog/bam-changelog-sept-23-oct-9/
[harmonic]: https://docs.harmonic.gg/concepts/scheduling-strategies
[rakurai]: https://docs.rakurai.io/docs/services/rakurai_jito_private/readme
[chorus]: https://chorus.one/reports-research/the-state-of-schedulers-on-solana-2
