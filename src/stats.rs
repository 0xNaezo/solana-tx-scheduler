//! Ordering statistics.
//!
//! Unit of evidence: a *pair* of our transactions that landed in the same
//! slot of the target window. For each pair we know which one has the higher
//! priority, which one we sent first and which one the leader put first.
//! * disagree pair (higher priority was sent later): the leader breaks the
//!   tie between "priority" and "arrival". D = share won by priority.
//!   D≈1 priority, D≈0 FIFO, D≈0.5 neither. D counts only pairs sent at
//!   least `MIN_GAP_MS` apart: closer ones arrive in an order we do not
//!   control, so the leader cannot know which was sent first.
//! * agree pair: both rules predict the same order. A = share where it held.
//!   A≈1 some rule, A≈0.5 noise.
//!
//! Windows are the independent units: CIs bootstrap windows, p-values come
//! from permuting landing positions within each slot (the null "position is
//! unrelated to priority and send order").

use std::fmt;

use rand::{RngExt, SeedableRng, rngs::StdRng, seq::SliceRandom};
use serde::Serialize;

use crate::model::{Conflict, SendOrder, Treatment};

pub const BOOTSTRAP: usize = 2_000;
pub const PERMUTATIONS: usize = 10_000;
/// Pre-registered decision thresholds (see PREREGISTRATION.md).
pub const ALPHA: f64 = 0.05;
pub const STRONG: f64 = 0.8;
pub const MIN_DISAGREE_PAIRS: u64 = 30;
pub const MIN_WINDOWS: usize = 5;
/// Disagree pairs sent closer than this are left out of D (see module docs).
pub const MIN_GAP_MS: u64 = 5;
/// Batching windows tried by the model comparison; 0 = FIFO, MAX = priority.
pub const BATCH_MODELS_MS: [u64; 8] = [0, 5, 10, 20, 50, 100, 200, u64::MAX];
/// A batching model becomes the label only if it beats both pure models by this much.
pub const BATCH_MARGIN: f64 = 0.05;
/// Smallest non-zero send gap of the design (the 10 ms pause): batch lengths
/// below it cannot be told apart.
pub const RESOLUTION_MS: u64 = 10;

#[derive(Debug, Clone)]
pub struct TxObs {
    /// Priority rank, higher = higher cu_price.
    pub rank: usize,
    /// 0 = sent first.
    pub send_seq: usize,
    pub sent_at_us: u64,
    /// Placebo key no scheduler should order by: the first 8 signature bytes.
    pub key: u64,
    /// (slot, index in block) if it landed inside the target window.
    pub pos: Option<(u64, usize)>,
}

#[derive(Debug, Clone)]
pub struct WindowObs {
    pub treatment: Treatment,
    /// First slot of the leader window.
    pub start: u64,
    pub txs: Vec<TxObs>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Pairs {
    pub disagree: u64,
    pub prio_wins: u64,
    pub agree: u64,
    pub holds: u64,
}

impl Pairs {
    fn add(&mut self, o: Pairs) {
        self.disagree += o.disagree;
        self.prio_wins += o.prio_wins;
        self.agree += o.agree;
        self.holds += o.holds;
    }
    pub fn total(&self) -> u64 {
        self.disagree + self.agree
    }
    pub fn d(&self) -> f64 {
        ratio(self.prio_wins, self.disagree)
    }
    pub fn a(&self) -> f64 {
        ratio(self.holds, self.agree)
    }
    /// Kendall tau between landing order and priority order (same-slot pairs).
    pub fn tau_prio(&self) -> f64 {
        2.0 * ratio(self.prio_wins + self.holds, self.total()) - 1.0
    }
    /// Kendall tau between landing order and send order (same-slot pairs).
    pub fn tau_fifo(&self) -> f64 {
        2.0 * ratio(self.disagree - self.prio_wins + self.holds, self.total()) - 1.0
    }
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        f64::NAN
    } else {
        a as f64 / b as f64
    }
}

/// Index pairs `(i, j)`, `i < j`, that `pos` puts in the same slot.
fn same_slot(pos: &[Option<(u64, usize)>]) -> impl Iterator<Item = (usize, usize)> + '_ {
    (0..pos.len())
        .flat_map(move |i| (i + 1..pos.len()).map(move |j| (i, j)))
        .filter(|&(i, j)| matches!((pos[i], pos[j]), (Some(a), Some(b)) if a.0 == b.0))
}

/// Whether `i` landed before `j` (both placed by `pos`).
fn first(pos: &[Option<(u64, usize)>], i: usize, j: usize) -> bool {
    pos[i].map(|p| p.1) < pos[j].map(|p| p.1)
}

fn gap_us(w: &WindowObs, i: usize, j: usize) -> u64 {
    w.txs[i].sent_at_us.abs_diff(w.txs[j].sent_at_us)
}

/// Pair counts over same-slot pairs whose send gap is in `[lo_us, hi_us)`.
pub fn same_slot_pairs(
    w: &WindowObs,
    pos: &[Option<(u64, usize)>],
    lo_us: u64,
    hi_us: u64,
) -> Pairs {
    let mut p = Pairs::default();
    for (i, j) in same_slot(pos) {
        if !(lo_us..hi_us).contains(&gap_us(w, i, j)) {
            continue;
        }
        let prio_i = w.txs[i].rank > w.txs[j].rank;
        let sent_i = w.txs[i].send_seq < w.txs[j].send_seq;
        let landed_i = first(pos, i, j);
        if prio_i == sent_i {
            p.agree += 1;
            p.holds += u64::from(landed_i == prio_i);
        } else {
            p.disagree += 1;
            p.prio_wins += u64::from(landed_i == prio_i);
        }
    }
    p
}

/// What D and A are computed from: disagree pairs at resolvable gaps, agree pairs at all gaps.
fn rule_pairs(w: &WindowObs, pos: &[Option<(u64, usize)>]) -> Pairs {
    let all = same_slot_pairs(w, pos, 0, u64::MAX);
    let wide = same_slot_pairs(w, pos, MIN_GAP_MS * 1_000, u64::MAX);
    Pairs {
        disagree: wide.disagree,
        prio_wins: wide.prio_wins,
        ..all
    }
}

/// Pairs that landed in different slots: does the earlier-sent one land in the earlier slot?
pub fn cross_slot(w: &WindowObs) -> (u64, u64) {
    let (mut n, mut earlier_sent_first) = (0, 0);
    for i in 0..w.txs.len() {
        for j in i + 1..w.txs.len() {
            if let (Some(pi), Some(pj)) = (w.txs[i].pos, w.txs[j].pos)
                && pi.0 != pj.0
            {
                n += 1;
                let sent_i = w.txs[i].send_seq < w.txs[j].send_seq;
                earlier_sent_first += u64::from(sent_i == (pi.0 < pj.0));
            }
        }
    }
    (earlier_sent_first, n)
}

fn positions(w: &WindowObs) -> Vec<Option<(u64, usize)>> {
    w.txs.iter().map(|t| t.pos).collect()
}

/// Shuffle landing positions among the transactions of each slot.
pub fn permuted(w: &WindowObs, rng: &mut StdRng) -> Vec<Option<(u64, usize)>> {
    let mut pos = positions(w);
    let mut slots: Vec<u64> = pos.iter().flatten().map(|p| p.0).collect();
    slots.sort_unstable();
    slots.dedup();
    for s in slots {
        let members: Vec<usize> = (0..pos.len())
            .filter(|&i| matches!(pos[i], Some((x, _)) if x == s))
            .collect();
        let mut idx: Vec<usize> = members
            .iter()
            .filter_map(|&i| pos[i])
            .map(|p| p.1)
            .collect();
        idx.shuffle(rng);
        for (&m, v) in members.iter().zip(idx) {
            pos[m] = Some((s, v));
        }
    }
    pos
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Est {
    pub value: f64,
    pub lo: f64,
    pub hi: f64,
}

impl Est {
    const NONE: Est = Est {
        value: f64::NAN,
        lo: f64::NAN,
        hi: f64::NAN,
    };
}

impl fmt::Display for Est {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.value.is_nan() {
            return write!(f, "–");
        }
        write!(f, "{:.2} [{:.2}, {:.2}]", self.value, self.lo, self.hi)
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}

/// Window-level bootstrap of a statistic computed from pooled pair counts.
pub fn bootstrap(per_window: &[Pairs], stat: impl Fn(&Pairs) -> f64, seed: u64) -> Est {
    let mut total = Pairs::default();
    per_window.iter().for_each(|p| total.add(*p));
    let value = stat(&total);
    if per_window.is_empty() || value.is_nan() {
        return Est { value, ..Est::NONE };
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let mut draws: Vec<f64> = (0..BOOTSTRAP)
        .map(|_| {
            let mut s = Pairs::default();
            for _ in 0..per_window.len() {
                s.add(per_window[rng.random_range(0..per_window.len())]);
            }
            stat(&s)
        })
        .filter(|x| !x.is_nan())
        .collect();
    draws.sort_by(f64::total_cmp);
    Est {
        value,
        lo: percentile(&draws, 0.025),
        hi: percentile(&draws, 0.975),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleEstimate {
    pub windows: usize,
    /// Disagree pairs at gaps ≥ `MIN_GAP_MS`, agree pairs at all gaps.
    pub pairs: Pairs,
    pub d: Est,
    pub a: Est,
    pub tau_prio: Est,
    pub tau_fifo: Est,
    /// Permutation p-values, before multiple-comparison correction.
    pub p_d: f64,
    pub p_a: f64,
}

/// Pooled pair counts with window-bootstrap CIs; no permutation test.
pub fn estimate_ci(windows: &[WindowObs], seed: u64) -> RuleEstimate {
    let rule: Vec<Pairs> = windows
        .iter()
        .map(|w| rule_pairs(w, &positions(w)))
        .filter(|p| p.total() > 0)
        .collect();
    let all: Vec<Pairs> = windows
        .iter()
        .map(|w| same_slot_pairs(w, &positions(w), 0, u64::MAX))
        .filter(|p| p.total() > 0)
        .collect();
    let mut pairs = Pairs::default();
    rule.iter().for_each(|p| pairs.add(*p));
    RuleEstimate {
        windows: rule.len(),
        pairs,
        d: bootstrap(&rule, Pairs::d, sub_seed(seed, "d")),
        a: bootstrap(&rule, Pairs::a, sub_seed(seed, "a")),
        tau_prio: bootstrap(&all, Pairs::tau_prio, sub_seed(seed, "tau priority")),
        tau_fifo: bootstrap(&all, Pairs::tau_fifo, sub_seed(seed, "tau send order")),
        p_d: f64::NAN,
        p_a: f64::NAN,
    }
}

/// `estimate_ci` plus permutation p-values for D != 0.5 (two-sided) and A > 0.5.
pub fn estimate(windows: &[WindowObs], seed: u64) -> RuleEstimate {
    estimate_n(windows, seed, PERMUTATIONS)
}

pub fn estimate_n(windows: &[WindowObs], seed: u64, permutations: usize) -> RuleEstimate {
    let mut e = estimate_ci(windows, seed);
    let (d_obs, a_obs) = (e.pairs.d(), e.pairs.a());
    let mut rng = StdRng::seed_from_u64(sub_seed(seed, "permutations"));
    let (mut ge_d, mut ge_a) = (1usize, 1usize);
    for _ in 0..permutations {
        let mut s = Pairs::default();
        for w in windows {
            s.add(rule_pairs(w, &permuted(w, &mut rng)));
        }
        ge_d += usize::from((s.d() - 0.5).abs() >= (d_obs - 0.5).abs() - 1e-12);
        ge_a += usize::from(s.a() >= a_obs - 1e-12);
    }
    let p = |ge: usize, obs: f64| {
        if obs.is_nan() {
            1.0
        } else {
            ge as f64 / (permutations + 1) as f64
        }
    };
    e.p_d = p(ge_d, d_obs);
    e.p_a = p(ge_a, a_obs);
    e
}

/// D restricted to disagree pairs with a send gap in `[lo_ms, hi_ms)`.
pub fn d_by_gap(windows: &[WindowObs], lo_ms: u64, hi_ms: u64, seed: u64) -> (Est, u64) {
    let per: Vec<Pairs> = windows
        .iter()
        .map(|w| same_slot_pairs(w, &positions(w), lo_ms * 1_000, hi_ms.saturating_mul(1_000)))
        .filter(|p| p.disagree > 0)
        .collect();
    let n = per.iter().map(|p| p.disagree).sum();
    (bootstrap(&per, Pairs::d, seed), n)
}

/// D (at resolvable gaps) over pairs that landed in the window's first slot,
/// or in its later slots. Packets that reached the leader before its bank
/// existed are held and scheduled at the start of the first slot.
pub fn d_by_slot(windows: &[WindowObs], first_slot: bool, seed: u64) -> (Est, u64) {
    let per: Vec<Pairs> = windows
        .iter()
        .map(|w| {
            let pos: Vec<_> = positions(w)
                .into_iter()
                .map(|p| p.filter(|&(s, _)| (s == w.start) == first_slot))
                .collect();
            same_slot_pairs(w, &pos, MIN_GAP_MS * 1_000, u64::MAX)
        })
        .filter(|p| p.disagree > 0)
        .collect();
    let n = per.iter().map(|p| p.disagree).sum();
    (bootstrap(&per, Pairs::d, seed), n)
}

/// Bounds on D if every transaction that left but did not land in the window
/// had landed beside its partners: all those disagree pairs lost, or all won.
pub fn d_bounds(windows: &[WindowObs]) -> (f64, f64) {
    let (mut wins, mut n, mut lost) = (0, 0, 0);
    for w in windows {
        let p = same_slot_pairs(w, &positions(w), MIN_GAP_MS * 1_000, u64::MAX);
        wins += p.prio_wins;
        n += p.disagree;
        for i in 0..w.txs.len() {
            for j in i + 1..w.txs.len() {
                let (a, b) = (&w.txs[i], &w.txs[j]);
                if (a.pos.is_none() || b.pos.is_none())
                    && (a.rank > b.rank) != (a.send_seq < b.send_seq)
                    && gap_us(w, i, j) >= MIN_GAP_MS * 1_000
                {
                    lost += 1;
                }
            }
        }
    }
    (ratio(wins, n + lost), ratio(wins + lost, n + lost))
}

/// Accuracy of "priority if sent within `batch_ms` of each other, else
/// arrival order" on the same-slot pairs of one window. batch 0 = pure FIFO,
/// u64::MAX = pure priority.
fn batch_model_window(w: &WindowObs, batch_ms: u64) -> (u64, u64) {
    let (mut ok, mut n) = (0, 0);
    let pos = positions(w);
    for (i, j) in same_slot(&pos) {
        let (a, b) = (&w.txs[i], &w.txs[j]);
        let gap_ms = gap_us(w, i, j) / 1_000;
        let predict_a_first = if gap_ms <= batch_ms && batch_ms > 0 || batch_ms == u64::MAX {
            a.rank > b.rank
        } else {
            a.send_seq < b.send_seq
        };
        n += 1;
        ok += u64::from(predict_a_first == first(&pos, i, j));
    }
    (ok, n)
}

pub fn batch_model_accuracy(windows: &[WindowObs], batch_ms: u64) -> (u64, u64) {
    windows
        .iter()
        .map(|w| batch_model_window(w, batch_ms))
        .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelFit {
    pub fifo: f64,
    pub priority: f64,
    /// Best pairwise threshold: priority if the send gap is at most this.
    pub best_ms: Option<u64>,
    pub best: f64,
    /// 2.5% window-bootstrap quantile of `best − max(fifo, priority)`.
    pub margin_lo: f64,
    /// Batch length that best explains D by gap (`batch_period`).
    pub period_ms: Option<u64>,
}

/// Pairwise accuracy of FIFO, priority and the best "priority within B ms" model.
pub fn model_fit(windows: &[WindowObs]) -> ModelFit {
    let acc = |b: u64| {
        let (ok, n) = batch_model_accuracy(windows, b);
        ratio(ok, n)
    };
    let mut best: Option<(u64, f64)> = None;
    for &b in &BATCH_MODELS_MS[1..BATCH_MODELS_MS.len() - 1] {
        let a = acc(b);
        // Strictly greater: ties keep the smaller window.
        if !a.is_nan() && best.is_none_or(|(_, x)| a > x) {
            best = Some((b, a));
        }
    }
    ModelFit {
        fifo: acc(0),
        priority: acc(u64::MAX),
        best_ms: best.map(|b| b.0),
        best: best.map_or(f64::NAN, |b| b.1),
        margin_lo: best.map_or(f64::NAN, |(b, _)| margin_lo(windows, b)),
        period_ms: batch_period(windows),
    }
}

/// Pairs inside a window are not independent, so the batching model's lead
/// over both pure models gets a window-bootstrap lower bound.
fn margin_lo(windows: &[WindowObs], batch_ms: u64) -> f64 {
    let per: Vec<[u64; 4]> = windows
        .iter()
        .map(|w| {
            let (best, n) = batch_model_window(w, batch_ms);
            let fifo = batch_model_window(w, 0).0;
            let prio = batch_model_window(w, u64::MAX).0;
            [best, fifo, prio, n]
        })
        .filter(|x| x[3] > 0)
        .collect();
    if per.is_empty() {
        return f64::NAN;
    }
    let mut rng = StdRng::seed_from_u64(seed_of("model margin"));
    let mut draws: Vec<f64> = (0..BOOTSTRAP)
        .map(|_| {
            let mut s = [0u64; 4];
            for _ in 0..per.len() {
                let x = per[rng.random_range(0..per.len())];
                (0..4).for_each(|k| s[k] += x[k]);
            }
            let n = s[3] as f64;
            (s[0] as f64 - s[1].max(s[2]) as f64) / n
        })
        .collect();
    draws.sort_by(f64::total_cmp);
    percentile(&draws, 0.025)
}

/// Batch length T that best explains disagree pairs. With fixed-length
/// batches at an unknown phase, two transactions sent g ms apart share a
/// batch with probability max(0, 1 - g/T): then priority wins, otherwise
/// send order does. Maximum likelihood on a 5 ms grid, with a 5% floor for
/// noise. (The best pairwise threshold B sits near T/2, so B is not T.)
pub fn batch_period(windows: &[WindowObs]) -> Option<u64> {
    const EPS: f64 = 0.05;
    let mut obs: Vec<(f64, bool)> = Vec::new();
    for w in windows {
        let pos = positions(w);
        for (i, j) in same_slot(&pos) {
            let (a, b) = (&w.txs[i], &w.txs[j]);
            let prio_a = a.rank > b.rank;
            if prio_a != (a.send_seq < b.send_seq) {
                obs.push((
                    gap_us(w, i, j) as f64 / 1_000.0,
                    first(&pos, i, j) == prio_a,
                ));
            }
        }
    }
    if obs.is_empty() {
        return None;
    }
    let log_lik = |t: f64| -> f64 {
        obs.iter()
            .map(|&(g, prio_won)| {
                let p = EPS + (1.0 - 2.0 * EPS) * (1.0 - g / t).max(0.0);
                if prio_won { p.ln() } else { (1.0 - p).ln() }
            })
            .sum()
    };
    (1..=100u64)
        .map(|k| k * 5)
        .map(|t| (t, log_lik(t as f64)))
        .fold(None, |best: Option<(u64, f64)>, (t, l)| match best {
            // Strictly greater: ties keep the shorter period.
            Some((_, bl)) if l <= bl => best,
            _ => Some((t, l)),
        })
        .map(|(t, _)| t)
}

/// Slots (≥3 of our transactions) whose order exactly matches descending priority.
pub fn exact_priority_slots(windows: &[WindowObs]) -> (usize, usize) {
    let (mut hit, mut n) = (0, 0);
    for w in windows {
        let mut by_slot: Vec<(u64, usize, usize)> = w
            .txs
            .iter()
            .filter_map(|t| t.pos.map(|(s, i)| (s, i, t.rank)))
            .collect();
        by_slot.sort_unstable();
        for chunk in by_slot.chunk_by(|a, b| a.0 == b.0) {
            if chunk.len() >= 3 {
                n += 1;
                hit += usize::from(chunk.windows(2).all(|x| x[0].2 > x[1].2));
            }
        }
    }
    (hit, n)
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Placebo {
    /// Share of same-slot pairs that landed in signature-key order (≈0.5 expected).
    pub share: f64,
    pub pairs: u64,
    /// Two-sided within-slot permutation p-value, before correction.
    pub p: f64,
}

/// Negative control: does landing order follow the signature bytes? No
/// scheduler should order by them, so a "rule" here is an artifact of the
/// pipeline (or a hash tie-break), never a finding about priority.
pub fn placebo(windows: &[WindowObs], seed: u64) -> Placebo {
    type Positions = Vec<Option<(u64, usize)>>;
    let count = |pos_of: &mut dyn FnMut(&WindowObs) -> Positions| {
        let (mut k, mut n) = (0u64, 0u64);
        for w in windows {
            let pos = pos_of(w);
            for (i, j) in same_slot(&pos) {
                n += 1;
                k += u64::from(first(&pos, i, j) == (w.txs[i].key < w.txs[j].key));
            }
        }
        (k, n)
    };
    let (k, n) = count(&mut positions);
    let share = ratio(k, n);
    if n == 0 {
        return Placebo {
            share,
            pairs: 0,
            p: 1.0,
        };
    }
    let mut rng = StdRng::seed_from_u64(sub_seed(seed, "placebo"));
    let mut ge = 1usize;
    for _ in 0..PERMUTATIONS {
        let (kp, np) = count(&mut |w| permuted(w, &mut rng));
        ge += usize::from((ratio(kp, np) - 0.5).abs() >= (share - 0.5).abs() - 1e-12);
    }
    Placebo {
        share,
        pairs: n,
        p: ge as f64 / (PERMUTATIONS + 1) as f64,
    }
}

/// Goodman–Kruskal gamma between priority and earliness over `(priority,
/// position)`: +1 = higher priority always earlier, ties in priority ignored.
pub fn gamma(items: &[(u64, usize)]) -> Option<f64> {
    let (mut c, mut d) = (0u64, 0u64);
    for i in 0..items.len() {
        for j in i + 1..items.len() {
            let ((pi, xi), (pj, xj)) = (items[i], items[j]);
            if pi == pj {
                continue;
            }
            if (pi > pj) == (xi < xj) {
                c += 1;
            } else {
                d += 1;
            }
        }
    }
    (c + d > 0).then(|| (c as f64 - d as f64) / (c + d) as f64)
}

/// Mean with a bootstrap CI over the given (independent) values.
pub fn bootstrap_mean(values: &[f64], seed: u64) -> Est {
    if values.is_empty() {
        return Est::NONE;
    }
    let mean = |v: &mut dyn Iterator<Item = f64>, n: usize| v.sum::<f64>() / n as f64;
    let value = mean(&mut values.iter().copied(), values.len());
    let mut rng = StdRng::seed_from_u64(seed);
    let mut draws: Vec<f64> = (0..BOOTSTRAP)
        .map(|_| {
            let mut it = (0..values.len()).map(|_| values[rng.random_range(0..values.len())]);
            mean(&mut it, values.len())
        })
        .collect();
    draws.sort_by(f64::total_cmp);
    Est {
        value,
        lo: percentile(&draws, 0.025),
        hi: percentile(&draws, 0.975),
    }
}

/// Holm-Bonferroni adjusted p-values, same order as input.
pub fn holm(p: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..p.len()).collect();
    idx.sort_by(|&a, &b| p[a].total_cmp(&p[b]));
    let m = p.len();
    let mut out = vec![1.0; m];
    let mut running: f64 = 0.0;
    for (k, &i) in idx.iter().enumerate() {
        running = running.max(((m - k) as f64 * p[i]).min(1.0));
        out[i] = running;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    Priority,
    Fifo,
    /// Priority among transactions arriving within ~N ms, arrival order beyond.
    BatchedPriority(u64),
    MixedPriorityLeaning,
    MixedArrivalLeaning,
    MixedBalanced,
    NoRule,
    InsufficientData,
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Label::BatchedPriority(ms) if *ms < RESOLUTION_MS => {
                "priority within batches of ≤10 ms (design resolution), arrival order beyond"
            }
            Label::BatchedPriority(ms) => {
                return write!(f, "priority within ~{ms} ms batches, arrival order beyond");
            }
            Label::Priority => "priority",
            Label::Fifo => "FIFO (arrival order)",
            Label::MixedPriorityLeaning => "mixed, priority-leaning",
            Label::MixedArrivalLeaning => "mixed, arrival-leaning",
            Label::MixedBalanced => "mixed: a rule exists, priority ≈ arrival",
            Label::NoRule => "no rule detected (indistinguishable from noise)",
            Label::InsufficientData => "insufficient data",
        })
    }
}

/// The pre-registered decision rule. `p_d`, `p_a` are Holm-adjusted.
pub fn classify(e: &RuleEstimate, p_d: f64, p_a: f64, fit: &ModelFit) -> Label {
    if e.pairs.disagree < MIN_DISAGREE_PAIRS || e.windows < MIN_WINDOWS {
        return Label::InsufficientData;
    }
    let d = e.d.value;
    let base = if p_d < ALPHA {
        if d >= STRONG {
            Label::Priority
        } else if d <= 1.0 - STRONG {
            Label::Fifo
        } else if d > 0.5 {
            Label::MixedPriorityLeaning
        } else {
            Label::MixedArrivalLeaning
        }
    } else if p_a < ALPHA {
        Label::MixedBalanced
    } else {
        return Label::NoRule;
    };
    match fit.best_ms {
        Some(ms)
            if fit.best >= fit.fifo.max(fit.priority) + BATCH_MARGIN && fit.margin_lo > 0.0 =>
        {
            Label::BatchedPriority(fit.period_ms.unwrap_or(2 * ms))
        }
        _ => base,
    }
}

/// Stable seed from a string (FNV-1a), so reports are reproducible.
pub fn seed_of(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A separate random stream for one purpose, derived from a base seed.
pub fn sub_seed(seed: u64, purpose: &str) -> u64 {
    seed ^ seed_of(purpose)
}

/// All 12 cells of the factorial design, in a seeded random order.
pub fn treatment_cells(pauses: &[u64], rng: &mut StdRng) -> Vec<Treatment> {
    let mut cells = Vec::new();
    for &order in &[SendOrder::Reverse, SendOrder::Random] {
        for &pause_ms in pauses {
            for &conflict in &[Conflict::Shared, Conflict::Distinct] {
                cells.push(Treatment {
                    order,
                    pause_ms,
                    conflict,
                });
            }
        }
    }
    cells.shuffle(rng);
    cells
}

/// Send sequence: `seq[k]` = rank sent k-th.
pub fn send_sequence(n: usize, order: SendOrder, rng: &mut StdRng) -> Vec<usize> {
    let mut seq: Vec<usize> = (0..n).collect();
    match order {
        SendOrder::Reverse => {}
        SendOrder::Random => seq.shuffle(rng),
    }
    seq
}

// ---------------------------------------------------------------- simulation

/// Mean mainnet slot after Alpenglow (perf samples, 2026-11): synthetic slot boundaries.
pub const SYNTH_SLOT_MS: f64 = 213.0;
/// Sd of the delay jitter between our host and the leader, per transaction.
pub const SYNTH_JITTER_MS: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    Priority,
    /// Arrival order at the leader (send order blurred by jitter).
    Fifo,
    /// Priority inside batches of `ms`, arrival order between batches.
    Batched(u64),
    Noise,
}

impl Truth {
    pub fn accepts(&self, l: Label) -> bool {
        match self {
            Truth::Priority => l == Label::Priority,
            Truth::Fifo => l == Label::Fifo,
            // Right kind of rule and a batch length within a factor of 1.5.
            Truth::Batched(ms) => {
                matches!(l, Label::BatchedPriority(t) if 3 * t >= 2 * ms && 2 * t <= 3 * ms)
            }
            // Only a false rule is a miss; too little data is not one.
            Truth::Noise => matches!(l, Label::NoRule | Label::InsufficientData),
        }
    }
}

fn normal(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.random_range(f64::EPSILON..1.0);
    let u2: f64 = rng.random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// One synthetic window under `truth`. Each transaction arrives at its send
/// time plus Gaussian jitter (`SYNTH_JITTER_MS`) and lands with probability
/// `land`, in the slot its arrival falls into (slot boundaries at a random
/// phase, `SYNTH_SLOT_MS` apart). Within a slot the order is the rule's score
/// plus Gaussian noise of sd `noise` on a 0..1 priority scale.
pub fn synth_window(
    truth: Truth,
    t: Treatment,
    n: usize,
    noise: f64,
    land: f64,
    rng: &mut StdRng,
) -> WindowObs {
    let seq = send_sequence(n, t.order, rng);
    let denom = (n.max(2) - 1) as f64;
    let arrival: Vec<f64> = (0..n)
        .map(|k| (k as u64 * t.pause_ms) as f64 + SYNTH_JITTER_MS * normal(rng))
        .collect();
    let mut arrival_rank = vec![0; n];
    let mut by_arrival: Vec<usize> = (0..n).collect();
    by_arrival.sort_by(|&a, &b| arrival[a].total_cmp(&arrival[b]));
    for (r, &k) in by_arrival.iter().enumerate() {
        arrival_rank[k] = r;
    }
    let slot_phase = rng.random_range(0.0..SYNTH_SLOT_MS);
    // Batch boundaries do not wait for our first transaction either.
    let batch_phase = match truth {
        Truth::Batched(ms) => rng.random_range(0.0..ms.max(1) as f64),
        _ => 0.0,
    };
    let mut txs: Vec<TxObs> = seq
        .iter()
        .enumerate()
        .map(|(k, &rank)| TxObs {
            rank,
            send_seq: k,
            sent_at_us: k as u64 * t.pause_ms * 1_000,
            key: rng.random(),
            pos: None,
        })
        .collect();
    let mut scored: Vec<(u64, f64, usize)> = Vec::new();
    for k in 0..n {
        if !rng.random_bool(land) {
            continue;
        }
        let slot = ((slot_phase + arrival[k]).max(0.0) / SYNTH_SLOT_MS) as u64;
        let rank = txs[k].rank as f64 / denom;
        let base = match truth {
            Truth::Priority => rank,
            Truth::Fifo => -(arrival_rank[k] as f64) / denom,
            Truth::Batched(ms) => {
                let batch = ((arrival[k] + batch_phase) / ms.max(1) as f64).floor();
                -batch * 10.0 + rank
            }
            Truth::Noise => 0.0,
        };
        let sd = if truth == Truth::Noise { 1.0 } else { noise };
        scored.push((slot, -(base + sd * normal(rng)), k));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    for (k, &(slot, _, i)) in scored.iter().enumerate() {
        txs[i].pos = Some((slot, k * 7));
    }
    WindowObs {
        treatment: t,
        start: 0,
        txs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(truth: Truth, windows: usize, seed: u64) -> Label {
        let mut rng = StdRng::seed_from_u64(seed);
        let cells = treatment_cells(&[0, 10, 50], &mut rng);
        let ws: Vec<WindowObs> = (0..windows)
            .map(|k| synth_window(truth, cells[k % cells.len()], 10, 0.05, 0.9, &mut rng))
            .collect();
        let e = estimate_n(&ws, seed, 2_000);
        classify(&e, e.p_d, e.p_a, &model_fit(&ws))
    }

    #[test]
    fn recovers_priority_fifo_and_batching() {
        assert_eq!(run(Truth::Priority, 20, 1), Label::Priority);
        assert_eq!(run(Truth::Fifo, 20, 2), Label::Fifo);
        let label = run(Truth::Batched(20), 24, 3);
        assert!(Truth::Batched(20).accepts(label), "{label:?}");
    }

    #[test]
    fn noise_rarely_yields_a_rule() {
        let false_rules = (0..40)
            .filter(|&s| {
                !matches!(
                    run(Truth::Noise, 20, 100 + s),
                    Label::NoRule | Label::InsufficientData
                )
            })
            .count();
        // Two tests at alpha 0.05 each: expect ~4 of 40, allow sampling noise.
        assert!(
            false_rules <= 8,
            "{false_rules} of 40 noise runs produced a rule"
        );
    }

    #[test]
    fn placebo_ignores_a_real_rule_and_finds_a_key_order() {
        let mut rng = StdRng::seed_from_u64(9);
        let cells = treatment_cells(&[0, 10, 50], &mut rng);
        let mut ws: Vec<WindowObs> = (0..20)
            .map(|k| synth_window(Truth::Priority, cells[k % 12], 10, 0.05, 0.9, &mut rng))
            .collect();
        assert!(placebo(&ws, 1).p > ALPHA);
        // A pipeline that sorted positions by signature would be caught.
        for w in &mut ws {
            for t in &mut w.txs {
                t.pos = t.pos.map(|(s, _)| (s, (t.key >> 1) as usize));
            }
        }
        let p = placebo(&ws, 1);
        assert!(p.p < 0.001 && p.share == 1.0, "{p:?}");
    }

    #[test]
    fn pair_counts_on_a_reverse_window() {
        // Sent lowest priority first, 10 ms apart; landed highest priority
        // first => priority wins every pair.
        let w = WindowObs {
            treatment: Treatment {
                order: SendOrder::Reverse,
                pause_ms: 10,
                conflict: Conflict::Shared,
            },
            start: 5,
            txs: (0..4)
                .map(|r| TxObs {
                    rank: r,
                    send_seq: r,
                    sent_at_us: r as u64 * 10_000,
                    key: 0,
                    pos: Some((5, 10 - r)),
                })
                .collect(),
        };
        let p = rule_pairs(&w, &positions(&w));
        assert_eq!(
            p,
            Pairs {
                disagree: 6,
                prio_wins: 6,
                agree: 0,
                holds: 0
            }
        );
        assert_eq!(p.tau_prio(), 1.0);
        assert_eq!(p.tau_fifo(), -1.0);
        assert_eq!(exact_priority_slots(std::slice::from_ref(&w)), (1, 1));
        assert_eq!(d_by_slot(std::slice::from_ref(&w), true, 0).1, 6);
        assert_eq!(d_bounds(&[w]), (1.0, 1.0));
    }

    #[test]
    fn close_pairs_are_left_out_of_d() {
        let w = WindowObs {
            treatment: Treatment {
                order: SendOrder::Reverse,
                pause_ms: 0,
                conflict: Conflict::Shared,
            },
            start: 5,
            txs: (0..3)
                .map(|r| TxObs {
                    rank: r,
                    send_seq: r,
                    sent_at_us: r as u64,
                    key: 0,
                    pos: Some((5, 10 - r)),
                })
                .collect(),
        };
        assert_eq!(rule_pairs(&w, &positions(&w)).disagree, 0);
        assert_eq!(same_slot_pairs(&w, &positions(&w), 0, u64::MAX).disagree, 3);
    }

    #[test]
    fn holm_matches_hand_computation() {
        let adj = holm(&[0.01, 0.04, 0.03, 0.2]);
        assert_eq!(adj, vec![0.04, 0.09, 0.09, 0.2]);
    }
}
