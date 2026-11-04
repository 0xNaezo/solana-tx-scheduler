//! RPC helpers: slot clock, leader windows, gossip view, raw getBlock.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{
    client_error::{Error as ClientError, ErrorKind},
    config::RpcLeaderScheduleConfig,
    request::{RpcError, RpcRequest},
    response::{RpcContactInfo, RpcPerfSample},
};
use tracing::{debug, warn};

use crate::model::MAINNET_GENESIS;

pub fn client(url: &str) -> Arc<RpcClient> {
    Arc::new(RpcClient::new_with_timeout_and_commitment(
        url.to_string(),
        Duration::from_secs(60),
        CommitmentConfig::confirmed(),
    ))
}

pub async fn cluster_name(rpc: &RpcClient) -> Result<String> {
    let genesis = rpc.get_genesis_hash().await?.to_string();
    Ok(if genesis == MAINNET_GENESIS {
        "mainnet-beta".into()
    } else {
        format!("genesis:{genesis}")
    })
}

/// Mean slot duration from `getRecentPerformanceSamples`.
pub async fn slot_duration(rpc: &RpcClient) -> Result<Duration> {
    Ok(mean_slot(
        &rpc.get_recent_performance_samples(Some(10)).await?,
    ))
}

pub fn mean_slot(samples: &[RpcPerfSample]) -> Duration {
    let (secs, slots) = samples.iter().fold((0u64, 0u64), |(s, n), x| {
        (s + u64::from(x.sample_period_secs), n + x.num_slots)
    });
    if slots == 0 {
        return Duration::from_millis(400);
    }
    Duration::from_secs_f64(secs as f64 / slots as f64)
}

/// Current slot extrapolated between `getSlot(processed)` polls. A slot is
/// timestamped when we *first* see it, so the clock trails the leader by the
/// RPC node's own lag; `lead_slots` in the design absorbs that.
#[derive(Clone)]
pub struct SlotClock {
    last: Arc<Mutex<(u64, Instant)>>,
    slot: Duration,
}

impl SlotClock {
    pub async fn start(rpc: Arc<RpcClient>, slot: Duration) -> Result<Self> {
        let first = rpc
            .get_slot_with_commitment(CommitmentConfig::processed())
            .await?;
        let last = Arc::new(Mutex::new((first, Instant::now())));
        let shared = last.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                match rpc
                    .get_slot_with_commitment(CommitmentConfig::processed())
                    .await
                {
                    Ok(s) => {
                        let mut g = shared.lock().unwrap();
                        if s > g.0 {
                            *g = (s, Instant::now());
                        }
                    }
                    Err(e) => debug!("getSlot: {e}"),
                }
                if Arc::strong_count(&shared) == 1 {
                    break;
                }
            }
        });
        Ok(Self { last, slot })
    }

    pub fn now(&self) -> u64 {
        let (s, t) = *self.last.lock().unwrap();
        s + (t.elapsed().as_nanos() / self.slot.as_nanos().max(1)) as u64
    }

    /// Instant at which `slot` is expected to start.
    pub fn instant_of(&self, slot: u64) -> Instant {
        let (s, t) = *self.last.lock().unwrap();
        if slot >= s {
            t + self.slot * (slot - s) as u32
        } else {
            t.checked_sub(self.slot * (s - slot) as u32).unwrap_or(t)
        }
    }

    pub async fn wait_until(&self, slot: u64) {
        loop {
            let target = self.instant_of(slot);
            let now = Instant::now();
            if now >= target {
                return;
            }
            // Re-read the clock at least every 50 ms: polls may move it.
            tokio::time::sleep((target - now).min(Duration::from_millis(50))).await;
        }
    }
}

/// NUM_CONSECUTIVE_LEADER_SLOTS: still 4 after Alpenglow (see research/validators.md).
pub const LEADER_WINDOW: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderWindow {
    pub leader: String,
    pub start: u64,
    pub len: u64,
}

/// Consecutive slot runs of each identity in the epoch containing `slot`.
pub async fn leader_windows(
    rpc: &RpcClient,
    identities: &[String],
    epoch_first_slot: u64,
) -> Result<Vec<LeaderWindow>> {
    let mut out = Vec::new();
    for id in identities {
        let schedule = rpc
            .get_leader_schedule_with_config(
                Some(epoch_first_slot),
                RpcLeaderScheduleConfig {
                    identity: Some(id.clone()),
                    commitment: None,
                },
            )
            .await?
            .unwrap_or_default();
        let mut slots: Vec<u64> = schedule
            .get(id)
            .map(|v| v.iter().map(|&i| epoch_first_slot + i as u64).collect())
            .unwrap_or_default();
        slots.sort_unstable();
        out.extend(group_runs(id, &slots));
    }
    out.sort_by_key(|w| w.start);
    Ok(out)
}

/// Leader windows: slots of one leader grouped by the 4-slot rotation unit,
/// so a leader holding 8 consecutive slots yields two windows.
pub fn group_runs(leader: &str, slots: &[u64]) -> Vec<LeaderWindow> {
    let mut out: Vec<LeaderWindow> = Vec::new();
    for &s in slots {
        match out.last_mut() {
            Some(w) if w.start + w.len == s && w.start / LEADER_WINDOW == s / LEADER_WINDOW => {
                w.len += 1
            }
            _ => out.push(LeaderWindow {
                leader: leader.to_string(),
                start: s,
                len: 1,
            }),
        }
    }
    out
}

/// `getClusterNodes` re-read at most every `max_age`: client id and TPU of a
/// BAM validator can change while it runs, so we record them per window.
pub struct Gossip {
    nodes: HashMap<String, RpcContactInfo>,
    fetched: Option<Instant>,
    max_age: Duration,
}

impl Gossip {
    pub fn new(max_age: Duration) -> Self {
        Self {
            nodes: HashMap::new(),
            fetched: None,
            max_age,
        }
    }

    pub async fn get(&mut self, rpc: &RpcClient, identity: &str) -> Result<Option<RpcContactInfo>> {
        if self.fetched.is_none_or(|t| t.elapsed() > self.max_age) {
            match rpc.get_cluster_nodes().await {
                Ok(nodes) => {
                    self.nodes = nodes.into_iter().map(|n| (n.pubkey.clone(), n)).collect();
                    self.fetched = Some(Instant::now());
                }
                Err(e) if self.fetched.is_some() => {
                    warn!("getClusterNodes failed, using cached view: {e}")
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(self.nodes.get(identity).cloned())
    }
}

pub enum BlockFetch {
    Block(Value),
    /// Leader produced nothing (or the RPC has no long-term copy).
    Skipped(String),
    /// Not available yet; try again later.
    NotYet(String),
}

/// Raw `getBlock` so the archive holds exactly what the node returned.
/// `maxSupportedTransactionVersion: 1` because v1 transactions are on mainnet.
pub async fn get_block_raw(rpc: &RpcClient, slot: u64, details: &str) -> Result<BlockFetch> {
    let params = json!([slot, {
        "encoding": "json",
        "transactionDetails": details,
        "rewards": false,
        "maxSupportedTransactionVersion": 1,
        "commitment": "confirmed",
    }]);
    let mut delay = Duration::from_millis(500);
    for attempt in 0..6 {
        match rpc
            .send::<Value>(RpcRequest::GetBlock, params.clone())
            .await
        {
            Ok(v) if v.is_null() => return Ok(BlockFetch::NotYet("null".into())),
            Ok(v) => return Ok(BlockFetch::Block(v)),
            Err(e) => match rpc_code(&e) {
                Some((-32007 | -32009, m)) => return Ok(BlockFetch::Skipped(m)),
                Some((-32004 | -32014, m)) => return Ok(BlockFetch::NotYet(m)),
                Some((code, m)) => bail!("getBlock {slot}: {code} {m}"),
                None if attempt < 5 => {
                    warn!(slot, attempt, "getBlock transport error, retrying: {e}");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                None => return Err(e.into()),
            },
        }
    }
    unreachable!()
}

pub fn rpc_code(e: &ClientError) -> Option<(i64, String)> {
    match e.kind() {
        ErrorKind::RpcError(RpcError::RpcResponseError { code, message, .. }) => {
            Some((*code, message.clone()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_runs_splits_into_rotation_units() {
        let w = group_runs("L", &[4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19, 21]);
        let got: Vec<(u64, u64)> = w.iter().map(|w| (w.start, w.len)).collect();
        assert_eq!(got, vec![(4, 4), (12, 4), (16, 4), (21, 1)]);
    }
}
