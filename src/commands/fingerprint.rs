//! Read-only "ordering card" for any validator, from its most recent blocks:
//! client and route from gossip, priority-vs-position of other users'
//! transactions, bundle share, and our own label if we measured it.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;
use solana_commitment_config::CommitmentConfig;

use crate::{
    blocks::{col, foreign_gamma, summarize},
    cli::{Context, FingerprintArgs},
    commands::plan::route,
    rpc::{self, BlockFetch},
    stats::{bootstrap_mean, seed_of},
};

pub async fn execute(ctx: &Context, args: FingerprintArgs) -> Result<()> {
    let rpc = rpc::client(&ctx.rpc_url);
    let id = args.validator.as_str();
    let node = rpc
        .get_cluster_nodes()
        .await?
        .into_iter()
        .find(|n| n.pubkey == id);
    let client = node
        .as_ref()
        .and_then(|n| n.client_id.clone())
        .unwrap_or_else(|| "unknown".into());

    let info = rpc.get_epoch_info().await?;
    let now = rpc
        .get_slot_with_commitment(CommitmentConfig::confirmed())
        .await?;
    let first = info.absolute_slot - info.slot_index;
    let mut slots: Vec<u64> = rpc::leader_windows(&rpc, &[id.to_string()], first)
        .await?
        .iter()
        .flat_map(|w| w.start..w.start + w.len)
        .filter(|&s| s + 2 < now)
        .collect();
    slots.reverse();
    if slots.is_empty() {
        bail!("{id} has not led any slot yet this epoch");
    }

    let (mut gammas, mut tips, mut txs, mut read, mut skipped) = (Vec::new(), 0u64, 0usize, 0, 0);
    for &slot in &slots {
        if read >= args.blocks {
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let block: Value = match rpc::get_block_raw(&rpc, slot, "full").await? {
            BlockFetch::Block(b) => b,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let s = summarize(slot, id, &block);
        if let Some((g, _)) = foreign_gamma(&s, &[]) {
            gammas.push(g);
        }
        tips += s.txs.iter().map(|r| r[col::TIP]).sum::<u64>();
        txs += s.txs.len();
        read += 1;
    }
    let g = bootstrap_mean(&gammas, seed_of(id));
    let verdict = match g.value {
        v if v.is_nan() => "no data",
        v if g.lo > 0.5 || v >= 0.6 => {
            "strong priority signal (consistent with reward/cost ordering)"
        }
        _ if g.lo > 0.0 => "weak priority signal (priority matters, but not alone)",
        v if v.abs() < 0.2 => {
            "no priority signal (arrival order, external sequencer or other rule)"
        }
        _ => "unclear",
    };

    println!("## Ordering fingerprint `{id}`\n");
    println!(
        "- client (gossip clientId): {client}, version {}",
        node.as_ref()
            .and_then(|n| n.version.clone())
            .unwrap_or_default()
    );
    println!(
        "- TPU: {} → route: {}",
        node.as_ref()
            .and_then(|n| n.tpu_quic)
            .map_or("none".into(), |a| a.to_string()),
        node.as_ref()
            .map_or("unknown".into(), |n| route(&client, n))
    );
    println!(
        "- blocks read: {read} (most recent of epoch {}, {skipped} skipped/unavailable)",
        info.epoch
    );
    println!(
        "- txs/block: {:.0}; Jito-tip (bundle) tx share: {:.1}%",
        txs as f64 / read.max(1) as f64,
        100.0 * tips as f64 / txs.max(1) as f64
    );
    println!("- γ(priority, earliness) of non-bundle txs: {g}");
    println!("- reading: {verdict}");
    println!(
        "\nγ uses other users' transactions whose arrival times are unknown: it shows whether priority predicts position, \
         not why. For a controlled measurement run `plan --validators {id}` + `run --live`."
    );
    Ok(())
}
