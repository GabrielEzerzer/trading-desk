//! `td-data`: list instruments, download history into the store, and show what the store holds.

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use td_core::{InstrumentId, InstrumentKind, Venue};
use td_data::time::{format_minute, parse_date};
use td_data::{DataError, Gap, Http, Store, download_candles, download_funding, fetch_instruments};

#[derive(Debug, Parser)]
#[command(
    name = "td-data",
    about = "Market data for Trading Desk. The store lives in TD_DATA_DIR, or the platform data folder."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the instruments a venue has open for trading.
    Instruments {
        #[arg(long)]
        venue: VenueArg,
        #[arg(long)]
        kind: KindArg,
    },
    /// Download 1-minute candles (and funding rates, for a perpetual) into the store.
    Download {
        #[command(flatten)]
        instrument: InstrumentArgs,
        /// First day, YYYY-MM-DD (UTC).
        #[arg(long)]
        from: String,
        /// Day after the last one, YYYY-MM-DD (UTC): the range is [from, to).
        #[arg(long)]
        to: String,
    },
    /// Show which months and minutes the store holds for an instrument.
    Coverage {
        #[command(flatten)]
        instrument: InstrumentArgs,
    },
}

#[derive(Debug, clap::Args)]
struct InstrumentArgs {
    #[arg(long)]
    venue: VenueArg,
    #[arg(long)]
    kind: KindArg,
    /// The venue's symbol, for example BTCUSDT.
    #[arg(long)]
    symbol: String,
}

impl InstrumentArgs {
    fn id(&self) -> InstrumentId {
        InstrumentId {
            venue: self.venue.into(),
            symbol: self.symbol.clone(),
            kind: self.kind.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum VenueArg {
    Binance,
    Bybit,
}

impl From<VenueArg> for Venue {
    fn from(v: VenueArg) -> Venue {
        match v {
            VenueArg::Binance => Venue::Binance,
            VenueArg::Bybit => Venue::Bybit,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum KindArg {
    Spot,
    Perpetual,
    Fx,
}

impl From<KindArg> for InstrumentKind {
    fn from(k: KindArg) -> InstrumentKind {
        match k {
            KindArg::Spot => InstrumentKind::Spot,
            KindArg::Perpetual => InstrumentKind::Perpetual,
            KindArg::Fx => InstrumentKind::Fx,
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), DataError> {
    match cli.command {
        Command::Instruments { venue, kind } => {
            let list = fetch_instruments(&Http::new()?, venue.into(), kind.into())?;
            println!("symbol\tbase\tquote\ttick\tlot\tmin_qty\tmin_notional\tmaker\ttaker");
            for i in &list.instruments {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    i.id.symbol,
                    i.base,
                    i.quote,
                    i.tick_size,
                    i.lot_size,
                    i.min_qty,
                    i.min_notional,
                    i.fees.maker,
                    i.fees.taker
                );
            }
            for (symbol, reason) in &list.rejected {
                eprintln!("left out {symbol}: {reason}");
            }
            eprintln!(
                "{} instruments, {} left out",
                list.instruments.len(),
                list.rejected.len()
            );
        }
        Command::Download {
            instrument,
            from,
            to,
        } => {
            let id = instrument.id();
            let (from_ms, to_ms) = (parse_date(&from)?, parse_date(&to)?);
            if from_ms >= to_ms {
                return Err(DataError::EmptyRange { from, to });
            }
            let (from, to) = (from_ms, to_ms);
            let store = Store::open_default()?;
            let http = Http::new()?;
            eprintln!("store: {}", store.root().display());
            let report = download_candles(&http, &store, &id, from, to, &mut |line| {
                eprintln!("{line}")
            })?;
            let total: usize = report.months.iter().map(|(_, n)| n).sum();
            println!("{id}: {total} candles downloaded");
            for file in &report.missing_files {
                println!("not in the archive: {file}");
            }
            print_gaps(&report.gaps);
            if id.kind == InstrumentKind::Perpetual {
                let rates = download_funding(&http, &store, &id, from, to)?;
                println!("{id}: {rates} funding rates downloaded");
            }
        }
        Command::Coverage { instrument } => {
            let id = instrument.id();
            let store = Store::open_default()?;
            let coverage = store.coverage(&id)?;
            if coverage.is_empty() {
                println!("{id}: no candles stored in {}", store.root().display());
            }
            for month in &coverage {
                println!(
                    "{} {}: {}/{} minutes, {} to {}",
                    id,
                    month.month,
                    month.minutes,
                    month.month.minutes(),
                    format_minute(month.first),
                    format_minute(month.last)
                );
                print_gaps(&month.gaps);
            }
            if id.kind == InstrumentKind::Perpetual {
                let rates = store.load_funding(&id, i64::MIN, i64::MAX)?;
                match (rates.first(), rates.last()) {
                    (Some(first), Some(last)) => println!(
                        "{id} funding: {} rates, {} to {}",
                        rates.len(),
                        format_minute(first.0),
                        format_minute(last.0)
                    ),
                    _ => println!("{id} funding: none stored"),
                }
            }
        }
    }
    Ok(())
}

/// Prints missing-minute runs, at most 20 of them.
fn print_gaps(gaps: &[Gap]) {
    if gaps.is_empty() {
        return;
    }
    let minutes: i64 = gaps.iter().map(Gap::minutes).sum();
    println!("  {} gaps, {minutes} minutes missing", gaps.len());
    for gap in gaps.iter().take(20) {
        println!(
            "  missing from {} up to {} ({} minutes)",
            format_minute(gap.from),
            format_minute(gap.to),
            gap.minutes()
        );
    }
    if gaps.len() > 20 {
        println!("  ... and {} more", gaps.len() - 20);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_argument_takes_each_kind_by_its_name() {
        for (name, kind) in [
            ("spot", InstrumentKind::Spot),
            ("perpetual", InstrumentKind::Perpetual),
            ("fx", InstrumentKind::Fx),
        ] {
            let cli = Cli::try_parse_from([
                "td-data",
                "instruments",
                "--venue",
                "binance",
                "--kind",
                name,
            ])
            .expect("parses");
            let Command::Instruments { kind: arg, .. } = cli.command else {
                panic!("not the instruments command");
            };
            assert_eq!(InstrumentKind::from(arg), kind);
            assert_eq!(kind.to_string(), name);
        }
        let bad = [
            "td-data",
            "instruments",
            "--venue",
            "binance",
            "--kind",
            "forex",
        ];
        assert!(Cli::try_parse_from(bad).is_err());
    }
}
