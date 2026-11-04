//! Wallet operations outside the measurement loop: on-chain anchoring of
//! file hashes and sweeping derived fee payers back to the budget wallet.

use std::fs;

use anyhow::Result;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::Transaction;
use tracing::info;

use crate::{
    blocks::sha256_hex,
    budget::{Budget, HARD_CAP_LAMPORTS},
    cli::{AnchorArgs, Context, SweepArgs},
    model::{Anchor, Paths, append_jsonl, now_utc, unix_us},
    rpc,
    tx::{BASE_FEE_PER_SIGNATURE, derive_payer, load_keypair, memo_ix},
};

pub async fn anchor(ctx: &Context, args: AnchorArgs) -> Result<()> {
    let paths = Paths::new(&ctx.data_dir);
    let hash = sha256_hex(&fs::read(&args.file)?);
    let memo = format!("solana-tx-scheduler:{}:sha256:{hash}", args.label);
    let mut rec = Anchor {
        at: now_utc(),
        file: args.file.display().to_string(),
        sha256: hash,
        memo: memo.clone(),
        signature: None,
        live: args.live,
    };
    if !args.live {
        println!("dry-run, not sent. memo: {memo}");
        return Ok(());
    }
    let key = load_keypair(&args.key.keypair)?;
    let rpc = rpc::client(&ctx.rpc_url);
    let mut budget = Budget::open(
        &paths.ledger,
        HARD_CAP_LAMPORTS,
        &(unix_us() / 1_000).to_string(),
    )?;
    let tx = Transaction::new_signed_with_payer(
        &[memo_ix(&memo)],
        Some(&key.pubkey()),
        &[&key],
        rpc.get_latest_blockhash().await?,
    );
    let sig = tx.signatures[0].to_string();
    budget.reserve("anchor", Some(sig.clone()), BASE_FEE_PER_SIGNATURE)?;
    rpc.send_and_confirm_transaction(&tx).await?;
    rec.signature = Some(sig.clone());
    append_jsonl(&paths.anchors, &rec)?;
    println!("anchored {} → https://solscan.io/tx/{sig}", rec.file);
    Ok(())
}

pub async fn sweep(ctx: &Context, args: SweepArgs) -> Result<()> {
    let paths = Paths::new(&ctx.data_dir);
    let main = load_keypair(&args.key.keypair)?;
    let rpc = rpc::client(&ctx.rpc_url);
    let payers: Vec<_> = (0..args.payers).map(|i| derive_payer(&main, i)).collect();
    let addrs: Vec<_> = payers.iter().map(|p| p.pubkey()).collect();
    let accounts = rpc.get_multiple_accounts(&addrs).await?;
    let mut budget = Budget::open(
        &paths.ledger,
        HARD_CAP_LAMPORTS,
        &(unix_us() / 1_000).to_string(),
    )?;
    let mut moved = 0;
    for (payer, acc) in payers.iter().zip(accounts) {
        let lamports = acc.map_or(0, |a| a.lamports);
        if lamports <= BASE_FEE_PER_SIGNATURE {
            continue;
        }
        let amount = lamports - BASE_FEE_PER_SIGNATURE;
        moved += amount;
        if !args.live {
            println!(
                "dry-run: would move {amount} from {} to {}",
                payer.pubkey(),
                main.pubkey()
            );
            continue;
        }
        let tx = Transaction::new_signed_with_payer(
            &[transfer(&payer.pubkey(), &main.pubkey(), amount)],
            Some(&payer.pubkey()),
            &[payer],
            rpc.get_latest_blockhash().await?,
        );
        budget.reserve(
            "sweep",
            Some(tx.signatures[0].to_string()),
            BASE_FEE_PER_SIGNATURE,
        )?;
        rpc.send_and_confirm_transaction(&tx).await?;
        info!(payer = %payer.pubkey(), amount, "swept");
    }
    let done = if args.live {
        "swept"
    } else {
        "dry-run: would sweep"
    };
    println!("{done} {moved} lamports back to {}", main.pubkey());
    Ok(())
}
