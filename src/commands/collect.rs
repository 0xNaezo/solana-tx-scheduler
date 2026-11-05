use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    str::FromStr,
    time::Duration,
};

use anyhow::Result;
use solana_transaction::Signature;
use tracing::{info, warn};

use crate::{
    blocks::{self, first_signatures, summarize},
    cli::{CollectArgs, Context},
    model::{BlockSummary, Landed, Paths, WindowRecord, read_jsonl, redact, write_jsonl},
    rpc::{self, BlockFetch},
};

/// Pause between getBlock calls; public RPCs rate-limit them hard.
const BLOCK_DELAY: Duration = Duration::from_millis(150);

pub async fn execute(ctx: &Context, args: CollectArgs) -> Result<()> {
    let paths = Paths::new(&ctx.data_dir);
    let windows: Vec<WindowRecord> = read_jsonl(&paths.windows)?;
    let rpc = rpc::client(&ctx.rpc_url);
    info!(rpc = %redact(&ctx.rpc_url), windows = windows.len(), "collect");

    // Where did each of our transactions land (any slot, not only the target window)?
    let ours: Vec<(&WindowRecord, String)> = windows
        .iter()
        .filter(|w| w.live && w.skipped.is_none())
        .flat_map(|w| {
            w.txs
                .iter()
                .filter(|t| t.sent_at_us.is_some() && t.send_error.is_none())
                .map(move |t| (w, t.signature.clone()))
        })
        .collect();
    let mut landed_slot: HashMap<String, u64> = HashMap::new();
    for chunk in ours.chunks(256) {
        let sigs: Vec<Signature> = chunk
            .iter()
            .map(|(_, s)| Signature::from_str(s))
            .collect::<Result<_, _>>()?;
        let statuses = rpc.get_signature_statuses_with_history(&sigs).await?.value;
        for ((_, sig), st) in chunk.iter().zip(statuses) {
            if let Some(st) = st {
                landed_slot.insert(sig.clone(), st.slot);
            }
        }
    }
    info!(
        sent = ours.len(),
        landed = landed_slot.len(),
        "signature statuses"
    );

    // Blocks: every slot of every targeted window + every slot where ours landed.
    let mut leader_of: BTreeMap<u64, String> = BTreeMap::new();
    for w in windows.iter().filter(|w| w.skipped.is_none()) {
        for s in w.start_slot..w.start_slot + w.num_slots {
            leader_of.insert(s, w.leader.clone());
        }
    }
    let extra: BTreeSet<u64> = landed_slot
        .values()
        .filter(|s| !leader_of.contains_key(s))
        .copied()
        .collect();
    for s in extra {
        let leader = rpc
            .get_slot_leaders(s, 1)
            .await
            .ok()
            .and_then(|v| v.first().map(|p| p.to_string()))
            .unwrap_or_default();
        leader_of.insert(s, leader);
    }

    // A slot where one of ours landed holds a block, whatever an RPC answers.
    let ours_in: BTreeSet<u64> = landed_slot.values().copied().collect();
    let mut summaries = blocks::read_summaries(&paths.blocks)?;
    let (mut fetched, mut pending) = (0, 0);
    for (&slot, leader) in &leader_of {
        let have = summaries.get(&slot).is_some_and(|b| {
            (b.status.starts_with("skipped") && !ours_in.contains(&slot))
                || blocks::archive_file(&paths.archive, slot).exists()
        });
        if have && !args.refetch {
            continue;
        }
        tokio::time::sleep(BLOCK_DELAY).await;
        let summary = match rpc::get_block_raw(&rpc, slot, "full").await {
            Ok(BlockFetch::Block(b)) => {
                blocks::save_raw(&paths.archive, slot, &b)?;
                fetched += 1;
                summarize(slot, leader, &b)
            }
            // "Skipped" also covers "missing in this node's long-term storage", so it never
            // overrides a block we already had or a slot where one of ours landed.
            Ok(BlockFetch::Skipped(m))
                if ours_in.contains(&slot)
                    || summaries.get(&slot).is_some_and(|b| b.status == "ok") =>
            {
                pending += 1;
                warn!(
                    slot,
                    "RPC says skipped ({m}) but the block exists; re-run collect later or with another RPC"
                );
                continue;
            }
            Ok(BlockFetch::Skipped(m)) => empty(slot, leader, format!("skipped: {m}")),
            Ok(BlockFetch::NotYet(m)) => {
                pending += 1;
                warn!(slot, "block not available yet ({m}); re-run collect later");
                continue;
            }
            Err(e) => {
                pending += 1;
                warn!(slot, "getBlock failed: {e:#}");
                continue;
            }
        };
        summaries.insert(slot, summary);
        if fetched % 25 == 1 {
            info!(slot, fetched, total = leader_of.len(), "blocks");
        }
    }
    blocks::write_summaries(&paths.blocks, &summaries)?;
    if paths.archive.exists() {
        blocks::write_manifest(&paths.archive)?;
    }

    // Position of each of our transactions inside its block.
    let previous: HashMap<String, Landed> = read_jsonl::<Landed>(&paths.landed)?
        .into_iter()
        .map(|l| (l.signature.clone(), l))
        .collect();
    let mut landed = Vec::new();
    // Only the fields used below: a parsed mainnet block takes tens of MB, and caching
    // whole blocks for every landed slot ran out of memory on the mainnet run.
    type TxMeta = (String, serde_json::Value, Option<u64>, Option<u64>);
    let mut cache: HashMap<u64, Option<Vec<TxMeta>>> = HashMap::new();
    for (w, sig) in &ours {
        let mut row = Landed {
            signature: sig.clone(),
            window_start: w.start_slot,
            slot: landed_slot.get(sig).copied(),
            index: None,
            block_txs: None,
            err: None,
            fee: None,
            cu_consumed: None,
        };
        if let Some(slot) = row.slot {
            let block = cache.entry(slot).or_insert_with(|| {
                let b = blocks::load_raw(&paths.archive, slot).ok().flatten()?;
                let txs = first_signatures(&b)
                    .into_iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let meta = &b["transactions"][i]["meta"];
                        (
                            s,
                            meta["err"].clone(),
                            meta["fee"].as_u64(),
                            meta["computeUnitsConsumed"].as_u64(),
                        )
                    })
                    .collect();
                Some(txs)
            });
            if let Some(txs) = block {
                row.block_txs = Some(txs.len());
                if let Some(i) = txs.iter().position(|t| t.0 == *sig) {
                    let (_, err, fee, cu) = &txs[i];
                    row.index = Some(i);
                    row.err = Some(err.clone()).filter(|e| !e.is_null());
                    row.fee = *fee;
                    row.cu_consumed = *cu;
                }
            }
        }
        // Without the archive (fresh clone), or once the RPC forgot a status, keep what an
        // earlier collect located rather than overwrite it with nothing.
        if row.index.is_none()
            && let Some(prev) = previous.get(sig).filter(|p| p.index.is_some())
        {
            row = prev.clone();
        }
        landed.push(row);
    }
    write_jsonl(&paths.landed, &landed)?;
    info!(
        blocks_fetched = fetched,
        blocks_pending = pending,
        blocks_total = summaries.len(),
        ours = landed.len(),
        located = landed.iter().filter(|l| l.index.is_some()).count(),
        "collect finished"
    );
    Ok(())
}

fn empty(slot: u64, leader: &str, status: String) -> BlockSummary {
    BlockSummary {
        slot,
        leader: leader.to_string(),
        status,
        blockhash: None,
        parent_slot: None,
        block_time: None,
        txs: Vec::new(),
    }
}
