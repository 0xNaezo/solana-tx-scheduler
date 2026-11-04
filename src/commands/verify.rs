//! Independent check of the raw data against the chain: every recorded
//! (signature, slot, index) and, with --full, every reduced block row.

use std::{collections::BTreeSet, fs, time::Duration};

use anyhow::{Result, bail};
use serde_json::Value;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use tracing::info;

use crate::{
    blocks::{self, first_signatures, sha256_hex, summarize},
    cli::{Context, VerifyArgs},
    model::{Landed, Paths, read_jsonl, redact},
    rpc::{self, BlockFetch},
};

pub async fn execute(ctx: &Context, args: VerifyArgs) -> Result<()> {
    let paths = Paths::new(&ctx.data_dir);
    let landed: Vec<Landed> = read_jsonl(&paths.landed)?;
    let summaries = blocks::read_summaries(&paths.blocks)?;
    let mut problems: Vec<String> = Vec::new();

    // 1. Archive files match the manifest (whose hash can be anchored on-chain).
    let manifest = paths.archive.join(blocks::MANIFEST);
    let (mut hashed, mut absent) = (0, 0);
    let any_block = summaries.values().any(|b| b.status == "ok");
    match fs::read_to_string(&manifest) {
        Err(_) if args.offline && any_block => {
            problems.push(format!("no {} next to the archive", blocks::MANIFEST))
        }
        Err(_) => {}
        Ok(text) => {
            for line in text.lines() {
                let Some((hash, name)) = line.split_once("  ") else {
                    continue;
                };
                match fs::read(paths.archive.join(name)) {
                    Ok(bytes) if sha256_hex(&bytes) == hash => hashed += 1,
                    Ok(_) => problems.push(format!("archive {name}: hash differs from manifest")),
                    Err(_) => absent += 1,
                }
            }
        }
    }
    info!(hashed, absent, "archive vs manifest");
    if args.offline && absent > 0 {
        problems.push(format!("{absent} archive files in the manifest are absent"));
    }
    let source = if args.offline {
        "the local archive"
    } else {
        "the RPC"
    };

    // 2. Our signatures sit at the recorded (slot, index).
    let mut slots: BTreeSet<u64> = landed.iter().filter_map(|l| l.slot).collect();
    if slots.is_empty() && hashed == 0 {
        bail!(
            "nothing to verify: no landed transactions in {} and no archived blocks",
            paths.landed.display()
        );
    }
    if let Some(n) = args.sample.filter(|&n| n > 0 && n < slots.len()) {
        // Evenly spaced, so the check covers the whole run and is repeatable.
        let all: Vec<u64> = slots.iter().copied().collect();
        slots = (0..n).map(|k| all[k * all.len() / n]).collect();
    }
    let mut checked = 0;
    if !args.offline {
        info!(rpc = %redact(&ctx.rpc_url), slots = slots.len(), "re-fetching blocks");
    }
    let rpc = rpc::client(&ctx.rpc_url);
    for &slot in &slots {
        let details = if args.full { "full" } else { "signatures" };
        let block = match fetch(&rpc, &paths, slot, args.offline, details).await? {
            Ok(b) => b,
            Err(why) => {
                problems.push(format!("slot {slot}: {why}"));
                continue;
            }
        };
        let sigs = first_signatures(&block);
        for l in landed.iter().filter(|l| l.slot == Some(slot)) {
            match l.index {
                Some(i) if sigs.get(i) == Some(&l.signature) => checked += 1,
                Some(i) => {
                    problems.push(format!("{} not at index {i} of slot {slot}", l.signature))
                }
                None => problems.push(format!("{} landed in {slot} but has no index", l.signature)),
            }
            if l.block_txs.is_some_and(|n| n != sigs.len()) {
                problems.push(format!(
                    "slot {slot}: block has {} txs, recorded {:?}",
                    sigs.len(),
                    l.block_txs
                ));
            }
        }
        if args.full
            && let Some(stored) = summaries.get(&slot)
            && summarize(slot, &stored.leader, &block) != *stored
        {
            problems.push(format!(
                "slot {slot}: reduced block row differs from {source}"
            ));
        }
    }

    // 3. With --full, also every block that holds no transaction of ours (foreign-tx evidence).
    if args.full && args.sample.is_none() {
        for (slot, stored) in summaries
            .iter()
            .filter(|(s, b)| !slots.contains(s) && b.status == "ok")
        {
            match fetch(&rpc, &paths, *slot, args.offline, "full").await? {
                Ok(b) if summarize(*slot, &stored.leader, &b) == *stored => {}
                Ok(_) => problems.push(format!(
                    "slot {slot}: reduced block row differs from {source}"
                )),
                Err(why) => problems.push(format!("slot {slot}: {why}")),
            }
        }
    }

    let in_slots = landed
        .iter()
        .filter(|l| l.slot.is_some_and(|s| slots.contains(&s)))
        .count();
    println!(
        "verified {checked} of {in_slots} landed signatures in {} blocks against {source}; {hashed} archive files match the manifest, {absent} absent",
        slots.len()
    );
    if problems.is_empty() {
        println!("OK: no discrepancies");
        return Ok(());
    }
    for p in &problems {
        println!("MISMATCH {p}");
    }
    bail!("discrepancies: {}", problems.len())
}

/// The block from the local archive (offline) or the RPC; `Err(reason)` when there is none.
async fn fetch(
    rpc: &RpcClient,
    paths: &Paths,
    slot: u64,
    offline: bool,
    details: &str,
) -> Result<Result<Value, String>> {
    if offline {
        return Ok(blocks::load_raw(&paths.archive, slot)?
            .ok_or_else(|| "not in the local archive (run without --offline)".to_string()));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(match rpc::get_block_raw(rpc, slot, details).await? {
        BlockFetch::Block(b) => Ok(b),
        BlockFetch::Skipped(m) | BlockFetch::NotYet(m) => Err(format!("RPC has no block ({m})")),
    })
}
