use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use serde::Serialize;
use solana_epoch_info::EpochInfo;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::response::{
    RpcBlockProduction, RpcContactInfo, RpcLeaderSchedule, RpcPerfSample, RpcVoteAccountStatus,
};
use tracing::{info, warn};

use crate::{
    budget::HARD_CAP_LAMPORTS,
    cli::{Context, PlanArgs},
    model::{Design, Paths, Plan, Target, now_utc, redact, write_json},
    rpc::{self, LEADER_WINDOW, group_runs, mean_slot},
    tx::{ladder, worst_case_fee},
};

/// Raw RPC state that validator selection is computed from.
pub struct Snapshot {
    pub epoch_info: EpochInfo,
    pub cluster_nodes: Vec<RpcContactInfo>,
    pub vote_accounts: RpcVoteAccountStatus,
    pub block_production: RpcBlockProduction,
    pub perf_samples: Vec<RpcPerfSample>,
    pub leader_schedule: RpcLeaderSchedule,
}

pub async fn fetch_snapshot(rpc: &RpcClient) -> Result<Snapshot> {
    let (epoch_info, cluster_nodes, vote_accounts, block_production, perf_samples, schedule) =
        tokio::try_join!(
            rpc.get_epoch_info(),
            rpc.get_cluster_nodes(),
            rpc.get_vote_accounts(),
            rpc.get_block_production(),
            rpc.get_recent_performance_samples(Some(5)),
            rpc.get_leader_schedule(None),
        )
        .context("fetch RPC snapshot")?;

    Ok(Snapshot {
        epoch_info,
        cluster_nodes,
        vote_accounts,
        // Default range is the current epoch so far.
        block_production: block_production.value,
        perf_samples,
        leader_schedule: schedule.unwrap_or_default(),
    })
}

/// Writes every response as JSON into `<data_dir>/snapshots/<unix_ms>/`.
pub fn save_snapshot(snapshot: &Snapshot, dir: &Path) -> Result<PathBuf> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let dir = dir.join(ts.to_string());
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    write_snapshot_json(&dir, "epoch_info", &snapshot.epoch_info)?;
    write_snapshot_json(&dir, "cluster_nodes", &snapshot.cluster_nodes)?;
    write_snapshot_json(&dir, "vote_accounts", &snapshot.vote_accounts)?;
    write_snapshot_json(&dir, "block_production", &snapshot.block_production)?;
    write_snapshot_json(&dir, "perf_samples", &snapshot.perf_samples)?;
    write_snapshot_json(&dir, "leader_schedule", &snapshot.leader_schedule)?;
    Ok(dir)
}

fn write_snapshot_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let path = dir.join(format!("{name}.json"));
    fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

/// Client group from gossip `clientId`. Unrecognised ids (`Unknown(N)`) are
/// one group: N is not stable for the same node between gossip reads.
pub fn client_group(id: Option<&str>) -> String {
    match id {
        Some(c) if !c.starts_with("Unknown") => c.to_string(),
        _ => "unknown".into(),
    }
}

/// Bounty minimum for a mainnet plan.
const MIN_VALIDATORS: usize = 6;
const MIN_CLIENT_GROUPS: usize = 3;
/// TPU port of a Jito relayer co-located with the validator.
const JITO_RELAYER_TPU_PORT: u16 = 11228;
/// TPU port of a BAM node, which sequences transactions for the validator.
const BAM_NODE_TPU_PORT: u16 = 5010;

/// Who actually receives packets sent to the advertised TPU.
pub fn route(client: &str, node: &RpcContactInfo) -> String {
    let (Some(tpu), Some(gossip)) = (node.tpu_quic, node.gossip) else {
        return if node.tpu_quic.is_some() {
            "direct"
        } else {
            "no-tpu"
        }
        .into();
    };
    if tpu.ip() == gossip.ip() {
        return if tpu.port() == JITO_RELAYER_TPU_PORT {
            "direct (jito-relayer port)"
        } else {
            "direct"
        }
        .into();
    }
    if tpu.port() == BAM_NODE_TPU_PORT {
        "bam-node".into()
    } else if client.starts_with("Harmonic") {
        "harmonic-proxy".into()
    } else {
        "foreign-ip".into()
    }
}

/// Candidates with every filter applied, before group selection.
fn candidates(s: &Snapshot, args: &PlanArgs) -> (Vec<Target>, Vec<(String, String)>, u64) {
    let nodes: HashMap<&str, &RpcContactInfo> = s
        .cluster_nodes
        .iter()
        .map(|n| (n.pubkey.as_str(), n))
        .collect();
    let total: u64 = s
        .vote_accounts
        .current
        .iter()
        .chain(&s.vote_accounts.delinquent)
        .map(|v| v.activated_stake)
        .sum();
    let first_slot = s.epoch_info.absolute_slot - s.epoch_info.slot_index;
    let explicit = !args.validators.is_empty();

    let mut out = Vec::new();
    let mut rejected = Vec::new();
    for v in &s.vote_accounts.current {
        if explicit && !args.validators.contains(&v.node_pubkey) {
            continue;
        }
        let Some(node) = nodes.get(v.node_pubkey.as_str()) else {
            rejected.push((v.node_pubkey.clone(), "not in gossip".into()));
            continue;
        };
        let client = client_group(node.client_id.as_deref());
        let stake_pct = 100.0 * v.activated_stake as f64 / total.max(1) as f64;
        let (leader_slots, produced) = s
            .block_production
            .by_identity
            .get(&v.node_pubkey)
            .copied()
            .unwrap_or((0, 0));
        let skip_rate = if leader_slots == 0 {
            0.0
        } else {
            1.0 - produced as f64 / leader_slots as f64
        };
        let left: Vec<u64> = s
            .leader_schedule
            .get(&v.node_pubkey)
            .map(|idx| {
                idx.iter()
                    .map(|&i| first_slot + i as u64)
                    .filter(|&slot| slot > s.epoch_info.absolute_slot)
                    .collect()
            })
            .unwrap_or_default();
        let windows_left = group_runs(&v.node_pubkey, &left).len();

        let reason = if stake_pct < args.min_stake_pct && !explicit {
            Some(format!("stake {stake_pct:.3}% < {}%", args.min_stake_pct))
        } else if skip_rate > args.max_skip_rate && !explicit {
            Some(format!("skip rate {:.1}%", skip_rate * 100.0))
        } else if node.tpu_quic.is_none() {
            Some("no tpuQuic in gossip".into())
        } else if windows_left < 5 {
            Some(format!("{windows_left} leader windows left in epoch"))
        } else {
            None
        };
        if let Some(r) = reason {
            rejected.push((v.node_pubkey.clone(), r));
            continue;
        }
        out.push(Target {
            identity: v.node_pubkey.clone(),
            vote: v.vote_pubkey.clone(),
            route: route(&client, node),
            client,
            version: node.version.clone().unwrap_or_default(),
            stake_lamports: v.activated_stake,
            stake_pct,
            skip_rate,
            tpu_quic: node.tpu_quic.map(|a| a.to_string()),
            gossip: node.gossip.map(|a| a.to_string()),
            leader_slots_left: left.len(),
        });
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.stake_lamports));
    (out, rejected, total)
}

pub async fn execute(ctx: &Context, args: PlanArgs) -> Result<()> {
    info!(rpc = %redact(&ctx.rpc_url), "plan");
    if args.txs_per_window < 2 || args.pauses_ms.is_empty() {
        bail!("need --txs-per-window >= 2 and at least one pause level");
    }
    let paths = Paths::new(&ctx.data_dir);
    let rpc = rpc::client(&ctx.rpc_url);
    let cluster = rpc::cluster_name(&rpc).await?;
    let snapshot = fetch_snapshot(&rpc).await?;
    let dir = save_snapshot(&snapshot, &paths.snapshots)?;
    info!(
        epoch = snapshot.epoch_info.epoch,
        slot = snapshot.epoch_info.absolute_slot,
        nodes = snapshot.cluster_nodes.len(),
        dir = %dir.display(),
        "snapshot saved"
    );

    let (cands, rejected, total_stake) = candidates(&snapshot, &args);
    let mut by_group: BTreeMap<String, Vec<Target>> = BTreeMap::new();
    for t in cands {
        by_group.entry(t.client.clone()).or_default().push(t);
    }
    let explicit = !args.validators.is_empty();
    let selected: Vec<Target> = by_group
        .iter()
        // Unknown clients cannot be attributed to an implementation: only on
        // request, or when nothing else exists (a local test validator).
        .filter(|(g, _)| {
            explicit
                || args.groups.contains(g)
                || (args.groups.is_empty() && (*g != "unknown" || by_group.len() == 1))
        })
        .flat_map(|(_, ts)| {
            ts.iter()
                .take(if explicit { usize::MAX } else { args.per_group })
                .cloned()
        })
        .collect();
    for (id, why) in rejected
        .iter()
        .filter(|(id, _)| args.validators.contains(id))
    {
        warn!(%id, %why, "requested validator rejected");
    }
    if selected.is_empty() {
        for (id, why) in rejected.iter().take(10) {
            warn!(%id, %why, "rejected");
        }
        bail!(
            "no validator passed the filters ({} rejected)",
            rejected.len()
        );
    }

    let mut groups: Vec<String> = selected.iter().map(|t| t.client.clone()).collect();
    groups.sort_unstable();
    groups.dedup();
    if cluster == "mainnet-beta"
        && (selected.len() < MIN_VALIDATORS || groups.len() < MIN_CLIENT_GROUPS)
    {
        bail!(
            "{} validators in {} client groups: a mainnet plan needs at least {MIN_VALIDATORS} validators in {MIN_CLIENT_GROUPS} groups",
            selected.len(),
            groups.len()
        );
    }

    let seed = args.seed.unwrap_or_else(rand::random);
    let design = Design {
        seed,
        txs_per_window: args.txs_per_window,
        windows_per_validator: args.windows,
        cu_limit: args.cu_limit,
        cu_price_max: args.cu_price_max,
        pauses_ms: args.pauses_ms.clone(),
        lead_slots: args.lead_slots,
    };
    let per_window: u64 = ladder(design.txs_per_window, design.cu_price_max)
        .iter()
        .map(|&p| worst_case_fee(design.cu_limit, p))
        .sum();
    let worst = per_window * (selected.len() * design.windows_per_validator) as u64;

    let plan = Plan {
        created_at: now_utc(),
        cluster,
        epoch: snapshot.epoch_info.epoch,
        slot: snapshot.epoch_info.absolute_slot,
        snapshot_dir: dir.display().to_string(),
        total_stake_lamports: total_stake,
        design,
        validators: selected,
    };
    write_json(&paths.plan, &plan)?;

    // Stake share of every client group (for "results cover X% of stake").
    let mut group_stake: BTreeMap<String, f64> = BTreeMap::new();
    let nodes: HashMap<&str, &RpcContactInfo> = snapshot
        .cluster_nodes
        .iter()
        .map(|n| (n.pubkey.as_str(), n))
        .collect();
    for v in &snapshot.vote_accounts.current {
        let g = client_group(
            nodes
                .get(v.node_pubkey.as_str())
                .and_then(|n| n.client_id.as_deref()),
        );
        *group_stake.entry(g).or_default() += 100.0 * v.activated_stake as f64 / total_stake as f64;
    }

    println!(
        "\n{:<46} {:<20} {:<12} {:>7} {:>6} {:<22} {:>6}",
        "identity", "client", "version", "stake%", "skip%", "route", "slots"
    );
    for t in &plan.validators {
        println!(
            "{:<46} {:<20} {:<12} {:>7.3} {:>6.2} {:<22} {:>6}",
            t.identity,
            t.client,
            t.version,
            t.stake_pct,
            t.skip_rate * 100.0,
            t.route,
            t.leader_slots_left
        );
    }
    let covered: f64 = plan.validators.iter().map(|t| t.stake_pct).sum();
    let group_cover: f64 = groups
        .iter()
        .map(|g| group_stake.get(g).copied().unwrap_or(0.0))
        .sum();
    println!(
        "\n{} validators, {} client groups; selected stake {covered:.2}%, their groups hold {group_cover:.2}% of stake",
        plan.validators.len(),
        groups.len()
    );
    println!(
        "group stake shares: {}",
        group_stake
            .iter()
            .map(|(g, s)| format!("{g} {s:.2}%"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "worst-case fees: {worst} lamports ({per_window} per window), hard cap {HARD_CAP_LAMPORTS}"
    );
    // Hours until a validator has had its quota of windows, at its rate in this epoch.
    let epoch_left = snapshot.epoch_info.slots_in_epoch - snapshot.epoch_info.slot_index;
    let slot_s = mean_slot(&snapshot.perf_samples).as_secs_f64();
    let hours = |t: &Target| {
        (plan.design.windows_per_validator as u64 * LEADER_WINDOW * epoch_left) as f64 * slot_s
            / t.leader_slots_left.max(1) as f64
            / 3600.0
    };
    if let Some(slow) = plan
        .validators
        .iter()
        .max_by(|a, b| hours(a).total_cmp(&hours(b)))
    {
        println!(
            "expected run time: ~{:.1} h, set by the slowest validator {} ({}); `run --max-minutes` caps it",
            hours(slow),
            slow.identity,
            slow.client
        );
    }
    if worst > HARD_CAP_LAMPORTS {
        warn!("planned worst case exceeds the cap: `run` will stop when the budget is exhausted");
    }
    println!("plan written to {}", paths.plan.display());
    Ok(())
}
