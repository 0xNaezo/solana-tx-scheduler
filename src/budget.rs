//! Spend limit in three layers:
//! 1. `Budget`: the worst-case fee of every transaction is reserved in
//!    `data/raw/ledger.jsonl` (fsync'ed) *before* it is sent. The ledger is
//!    cumulative across runs and the cap can never exceed `HARD_CAP_LAMPORTS`.
//! 2. `BalanceWatch`: kill-switch on the real on-chain balance of all our
//!    wallets, independent of our own bookkeeping.
//! 3. Dedicated wallet: `run --live` refuses a wallet that holds more than
//!    `--max-wallet-lamports`, so the cap is also a physical limit.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, bail};
use solana_address::Address;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use tracing::warn;

use crate::model::{LedgerEntry, append_jsonl, now_utc, read_jsonl};

/// 0.05 SOL: the whole experiment, all runs together.
pub const HARD_CAP_LAMPORTS: u64 = 50_000_000;

pub struct Budget {
    cap: u64,
    spent: u64,
    path: PathBuf,
    run_id: String,
}

impl Budget {
    pub fn open(path: &Path, requested_cap: u64, run_id: &str) -> Result<Self> {
        let spent = read_jsonl::<LedgerEntry>(path)?
            .iter()
            .map(|e| e.fee_lamports)
            .sum();
        Ok(Self {
            cap: requested_cap.min(HARD_CAP_LAMPORTS),
            spent,
            path: path.to_path_buf(),
            run_id: run_id.to_string(),
        })
    }

    pub fn cap(&self) -> u64 {
        self.cap
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub fn remaining(&self) -> u64 {
        self.cap.saturating_sub(self.spent)
    }

    /// Reserve `fee` before sending; refuses anything that would cross the cap.
    pub fn reserve(&mut self, kind: &str, signature: Option<String>, fee: u64) -> Result<()> {
        if self.spent.saturating_add(fee) > self.cap {
            bail!(
                "spend cap: {} spent + {fee} > cap {} lamports",
                self.spent,
                self.cap
            );
        }
        append_jsonl(
            &self.path,
            &LedgerEntry {
                at: now_utc(),
                run_id: self.run_id.clone(),
                kind: kind.to_string(),
                signature,
                fee_lamports: fee,
                balance: None,
            },
        )?;
        self.spent += fee;
        Ok(())
    }

    /// Balance checkpoint for the spend reconciliation in the report.
    pub fn record_balance(&self, total: u64) -> Result<()> {
        append_jsonl(
            &self.path,
            &LedgerEntry {
                at: now_utc(),
                run_id: self.run_id.clone(),
                kind: "balance".into(),
                signature: None,
                fee_lamports: 0,
                balance: Some(total),
            },
        )
    }
}

pub async fn total_balance(rpc: &RpcClient, addresses: &[Address]) -> Result<u64> {
    let mut total = 0;
    for chunk in addresses.chunks(100) {
        let accounts = rpc
            .get_multiple_accounts_with_commitment(chunk, CommitmentConfig::confirmed())
            .await?
            .value;
        total += accounts.iter().flatten().map(|a| a.lamports).sum::<u64>();
    }
    Ok(total)
}

pub struct BalanceWatch {
    addresses: Vec<Address>,
    start: u64,
    limit: u64,
}

impl BalanceWatch {
    pub async fn start(rpc: &RpcClient, addresses: Vec<Address>, limit: u64) -> Result<Self> {
        let start = total_balance(rpc, &addresses).await?;
        Ok(Self {
            addresses,
            start,
            limit,
        })
    }

    pub fn start_balance(&self) -> u64 {
        self.start
    }

    /// Returns lamports spent since start. Past the limit it records the
    /// balance (so the reconciliation shows the loss) and errors: kill-switch.
    pub async fn check(&self, rpc: &RpcClient, budget: &Budget) -> Result<u64> {
        // One flaky RPC answer must not end a long run; three in a row still do (fail closed).
        let mut tries = 1;
        let now = loop {
            match total_balance(rpc, &self.addresses).await {
                Ok(balance) => break balance,
                Err(e) if tries < 3 => {
                    warn!("balance check failed (try {tries} of 3): {e:#}");
                    tries += 1;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e.context("balance check failed 3 times, stopping")),
            }
        };
        let spent = self.start.saturating_sub(now);
        if spent > self.limit {
            budget.record_balance(now)?;
            bail!(
                "KILL-SWITCH: wallets lost {spent} lamports this run, limit {}",
                self.limit
            );
        }
        if now > self.start {
            warn!(
                gained = now - self.start,
                "wallet balance grew during the run"
            );
        }
        Ok(spent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_is_cumulative_and_hard_capped() {
        let dir = std::env::temp_dir().join(format!("sts-budget-{}", std::process::id()));
        let path = dir.join("ledger.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut b = Budget::open(&path, u64::MAX, "r1").unwrap();
        assert_eq!(b.cap(), HARD_CAP_LAMPORTS);

        let mut b2 = Budget::open(&path, 12_000, "r1").unwrap();
        b2.reserve("tx", None, 6_000).unwrap();
        b2.reserve("tx", None, 6_000).unwrap();
        assert!(b2.reserve("tx", None, 1).is_err());

        // A new run sees what earlier runs reserved.
        b = Budget::open(&path, 12_000, "r2").unwrap();
        assert_eq!(b.spent(), 12_000);
        assert_eq!(b.remaining(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
