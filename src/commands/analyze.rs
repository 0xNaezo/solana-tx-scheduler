//! Rebuilds `report/REPORT.md` and `report/summary.json` from `data/` only.
//! Output is deterministic (fixed seeds, no wall-clock), so CI can diff it.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::{
    blocks::{self, col, foreign_gamma, sha256_hex},
    budget::HARD_CAP_LAMPORTS,
    cli::{AnalyzeArgs, Context},
    commands::plan::client_group,
    model::{
        ALPENGLOW_EPOCH, Anchor, BlockSummary, Conflict, Landed, LedgerEntry, Paths, Plan,
        SendOrder, WindowRecord, read_json, read_jsonl, utc, write_json,
    },
    stats::{
        self, Est, Label, ModelFit, Placebo, RESOLUTION_MS, RuleEstimate, TxObs, WindowObs,
        bootstrap_mean, classify, cross_slot, d_bounds, d_by_gap, d_by_slot, estimate, estimate_ci,
        exact_priority_slots, holm, model_fit, placebo, seed_of, sub_seed,
    },
};

const GAP_BUCKETS_MS: [(u64, u64); 5] = [(0, 5), (5, 20), (20, 60), (60, 150), (150, u64::MAX)];
const EXPLORER: &str = "https://solscan.io";

/// Everything `analyze` reads, loaded once.
struct Data {
    paths: Paths,
    plan: Option<Plan>,
    windows: Vec<WindowRecord>,
    landed: Vec<Landed>,
    blocks: BTreeMap<u64, BlockSummary>,
    ledger: Vec<LedgerEntry>,
    anchors: Vec<Anchor>,
    notes: Option<String>,
}

#[derive(Serialize)]
struct ValidatorResult {
    identity: String,
    client: String,
    version: String,
    route: String,
    stake_pct: Option<f64>,
    windows: usize,
    first_utc: String,
    last_utc: String,
    sent: usize,
    landed_in_window: usize,
    in_first_slot: usize,
    landed_elsewhere: usize,
    missing: usize,
    failed: usize,
    rule: RuleEstimate,
    p_d_holm: f64,
    p_a_holm: f64,
    label: Label,
    prediction: Option<&'static str>,
    matches_prediction: Option<bool>,
    /// D if every lost transaction had landed: all its conflicts lost / won.
    d_bounds: (f64, f64),
    gap: Vec<(String, Est, u64)>,
    /// D in the window's first slot, and in its later slots.
    first_slot: (Est, u64),
    later_slots: (Est, u64),
    conflict: Vec<Subset>,
    order: Vec<Subset>,
    model: ModelFit,
    exact_priority: (usize, usize),
    cross_slot: (u64, u64),
    placebo: Placebo,
    placebo_p_holm: f64,
    evidence: Vec<Evidence>,
    foreign: Option<Foreign>,
}

/// The windows of one design level, analysed alone (descriptive, no Holm).
#[derive(Serialize)]
struct Subset {
    level: String,
    windows: usize,
    disagree: u64,
    d: Est,
    a: Est,
    label: Option<Label>,
}

/// One window shown in full: the ranks in the order we sent them and in the
/// order the leader put them into one slot.
#[derive(Serialize)]
struct Evidence {
    window_start: u64,
    slot: u64,
    order: SendOrder,
    pause_ms: u64,
    conflict: Conflict,
    sent: Vec<usize>,
    landed: Vec<usize>,
}

#[derive(Serialize, Clone)]
struct Foreign {
    blocks: usize,
    txs: usize,
    gamma: Est,
    tip_share: f64,
    mean_block_txs: f64,
}

pub fn execute(ctx: &Context, args: AnalyzeArgs) -> Result<()> {
    if !ctx.data_dir.is_dir() {
        bail!("no data directory {}", ctx.data_dir.display());
    }
    let paths = Paths::new(&ctx.data_dir);
    let data = Data {
        plan: read_json(&paths.plan).ok(),
        windows: read_jsonl(&paths.windows)?,
        landed: read_jsonl(&paths.landed)?,
        blocks: blocks::read_summaries(&paths.blocks)?,
        ledger: read_jsonl(&paths.ledger)?,
        anchors: read_jsonl(&paths.anchors)?,
        notes: fs::read_to_string(ctx.data_dir.join("NOTES.md")).ok(),
        paths,
    };
    let out = args.out.unwrap_or_else(|| {
        if ctx.data_dir.as_path() == Path::new("data") {
            PathBuf::from("report")
        } else {
            ctx.data_dir.join("report")
        }
    });

    let report = Report::new(&data);
    let mut md = String::new();
    report
        .render(&mut md)
        .expect("formatting into a String cannot fail");
    fs::create_dir_all(&out)?;
    fs::write(out.join("REPORT.md"), md)?;
    write_json(&out.join("summary.json"), &report.results)?;
    for r in &report.results {
        println!("{} {:<20} {}", short(&r.identity), r.client, r.label);
    }
    println!("report written to {}", out.join("REPORT.md").display());
    Ok(())
}

fn short(id: &str) -> &str {
    &id[..id.len().min(6)]
}

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        "–".into()
    } else {
        format!("{:.0}%", 100.0 * a as f64 / b as f64)
    }
}

fn pct_f(x: f64) -> String {
    if x.is_nan() {
        "–".into()
    } else {
        format!("{:.0}%", 100.0 * x)
    }
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn file_hash(p: &Path) -> String {
    fs::read(p)
        .map(|b| sha256_hex(&b))
        .unwrap_or_else(|_| "(missing)".into())
}

fn join<T: ToString>(xs: impl IntoIterator<Item = T>, sep: &str) -> String {
    xs.into_iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(sep)
}

fn batch_len(t: u64) -> String {
    if t < RESOLUTION_MS {
        format!("≤{RESOLUTION_MS} ms")
    } else {
        format!("{t} ms")
    }
}

/// The pre-registered prediction for a client group (PREREGISTRATION.md) and
/// whether `label` matches it; `None` where nothing was predicted.
fn prediction(client: &str, label: Label) -> Option<(&'static str, bool)> {
    match client {
        "Agave" | "JitoLabs" | "Firedancer" => Some((
            "priority, or priority within arrival batches",
            matches!(label, Label::Priority | Label::BatchedPriority(_)),
        )),
        "AgaveBam" | "FireBAM" => Some((
            "priority within 10–100 ms batches",
            matches!(label, Label::BatchedPriority(t) if (10..=100).contains(&t)),
        )),
        _ => None,
    }
}

/// The label in one plain sentence, with the numbers it rests on.
fn plain(r: &ValidatorResult) -> String {
    let (d, m) = (r.rule.d, r.model);
    let mut s = match r.label {
        Label::Priority => format!(
            "Fee decides: the higher fee landed first in {} of conflicts with send order.",
            pct_f(d.value)
        ),
        Label::Fifo => format!(
            "Arrival decides: the earlier-sent transaction landed first in {} of conflicts with fee.",
            pct_f(1.0 - d.value)
        ),
        Label::BatchedPriority(t) => format!(
            "Fee decides among transactions sent within {} of each other, arrival order beyond. \
             This predicts {} of pairs (arrival alone {}, fee alone {}).",
            batch_len(t),
            pct_f(m.best),
            pct_f(m.fifo),
            pct_f(m.priority)
        ),
        Label::MixedPriorityLeaning | Label::MixedArrivalLeaning => format!(
            "No strict rule: the higher fee won {} of conflicts with send order.",
            pct_f(d.value)
        ),
        Label::MixedBalanced => format!(
            "Not random (the order both rules agree on held in {}), but neither fee nor arrival wins conflicts.",
            pct_f(r.rule.a.value)
        ),
        Label::NoRule => "Indistinguishable from random order.".into(),
        Label::InsufficientData => format!(
            "Too few comparable pairs: {} conflicts in {} windows (the rule needs {} in {}).",
            r.rule.pairs.disagree,
            r.rule.windows,
            stats::MIN_DISAGREE_PAIRS,
            stats::MIN_WINDOWS
        ),
    };
    let crosses = match r.label {
        Label::Priority => d.lo < stats::STRONG,
        Label::Fifo => d.hi > 1.0 - stats::STRONG,
        _ => false,
    };
    if crosses {
        let _ = write!(
            s,
            " Borderline: the 95% CI of D, [{:.2}, {:.2}], crosses the threshold.",
            d.lo, d.hi
        );
    }
    s
}

/// Our transactions of one window as the statistics see them: a position only
/// if the transaction landed in a block produced by the targeted leader.
fn window_obs(
    w: &WindowRecord,
    by_sig: &HashMap<&str, &Landed>,
    blocks: &BTreeMap<u64, BlockSummary>,
) -> WindowObs {
    let txs = w
        .txs
        .iter()
        .filter(|t| t.sent_at_us.is_some() && t.send_error.is_none())
        .map(|t| {
            let pos = by_sig
                .get(t.signature.as_str())
                .and_then(|l| match (l.slot, l.index) {
                    (Some(s), Some(i)) if led_by(blocks, s, &w.leader) => Some((s, i)),
                    _ => None,
                });
            TxObs {
                rank: t.rank,
                send_seq: t.send_seq,
                sent_at_us: t.sent_at_us.unwrap_or(0),
                key: signature_key(&t.signature),
                pos,
            }
        })
        .collect();
    WindowObs {
        treatment: w.treatment,
        start: w.start_slot,
        txs,
    }
}

/// First 8 bytes of a base58 signature, big-endian.
fn signature_key(sig: &str) -> u64 {
    let bytes = bs58::decode(sig).into_vec().unwrap_or_default();
    bytes
        .iter()
        .take(8)
        .fold(0, |k, &b| (k << 8) | u64::from(b))
}

fn led_by(blocks: &BTreeMap<u64, BlockSummary>, slot: u64, leader: &str) -> bool {
    blocks.get(&slot).is_some_and(|b| b.leader == leader)
}

fn bucket_name(lo: u64, hi: u64) -> String {
    if hi == u64::MAX {
        format!("≥{lo} ms")
    } else {
        format!("{lo}–{hi} ms")
    }
}

/// For the shortest and the longest pause: the window with the most of our
/// transactions in one slot, reverse send order first (it puts fee and
/// arrival in direct conflict), then the earliest.
fn evidence(ws: &[&WindowRecord], obs: &[WindowObs]) -> Vec<Evidence> {
    let pauses = obs.iter().map(|o| o.treatment.pause_ms);
    let (Some(lo), Some(hi)) = (pauses.clone().min(), pauses.max()) else {
        return Vec::new();
    };
    let mut levels = vec![lo];
    if hi != lo {
        levels.push(hi);
    }
    levels
        .into_iter()
        .filter_map(|pause| {
            let (k, slot) = obs
                .iter()
                .enumerate()
                .filter(|(_, o)| o.treatment.pause_ms == pause)
                .filter_map(|(k, o)| {
                    let mut per_slot: BTreeMap<u64, usize> = BTreeMap::new();
                    for p in o.txs.iter().filter_map(|t| t.pos) {
                        *per_slot.entry(p.0).or_default() += 1;
                    }
                    let (&slot, &n) = per_slot.iter().max_by_key(|(s, n)| (**n, Reverse(**s)))?;
                    let reverse = o.treatment.order == SendOrder::Reverse;
                    Some(((n, reverse, Reverse(k)), (k, slot)))
                })
                .max_by_key(|c| c.0)?
                .1;
            let o = &obs[k];
            let mut sent: Vec<&TxObs> = o.txs.iter().collect();
            sent.sort_by_key(|t| t.send_seq);
            let mut landed: Vec<(usize, usize)> = o
                .txs
                .iter()
                .filter_map(|t| t.pos.filter(|p| p.0 == slot).map(|p| (p.1, t.rank)))
                .collect();
            landed.sort_unstable();
            Some(Evidence {
                window_start: ws[k].start_slot,
                slot,
                order: o.treatment.order,
                pause_ms: pause,
                conflict: o.treatment.conflict,
                sent: sent.iter().map(|t| t.rank).collect(),
                landed: landed.into_iter().map(|(_, r)| r).collect(),
            })
        })
        .collect()
}

fn foreign_for(
    id: &str,
    blocks: &BTreeMap<u64, BlockSummary>,
    landed: &[Landed],
) -> Option<Foreign> {
    let mine: Vec<&BlockSummary> = blocks
        .values()
        .filter(|b| b.leader == id && b.status == "ok")
        .collect();
    if mine.is_empty() {
        return None;
    }
    let (mut gammas, mut txs, mut tips, mut total) = (Vec::new(), 0, 0u64, 0usize);
    for b in &mine {
        let ours: Vec<usize> = landed
            .iter()
            .filter(|l| l.slot == Some(b.slot))
            .filter_map(|l| l.index)
            .collect();
        if let Some((g, n)) = foreign_gamma(b, &ours) {
            gammas.push(g);
            txs += n;
        }
        tips += b.txs.iter().map(|r| r[col::TIP]).sum::<u64>();
        total += b.txs.len();
    }
    Some(Foreign {
        blocks: mine.len(),
        txs,
        gamma: bootstrap_mean(&gammas, sub_seed(seed_of(id), "foreign gamma")),
        tip_share: tips as f64 / total.max(1) as f64,
        mean_block_txs: total as f64 / mine.len() as f64,
    })
}

struct Report<'a> {
    d: &'a Data,
    live: Vec<&'a WindowRecord>,
    by_sig: HashMap<&'a str, &'a Landed>,
    results: Vec<ValidatorResult>,
    /// Explorer links make sense only for mainnet data.
    mainnet: bool,
}

impl<'a> Report<'a> {
    fn new(d: &'a Data) -> Self {
        let by_sig: HashMap<&str, &Landed> =
            d.landed.iter().map(|l| (l.signature.as_str(), l)).collect();
        let live: Vec<&WindowRecord> = d
            .windows
            .iter()
            .filter(|w| w.live && w.skipped.is_none())
            .collect();
        let mut r = Report {
            d,
            live,
            by_sig,
            results: Vec::new(),
            mainnet: d.plan.as_ref().is_some_and(|p| p.cluster == "mainnet-beta"),
        };
        r.results = r.validators();
        r
    }

    fn validators(&self) -> Vec<ValidatorResult> {
        // Group live windows by (leader, client at window time): a BAM node that
        // drops to plain Jito mid-run shows up as two rows.
        let mut groups: BTreeMap<(String, String), Vec<&WindowRecord>> = BTreeMap::new();
        for w in &self.live {
            groups
                .entry((w.leader.clone(), client_group(w.client.as_deref())))
                .or_default()
                .push(w);
        }
        let mut results: Vec<ValidatorResult> = groups
            .iter()
            .map(|((id, client), ws)| self.validator(id, client, ws))
            .collect();

        // One family for multiple comparisons: two tests per validator row.
        let raw: Vec<f64> = results
            .iter()
            .flat_map(|r| [r.rule.p_d, r.rule.p_a])
            .collect();
        let adj = holm(&raw);
        let placebo_adj = holm(&results.iter().map(|r| r.placebo.p).collect::<Vec<_>>());
        for (i, r) in results.iter_mut().enumerate() {
            r.p_d_holm = adj[2 * i];
            r.p_a_holm = adj[2 * i + 1];
            r.label = classify(&r.rule, r.p_d_holm, r.p_a_holm, &r.model);
            r.placebo_p_holm = placebo_adj[i];
            if let Some((what, ok)) = prediction(&r.client, r.label) {
                r.prediction = Some(what);
                r.matches_prediction = (r.label != Label::InsufficientData).then_some(ok);
            }
        }
        results
    }

    fn validator(&self, id: &str, client: &str, ws: &[&WindowRecord]) -> ValidatorResult {
        let blocks = &self.d.blocks;
        let obs: Vec<WindowObs> = ws
            .iter()
            .map(|w| window_obs(w, &self.by_sig, blocks))
            .collect();
        let seed = seed_of(id) ^ seed_of(client);

        let sent: Vec<&str> = ws
            .iter()
            .flat_map(|w| {
                w.txs
                    .iter()
                    .filter(|t| t.send_error.is_none())
                    .map(|t| t.signature.as_str())
            })
            .collect();
        let placed = || obs.iter().flat_map(|w| w.txs.iter().map(move |t| (w, t)));
        let in_window = placed().filter(|(_, t)| t.pos.is_some()).count();
        let in_first_slot = placed()
            .filter(|(w, t)| t.pos.is_some_and(|p| p.0 == w.start))
            .count();
        let landed_any = sent
            .iter()
            .filter(|s| self.by_sig.get(*s).is_some_and(|l| l.slot.is_some()))
            .count();
        let failed = sent
            .iter()
            .filter(|s| self.by_sig.get(*s).is_some_and(|l| l.err.is_some()))
            .count();

        let subset = |level: String, keep: &dyn Fn(&WindowObs) -> bool, test: bool| {
            let sub: Vec<WindowObs> = obs.iter().filter(|w| keep(w)).cloned().collect();
            let s = sub_seed(seed, &level);
            let e = if test {
                estimate(&sub, s)
            } else {
                estimate_ci(&sub, s)
            };
            Subset {
                windows: e.windows,
                disagree: e.pairs.disagree,
                d: e.d,
                a: e.a,
                label: test.then(|| classify(&e, e.p_d, e.p_a, &model_fit(&sub))),
                level,
            }
        };
        let conflict = [Conflict::Shared, Conflict::Distinct]
            .into_iter()
            .map(|c| {
                subset(
                    format!("{c:?}").to_lowercase(),
                    &|w| w.treatment.conflict == c,
                    true,
                )
            })
            .collect();
        let order = [SendOrder::Reverse, SendOrder::Random]
            .into_iter()
            .map(|o| {
                subset(
                    format!("{o:?}").to_lowercase(),
                    &|w| w.treatment.order == o,
                    false,
                )
            })
            .collect();
        let target = self
            .d
            .plan
            .as_ref()
            .and_then(|p| p.validators.iter().find(|t| t.identity == id));

        ValidatorResult {
            identity: id.to_string(),
            client: client.to_string(),
            version: ws
                .last()
                .and_then(|w| w.version.clone())
                .unwrap_or_default(),
            route: target.map(|t| t.route.clone()).unwrap_or_default(),
            stake_pct: target.map(|t| t.stake_pct),
            windows: ws.len(),
            first_utc: utc((ws[0].fire_at_us / 1_000_000) as i64),
            last_utc: utc((ws[ws.len() - 1].fire_at_us / 1_000_000) as i64),
            sent: sent.len(),
            landed_in_window: in_window,
            in_first_slot,
            landed_elsewhere: landed_any.saturating_sub(in_window),
            missing: sent.len() - landed_any,
            failed,
            rule: estimate(&obs, seed),
            p_d_holm: 1.0,
            p_a_holm: 1.0,
            label: Label::InsufficientData,
            prediction: None,
            matches_prediction: None,
            d_bounds: d_bounds(&obs),
            gap: GAP_BUCKETS_MS
                .iter()
                .map(|&(lo, hi)| {
                    let name = bucket_name(lo, hi);
                    let (e, n) = d_by_gap(&obs, lo, hi, sub_seed(seed, &name));
                    (name, e, n)
                })
                .collect(),
            first_slot: d_by_slot(&obs, true, sub_seed(seed, "first slot")),
            later_slots: d_by_slot(&obs, false, sub_seed(seed, "later slots")),
            conflict,
            order,
            model: model_fit(&obs),
            exact_priority: exact_priority_slots(&obs),
            cross_slot: obs
                .iter()
                .map(cross_slot)
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
            placebo: placebo(&obs, seed),
            placebo_p_holm: 1.0,
            evidence: evidence(ws, &obs),
            foreign: foreign_for(id, blocks, &self.d.landed),
        }
    }

    fn account(&self, id: &str) -> String {
        if self.mainnet {
            format!("[`{}`]({EXPLORER}/account/{id})", short(id))
        } else {
            format!("`{}`", short(id))
        }
    }

    fn block(&self, slot: u64) -> String {
        if self.mainnet {
            format!("[{slot}]({EXPLORER}/block/{slot})")
        } else {
            slot.to_string()
        }
    }

    fn tx(&self, sig: &str) -> String {
        if self.mainnet {
            format!("[`{}…`]({EXPLORER}/tx/{sig})", &sig[..sig.len().min(8)])
        } else {
            format!("`{}…`", &sig[..sig.len().min(8)])
        }
    }

    /// Validator id plus client, as row heading.
    fn who(&self, r: &ValidatorResult) -> String {
        format!("{} {}", self.account(&r.identity), r.client)
    }

    fn fired_span(ws: &[&WindowRecord]) -> Option<(String, String)> {
        // Skipped windows never fired (fire_at_us = 0).
        let fired = || ws.iter().map(|w| w.fire_at_us).filter(|&t| t > 0);
        Some((
            utc((fired().min()? / 1_000_000) as i64),
            utc((fired().max()? / 1_000_000) as i64),
        ))
    }

    fn render(&self, w: &mut String) -> fmt::Result {
        writeln!(w, "# Report: how Solana leaders order transactions\n")?;
        writeln!(
            w,
            "> Generated by `solana-tx-scheduler analyze` from `{}/`. No number here is typed by hand; \
             CI regenerates this file and fails if it differs. Raw-file hashes and on-chain anchors are in Appendix A.\n",
            self.d
                .paths
                .plan
                .parent()
                .unwrap_or(Path::new("."))
                .display()
        )?;
        self.verdict(w)?;
        self.measurements(w)?;
        self.rules(w)?;
        self.effects(w)?;
        self.models(w)?;
        self.evidence(w)?;
        self.foreign(w)?;
        self.calibration(w)?;
        self.spend(w)?;
        self.went_wrong(w)?;
        self.hashes(w)?;
        self.appendix(w)
    }

    fn verdict(&self, w: &mut String) -> fmt::Result {
        writeln!(w, "## 0. Verdict\n")?;
        let rs = &self.results;
        if rs.is_empty() {
            return writeln!(
                w,
                "No live windows yet: nothing to classify. The design and the decision rule are fixed in `PREREGISTRATION.md`.\n"
            );
        }
        let labelled: Vec<&ValidatorResult> = rs
            .iter()
            .filter(|r| r.label != Label::InsufficientData)
            .collect();
        let groups = |xs: &mut dyn Iterator<Item = &ValidatorResult>| {
            let mut g: Vec<&str> = xs.map(|r| r.client.as_str()).collect();
            g.sort_unstable();
            g.dedup();
            g.len()
        };
        let fees: u64 = self.d.landed.iter().filter_map(|l| l.fee).sum();
        let (first, last) = Self::fired_span(&self.live).unwrap_or_default();
        let (n, g) = (
            groups(&mut rs.iter()),
            groups(&mut labelled.iter().copied()),
        );
        writeln!(
            w,
            "- Measured: {}, {}. Got a label: {} in {}.",
            count(rs.len(), "validator"),
            count(n, "client group"),
            labelled.len(),
            count(g, "group")
        )?;
        writeln!(
            w,
            "- {} live windows, {first} → {last}; fees of our measurement transactions {fees} lamports (hard cap {HARD_CAP_LAMPORTS}).",
            self.live.len()
        )?;
        if self.mainnet {
            let after = self.d.windows.iter().all(|x| x.epoch >= ALPENGLOW_EPOCH);
            writeln!(
                w,
                "- Every window is in epoch ≥ {ALPENGLOW_EPOCH} (after Alpenglow): {}.",
                if after { "yes" } else { "**no**" }
            )?;
        }
        match self
            .d
            .anchors
            .iter()
            .find(|a| a.file.ends_with("PREREGISTRATION.md") && a.signature.is_some())
        {
            Some(a) => writeln!(
                w,
                "- Pre-registration anchored on-chain at {} ({}), {} the first live window.",
                a.at,
                self.tx(a.signature.as_deref().unwrap_or_default()),
                if a.at < first { "before" } else { "**after**" }
            )?,
            None => writeln!(w, "- Pre-registration not anchored on-chain.")?,
        }
        writeln!(
            w,
            "\n| validator | client | route | windows | landed in its blocks | rule | in plain words | predicted from source | match |\n|---|---|---|---|---|---|---|---|---|"
        )?;
        for r in rs {
            writeln!(
                w,
                "| {} | {} | {} | {} | {} | **{}** | {} | {} | {} |",
                self.account(&r.identity),
                r.client,
                r.route,
                r.windows,
                pct(r.landed_in_window, r.sent),
                r.label,
                plain(r),
                r.prediction.unwrap_or("none"),
                match r.matches_prediction {
                    Some(true) => "yes",
                    Some(false) => "**no**",
                    None => "–",
                }
            )?;
        }
        writeln!(
            w,
            "\nThe label follows the pre-registered rule (§2). D = share of conflicts between fee and send order won by fee. \
             \"Predicted from source\" was written before the run (`PREREGISTRATION.md`); \"match\" says whether the label is in that set.\n"
        )
    }

    fn measurements(&self, w: &mut String) -> fmt::Result {
        let (d, live) = (self.d, &self.live);
        writeln!(w, "## 1. Measurements\n")?;
        if let Some(p) = &d.plan {
            let x = &p.design;
            writeln!(
                w,
                "Plan: cluster `{}`, epoch {} (slot {}), made {}. Design: {} tx/window, cu_limit {}, cu_price ladder up to {} µlamports/CU, \
                 pauses {} ms, send order reverse or random, fee payer shared or distinct, lead {} slots, seed {}.\n",
                p.cluster,
                p.epoch,
                p.slot,
                p.created_at,
                x.txs_per_window,
                x.cu_limit,
                x.cu_price_max,
                join(&x.pauses_ms, ", "),
                x.lead_slots,
                x.seed
            )?;
        }
        let mut epochs: Vec<u64> = d.windows.iter().map(|w| w.epoch).collect();
        epochs.sort_unstable();
        epochs.dedup();
        let dry: Vec<&WindowRecord> = d.windows.iter().filter(|w| !w.live).collect();
        let span = |ws: &[&WindowRecord]| {
            Self::fired_span(ws).map_or("–".into(), |(a, b)| format!("{a} → {b}"))
        };
        let sent: usize = live
            .iter()
            .map(|w| w.txs.iter().filter(|t| t.send_error.is_none()).count())
            .sum();
        let rs = &self.results;
        let in_window: usize = rs.iter().map(|r| r.landed_in_window).sum();
        writeln!(
            w,
            "- Epochs: {}. Live windows: {} ({}). Dry-run windows: {} ({}). Skipped windows: {}.",
            if epochs.is_empty() {
                "none".into()
            } else {
                join(&epochs, ", ")
            },
            live.len(),
            span(live),
            dry.len(),
            span(&dry),
            d.windows.iter().filter(|w| w.skipped.is_some()).count()
        )?;
        writeln!(
            w,
            "- Our transactions sent: {sent}; landed in a block of the targeted leader: {in_window} ({}); in another leader's block: {}; never landed: {}.",
            pct(in_window, sent),
            rs.iter().map(|r| r.landed_elsewhere).sum::<usize>(),
            rs.iter().map(|r| r.missing).sum::<usize>(),
        )?;
        let ok_blocks: Vec<&BlockSummary> =
            d.blocks.values().filter(|b| b.status == "ok").collect();
        let votes: u64 = ok_blocks
            .iter()
            .flat_map(|b| &b.txs)
            .map(|t| t[col::VOTE])
            .sum();
        writeln!(
            w,
            "- Vote transactions in the {} collected blocks: {votes}{}.\n",
            ok_blocks.len(),
            if self.mainnet {
                " (Alpenglow moved votes out of blocks, so 0 is expected)"
            } else {
                ""
            }
        )?;

        // Reachability, live and dry alike.
        let mut by_leader: BTreeMap<&str, Vec<&WindowRecord>> = BTreeMap::new();
        for x in &d.windows {
            by_leader.entry(x.leader.as_str()).or_default().push(x);
        }
        writeln!(
            w,
            "TPU reachability per leader (all windows, live and dry):\n\n\
             | leader | client (at window) | route | windows | skipped | QUIC ok | median handshake ms |\n|---|---|---|---|---|---|---|"
        )?;
        for (id, ws) in &by_leader {
            let mut hs: Vec<f64> = ws.iter().filter_map(|x| x.quic_handshake_ms).collect();
            hs.sort_by(f64::total_cmp);
            let tried: Vec<_> = ws
                .iter()
                .filter(|x| x.quic_handshake_ms.is_some() || x.quic_error.is_some())
                .collect();
            let ok = tried.iter().filter(|x| x.quic_error.is_none()).count();
            let route = d
                .plan
                .as_ref()
                .and_then(|p| p.validators.iter().find(|t| t.identity == *id))
                .map(|t| t.route.as_str())
                .unwrap_or_default();
            writeln!(
                w,
                "| {} | {} | {route} | {} | {} | {} | {} |",
                self.account(id),
                client_group(ws.last().and_then(|x| x.client.as_deref())),
                ws.len(),
                ws.iter().filter(|x| x.skipped.is_some()).count(),
                pct(ok, tried.len()),
                hs.get(hs.len() / 2)
                    .map_or("–".into(), |v| format!("{v:.0}")),
            )?;
        }
        writeln!(w)?;
        if rs.is_empty() {
            return Ok(());
        }
        writeln!(
            w,
            "Landing per validator (live). \"1st slot\": share of the landed ones in the window's first slot, i.e. held \
             before the leader's bank started. \"D if lost had landed\": D with every conflict of a lost transaction counted \
             as lost, or as won by fee (survivorship bounds).\n\n\
             | validator | version | stake % | windows | first → last (UTC) | sent | in its blocks | 1st slot | elsewhere | missing | failed | D if lost had landed |\n\
             |---|---|---|---|---|---|---|---|---|---|---|---|"
        )?;
        for r in rs {
            let (lo, hi) = r.d_bounds;
            writeln!(
                w,
                "| {} | {} | {} | {} | {} → {} | {} | {} ({}) | {} | {} | {} | {} | {} |",
                self.who(r),
                if r.version.is_empty() {
                    "–"
                } else {
                    &r.version
                },
                r.stake_pct.map_or("–".into(), |s| format!("{s:.3}")),
                r.windows,
                r.first_utc,
                r.last_utc,
                r.sent,
                r.landed_in_window,
                pct(r.landed_in_window, r.sent),
                pct(r.in_first_slot, r.landed_in_window),
                r.landed_elsewhere,
                r.missing,
                r.failed,
                if lo.is_nan() {
                    "–".into()
                } else {
                    format!("{lo:.2}–{hi:.2}")
                }
            )?;
        }
        writeln!(w)
    }

    fn rules(&self, w: &mut String) -> fmt::Result {
        writeln!(
            w,
            "## 2. Ordering rule per validator (primary evidence: our transactions)\n"
        )?;
        let rs = &self.results;
        if rs.is_empty() {
            return writeln!(w, "No live windows yet.\n");
        }
        writeln!(
            w,
            "Unit: pairs of our transactions that landed in the same slot. **D** = share of *disagree* pairs (higher fee sent later) \
             where the higher fee landed first: 1 = priority, 0 = arrival order (FIFO), 0.5 = neither. D counts pairs sent ≥{} ms \
             apart; closer ones reach the leader in an order we do not control. **A** = share of *agree* pairs where the common \
             order held: ≈0.5 means noise. τ = Kendall tau of landing order with priority / with send order (all same-slot pairs). \
             95% CIs bootstrap windows; p-values from {} within-slot permutations, Holm-adjusted over all {} tests. \
             Pre-registered rule: a label needs ≥{} disagree pairs from ≥{} windows; p_D < {} then D ≥ {:.1} priority, D ≤ {:.1} FIFO, \
             else mixed; otherwise p_A < {} = mixed/balanced, else no rule. A rule that exists becomes \"priority within T ms batches\" \
             when that model (§4) beats both FIFO and priority by ≥{:.0} points of pair accuracy.\n",
            stats::MIN_GAP_MS,
            stats::PERMUTATIONS,
            2 * rs.len(),
            stats::MIN_DISAGREE_PAIRS,
            stats::MIN_WINDOWS,
            stats::ALPHA,
            stats::STRONG,
            1.0 - stats::STRONG,
            stats::ALPHA,
            stats::BATCH_MARGIN * 100.0
        )?;
        writeln!(
            w,
            "| validator | windows | pairs (disagree/agree) | D | A | τ priority | τ send order | p_D | p_A | **label** |\n|---|---|---|---|---|---|---|---|---|---|"
        )?;
        for r in rs {
            writeln!(
                w,
                "| {} | {} | {}/{} | {} | {} | {} | {} | {:.4} | {:.4} | **{}** |",
                self.who(r),
                r.rule.windows,
                r.rule.pairs.disagree,
                r.rule.pairs.agree,
                r.rule.d,
                r.rule.a,
                r.rule.tau_prio,
                r.rule.tau_fifo,
                r.p_d_holm,
                r.p_a_holm,
                r.label
            )?;
        }
        writeln!(
            w,
            "\nStrictness: for a priority label it is D, for FIFO 1 − D, for a batched label the batching model's pair accuracy (§4).\n"
        )?;

        // Pooled by client: descriptive only (strategies can differ inside a group).
        let mut by_client: BTreeMap<&str, Vec<&ValidatorResult>> = BTreeMap::new();
        for r in rs {
            by_client.entry(r.client.as_str()).or_default().push(r);
        }
        writeln!(
            w,
            "Pooled by client (descriptive; members may run different strategies):\n\n\
             | client | validators | disagree pairs | pooled D | labels |\n|---|---|---|---|---|"
        )?;
        for (c, rs) in by_client {
            let (wins, n): (u64, u64) = rs.iter().fold((0, 0), |a, r| {
                (a.0 + r.rule.pairs.prio_wins, a.1 + r.rule.pairs.disagree)
            });
            writeln!(
                w,
                "| {c} | {} | {n} | {} | {} |",
                rs.len(),
                if n == 0 {
                    "–".into()
                } else {
                    format!("{:.2}", wins as f64 / n as f64)
                },
                join(rs.iter().map(|r| r.label), "; ")
            )?;
        }
        writeln!(w)
    }

    fn effects(&self, w: &mut String) -> fmt::Result {
        let rs = &self.results;
        writeln!(
            w,
            "## 3. What changes the order: send gap, slot, locks, send order\n"
        )?;
        if rs.is_empty() {
            return writeln!(w, "No live windows yet.\n");
        }
        writeln!(
            w,
            "**D by send gap** (all disagree pairs, including those sent < {} ms apart). A priority scheduler that batches \
             for B ms shows D≈1 below B and D→0 above it.\n",
            stats::MIN_GAP_MS
        )?;
        let head: Vec<String> = GAP_BUCKETS_MS
            .iter()
            .map(|&(a, b)| bucket_name(a, b))
            .collect();
        writeln!(
            w,
            "| validator | {} |\n|---|{}",
            head.join(" | "),
            "---|".repeat(head.len())
        )?;
        for r in rs {
            let cells = r.gap.iter().map(|(_, e, n)| {
                if *n == 0 {
                    "–".into()
                } else {
                    format!("{e} n={n}")
                }
            });
            writeln!(w, "| {} | {} |", self.who(r), join(cells, " | "))?;
        }
        let cell = |(e, n): &(Est, u64)| {
            if *n == 0 {
                "–".into()
            } else {
                format!("{e} n={n}")
            }
        };
        writeln!(
            w,
            "\n**D by slot of the window.** Transactions that reach a leader before its bank exists are held and scheduled \
             together at the start of the first slot; later ones stream in. Descriptive (pre-registered S4).\n\n\
             | validator | D in the first slot | D in later slots |\n|---|---|---|"
        )?;
        for r in rs {
            writeln!(
                w,
                "| {} | {} | {} |",
                self.who(r),
                cell(&r.first_slot),
                cell(&r.later_slots)
            )?;
        }
        writeln!(
            w,
            "\n**Account conflicts and send order.** Shared = one fee payer for the batch (every pair write-locks it, so the \
             scheduler must serialize them: the cleaner view of its queue order); distinct = one payer per transaction (parallel \
             threads may interleave them, so their order in `getBlock` partly reflects a race). Labels per payer level apply the \
             same rule without Holm correction (descriptive, pre-registered S1).\n\n\
             | validator | shared: D, A, label | distinct: D, A, label | D reverse-order windows | D random-order windows |\n|---|---|---|---|---|"
        )?;
        let level = |s: &Subset| {
            format!(
                "{}, {}, {}",
                s.d,
                s.a,
                s.label.map_or("–".into(), |l| l.to_string())
            )
        };
        for r in rs {
            writeln!(
                w,
                "| {} | {} | {} | {} | {} |",
                self.who(r),
                level(&r.conflict[0]),
                level(&r.conflict[1]),
                r.order[0].d,
                r.order[1].d
            )?;
        }
        writeln!(w)
    }

    fn models(&self, w: &mut String) -> fmt::Result {
        let rs = &self.results;
        writeln!(w, "## 4. Model prediction vs. fact\n")?;
        if rs.is_empty() {
            return writeln!(w, "No live windows yet.\n");
        }
        writeln!(
            w,
            "Share of same-slot pairs predicted correctly by FIFO (send order), by Agave-style priority (reward/cost; our \
             transactions have equal cost, so this is cu_price order), and by the best pairwise batching model \"priority if \
             sent within B ms of each other, send order beyond\". Batch length T: maximum-likelihood fit of \
             P(priority wins | gap g) = ε + (1 − 2ε)·max(0, 1 − g/T), ε = 0.05, over disagree pairs (fixed-length batches \
             at an unknown phase; the best B sits near T/2). The design has no send gaps between ~0.2 and 10 ms, so a fit \
             below 10 ms reads \"≤10 ms\". Exact-priority slots: slots with ≥3 of our transactions in exactly descending fee order.\n\n\
             | validator | FIFO | priority | best pairwise B | its accuracy | batch length T | exact-priority slots |\n|---|---|---|---|---|---|---|"
        )?;
        for r in rs {
            let m = &r.model;
            writeln!(
                w,
                "| {} | {} | {} | {} | {} | {} | {}/{} |",
                self.who(r),
                pct_f(m.fifo),
                pct_f(m.priority),
                m.best_ms.map_or("–".into(), |b| format!("{b} ms")),
                pct_f(m.best),
                m.period_ms.map_or("–".into(), batch_len),
                r.exact_priority.0,
                r.exact_priority.1
            )?;
        }
        writeln!(
            w,
            "\nPairs split across slots: share where the earlier-sent transaction landed in the earlier slot. Arrival time, \
             not the scheduler, decides these, which is why the rule is measured within slots.\n"
        )?;
        for r in rs {
            let (a, n) = r.cross_slot;
            writeln!(
                w,
                "- {}: {a} of {n} ({})",
                self.who(r),
                pct(a as usize, n as usize)
            )?;
        }
        writeln!(w)
    }

    fn evidence(&self, w: &mut String) -> fmt::Result {
        writeln!(w, "## 5. One window per validator, in full\n")?;
        if self.results.is_empty() {
            return writeln!(w, "No live windows yet.\n");
        }
        writeln!(
            w,
            "Ranks on the fee ladder (0 = lowest fee, 9 = highest) in the order we sent them, and in the order the leader put \
             them into one block. Shown per validator: for the shortest and the longest pause, the window with the most of our \
             transactions in one slot, reverse send order preferred. `landed.jsonl` has the signature and block index of every \
             transaction.\n\n\
             | validator | block | send order, pause, payer | sent (ranks) | in the block (ranks) |\n|---|---|---|---|---|"
        )?;
        for r in &self.results {
            if r.evidence.is_empty() {
                writeln!(w, "| {} | – | – | – | none landed |", self.who(r))?;
            }
            for e in &r.evidence {
                writeln!(
                    w,
                    "| {} | {} | {}, {} ms, {} | `{}` | `{}` |",
                    self.who(r),
                    self.block(e.slot),
                    format!("{:?}", e.order).to_lowercase(),
                    e.pause_ms,
                    format!("{:?}", e.conflict).to_lowercase(),
                    join(&e.sent, " "),
                    join(&e.landed, " ")
                )?;
            }
        }
        writeln!(w)
    }

    fn foreign(&self, w: &mut String) -> fmt::Result {
        let d = self.d;
        writeln!(
            w,
            "## 6. Corroboration from other users' transactions (costs 0 SOL)\n"
        )?;
        writeln!(
            w,
            "Every block of a targeted window also holds hundreds of foreign transactions. γ = Goodman–Kruskal gamma between Agave priority \
             (fee minus burnt half of base fee, × 10⁶ / costUnits) and earliness, per block, averaged over blocks (bootstrap CI over blocks). \
             Transactions paying a Jito tip are excluded; the other members of their bundles cannot be told apart in `getBlock` and \
             stay in, pulling γ toward 0 on bundle-heavy leaders. Arrival times of foreign transactions are unknown, so this is \
             supporting evidence only.\n\n\
             | leader | client | blocks | foreign txs | mean γ [95% CI] | Jito-tip tx share | txs/block | our label |\n|---|---|---|---|---|---|---|---|"
        )?;
        let mut leaders: Vec<&str> = d.windows.iter().map(|w| w.leader.as_str()).collect();
        leaders.sort_unstable();
        leaders.dedup();
        for id in leaders {
            let ours = self.results.iter().find(|r| r.identity == id);
            let Some(f) = ours
                .and_then(|r| r.foreign.clone())
                .or_else(|| foreign_for(id, &d.blocks, &d.landed))
            else {
                continue;
            };
            let client = client_group(
                d.windows
                    .iter()
                    .rev()
                    .find(|w| w.leader == id)
                    .and_then(|w| w.client.as_deref()),
            );
            writeln!(
                w,
                "| {} | {client} | {} | {} | {} | {:.1}% | {:.0} | {} |",
                self.account(id),
                f.blocks,
                f.txs,
                f.gamma,
                100.0 * f.tip_share,
                f.mean_block_txs,
                ours.map_or("–".into(), |r| r.label.to_string())
            )?;
        }
        writeln!(w)
    }

    fn calibration(&self, w: &mut String) -> fmt::Result {
        writeln!(w, "## 7. Calibration\n")?;
        writeln!(
            w,
            "- Positive controls, synthetic: priority, FIFO and batched schedulers in `report/power.md` \
             (`solana-tx-scheduler simulate`), through the same classifier and thresholds.\n\
             - Positive control, real: a local Agave validator, `report/localnet/REPORT.md`.\n\
             - Known-rule control on mainnet: vanilla Agave (`clientId=Agave`) validators, whose rule is known from source.\n\
             - Negative control (placebo): does landing order follow the first 8 bytes of the signature? No scheduler should \
             order by them, so a significant result means an artifact in the pipeline (or a hash tie-break), not a finding. \
             Same-slot pairs, {} within-slot permutations, Holm across validators.\n",
            stats::PERMUTATIONS
        )?;
        if self.results.is_empty() {
            return Ok(());
        }
        writeln!(
            w,
            "| validator | same-slot pairs | landed in signature order | p (Holm) | placebo |\n|---|---|---|---|---|"
        )?;
        for r in &self.results {
            writeln!(
                w,
                "| {} | {} | {} | {:.4} | {} |",
                self.who(r),
                r.placebo.pairs,
                pct_f(r.placebo.share),
                r.placebo_p_holm,
                if r.placebo_p_holm < stats::ALPHA {
                    "**rule found: check the pipeline**"
                } else {
                    "no rule"
                }
            )?;
        }
        writeln!(w)
    }

    fn spend(&self, w: &mut String) -> fmt::Result {
        let (ledger, landed) = (&self.d.ledger, &self.d.landed);
        writeln!(w, "## 8. Spend reconciliation\n")?;
        if ledger.is_empty() {
            return writeln!(w, "No live spend recorded.\n");
        }
        let mut by_kind: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
        for e in ledger.iter().filter(|e| e.kind != "balance") {
            let x = by_kind.entry(e.kind.as_str()).or_default();
            x.0 += 1;
            x.1 += e.fee_lamports;
        }
        let reserved: u64 = by_kind.values().map(|x| x.1).sum();
        let actual: u64 = landed.iter().filter_map(|l| l.fee).sum();
        writeln!(
            w,
            "| ledger kind | entries | worst-case lamports reserved |\n|---|---|---|"
        )?;
        for (k, (n, l)) in &by_kind {
            writeln!(w, "| {k} | {n} | {l} |")?;
        }
        writeln!(
            w,
            "| **total** | | **{reserved}** (cap {HARD_CAP_LAMPORTS}) |\n\n\
             Actual fees of our measurement transactions, summed from `meta.fee` of the landed ones: **{actual}** lamports.\n"
        )?;
        let mut runs: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for e in ledger.iter().filter(|e| e.kind == "balance") {
            runs.entry(e.run_id.as_str()).or_default().extend(e.balance);
        }
        writeln!(
            w,
            "| run | wallet total at start | at end | on-chain spend | reserved in run |\n|---|---|---|---|---|"
        )?;
        let mut outside: Vec<String> = Vec::new();
        for (run, b) in &runs {
            let res: u64 = ledger
                .iter()
                .filter(|e| e.run_id == *run && e.kind != "balance")
                .map(|e| e.fee_lamports)
                .sum();
            let s = b.first().copied().unwrap_or(0);
            let Some(&e) = b.get(1..).and_then(<[u64]>::last) else {
                writeln!(
                    w,
                    "| {run} | {s} | – (no closing balance: the run crashed) | – | {res} |"
                )?;
                continue;
            };
            writeln!(w, "| {run} | {s} | {e} | {} | {res} |", s.saturating_sub(e))?;
            // Fees can only undershoot the reservation; more means money left by another path.
            if s.saturating_sub(e) > res {
                outside.push(format!(
                    "run {run}: on-chain spend exceeds what was reserved by {} lamports",
                    s - e - res
                ));
            }
        }
        writeln!(w)?;
        for o in &outside {
            writeln!(
                w,
                "- **{o}**: lamports left the wallets outside this tool's ledger."
            )?;
        }
        if !outside.is_empty() {
            writeln!(w)?;
        }
        Ok(())
    }

    fn went_wrong(&self, w: &mut String) -> fmt::Result {
        let (windows, landed, blocks) = (&self.d.windows, &self.d.landed, &self.d.blocks);
        writeln!(w, "## 9. What went wrong\n")?;
        writeln!(w, "Automatically extracted from the raw data:\n")?;
        let mut lines: Vec<String> = Vec::new();
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for x in windows.iter().filter_map(|x| x.skipped.as_ref()) {
            let key = x.split(':').next().unwrap_or(x).to_string();
            *reasons.entry(key).or_default() += 1;
        }
        for (r, n) in &reasons {
            lines.push(format!("{n} window(s) skipped: {r}."));
        }
        let mut quic: BTreeMap<String, usize> = BTreeMap::new();
        for x in windows.iter().filter_map(|x| x.quic_error.as_ref()) {
            *quic.entry(x.chars().take(80).collect()).or_default() += 1;
        }
        for (e, n) in &quic {
            lines.push(format!("QUIC handshake failed {n}×: `{e}`."));
        }
        let send_errors = windows
            .iter()
            .flat_map(|x| &x.txs)
            .filter(|t| t.send_error.is_some())
            .count();
        if send_errors > 0 {
            lines.push(format!(
                "{send_errors} transaction(s) failed to send over QUIC."
            ));
        }
        let missing = landed.iter().filter(|l| l.slot.is_none()).count();
        if missing > 0 {
            lines.push(format!(
                "{missing} of {} sent transactions never landed (dropped before or by the leader).",
                landed.len()
            ));
        }
        let by_window: HashMap<u64, &WindowRecord> = windows
            .iter()
            .filter(|x| x.live)
            .map(|x| (x.start_slot, x))
            .collect();
        let mut elsewhere: BTreeMap<String, usize> = BTreeMap::new();
        for l in landed {
            if let (Some(s), Some(win)) = (l.slot, by_window.get(&l.window_start))
                && !led_by(blocks, s, &win.leader)
            {
                let leader = blocks.get(&s).map_or("?", |b| b.leader.as_str());
                *elsewhere
                    .entry(format!(
                        "target `{}` → slot of `{}`",
                        short(&win.leader),
                        short(leader)
                    ))
                    .or_default() += 1;
            }
        }
        for (k, n) in &elsewhere {
            lines.push(format!(
                "{n} transaction(s) landed in another leader's block: {k}."
            ));
        }
        let failed: Vec<&Landed> = landed.iter().filter(|l| l.err.is_some()).collect();
        if let Some(f) = failed.first().and_then(|l| l.err.as_ref()) {
            lines.push(format!(
                "{} transaction(s) landed but failed, e.g. `{f}`.",
                failed.len()
            ));
        }
        let skipped_slots = blocks
            .values()
            .filter(|b| b.status.starts_with("skipped"))
            .count();
        if skipped_slots > 0 {
            lines.push(format!(
                "{skipped_slots} targeted slot(s) produced no block (skipped)."
            ));
        }
        let unlocated = landed
            .iter()
            .filter(|l| l.slot.is_some() && l.index.is_none())
            .count();
        if unlocated > 0 {
            lines.push(format!(
                "{unlocated} landed transaction(s) could not be located in an archived block (re-run `collect`)."
            ));
        }
        if lines.is_empty() {
            lines.push("nothing detected.".into());
        }
        for l in &lines {
            writeln!(w, "- {l}")?;
        }
        writeln!(
            w,
            "\nNotes written during the experiment (`NOTES.md` in the data directory):\n\n{}\n",
            self.d
                .notes
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("_none_")
        )
    }

    fn hashes(&self, w: &mut String) -> fmt::Result {
        let p = &self.d.paths;
        writeln!(
            w,
            "## Appendix A: raw files and on-chain anchors\n\n| raw file | sha256 |\n|---|---|"
        )?;
        let manifest = p.archive.join(blocks::MANIFEST);
        for f in [
            &p.plan, &p.windows, &p.landed, &p.blocks, &p.ledger, &p.anchors, &manifest,
        ] {
            writeln!(w, "| `{}` | `{}` |", f.display(), file_hash(f))?;
        }
        writeln!(w)?;
        if self.d.anchors.is_empty() {
            return Ok(());
        }
        writeln!(
            w,
            "Memo transactions holding a file's sha256 (`sha256sum <file>` must match):\n\n\
             | when | file | sha256 | transaction |\n|---|---|---|---|"
        )?;
        for a in &self.d.anchors {
            writeln!(
                w,
                "| {} | `{}` | `{}` | {} |",
                a.at,
                a.file,
                a.sha256,
                a.signature
                    .as_deref()
                    .map_or("dry-run".into(), |s| self.tx(s))
            )?;
        }
        writeln!(w)
    }

    fn appendix(&self, w: &mut String) -> fmt::Result {
        if self.live.is_empty() {
            return Ok(());
        }
        writeln!(
            w,
            "## Appendix B: every live measurement\n\n\
             Any row can be checked with `getBlock(<slot>)`: `landed.jsonl` gives each signature's slot and index.\n\n\
             | fired (UTC) | window start slot | leader | client | order | pause ms | payer | sent | in leader's blocks | landed slots |\n|---|---|---|---|---|---|---|---|---|---|"
        )?;
        for x in &self.live {
            let landed: Vec<(u64, usize)> = x
                .txs
                .iter()
                .filter_map(|t| {
                    self.by_sig
                        .get(t.signature.as_str())
                        .and_then(|l| l.slot.zip(l.index))
                })
                .collect();
            let in_window = landed
                .iter()
                .filter(|(s, _)| led_by(&self.d.blocks, *s, &x.leader))
                .count();
            let mut slots: Vec<u64> = landed.iter().map(|(s, _)| *s).collect();
            slots.sort_unstable();
            slots.dedup();
            writeln!(
                w,
                "| {} | {} | `{}` | {} | {} | {} | {} | {} | {} | {} |",
                utc((x.fire_at_us / 1_000_000) as i64),
                x.start_slot,
                short(&x.leader),
                client_group(x.client.as_deref()),
                format!("{:?}", x.treatment.order).to_lowercase(),
                x.treatment.pause_ms,
                format!("{:?}", x.treatment.conflict).to_lowercase(),
                x.txs.len(),
                in_window,
                if slots.is_empty() {
                    "–".into()
                } else {
                    join(&slots, ", ")
                }
            )?;
        }
        Ok(())
    }
}
