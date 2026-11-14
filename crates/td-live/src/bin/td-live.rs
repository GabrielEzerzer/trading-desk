//! `td-live`: runs a live session on a venue's demo-trading environment with its stop rules.
//!
//! Run it as `hush run -e dev -- td-live <command> ...`: the keys come only from the environment
//! (`td_orders::DemoKeys`), and the client is built for the demo endpoint only. See the README,
//! "Live sessions on a demo account".

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use td_data::live::{SharedStream, VenueRest, install_crypto_provider, stream_url};
use td_live::cli::{Cli, Env, execute};
use td_live::{GuardTiming, StopHandle};
use td_orders::{Client, DemoKeys, Endpoint};

fn main() -> ExitCode {
    let cli = Cli::parse();
    install_crypto_provider();
    let stop = StopHandle::new();
    watch_ctrl_c(stop.clone());
    let env = Env {
        connect: Box::new(|| {
            let keys = DemoKeys::from_env().map_err(|e| e.to_string())?;
            Client::new(Endpoint::Demo, keys)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        }),
        rest: Arc::new(VenueRest),
        stream: Arc::new(|id| {
            let url = stream_url(&id);
            Arc::new(SharedStream::open(id, url))
        }),
        build: td_engine::build_strategy,
        data_dir: Box::new(|| td_data::data_dir().map_err(|e| e.to_string())),
        timing: GuardTiming::default(),
        stop,
    };
    let code = execute(cli, &env, &mut |line| println!("{line}"));
    ExitCode::from(code)
}

/// Asks the run to stop on Ctrl-C, from a thread of its own.
fn watch_ctrl_c(stop: StopHandle) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        if runtime.block_on(tokio::signal::ctrl_c()).is_ok() {
            stop.stop();
        }
    });
}
