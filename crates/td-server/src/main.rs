//! `td-server`: the engine sidecar. Electron starts it, reads the handshake line from stdout, and
//! talks to it over 127.0.0.1. It stops when its stdin closes (the parent app exited) or on
//! Ctrl-C.
//!
//! stdout carries exactly one line, `{"port":<n>,"token":"<hex>"}`; logs go to stderr. Set
//! `TD_TOKEN` to fix the token for scripts, `TD_DATA_DIR` to choose the data directory, and
//! `TD_DATA_CUTOFF=YYYY-MM-DD` to refuse every run that reads bars after that UTC date; a value
//! that is not a date stops the engine at start.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use td_data::Store;
use td_server::api::auth::Token;
use td_server::api::runs::DataCutoff;
use td_server::api::source::{VenueLive, VenueSource};
use td_server::api::{ApiConfig, JOB_GRACE, Server};
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
    let data_cutoff = DataCutoff::from_env()?;
    let token = match std::env::var("TD_TOKEN") {
        Ok(value) if !value.is_empty() => {
            Token::from_value(&value).map_err(|e| format!("TD_TOKEN: {e}"))?
        }
        _ => Token::generate().map_err(|e| format!("cannot draw a token: {e}"))?,
    };
    let store = Store::open_default().map_err(|e| e.to_string())?;
    let data_dir = store.root().to_path_buf();
    let server = Server::start(ApiConfig {
        source: Arc::new(VenueSource::new(store.clone())),
        store,
        live: Arc::new(VenueLive),
        token,
        port: cli.port,
        data_cutoff: data_cutoff.clone(),
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
    if let Some(cutoff) = &data_cutoff {
        eprintln!(
            "td-server: data cutoff {}: runs that read bars after it are refused",
            cutoff.date()
        );
    }

    let reason = tokio::select! {
        () = stdin_closed() => "stdin closed",
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => "Ctrl-C",
            Err(e) => return Err(format!("cannot listen for Ctrl-C: {e}")),
        },
    };
    eprintln!("td-server: {reason}, shutting down");
    // Open requests get the grace period to finish; one that never completes (a client that sends
    // half a request and stalls) must not keep the sidecar alive after its parent has gone.
    match tokio::time::timeout(JOB_GRACE, server.shutdown()).await {
        Ok(result) => result.map_err(|e| e.to_string())?,
        Err(_) => eprintln!(
            "td-server: open requests did not finish within {}s, exiting anyway",
            JOB_GRACE.as_secs()
        ),
    }
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
