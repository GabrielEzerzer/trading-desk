//! `td-server`: the engine sidecar. Electron starts it, reads the handshake line from stdout, and
//! talks to it over 127.0.0.1. It stops when its stdin closes (the parent app exited) or on
//! Ctrl-C.
//!
//! stdout carries exactly one line, `{"port":<n>,"token":"<hex>"}`; logs go to stderr. Set
//! `TD_TOKEN` to fix the token for scripts, and `TD_DATA_DIR` to choose the data directory.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use td_data::Store;
use td_server::api::auth::Token;
use td_server::api::source::VenueSource;
use td_server::api::{ApiConfig, Server};
use tokio::sync::oneshot;

#[derive(Debug, Parser)]
#[command(
    name = "td-server",
    about = "The Trading Desk engine sidecar: a localhost API guarded by a per-launch token."
)]
struct Cli {
    /// The port on 127.0.0.1; 0 lets the operating system choose.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

/// How long running jobs get to reach a checkpoint after shutdown starts.
const JOB_GRACE: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("td-server: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli));
    // Blocking job threads that do not reach a checkpoint in time are abandoned, not waited on.
    runtime.shutdown_timeout(JOB_GRACE);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-server: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let token = match std::env::var("TD_TOKEN") {
        Ok(value) if !value.is_empty() => {
            Token::from_value(&value).map_err(|e| format!("TD_TOKEN: {e}"))?
        }
        _ => Token::generate().map_err(|e| format!("cannot draw a token: {e}"))?,
    };
    let store = Store::open_default().map_err(|e| e.to_string())?;
    let data_dir = store.root().to_path_buf();
    let server = Server::start(ApiConfig {
        store,
        source: Arc::new(VenueSource),
        token,
        port: cli.port,
    })
    .await
    .map_err(|e| format!("cannot listen on 127.0.0.1:{}: {e}", cli.port))?;

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", server.handshake_line())
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("cannot write the handshake: {e}"))?;
    drop(stdout);
    eprintln!(
        "td-server: listening on {}, data in {}",
        server.addr(),
        data_dir.display()
    );

    let reason = tokio::select! {
        () = stdin_closed() => "stdin closed",
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => "Ctrl-C",
            Err(e) => return Err(format!("cannot listen for Ctrl-C: {e}")),
        },
    };
    eprintln!("td-server: {reason}, shutting down");
    server.shutdown().await.map_err(|e| e.to_string())?;
    eprintln!("td-server: stopped");
    Ok(())
}

/// Resolves when stdin reaches end of file, which is how a parent process that exits (or closes
/// the pipe) tells the sidecar to stop. Reads on a plain thread, which the process does not wait
/// for on exit.
async fn stdin_closed() {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let mut sink = [0u8; 1024];
        let mut stdin = std::io::stdin().lock();
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
        let _ = tx.send(());
    });
    let _ = rx.await;
}
