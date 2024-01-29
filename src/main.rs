//! A simple file sharing service.  The service program can operate in two modes:
//! * Server mode:  The server mode is a simple file sharing service that allows clients to connect
//! using peer-to-peer networking services from libp2p and download files from the server.
//! * Client mode: The client mode is a simple command line REPL (read-eval-print-loop) that allows
//! the user to view remote peers, request the list of files from the remote peer and download
//! files from the remote peer.

use crate::command_core::{setup_client_system, setup_file_provider_system};

mod command_core;
mod files;
mod repl;

use clap::Parser;

#[doc(hidden)]
pub type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

#[doc(hidden)]
#[derive(Parser)]
#[command(author="Steve Wilson", version, long_about = None)]
#[command(about = "File Sharing Service")]
struct CLInterface {
    /// Run in server mode.
    #[arg(short, long, default_value_t = false)]
    server: bool,

    /// Run in client mode.
    #[arg(short, long, default_value_t = true)]
    client: bool,

    /// Report file names from this directory.
    #[arg(long, default_value = ".")]
    directory: String,

    /// Manually set the program log level.
    #[arg(short, long, default_value_t = log::LevelFilter::Off)]
    log_level: log::LevelFilter,
}

#[doc(hidden)]
#[tokio::main]
async fn main() -> DynResult<()> {
    let cli = CLInterface::parse();

    let log_level = if cli.server && cli.log_level == log::LevelFilter::Off {
        log::LevelFilter::Info
    } else {
        cli.log_level
    };

    pretty_env_logger::formatted_timed_builder()
        .filter_level(log_level)
        .init();

    if cli.server {
        let mut command_core = setup_file_provider_system(&cli.directory)?;

        let core_handle = command_core.run();

        let join_results = tokio::try_join!(core_handle);

        if let Err(e) = join_results {
            return Err(e);
        }
    } else if cli.client {
        let (mut repl, mut command_core) = setup_client_system(&cli.directory)?;

        let core_handle = command_core.run();

        let repl_handle = repl.run();

        let join_results = tokio::try_join!(core_handle, repl_handle);

        if let Err(e) = join_results {
            return Err(e);
        }
    }

    Ok(())
}
