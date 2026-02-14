#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod args;
mod client;
mod commands;
mod error;
mod output;

use clap::Parser;

use crate::args::Cli;
use crate::client::MillClient;

use crate::output::OutputMode;

fn main() {
    let cli = Cli::parse();

    let mode = if cli.json { OutputMode::Json } else { OutputMode::Human };

    let client = MillClient::new(&cli.address, cli.token.as_deref());

    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start runtime: {e}");
            std::process::exit(1);
        }
    };

    let result = rt.block_on(commands::dispatch(cli.command, &client, mode, &cli.data_dir));

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
}
