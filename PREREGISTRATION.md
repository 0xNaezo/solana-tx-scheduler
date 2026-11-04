# Pre-registration

Written 2026-11-03 and revised before any live mainnet measurement, after a review of the design (the changes are in git history). Before `run --live`, the SHA-256 of this file is written to mainnet:

```bash
solana-tx-scheduler anchor --file PREREGISTRATION.md --label prereg --live
```

The memo signature is stored in `data/raw/anchors.jsonl` and listed in the report. `sha256sum PREREGISTRATION.md` must match the memo, and the hashes of the analysis code below must match the committed files.

## Question

For each selected mainnet validator, after Alpenglow (epoch ≥ 1064):

- does fee priority or send order decide which of our transactions lands earlier in its block;
- how strictly;
- or is the order indistinguishable from noise?

## Validators

The selection is fixed as a rule, because the concrete list depends on stake and gossip at the moment of the run. The rule is the `plan` defaults:

- group nodes by gossip `clientId`, with unknown ids excluded;
- keep validators with stake ≥ 0.05%, skip rate ≤ 5% in the current epoch, an advertised `tpuQuic`, and at least 5 leader windows left;
- take the top 3 by stake in each group;
- refuse the plan if this gives fewer than 6 validators or fewer than 3 client groups.

The `plan.json` created right before the live run, and its RPC snapshot in `data/snapshots/`, are committed with the data.

## Design

- **Batch.** One per leader window: 10 transactions with identical cost (three ComputeBudget instructions, CU limit 1,000). `cu_price` is on a ladder of 70,000 to 700,000 µlamports/CU.
- **Factors.** Each window gets one cell of send order {reverse, random} × pause between sends {0, 10, 50} ms × fee payer {shared, distinct}. Cells are shuffled with the plan's seed and cycled per validator. With distinct payers, the payers are assigned to ranks in a new random order in every window.
- **Sending.** Directly to the leader's `tpuQuic`, starting 2 slots before the window.
- **Quota.** 20 live windows per validator.

## Hypotheses and predictions

| | hypothesis | label |
|---|---|---|
| H-P | priority decides | `priority` |
| H-F | send order decides | `FIFO` |
| H-B | priority decides inside arrival batches of about T ms, send order across batches | `priority within ~T ms batches` |
| H-0 | no rule | `no rule detected` |

Partial rules get one of the mixed labels.

Predictions from the source code, written before any mainnet data (code references in the README). The report prints, per validator, whether its label is in the predicted set.

| client | mechanism | predicted labels |
|---|---|---|
| Agave, JitoLabs | greedy scheduler: priority among what is buffered (held before the slot, or in one 10 ms receive pass), arrival order across passes | `priority` or `priority within ~T ms batches` (any T) |
| Firedancer | `pack` treap by reward/compute; the default `schedule_strategy = "balanced"` paces block filling, which leaves more pending transactions to choose from | `priority` or `priority within ~T ms batches` (any T) |
| AgaveBam, FireBAM | BAM auction rounds of about 20–50 ms | `priority within ~T ms batches` with 10 ≤ T ≤ 100 |
| HarmonicAgave, HarmonicFiredancer | operator's builder strategy: 50 ms batch auctions, FIFO, or proprietary | none testable: any of the three is consistent |
| Rakurai | proprietary scheduler, described only as heuristics that select high-value transactions | none |

Secondary questions are descriptive and give no label:

- S1. Do conflicting transactions (shared payer) keep their order more often than independent ones? D, A and the label per payer level, without Holm correction. The shared-payer level is the cleaner view of the scheduler's queue order: those transactions must be serialized, while distinct-payer ones may run on parallel threads.
- S2. Does D fall as the send gap grows (D by gap bucket)?
- S3. Is γ between Agave priority and position positive in the same validators' blocks for foreign transactions?
- S4. Is D higher among transactions that landed in the window's first slot (held before the leader's bank started) than in its later slots?
- S5. How far could lost transactions move D? Bounds with every conflict of a lost transaction counted as lost, or as won by fee.

## Metrics

Defined in the README and computed by `analyze` (`src/stats.rs`). The unit is a same-slot pair of our transactions in a block produced by the targeted leader.

- **D**: share of disagree pairs (higher fee sent later) won by priority, over pairs sent at least 5 ms apart. Pairs sent closer reach the leader in an order we do not control, so they cannot tell priority from arrival.
- **A**: share of agree pairs, at any gap, that kept the common order.
- **Kendall τ** with priority and with send order.
- **Pair accuracy** of FIFO, priority, and "priority if sent within B ms of each other, send order beyond" for B ∈ {5, 10, 20, 50, 100, 200} ms.
- **Batch length T**: maximum-likelihood fit of P(priority wins | gap g) = ε + (1 − 2ε)·max(0, 1 − g/T), ε = 0.05, over the disagree pairs, with T on a 5 ms grid from 5 to 500 ms (fixed-length batches at an unknown phase). The design has no gaps between about 0.2 and 10 ms, so a fit below 10 ms is reported as "≤10 ms".

## Decision rule

- p-values come from 10,000 within-slot permutations of landing order, for D (two-sided) and for A.
- Holm correction over the 2 tests × all validator rows in the run.
- 95% CIs from 2,000 bootstrap resamples of windows.
- The label:
  1. Fewer than 30 disagree pairs or fewer than 5 windows: `insufficient data`.
  2. If p_D < 0.05:
     - D ≥ 0.8: `priority`;
     - D ≤ 0.2: `FIFO`;
     - D > 0.5: `mixed, priority-leaning`;
     - otherwise: `mixed, arrival-leaning`.
  3. Else if p_A < 0.05: `mixed/balanced`.
  4. Else: `no rule detected`.
  5. If the label from steps 2–3 names a rule, and the best batching model beats both FIFO and priority by at least 5 points of pair accuracy, with the 2.5% window-bootstrap quantile of that margin above 0, the label becomes `priority within ~T ms batches`, with T the fitted batch length.
- A strict label whose 95% CI of D crosses its threshold is printed as borderline.
- Negative control (placebo): the same pairs tested for whether landing order follows the first 8 bytes of the signature, with 10,000 within-slot permutations and Holm across validators. No scheduler should order by it, so it must come out as no rule. A significant placebo means a pipeline artifact, and that validator's label is reported with this warning.

## Sample size

[`report/power.md`](report/power.md) runs the same classifier on synthetic windows: this design, 10 transactions per window, 80% landing, 1 ms sd of arrival jitter (pause-0 transactions arrive in random order), slots of 213 ms at a random phase (a 50 ms-pause batch spans 3 slots), and Holm over 2 × 23 tests with the other validators null. With 20 windows per validator:

| true rule | true D | labelled correctly |
|---|---|---|
| priority, little noise (sd 0.05) | 0.98 | 100% |
| priority, noisy (sd 0.15) | 0.88 | 98% |
| FIFO, little noise | 0.02 | 93% |
| FIFO, noisy | 0.14 | 90% |
| priority within 20 ms or 50 ms batches, any noise level, fitted batch length within ×1.5 | 0.06–0.28 | 97–100% |
| priority or FIFO with D on the threshold (sd 0.25) | 0.80 / 0.22 | 47% / 33% |
| noise: a false rule | 0.50 | 0% |

Most FIFO misses are a "≤10 ms batches" label: pause-0 transactions arrive in random order, and now and then that looks like priority. At 10 windows clear rules are found in 91–100% and batching in 78–98%; at 5 windows most validators get `insufficient data`.

Hence 20 windows.

## Stopping rule

- `run` stops when every validator has 20 usable windows, when the spend cap is reached (at most 0.05 SOL in total, compiled in), or at the deadline: no live window is sent after **2026-11-05 06:00 UTC**. Each start passes the minutes left until then as `MAX_MINUTES`; every window's fire time is in `windows.jsonl`, so the deadline can be checked.
- No interim analysis of order decides whether to continue. Windows are never discarded afterwards. Skipped windows (late, no TPU, RPC error, budget), and windows in which fewer than two transactions left without a send error, are logged with the reason and do not count toward the quota; the failed design cell is repeated.
- One data-quality stop is allowed: if, after the first 10 live windows, `collect` finds fewer than 30% of the sent transactions in any block, the run is stopped to look for a bug in sending. This looks at landing only, never at order, and is logged in `data/NOTES.md`.
- An interrupted run may be resumed with the same `plan.json` only to fill missing quotas. It never replaces data.
- A validator with fewer than 5 windows is reported as `insufficient data`, not dropped.

## Exclusions

- Transactions landing in a block not produced by the targeted leader are counted and reported, but not ordered.
- Pairs split across slots are reported separately and are not used for the label.
- A validator whose gossip `clientId` changes during the run is reported once per client.

## Analysis code

The analysis is frozen with this file. SHA-256 at anchoring time, built with the committed `Cargo.lock` (`scripts/check.sh` compares them with the checked-out files):

```
42cfe3f977a18ea81ef2b14ff4d1caebce185d53bf1e684e101bbc07c32ee822  src/stats.rs
cea01c09775930e25e34b404d3490d9e6cbc4e501659c2bd23cd6993e272f806  src/commands/analyze.rs
c3fdd6da979f31319081fdfe147590661a0532ff264a143e1dfa054d24450c1d  src/commands/simulate.rs
```

## Deviations

Any deviation from this document, with its date and reason, goes into `data/NOTES.md` and appears in the report section "What went wrong". After the anchor, any change to the files above is a deviation.
