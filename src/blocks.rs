//! Raw block archive (`data/blocks/<slot>.json.zst`) and its reduction to
//! `BlockSummary` rows that the analysis and `verify` read.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::BlockSummary;

/// Jito tip accounts: transactions touching them belong to bundles, which
/// are out of scope (the bundle path is not the leader's scheduler).
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

pub const MANIFEST: &str = "MANIFEST.sha256";

const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
const VOTE_PROGRAM: &str = "Vote111111111111111111111111111111111111111";
/// Builtins get 3k default CUs instead of 200k when no limit is requested.
const BUILTINS: [&str; 4] = [
    "11111111111111111111111111111111",
    VOTE_PROGRAM,
    "Stake11111111111111111111111111111111111111",
    "AddressLookupTab1e1111111111111111111111111",
];

/// Columns of `BlockSummary::txs` rows.
pub mod col {
    pub const FEE: usize = 0;
    pub const SIGS: usize = 1;
    pub const COST: usize = 2;
    pub const CU_USED: usize = 3;
    pub const CU_REQUESTED: usize = 4;
    pub const OK: usize = 5;
    pub const TIP: usize = 6;
    pub const VOTE: usize = 7;
}

pub fn archive_file(dir: &Path, slot: u64) -> PathBuf {
    dir.join(format!("{slot}.json.zst"))
}

pub fn save_raw(dir: &Path, slot: u64, block: &Value) -> Result<()> {
    fs::create_dir_all(dir)?;
    let bytes = zstd::encode_all(serde_json::to_vec(block)?.as_slice(), 9)?;
    let path = archive_file(dir, slot);
    fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
}

pub fn load_raw(dir: &Path, slot: u64) -> Result<Option<Value>> {
    let path = archive_file(dir, slot);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = zstd::decode_all(fs::File::open(&path)?)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `sha256sum`-compatible manifest of every archived block.
pub fn write_manifest(dir: &Path) -> Result<PathBuf> {
    let mut lines = Vec::new();
    let mut names: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json.zst"))
        .collect();
    names.sort();
    for name in names {
        let hash = sha256_hex(&fs::read(dir.join(&name))?);
        lines.push(format!("{hash}  {name}\n"));
    }
    let path = dir.join(MANIFEST);
    fs::write(&path, lines.concat())?;
    Ok(path)
}

pub fn first_signatures(block: &Value) -> Vec<String> {
    let Some(txs) = block["transactions"].as_array() else {
        // transactionDetails: "signatures" returns a flat list.
        return block["signatures"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
    };
    txs.iter()
        .map(|t| {
            t["transaction"]["signatures"][0]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn touches_jito_tip(tx: &Value) -> bool {
    let keys = tx["transaction"]["message"]["accountKeys"].as_array();
    let loaded = &tx["meta"]["loadedAddresses"];
    keys.into_iter()
        .flatten()
        .chain(loaded["writable"].as_array().into_iter().flatten())
        .chain(loaded["readonly"].as_array().into_iter().flatten())
        .filter_map(|k| k.as_str())
        .any(|k| JITO_TIP_ACCOUNTS.contains(&k))
}

/// Program id of every top-level instruction (programs are always static keys).
fn programs(tx: &Value) -> Vec<(&str, &Value)> {
    let msg = &tx["transaction"]["message"];
    msg["instructions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ix| {
            let i = ix["programIdIndex"].as_u64()? as usize;
            Some((msg["accountKeys"][i].as_str()?, ix))
        })
        .collect()
}

/// The compute-unit limit the scheduler sees: v1 config, else
/// SetComputeUnitLimit, else Agave's default (200k per instruction, 3k per builtin).
pub fn requested_cu(tx: &Value) -> u64 {
    let msg = &tx["transaction"]["message"];
    if let Some(cfg) = msg.get("transactionConfig").filter(|c| c.is_object()) {
        return cfg["computeUnitLimit"].as_u64().unwrap_or(0);
    }
    let progs = programs(tx);
    for (p, ix) in &progs {
        if *p == COMPUTE_BUDGET
            && let Some(data) = ix["data"]
                .as_str()
                .and_then(|d| bs58::decode(d).into_vec().ok())
            && data.len() >= 5
            && data[0] == 2
        {
            return u64::from(u32::from_le_bytes([data[1], data[2], data[3], data[4]]));
        }
    }
    progs
        .iter()
        .filter(|(p, _)| *p != COMPUTE_BUDGET)
        .map(|(p, _)| if BUILTINS.contains(p) { 3_000 } else { 200_000 })
        .sum::<u64>()
        .min(1_400_000)
}

pub fn summarize(slot: u64, leader: &str, block: &Value) -> BlockSummary {
    let txs = block["transactions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let meta = &t["meta"];
            [
                meta["fee"].as_u64().unwrap_or(0),
                t["transaction"]["signatures"]
                    .as_array()
                    .map_or(0, |s| s.len() as u64),
                meta["costUnits"].as_u64().unwrap_or(0),
                meta["computeUnitsConsumed"].as_u64().unwrap_or(0),
                requested_cu(t),
                u64::from(meta["err"].is_null()),
                u64::from(touches_jito_tip(t)),
                u64::from(programs(t).iter().any(|(p, _)| *p == VOTE_PROGRAM)),
            ]
        })
        .collect();
    BlockSummary {
        slot,
        leader: leader.to_string(),
        status: "ok".into(),
        blockhash: block["blockhash"].as_str().map(String::from),
        parent_slot: block["parentSlot"].as_u64(),
        block_time: block["blockTime"].as_i64(),
        txs,
    }
}

pub fn read_summaries(path: &Path) -> Result<BTreeMap<u64, BlockSummary>> {
    let mut out = BTreeMap::new();
    if !path.exists() {
        return Ok(out);
    }
    let decoder = zstd::Decoder::new(fs::File::open(path)?)?;
    for line in BufReader::new(decoder).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let b: BlockSummary = serde_json::from_str(&line)?;
        out.insert(b.slot, b);
    }
    Ok(out)
}

pub fn write_summaries(path: &Path, blocks: &BTreeMap<u64, BlockSummary>) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut raw = Vec::new();
    for b in blocks.values() {
        serde_json::to_writer(&mut raw, b)?;
        raw.push(b'\n');
    }
    fs::write(path, zstd::encode_all(raw.as_slice(), 19)?)
        .with_context(|| format!("write {}", path.display()))
}

/// Agave's scheduler priority for a foreign transaction, rebuilt from the block:
/// reward * 1e6 / (cost + 1). reward = fee minus the burnt half of the base fee;
/// cost = executed `costUnits` with consumed CUs swapped back for requested ones,
/// because the scheduler prices the request, not the outcome.
pub fn agave_priority(row: &[u64; 8]) -> Option<u64> {
    let cost = row[col::COST];
    if cost == 0 {
        return None;
    }
    let cost = cost - row[col::CU_USED].min(cost) + row[col::CU_REQUESTED];
    let reward = row[col::FEE].saturating_sub(2_500 * row[col::SIGS]);
    Some(reward.saturating_mul(1_000_000) / (cost + 1))
}

/// Gamma(priority, earliness) over other users' non-bundle transactions of
/// one block; `exclude` holds the indices of our own transactions.
pub fn foreign_gamma(b: &BlockSummary, exclude: &[usize]) -> Option<(f64, usize)> {
    let items: Vec<(u64, usize)> = b
        .txs
        .iter()
        .enumerate()
        .filter(|(i, row)| row[col::TIP] == 0 && row[col::VOTE] == 0 && !exclude.contains(i))
        .filter_map(|(i, row)| agave_priority(row).map(|p| (p, i)))
        .collect();
    crate::stats::gamma(&items).map(|g| (g, items.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_reads_fee_cost_status_tips_and_requested_cu() {
        // SetComputeUnitLimit(1000) = [2, 0xe8, 0x03, 0, 0]
        let limit = bs58::encode([2u8, 0xe8, 0x03, 0, 0]).into_string();
        let block = json!({
            "blockhash": "h", "parentSlot": 9, "blockTime": 100,
            "transactions": [
                {"transaction": {"signatures": ["s1"], "message": {
                    "accountKeys": ["a", COMPUTE_BUDGET],
                    "instructions": [{"programIdIndex": 1, "accounts": [], "data": limit}]}},
                 "meta": {"fee": 5_700, "costUnits": 2_000, "computeUnitsConsumed": 300, "err": null,
                          "loadedAddresses": {"writable": [], "readonly": []}}},
                {"transaction": {"signatures": ["s2", "s3"], "message": {
                    "accountKeys": ["b", JITO_TIP_ACCOUNTS[3], "11111111111111111111111111111111"],
                    "instructions": [{"programIdIndex": 2, "accounts": [0, 1], "data": "3Bxs4Bc3VYuGVB19"}]}},
                 "meta": {"fee": 10_000, "costUnits": 3_000, "computeUnitsConsumed": 900, "err": {"x": 1}}},
                {"transaction": {"signatures": ["s4"], "message": {
                    "accountKeys": ["c"], "instructions": [],
                    "transactionConfig": {"priorityFee": 7, "computeUnitLimit": 4_000}}},
                 "meta": {"fee": 5_007, "costUnits": 1_500, "computeUnitsConsumed": 100, "err": null}}
            ]
        });
        let s = summarize(10, "L", &block);
        assert_eq!(
            s.txs,
            vec![
                [5_700, 1, 2_000, 300, 1_000, 1, 0, 0],
                [10_000, 2, 3_000, 900, 3_000, 0, 1, 0],
                [5_007, 1, 1_500, 100, 4_000, 1, 0, 0],
            ]
        );
        assert_eq!(first_signatures(&block), vec!["s1", "s2", "s4"]);
        // cost = 2000 - 300 + 1000
        assert_eq!(agave_priority(&s.txs[0]), Some(3_200 * 1_000_000 / 2_701));
    }
}
