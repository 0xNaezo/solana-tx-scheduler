//! Measurement transactions: three ComputeBudget instructions and nothing else
//! (~450 CU, no program invocation that could fail or vary in cost).
//!
//! Every transaction in a window has the same shape (1 signature, 1 writable
//! account, same instruction sizes, same CU limit), so the cost-model cost is
//! identical and any reward/cost priority reduces to the cu_price ladder.
//! Uniqueness comes from a nonce in SetLoadedAccountsDataSizeLimit, kept in
//! (32 KiB, 64 KiB] so the cost model charges every nonce the same.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use sha2::{Digest, Sha256};
use solana_address::Address;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_keypair::{Keypair, read_keypair_file};
use solana_signer::Signer;
use solana_transaction::{Hash, Instruction, Transaction};

pub const BASE_FEE_PER_SIGNATURE: u64 = 5_000;
pub const MEMO_PROGRAM: Address = spl_memo_interface::v3::ID;

/// Agave rounds the priority fee up: ceil(cu_price * cu_limit / 1e6).
pub fn priority_fee(cu_limit: u32, cu_price: u64) -> u64 {
    (u128::from(cu_price) * u128::from(cu_limit)).div_ceil(1_000_000) as u64
}

/// Upper bound on what one single-signature transaction can cost.
pub fn worst_case_fee(cu_limit: u32, cu_price: u64) -> u64 {
    BASE_FEE_PER_SIGNATURE + priority_fee(cu_limit, cu_price)
}

/// cu_price for ladder rank `0..n`: evenly spaced, strictly positive, top = `max`.
pub fn ladder(n: usize, max: u64) -> Vec<u64> {
    let n = n.max(1) as u64;
    (1..=n).map(|i| max * i / n).collect()
}

/// Fee payer `i`, derived from the budget wallet: no extra key files to lose,
/// and `sweep` can always find the lamports again.
pub fn derive_payer(main: &Keypair, i: usize) -> Keypair {
    let mut hasher = Sha256::new();
    hasher.update(b"solana-tx-scheduler/payer/v1");
    hasher.update(main.secret_bytes());
    hasher.update((i as u64).to_le_bytes());
    Keypair::new_from_array(hasher.finalize().into())
}

pub fn memo_ix(memo: &str) -> Instruction {
    spl_memo_interface::instruction::build_memo(&MEMO_PROGRAM, memo.as_bytes(), &[])
}

/// Nonce for SetLoadedAccountsDataSizeLimit: distinct for every (window, rank)
/// within 2048 slots, far longer than a blockhash lives.
pub fn nonce(window_start: u64, rank: usize) -> u32 {
    1 + ((window_start % 2048) * 16) as u32 + (rank as u32 % 16)
}

pub fn measurement_tx(
    payer: &Keypair,
    cu_limit: u32,
    cu_price: u64,
    nonce: u32,
    blockhash: Hash,
) -> Transaction {
    let ixs = [
        ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(cu_price),
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(32 * 1024 + nonce),
    ];
    Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[payer], blockhash)
}

pub fn wire(tx: &Transaction) -> Vec<u8> {
    wincode::serialize(tx).expect("legacy transaction always serializes")
}

pub fn load_keypair(path: &Path) -> Result<Keypair> {
    read_keypair_file(path)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| {
            format!(
                "read keypair {} (mount it via the keys volume or set KEYPAIR)",
                path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_fee_rounds_up() {
        assert_eq!(priority_fee(1_000, 0), 0);
        assert_eq!(priority_fee(1_000, 1), 1);
        assert_eq!(priority_fee(1_000, 700_000), 700);
        assert_eq!(worst_case_fee(1_000, 700_000), 5_700);
    }

    #[test]
    fn ladder_is_strictly_increasing_and_distinct_in_lamports() {
        let l = ladder(10, 700_000);
        assert_eq!(l.len(), 10);
        assert_eq!(*l.last().unwrap(), 700_000);
        let fees: Vec<u64> = l.iter().map(|&p| priority_fee(1_000, p)).collect();
        assert!(fees.windows(2).all(|w| w[0] < w[1]), "{fees:?}");
    }

    #[test]
    fn payers_are_deterministic_and_distinct() {
        let main = Keypair::new_from_array([7; 32]);
        let a = derive_payer(&main, 0).pubkey();
        assert_eq!(a, derive_payer(&main, 0).pubkey());
        assert_ne!(a, derive_payer(&main, 1).pubkey());
        assert_ne!(a, main.pubkey());
    }

    #[test]
    fn measurement_tx_has_one_signature_and_unique_nonce() {
        let payer = Keypair::new_from_array([1; 32]);
        let a = measurement_tx(&payer, 1_000, 10, nonce(100, 0), Hash::default());
        let b = measurement_tx(&payer, 1_000, 10, nonce(104, 0), Hash::default());
        assert_eq!(a.signatures.len(), 1);
        assert_ne!(a.signatures[0], b.signatures[0]);
        assert!(wire(&a).len() < 1232);
        // Nonce stays in (0, 32 KiB]: same loaded-accounts cost bucket for all.
        assert_eq!(nonce(2047, 15), 32 * 1024);
        assert_eq!(nonce(2048, 0), 1);
    }
}
