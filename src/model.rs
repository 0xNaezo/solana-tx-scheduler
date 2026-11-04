//! Records written to `data/`. Every file is JSON / JSON Lines so that any
//! claim in the report can be traced back to a signature + slot.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
/// Alpenglow activation on mainnet (21 Oct 2026). Every measurement must be after it.
pub const ALPENGLOW_EPOCH: u64 = 1064;

/// `data/plan.json`: who we measure and with which design.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub created_at: String,
    pub cluster: String,
    pub epoch: u64,
    pub slot: u64,
    pub snapshot_dir: String,
    pub total_stake_lamports: u64,
    pub design: Design,
    pub validators: Vec<Target>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Design {
    pub seed: u64,
    pub txs_per_window: usize,
    pub windows_per_validator: usize,
    pub cu_limit: u32,
    pub cu_price_max: u64,
    pub pauses_ms: Vec<u64>,
    pub lead_slots: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub identity: String,
    pub vote: String,
    /// gossip `clientId` (self-reported by the node)
    pub client: String,
    pub version: String,
    pub stake_lamports: u64,
    pub stake_pct: f64,
    pub skip_rate: f64,
    pub tpu_quic: Option<String>,
    pub gossip: Option<String>,
    /// Who receives our packets: the validator itself or a sequencer/proxy in front of it.
    pub route: String,
    pub leader_slots_left: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum SendOrder {
    /// Lowest priority first: FIFO and priority predict opposite orders.
    Reverse,
    /// Random permutation: send order independent of priority.
    Random,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Conflict {
    /// One fee payer for the whole batch: every pair write-locks the same account.
    Shared,
    /// One fee payer per transaction: no account conflicts inside the batch.
    Distinct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Treatment {
    pub order: SendOrder,
    pub pause_ms: u64,
    pub conflict: Conflict,
}

/// One line of `data/raw/windows.jsonl`: one leader window we targeted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRecord {
    pub run_id: String,
    pub live: bool,
    pub epoch: u64,
    pub leader: String,
    pub start_slot: u64,
    pub num_slots: u64,
    /// Gossip view of the leader right before the window.
    pub client: Option<String>,
    pub version: Option<String>,
    pub tpu_quic: Option<String>,
    pub treatment: Treatment,
    pub blockhash: String,
    /// Our slot estimate when the first transaction left.
    pub fire_slot_est: u64,
    pub fire_at_us: u64,
    pub quic_handshake_ms: Option<f64>,
    pub quic_error: Option<String>,
    /// Set when the window was skipped (late, no TPU, budget).
    pub skipped: Option<String>,
    pub txs: Vec<TxRecord>,
}

impl WindowRecord {
    /// Counts toward the leader's quota: not skipped, and at least two transactions
    /// left without a send error (one transaction makes no pair).
    pub fn usable(&self) -> bool {
        let sent = self
            .txs
            .iter()
            .filter(|t| t.sent_at_us.is_some() && t.send_error.is_none());
        self.skipped.is_none() && sent.count() >= 2
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRecord {
    pub signature: String,
    /// Rank on the priority ladder: 0 = lowest cu_price.
    pub rank: usize,
    pub cu_price: u64,
    pub cu_limit: u32,
    pub payer: String,
    /// Position in the send sequence: 0 = sent first.
    pub send_seq: usize,
    pub sent_at_us: Option<u64>,
    pub send_error: Option<String>,
    pub worst_case_fee: u64,
}

/// One line of `data/raw/landed.jsonl`: where one of our transactions ended up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Landed {
    pub signature: String,
    pub window_start: u64,
    pub slot: Option<u64>,
    /// Index in `getBlock(slot).transactions`.
    pub index: Option<usize>,
    pub block_txs: Option<usize>,
    pub err: Option<serde_json::Value>,
    pub fee: Option<u64>,
    pub cu_consumed: Option<u64>,
}

/// One line of `data/raw/blocks.jsonl.zst`: a block reduced to what the analysis reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockSummary {
    pub slot: u64,
    pub leader: String,
    /// "ok" | "skipped" | "error: ..."
    pub status: String,
    pub blockhash: Option<String>,
    pub parent_slot: Option<u64>,
    pub block_time: Option<i64>,
    /// Per transaction in block order, columns in `blocks::col`:
    /// [fee, signatures, cost_units, cu_consumed, cu_requested, ok, jito_tip, vote].
    pub txs: Vec<[u64; 8]>,
}

/// One line of `data/raw/ledger.jsonl`: written *before* a transaction is sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub at: String,
    pub run_id: String,
    /// tx | fund | sweep | anchor | balance
    pub kind: String,
    pub signature: Option<String>,
    /// Worst-case fee reserved against the cap.
    pub fee_lamports: u64,
    /// For `balance`: total lamports across our wallets at that moment.
    pub balance: Option<u64>,
}

/// One line of `data/raw/anchors.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anchor {
    pub at: String,
    pub file: String,
    pub sha256: String,
    pub memo: String,
    pub signature: Option<String>,
    pub live: bool,
}

pub struct Paths {
    pub plan: PathBuf,
    pub windows: PathBuf,
    pub landed: PathBuf,
    pub blocks: PathBuf,
    pub ledger: PathBuf,
    pub anchors: PathBuf,
    pub archive: PathBuf,
    pub snapshots: PathBuf,
}

impl Paths {
    pub fn new(data_dir: &Path) -> Self {
        let raw = data_dir.join("raw");
        Self {
            plan: data_dir.join("plan.json"),
            windows: raw.join("windows.jsonl"),
            landed: raw.join("landed.jsonl"),
            blocks: raw.join("blocks.jsonl.zst"),
            ledger: raw.join("ledger.jsonl"),
            anchors: raw.join("anchors.jsonl"),
            archive: data_dir.join("blocks"),
            snapshots: data_dir.join("snapshots"),
        }
    }
}

pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    // Raw data is the product; never lose a line to a crash.
    file.sync_data()?;
    Ok(())
}

pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter(|(_, l)| !matches!(l, Ok(s) if s.trim().is_empty()))
        .map(|(i, line)| {
            serde_json::from_str(&line?).with_context(|| format!("{}:{}", path.display(), i + 1))
        })
        .collect()
}

pub fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut out = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut out, row)?;
        out.push(b'\n');
    }
    fs::write(path, out).with_context(|| format!("write {}", path.display()))
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

pub fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or_default()
}

pub fn now_utc() -> String {
    utc(unix_us() as i64 / 1_000_000)
}

/// Unix seconds to `YYYY-MM-DD HH:MM:SS UTC` (civil-from-days, H. Hinnant).
pub fn utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Parts of an RPC URL that can carry an API key: the query string and long
/// path segments (Helius `?api-key=`, QuickNode/Triton/Alchemy path tokens).
pub fn url_secrets(url: &str) -> Vec<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let Some(i) = rest.find(['/', '?']) else {
        return Vec::new();
    };
    rest[i..]
        .split(['/', '?', '&'])
        .filter(|s| s.len() >= 8)
        .map(str::to_string)
        .collect()
}

/// `text` with every secret replaced by `<redacted>`.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .fold(text.to_string(), |t, s| t.replace(s.as_str(), "<redacted>"))
}

/// The URL with its API key hidden, safe for logs and files.
pub fn redact(url: &str) -> String {
    scrub(url, &url_secrets(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        // Alpenglow activation day.
        assert_eq!(utc(1_792_594_200), "2026-10-21 14:50:00 UTC");
    }

    #[test]
    fn redact_hides_api_key() {
        assert_eq!(
            redact("https://x.io/?api-key=secret"),
            "https://x.io/?<redacted>"
        );
        assert_eq!(redact("http://127.0.0.1:8899"), "http://127.0.0.1:8899");
        assert_eq!(
            redact("https://a.quiknode.pro/0123456789abcdef/"),
            "https://a.quiknode.pro/<redacted>/"
        );
        // reqwest quotes the full request URL in its errors.
        let secrets = url_secrets("https://x.io?api-key=secret");
        assert_eq!(
            scrub(
                "error sending request for url (https://x.io/?api-key=secret)",
                &secrets
            ),
            "error sending request for url (https://x.io/?<redacted>)"
        );
    }
}
