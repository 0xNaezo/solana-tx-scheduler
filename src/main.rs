use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
    sync::Arc,
};

use clap::Parser;
use solana_tx_scheduler::{
    cli::{Cli, Command, Context},
    commands,
    model::{scrub, url_secrets},
};

/// Log writer that scrubs the RPC API key: HTTP client errors quote the full
/// request URL, and those errors are logged as they are.
struct Scrubbed(Arc<Vec<String>>);

impl Write for Scrubbed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = String::from_utf8_lossy(buf);
        io::stdout().write_all(scrub(&line, &self.0).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let Cli {
        rpc_url,
        data_dir,
        command,
    } = Cli::parse();

    let secrets = Arc::new(url_secrets(&rpc_url));
    let writer = secrets.clone();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(io::stdout().is_terminal())
        .with_writer(move || Scrubbed(writer.clone()))
        .init();

    let ctx = Context { rpc_url, data_dir };

    let result = match command {
        Command::Plan(args) => commands::plan::execute(&ctx, args).await,
        Command::Run(args) => commands::run::execute(&ctx, args).await,
        Command::Collect(args) => commands::collect::execute(&ctx, args).await,
        Command::Analyze(args) => commands::analyze::execute(&ctx, args),
        Command::Verify(args) => commands::verify::execute(&ctx, args).await,
        Command::Fingerprint(args) => commands::fingerprint::execute(&ctx, args).await,
        Command::Simulate(args) => commands::simulate::execute(args),
        Command::Anchor(args) => commands::wallet::anchor(&ctx, args).await,
        Command::Sweep(args) => commands::wallet::sweep(&ctx, args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", scrub(&format!("{e:?}"), &secrets));
            ExitCode::FAILURE
        }
    }
}
