mod cli;
mod mcp;

use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.command.verbose());

    match cli.command {
        Command::Console(args) => cli::run_console(args, true),
        Command::On(args) => cli::run_console(args, true),
        Command::Reset(args) => cli::run_console(args, true),
        Command::Off(args) => cli::run_off(args),
        Command::Watch(args) => {
            let code = cli::run_watch(args)?;
            std::process::exit(code);
        }
        Command::Mcp(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(mcp::run_server(args))
        }
    }
}

/// All diagnostic output goes to stderr — for the `mcp` subcommand, stdout
/// is the JSON-RPC transport and must never see anything else.
fn init_tracing(verbose: u8) {
    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
