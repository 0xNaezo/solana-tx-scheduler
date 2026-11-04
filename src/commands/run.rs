use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use solana_keypair::Keypair;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::{Hash, Transaction};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};
use tracing::{info, warn};

use crate::{
    budget::{BalanceWatch, Budget},
    cli::{Context, RunArgs},
    model::{
        ALPENGLOW_EPOCH, Conflict, Paths, Plan, TxRecord, WindowRecord, append_jsonl, read_json,
        read_jsonl, redact, scrub, unix_us, url_secrets,
    },
    quic::Tpu,
    rpc::{self, Gossip, SlotClock},
    stats::{seed_of, send_sequence, treatment_cells},
    tx::{
        BASE_FEE_PER_SIGNATURE, derive_payer, ladder, load_keypair, measurement_tx, nonce, wire,
        worst_case_fee,
    },
};

/// Connect to the leader's TPU this long before firing.
const PREPARE: Duration = Duration::from_secs(3);
const BLOCKHASH_MAX_AGE: Duration = Duration::from_secs(10);

pub async fn execute(ctx: &Context, args: RunArgs) -> Result<()> {
    let paths = Paths::new(&ctx.data_dir);
    let plan: Plan = read_json(&paths.plan)?;
    let design = plan.design.clone();
    let n = design.txs_per_window;
    let rpc = rpc::client(&ctx.rpc_url);
    // RPC errors end up in windows.jsonl, which is committed: keep the API key out.
    let secrets = url_secrets(&ctx.rpc_url);
    let failed =
        |what: &str, e: &dyn std::fmt::Display| scrub(&format!("{what} failed: {e:#}"), &secrets);

    let cluster = rpc::cluster_name(&rpc).await?;
    if cluster != plan.cluster {
        bail!(
            "plan.json was made for {}, RPC serves {cluster}",
            plan.cluster
        );
    }
    let epoch_info = rpc.get_epoch_info().await?;
    if cluster == "mainnet-beta" && epoch_info.epoch < ALPENGLOW_EPOCH {
        bail!(
            "epoch {} is before Alpenglow ({ALPENGLOW_EPOCH}); measurements must not mix",
            epoch_info.epoch
        );
    }

    let run_id = (unix_us() / 1_000).to_string();
    let main = match load_keypair(&args.key.keypair) {
        Ok(k) => k,
        Err(e) if args.live => return Err(e),
        // Dry-run needs no wallet: sign with a throwaway key.
        Err(_) => Keypair::new(),
    };
    let payers: Vec<Keypair> = (0..n).map(|i| derive_payer(&main, i)).collect();
    let prices = ladder(n, design.cu_price_max);
    let per_window: u64 = prices
        .iter()
        .map(|&p| worst_case_fee(design.cu_limit, p))
        .sum();
    let max_tx_fee = worst_case_fee(design.cu_limit, design.cu_price_max);
    // Always leave enough budget to sweep the payers back and anchor the archive hash.
    let reserve = (n as u64 + 1) * BASE_FEE_PER_SIGNATURE;

    let mut budget = Budget::open(&paths.ledger, args.max_spend_lamports, &run_id)?;
    let mut dry_spent = 0u64;
    info!(
        mode = if args.live { "LIVE" } else { "dry-run" },
        rpc = %redact(&ctx.rpc_url),
        %run_id,
        wallet = %main.pubkey(),
        cap = budget.cap(),
        spent_before = budget.spent(),
        per_window,
        "run"
    );

    let mut payer_left = vec![0u64; n];
    let watch = if args.live {
        let balance = rpc.get_balance(&main.pubkey()).await?;
        if balance > args.max_wallet_lamports {
            bail!(
                "wallet holds {balance} lamports > --max-wallet-lamports {}: use a dedicated wallet that holds only the budget",
                args.max_wallet_lamports
            );
        }
        let rent = rpc.get_minimum_balance_for_rent_exemption(0).await?;
        if args.fund_lamports < rent + 2 * max_tx_fee {
            bail!("--fund-lamports must cover rent exemption ({rent}) plus fees");
        }
        let addrs: Vec<_> = std::iter::once(main.pubkey())
            .chain(payers.iter().map(|p| p.pubkey()))
            .collect();
        for (i, acc) in rpc
            .get_multiple_accounts(&addrs[1..])
            .await?
            .iter()
            .enumerate()
        {
            payer_left[i] = acc.as_ref().map_or(0, |a| a.lamports);
        }
        let watch = BalanceWatch::start(&rpc, addrs, budget.remaining()).await?;
        budget.record_balance(watch.start_balance())?;
        Some((watch, rent))
    } else {
        None
    };

    // Resume: live quotas continue from earlier live runs, keeping treatment cells balanced.
    let mut done: HashMap<String, usize> = HashMap::new();
    if args.live {
        for w in read_jsonl::<WindowRecord>(&paths.windows)? {
            if w.live && w.usable() {
                *done.entry(w.leader).or_default() += 1;
            }
        }
    }
    let cells: HashMap<String, Vec<_>> = plan
        .validators
        .iter()
        .map(|t| {
            let mut rng = StdRng::seed_from_u64(design.seed ^ seed_of(&t.identity));
            (
                t.identity.clone(),
                treatment_cells(&design.pauses_ms, &mut rng),
            )
        })
        .collect();

    let slot_time = rpc::slot_duration(&rpc).await?;
    let clock = SlotClock::start(rpc.clone(), slot_time).await?;
    let ids: Vec<String> = plan.validators.iter().map(|t| t.identity.clone()).collect();
    let first = epoch_info.absolute_slot - epoch_info.slot_index;
    let mut windows = rpc::leader_windows(&rpc, &ids, first).await?;
    match rpc::leader_windows(&rpc, &ids, first + epoch_info.slots_in_epoch).await {
        Ok(next) => windows.extend(next),
        Err(e) => warn!("next epoch schedule unavailable: {e}"),
    }
    let now = clock.now();
    windows.retain(|w| w.start > now + design.lead_slots + 2);
    info!(
        slot_ms = slot_time.as_secs_f64() * 1e3,
        upcoming = windows.len(),
        first = windows.first().map(|w| w.start),
        "leader windows of targets"
    );

    let (stop_tx, mut stop) = watch::channel(false);
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        let _ = stop_tx.send(true);
    });

    let deadline = args
        .max_minutes
        .map(|m| Instant::now() + Duration::from_secs(m * 60));
    let mut tpu = Tpu::new()?;
    let mut gossip = Gossip::new(Duration::from_secs(120));
    let mut blockhash = (Hash::default(), Instant::now() - BLOCKHASH_MAX_AGE * 2);
    let (mut fired, mut skipped) = (0usize, 0usize);

    for w in windows {
        let quota_left = |done: &HashMap<String, usize>, id: &str| {
            design
                .windows_per_validator
                .saturating_sub(done.get(id).copied().unwrap_or(0))
        };
        if ids.iter().all(|id| quota_left(&done, id) == 0)
            || args.max_windows.is_some_and(|m| fired >= m)
            || deadline.is_some_and(|d| Instant::now() >= d)
            || *stop.borrow()
        {
            break;
        }
        if quota_left(&done, &w.leader) == 0 {
            continue;
        }
        let remaining = if args.live {
            budget.remaining()
        } else {
            budget.remaining().saturating_sub(dry_spent)
        };
        if remaining < per_window + reserve {
            warn!(remaining, per_window, reserve, "budget exhausted, stopping");
            break;
        }

        let fire_slot = w.start.saturating_sub(design.lead_slots);
        let prepare_at = clock
            .instant_of(fire_slot)
            .checked_sub(PREPARE)
            .unwrap_or_else(Instant::now);
        tokio::select! {
            _ = tokio::time::sleep_until(prepare_at.into()) => {}
            _ = stop.wait_for(|s| *s) => break,
        }

        let treatment =
            cells[&w.leader][done.get(&w.leader).copied().unwrap_or(0) % cells[&w.leader].len()];
        let (node, gossip_error) = match gossip.get(&rpc, &w.leader).await {
            Ok(node) => (node, None),
            Err(e) => (None, Some(failed("getClusterNodes", &e))),
        };
        let mut rec = WindowRecord {
            run_id: run_id.clone(),
            live: args.live,
            epoch: epoch_info.epoch + u64::from(w.start >= first + epoch_info.slots_in_epoch),
            leader: w.leader.clone(),
            start_slot: w.start,
            num_slots: w.len,
            client: node.as_ref().and_then(|n| n.client_id.clone()),
            version: node.as_ref().and_then(|n| n.version.clone()),
            tpu_quic: node
                .as_ref()
                .and_then(|n| n.tpu_quic)
                .map(|a| a.to_string()),
            treatment,
            blockhash: String::new(),
            fire_slot_est: 0,
            fire_at_us: 0,
            quic_handshake_ms: None,
            quic_error: None,
            skipped: None,
            txs: Vec::new(),
        };
        if let Some(why) = gossip_error {
            skip(&paths, rec, &why, &mut skipped)?;
            continue;
        }
        let Some(addr) = node.as_ref().and_then(|n| n.tpu_quic) else {
            skip(&paths, rec, "leader has no tpuQuic in gossip", &mut skipped)?;
            continue;
        };
        if args.live || !args.no_probe {
            match tpu.connect(addr).await {
                Ok(took) => rec.quic_handshake_ms = took.map(|d| d.as_secs_f64() * 1e3),
                Err(e) => rec.quic_error = Some(format!("{e:#}")),
            }
            if args.live && rec.quic_error.is_some() {
                let why = format!(
                    "QUIC connect failed: {}",
                    rec.quic_error.clone().unwrap_or_default()
                );
                skip(&paths, rec, &why, &mut skipped)?;
                continue;
            }
        }

        // A transient RPC error costs this window, not the run.
        if blockhash.1.elapsed() > BLOCKHASH_MAX_AGE {
            match rpc.get_latest_blockhash().await {
                Ok(hash) => blockhash = (hash, Instant::now()),
                Err(e) => {
                    skip(&paths, rec, &failed("getLatestBlockhash", &e), &mut skipped)?;
                    continue;
                }
            }
        }
        if let Some((_, rent)) = &watch
            && treatment.conflict == Conflict::Distinct
            && let Err(e) = top_up(
                &rpc,
                &main,
                &payers,
                &mut payer_left,
                *rent + 2 * max_tx_fee,
                args.fund_lamports,
                &mut budget,
            )
            .await
        {
            skip(&paths, rec, &failed("funding fee payers", &e), &mut skipped)?;
            continue;
        }

        let mut rng = StdRng::seed_from_u64(design.seed ^ w.start);
        let seq = send_sequence(n, treatment.order, &mut rng);
        // Distinct payers are reassigned to ranks in every window, so nothing
        // address-dependent (thread assignment, a sequencer's filters) can
        // follow the priority rank.
        let mut payer_of: Vec<usize> = (0..n).collect();
        payer_of.shuffle(&mut rng);
        let mut batch: Vec<(TxRecord, Vec<u8>)> = seq
            .iter()
            .enumerate()
            .map(|(k, &rank)| {
                let payer = match treatment.conflict {
                    Conflict::Shared => &main,
                    Conflict::Distinct => &payers[payer_of[rank]],
                };
                let tx = measurement_tx(
                    payer,
                    design.cu_limit,
                    prices[rank],
                    nonce(w.start, rank),
                    blockhash.0,
                );
                let record = TxRecord {
                    signature: tx.signatures[0].to_string(),
                    rank,
                    cu_price: prices[rank],
                    cu_limit: design.cu_limit,
                    payer: payer.pubkey().to_string(),
                    send_seq: k,
                    sent_at_us: None,
                    send_error: None,
                    worst_case_fee: worst_case_fee(design.cu_limit, prices[rank]),
                };
                (record, wire(&tx))
            })
            .collect();
        rec.blockhash = blockhash.0.to_string();

        if clock.now() >= w.start {
            skip(&paths, rec, "late: window already started", &mut skipped)?;
            continue;
        }
        if args.live {
            for (t, _) in &batch {
                budget.reserve("tx", Some(t.signature.clone()), t.worst_case_fee)?;
                if treatment.conflict == Conflict::Distinct {
                    let i = payer_of[t.rank];
                    payer_left[i] = payer_left[i].saturating_sub(t.worst_case_fee);
                }
            }
        } else {
            dry_spent += per_window;
        }

        clock.wait_until(fire_slot).await;
        rec.fire_slot_est = clock.now();
        rec.fire_at_us = unix_us();
        let t0 = Instant::now();
        for (k, (t, bytes)) in batch.iter_mut().enumerate() {
            let at = t0 + Duration::from_millis(treatment.pause_ms * k as u64);
            tokio::time::sleep_until(at.into()).await;
            t.sent_at_us = Some(unix_us());
            // A dead connection fails the remaining sends fast; a stream the leader
            // stopped (throttling) fails only that one. `connect` replaces dead ones.
            if args.live
                && let Err(e) = tpu.send(addr, bytes).await
            {
                t.send_error = Some(format!("{e:#}"));
            }
        }
        rec.txs = batch.into_iter().map(|(t, _)| t).collect();
        let errors = rec.txs.iter().filter(|t| t.send_error.is_some()).count();
        info!(
            leader = %w.leader,
            client = rec.client.as_deref().unwrap_or("?"),
            start = w.start,
            fire_slot_est = rec.fire_slot_est,
            lead = w.start as i64 - rec.fire_slot_est as i64,
            order = ?treatment.order,
            pause_ms = treatment.pause_ms,
            conflict = ?treatment.conflict,
            handshake_ms = rec.quic_handshake_ms,
            quic_error = rec.quic_error.as_deref(),
            send_errors = errors,
            "window"
        );
        append_jsonl(&paths.windows, &rec)?;
        // A window whose sends failed is repeated later with the same treatment cell.
        if rec.usable() {
            *done.entry(w.leader.clone()).or_default() += 1;
        }
        fired += 1;

        if let Some((watch, _)) = &watch {
            watch.check(&rpc, &budget).await?;
        }
    }

    tpu.close().await;
    if let Some((watch, _)) = &watch {
        // Give the last window time to land before the closing balance.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let spent = watch.check(&rpc, &budget).await?;
        budget.record_balance(watch.start_balance() - spent)?;
        info!(
            spent_on_chain = spent,
            reserved_total = budget.spent(),
            "balance check"
        );
    }
    info!(fired, skipped, windows_file = %paths.windows.display(), "run finished");
    Ok(())
}

fn skip(paths: &Paths, mut rec: WindowRecord, why: &str, skipped: &mut usize) -> Result<()> {
    warn!(leader = %rec.leader, start = rec.start_slot, "skip window: {why}");
    rec.skipped = Some(why.to_string());
    *skipped += 1;
    append_jsonl(&paths.windows, &rec)
}

/// Refill derived payers whose (conservatively tracked) balance is low, in one transaction.
async fn top_up(
    rpc: &RpcClient,
    main: &Keypair,
    payers: &[Keypair],
    left: &mut [u64],
    min_keep: u64,
    amount: u64,
    budget: &mut Budget,
) -> Result<()> {
    let low: Vec<usize> = (0..payers.len()).filter(|&i| left[i] < min_keep).collect();
    if low.is_empty() {
        return Ok(());
    }
    let ixs: Vec<_> = low
        .iter()
        .map(|&i| transfer(&main.pubkey(), &payers[i].pubkey(), amount))
        .collect();
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&main.pubkey()),
        &[main],
        rpc.get_latest_blockhash().await?,
    );
    budget.reserve(
        "fund",
        Some(tx.signatures[0].to_string()),
        BASE_FEE_PER_SIGNATURE,
    )?;
    rpc.send_and_confirm_transaction(&tx).await?;
    for &i in &low {
        left[i] += amount;
    }
    info!(payers = low.len(), amount, signature = %tx.signatures[0], "funded fee payers");
    Ok(())
}
