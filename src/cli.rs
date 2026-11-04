use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Measure how Solana validators order transactions inside their blocks.
#[derive(Debug, Parser)]
#[command(name = "solana-tx-scheduler", version, about)]
pub struct Cli {
    /// JSON-RPC endpoint (an API key in its query or path is never logged)
    #[arg(
        long,
        global = true,
        env = "RPC_URL",
        hide_env_values = true,
        default_value = "https://api.mainnet-beta.solana.com"
    )]
    pub rpc_url: String,

    /// Root of all raw data (plan.json, raw/*.jsonl, blocks/)
    #[arg(long, global = true, default_value = "data")]
    pub data_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

/// Shared runtime settings passed to every command.
#[derive(Debug, Clone)]
pub struct Context {
    pub rpc_url: String,
    pub data_dir: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Snapshot the cluster, classify leaders by client and pick validators + design
    Plan(PlanArgs),
    /// Build measurement transactions per leader window and send them over QUIC (dry-run unless --live)
    Run(RunArgs),
    /// Fetch statuses and blocks, locate our transactions, archive raw getBlock responses
    Collect(CollectArgs),
    /// Compute ordering statistics and regenerate report/ from raw data
    Analyze(AnalyzeArgs),
    /// Re-fetch every block from an RPC and check recorded slot/index of every transaction
    Verify(VerifyArgs),
    /// Print an ordering-rule card for any validator from its recent blocks (read-only)
    Fingerprint(FingerprintArgs),
    /// Power analysis and calibration of the classifier on synthetic windows
    Simulate(SimulateArgs),
    /// Anchor a file's SHA-256 on-chain with a memo transaction (dry-run unless --live)
    Anchor(AnchorArgs),
    /// Return lamports from derived fee payers to the main wallet (dry-run unless --live)
    Sweep(SweepArgs),
}

#[derive(Debug, Args)]
pub struct KeyArgs {
    /// Budget wallet keypair (mount via volume, never commit)
    #[arg(long, env = "KEYPAIR", default_value = "keys/id.json")]
    pub keypair: PathBuf,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    /// Validator identities; omit to select top --per-group by stake from every client group
    #[arg(long, value_delimiter = ',')]
    pub validators: Vec<String>,

    /// Client groups (gossip clientId) to select from; empty = all groups found
    #[arg(long, value_delimiter = ',')]
    pub groups: Vec<String>,

    /// Validators per client group
    #[arg(long, default_value_t = 3)]
    pub per_group: usize,

    /// Leader windows per validator
    #[arg(long, default_value_t = 20)]
    pub windows: usize,

    /// Transactions per window
    #[arg(long, default_value_t = 10)]
    pub txs_per_window: usize,

    /// Minimum activated stake, percent of total
    #[arg(long, default_value_t = 0.05)]
    pub min_stake_pct: f64,

    /// Maximum skip rate in the current epoch (0..1)
    #[arg(long, default_value_t = 0.05)]
    pub max_skip_rate: f64,

    /// Compute unit limit of every measurement transaction (they consume ~450)
    #[arg(long, default_value_t = 1_000)]
    pub cu_limit: u32,

    /// Highest compute unit price (micro-lamports per CU) of the priority ladder
    #[arg(long, default_value_t = 700_000)]
    pub cu_price_max: u64,

    /// Pause levels between consecutive sends, ms
    #[arg(long, value_delimiter = ',', default_value = "0,10,50")]
    pub pauses_ms: Vec<u64>,

    /// Start sending this many slots before the window's first slot
    #[arg(long, default_value_t = 2)]
    pub lead_slots: u64,

    /// RNG seed for treatment assignment and random send orders (default: random)
    #[arg(long)]
    pub seed: Option<u64>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Actually send transactions (default is dry-run: build and sign only)
    #[arg(long)]
    pub live: bool,

    /// Fee cap for all live runs together (lamports); cannot exceed the compiled-in hard cap
    #[arg(long, default_value_t = crate::budget::HARD_CAP_LAMPORTS)]
    pub max_spend_lamports: u64,

    /// Refuse --live if the wallet holds more than this: it must be a dedicated budget wallet
    #[arg(long, default_value_t = 100_000_000)]
    pub max_wallet_lamports: u64,

    /// Lamports moved to each derived fee payer when its balance runs low
    #[arg(long, default_value_t = 1_500_000)]
    pub fund_lamports: u64,

    /// Stop after this many windows in total
    #[arg(long)]
    pub max_windows: Option<usize>,

    /// Stop after this many minutes
    #[arg(long)]
    pub max_minutes: Option<u64>,

    /// Skip the QUIC handshake probe in dry-run
    #[arg(long)]
    pub no_probe: bool,

    #[command(flatten)]
    pub key: KeyArgs,
}

#[derive(Debug, Args)]
pub struct CollectArgs {
    /// Re-fetch blocks that are already archived
    #[arg(long)]
    pub refetch: bool,
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Output directory for REPORT.md and summary.json [default: report for
    /// --data-dir data, else <data-dir>/report]
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Only check archive hashes and recorded indices against the local archive (no network)
    #[arg(long)]
    pub offline: bool,

    /// Also re-derive every reduced block row (foreign-transaction evidence) from the RPC
    #[arg(long)]
    pub full: bool,

    /// Re-fetch only this many blocks, spread evenly over the run (a quick on-chain spot check)
    #[arg(long)]
    pub sample: Option<usize>,
}

#[derive(Debug, Args)]
pub struct FingerprintArgs {
    /// Validator identity pubkey
    #[arg(long)]
    pub validator: String,

    /// How many of its most recent blocks to read
    #[arg(long, default_value_t = 12)]
    pub blocks: usize,
}

#[derive(Debug, Args)]
pub struct SimulateArgs {
    /// Numbers of windows per validator to evaluate
    #[arg(long, value_delimiter = ',', default_value = "5,10,20,40")]
    pub windows: Vec<usize>,

    /// Monte-Carlo repetitions per cell
    #[arg(long, default_value_t = 300)]
    pub reps: usize,

    /// Validators in the run: Holm corrects over two tests for each
    #[arg(long, default_value_t = 23)]
    pub validators: usize,

    /// Transactions per window
    #[arg(long, default_value_t = 10)]
    pub txs_per_window: usize,

    /// Output markdown file
    #[arg(long, default_value = "report/power.md")]
    pub out: PathBuf,
}

#[derive(Debug, Args)]
pub struct AnchorArgs {
    /// File whose SHA-256 is written on-chain
    #[arg(long)]
    pub file: PathBuf,

    /// Short label stored in the memo
    #[arg(long, default_value = "prereg")]
    pub label: String,

    /// Actually send the memo transaction
    #[arg(long)]
    pub live: bool,

    #[command(flatten)]
    pub key: KeyArgs,
}

#[derive(Debug, Args)]
pub struct SweepArgs {
    /// Actually send the sweep transactions
    #[arg(long)]
    pub live: bool,

    /// Number of derived payers to sweep
    #[arg(long, default_value_t = 32)]
    pub payers: usize,

    #[command(flatten)]
    pub key: KeyArgs,
}
