//! Reference instruments (#187): instruments on the run's venue whose closed bars a strategy reads
//! beside its own, never trades.
//!
//! A run names them in [`BacktestConfig::references`] and is given their bars at its timeframe as
//! [`ReferenceBars`]. Each is aligned to the run's own bars by open time: one slot per own bar,
//! warm-up bars included, holding the reference bar that opens at the same time, or `None` where
//! the reference has no bar. Both series are on one timeframe, so a reference bar in a slot closes
//! when the strategy's own bar does, never later; the session holds no later slot, so a strategy
//! cannot see a reference bar that closes after its own bar. A missing bar stays `None`: nothing is
//! filled forward. A reference bar at a time the run has no bar of its own is not shown.

use std::collections::HashMap;

use td_core::{Candle, InstrumentId};

use crate::{BacktestConfig, EngineError};

/// The bars of one reference instrument a run names, at the run's timeframe, in any order. Only
/// those that open at the open time of one of the run's own bars are used.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceBars {
    pub instrument: InstrumentId,
    pub bars: Vec<Candle>,
}

/// Each reference instrument's bars aligned to a run's own bars: one slot per own bar, holding the
/// reference bar that opens at the same time, or `None`.
pub(crate) type Aligned = Vec<(InstrumentId, Vec<Option<Candle>>)>;

/// The bars `given` for `config`'s reference instruments, aligned to `used`, the run's own bars in
/// time order. Refuses names that [`BacktestConfig::check_references`] refuses, a named reference
/// without bars, bars for an instrument the configuration does not name or given twice, two bars of
/// one reference with the same open time, and an invalid bar that a slot would hold. Empty, and
/// without allocating, when the configuration names none and none are given.
pub(crate) fn align(
    config: &BacktestConfig,
    used: &[Candle],
    given: &[ReferenceBars],
) -> Result<Aligned, EngineError> {
    config.check_references()?;
    let refuse = |why: String| Err(EngineError::InvalidReferences(why));
    for (i, r) in given.iter().enumerate() {
        if !config.references.contains(&r.instrument) {
            return refuse(format!(
                "bars were given for {}, which the run does not name",
                r.instrument
            ));
        }
        if given[..i].iter().any(|g| g.instrument == r.instrument) {
            return refuse(format!("bars were given twice for {}", r.instrument));
        }
    }
    let mut aligned = Vec::with_capacity(config.references.len());
    for id in &config.references {
        let Some(r) = given.iter().find(|r| &r.instrument == id) else {
            return refuse(format!("no bars were given for {id}"));
        };
        let mut by_time = HashMap::with_capacity(r.bars.len());
        for bar in &r.bars {
            if by_time.insert(bar.open_time, *bar).is_some() {
                return refuse(format!("two bars of {id} open at {}", bar.open_time));
            }
        }
        let slots = used
            .iter()
            .map(|own| by_time.get(&own.open_time).copied())
            .collect::<Vec<_>>();
        for bar in slots.iter().flatten() {
            bar.validate()?;
        }
        aligned.push((id.clone(), slots));
    }
    Ok(aligned)
}

/// The slot of each reference at own bar `j`, in the configuration's order: what the session is
/// fed with that bar.
pub(crate) fn row(aligned: &Aligned, j: usize) -> Vec<Option<Candle>> {
    aligned.iter().map(|(_, slots)| slots[j]).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use td_core::{
        AccountCurrency, FeeSchedule, Instrument, InstrumentKind, Millis, OrderRequest, OrderType,
        Side, TimeInForce, Timeframe, Venue,
    };

    use super::*;
    use crate::runner::{run_built, run_with_references};
    use crate::{
        Context, Params, Session, SessionConfig, Strategy, StrategyEntry, config_fingerprint,
        data_fingerprint, run,
    };

    const HOUR: Millis = 3_600_000;

    fn id(symbol: &str) -> InstrumentId {
        InstrumentId {
            venue: Venue::Binance,
            symbol: symbol.into(),
            kind: InstrumentKind::Perpetual,
        }
    }

    fn instrument() -> Instrument {
        Instrument {
            id: id("TESTUSDT"),
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 1.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.002,
                per_unit: 0.0,
            },
        }
    }

    fn bar(i: i64, close: f64) -> Candle {
        Candle {
            open_time: i * HOUR,
            open: close,
            high: close * 1.01,
            low: close * 0.99,
            close,
            volume: 1.0,
            quote_volume: close,
        }
    }

    /// Own bars at 0h to 11h, a wave, with none at 7h.
    fn own_bars() -> Vec<Candle> {
        (0..12)
            .filter(|&i| i != 7)
            .map(|i| bar(i, 100.0 + 5.0 * ((i as f64) / 2.0).sin()))
            .collect()
    }

    /// Reference bars at 0h to 12h with none at 4h, one at 7h where the run has none, and one at
    /// 12h, after the run's last bar.
    fn reference_bars() -> Vec<Candle> {
        (0..13)
            .filter(|&i| i != 4)
            .map(|i| bar(i, 50.0 + i as f64))
            .collect()
    }

    fn config(references: Vec<InstrumentId>) -> BacktestConfig {
        BacktestConfig {
            strategy: "test_sees_references".into(),
            params: Params::new(),
            instrument: instrument(),
            timeframe: Timeframe::H1,
            from: 3 * HOUR,
            to: 12 * HOUR,
            starting_cash: 10_000.0,
            slippage_bps: 2.0,
            fee_override: None,
            max_leverage: 1.0,
            warmup_from: Some(0),
            close_at_end: false,
            account_currency: AccountCurrency::Quote,
            references,
        }
    }

    /// What the test double saw on each bar: its open time, whether it was warming up, and the
    /// reference's slots, or `None` when the context had no such reference.
    type Sight = (Millis, bool, Option<Vec<Option<Candle>>>);
    type Seen = Arc<Mutex<Vec<Sight>>>;

    /// Records what it sees of `reference`, and on every bar from the fourth buys one unit when the
    /// reference closed higher than its bar before and sells when lower, so its trades depend on
    /// the reference.
    struct SeesReference {
        reference: InstrumentId,
        seen: Seen,
    }

    impl Strategy for SeesReference {
        fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
            let series = ctx.reference(&self.reference).map(<[_]>::to_vec);
            let warm = ctx.is_warming_up();
            self.seen
                .lock()
                .unwrap()
                .push((bar.open_time, warm, series.clone()));
            let Some(series) = series else { return };
            if warm || series.len() < 2 {
                return;
            }
            let (Some(now), Some(before)) = (series[series.len() - 1], series[series.len() - 2])
            else {
                return;
            };
            let side = if now.close > before.close {
                Side::Buy
            } else {
                Side::Sell
            };
            let _ = ctx.submit(OrderRequest {
                instrument: ctx.instrument().id.clone(),
                side,
                qty: 1.0,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            });
        }
    }

    fn backtest(
        config: &BacktestConfig,
        given: &[ReferenceBars],
        reference: InstrumentId,
    ) -> Result<(crate::BacktestResult, Vec<Sight>), EngineError> {
        let seen = Seen::default();
        let strategy = SeesReference {
            reference,
            seen: seen.clone(),
        };
        let result = run_built(config, &own_bars(), &[], &[], given, || {
            Ok(Box::new(strategy))
        })?;
        let seen = seen.lock().unwrap().clone();
        Ok((result, seen))
    }

    fn given() -> Vec<ReferenceBars> {
        vec![ReferenceBars {
            instrument: id("OTHERUSDT"),
            bars: reference_bars(),
        }]
    }

    #[test]
    fn a_strategy_sees_each_reference_bar_that_opens_with_one_of_its_own_and_nothing_later() {
        let (_, seen) =
            backtest(&config(vec![id("OTHERUSDT")]), &given(), id("OTHERUSDT")).unwrap();
        let own = own_bars();
        let refs = reference_bars();
        assert_eq!(seen.len(), own.len());
        assert_eq!(seen.iter().filter(|s| s.1).count(), 3, "three warm-up bars");
        for (k, (open_time, _, series)) in seen.iter().enumerate() {
            let series = series.as_ref().expect("the run names the reference");
            // One slot per own bar so far, the warm-up bars included, ending at this bar.
            assert_eq!(series.len(), k + 1, "on the bar at {open_time}");
            let close = open_time + HOUR;
            for (slot, own_bar) in series.iter().zip(&own[..=k]) {
                match slot {
                    Some(r) => {
                        // Nothing from the future: every reference bar seen has closed by the
                        // close of the bar the strategy is called on.
                        assert!(r.open_time + HOUR <= close);
                        assert_eq!(r.open_time, own_bar.open_time);
                        assert!(refs.contains(r));
                    }
                    // A gap is seen as missing and stays missing, never filled forward.
                    None => assert_eq!(own_bar.open_time, 4 * HOUR),
                }
            }
        }
        // The reference bar at 7h, where the run has no bar, and the one at 12h, after its last,
        // are never seen.
        let last = seen.last().unwrap().2.as_ref().unwrap();
        assert!(last.iter().flatten().all(|r| r.open_time != 7 * HOUR));
        assert!(last.iter().flatten().all(|r| r.open_time < 12 * HOUR));
        assert_eq!(last.iter().filter(|s| s.is_none()).count(), 1);
    }

    #[test]
    fn a_reference_is_read_only_and_its_orders_are_refused() {
        /// Sends one order for the reference instrument on the first traded bar.
        struct OrdersTheReference {
            sent: bool,
        }
        impl Strategy for OrdersTheReference {
            fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
                if !ctx.is_warming_up() && !self.sent {
                    self.sent = true;
                    let _ = ctx.submit(OrderRequest {
                        instrument: id("OTHERUSDT"),
                        side: Side::Buy,
                        qty: 1.0,
                        order_type: OrderType::Market,
                        tif: TimeInForce::Gtc,
                        reduce_only: false,
                        client_tag: None,
                    });
                }
            }
        }
        let config = config(vec![id("OTHERUSDT")]);
        let result = run_built(&config, &own_bars(), &[], &[], &given(), || {
            Ok(Box::new(OrdersTheReference { sent: false }))
        })
        .unwrap();
        assert_eq!(result.orders.len(), 1);
        assert!(
            matches!(&result.orders[0].status, crate::OrderStatus::Rejected(why) if why.contains("OTHERUSDT")),
            "{:?}",
            result.orders[0].status
        );
        assert!(result.fills.is_empty());
        assert_eq!(result.metrics.fees_paid, 0.0);
    }

    #[test]
    fn trades_follow_the_reference_but_fill_and_pay_on_the_traded_instrument() {
        let (result, _) =
            backtest(&config(vec![id("OTHERUSDT")]), &given(), id("OTHERUSDT")).unwrap();
        assert!(!result.fills.is_empty());
        for fill in &result.fills {
            assert_eq!(fill.instrument, instrument().id);
            // At the traded instrument's own opens, near 100, not the reference's, near 50.
            assert!(fill.price > 90.0, "{fill:?}");
        }
        // The account stays one account on the traded instrument's prices.
        let own: Vec<Millis> = own_bars()
            .iter()
            .filter(|b| b.open_time >= 3 * HOUR)
            .map(|b| b.open_time)
            .collect();
        let times: Vec<Millis> = result.equity.iter().map(|p| p.time).collect();
        assert_eq!(times, own);
    }

    #[test]
    fn a_run_without_references_sees_none_and_is_unchanged_by_the_change() {
        let (without, seen) = backtest(&config(Vec::new()), &[], id("OTHERUSDT")).unwrap();
        assert!(seen.iter().all(|s| s.2.is_none()));
        // The fingerprint is the one data_fingerprint gives for the bars alone.
        assert_eq!(
            without.data_fingerprint,
            data_fingerprint(&own_bars(), &[], &[])
        );
        // The saved configuration leaves the field out.
        let json = serde_json::to_value(&without.config).unwrap();
        assert!(json.get("references").is_none(), "{json}");
        let back: BacktestConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, without.config);
    }

    #[test]
    fn the_fingerprint_covers_the_reference_bars_seen_and_only_those() {
        let config = config(vec![id("OTHERUSDT")]);
        let (result, _) = backtest(&config, &given(), id("OTHERUSDT")).unwrap();
        let fingerprint = |given: &[ReferenceBars]| -> String {
            crate::runner::config_fingerprint_with_references(&config, &own_bars(), &[], &[], given)
                .unwrap()
        };
        assert_eq!(fingerprint(&given()), result.data_fingerprint);
        assert_ne!(
            result.data_fingerprint,
            data_fingerprint(&own_bars(), &[], &[])
        );
        // A changed bar the strategy saw changes it; one it never saw (7h, 12h) does not.
        let mut seen_changed = given();
        seen_changed[0].bars[2].volume += 1.0;
        assert_ne!(fingerprint(&seen_changed), result.data_fingerprint);
        let mut unseen_changed = given();
        for b in &mut unseen_changed[0].bars {
            if b.open_time == 7 * HOUR || b.open_time == 12 * HOUR {
                b.volume += 1.0;
            }
        }
        assert_eq!(fingerprint(&unseen_changed), result.data_fingerprint);
        // A missing bar where there was one changes it.
        let mut gap = given();
        gap[0].bars.retain(|b| b.open_time != 5 * HOUR);
        assert_ne!(fingerprint(&gap), result.data_fingerprint);
    }

    #[test]
    fn references_are_refused_when_named_wrongly_or_without_their_bars() {
        let named = config(vec![id("OTHERUSDT")]);
        let refused = |config: &BacktestConfig, given: &[ReferenceBars]| match run_with_references(
            config,
            &own_bars(),
            &[],
            &[],
            given,
        ) {
            Err(EngineError::InvalidReferences(why)) => why,
            other => panic!("{other:?}"),
        };
        // A mode that cannot supply the bars fails clearly instead of running without them.
        assert!(refused(&named, &[]).contains("no bars were given for"));
        let err = run(&named, &own_bars(), &[], &[]).unwrap_err();
        assert!(matches!(err, EngineError::InvalidReferences(_)), "{err:?}");
        assert!(matches!(
            config_fingerprint(&named, &own_bars(), &[], &[]),
            Err(EngineError::InvalidReferences(_))
        ));
        assert!(refused(&config(Vec::new()), &given()).contains("does not name"));
        let twice = [given(), given()].concat();
        assert!(refused(&named, &twice).contains("twice"));
        let mut duplicate_bar = given();
        let first = duplicate_bar[0].bars[0];
        duplicate_bar[0].bars.push(first);
        assert!(refused(&named, &duplicate_bar).contains("open at"));
        let other_venue = InstrumentId {
            venue: Venue::Bybit,
            ..id("OTHERUSDT")
        };
        assert!(refused(&config(vec![other_venue]), &[]).contains("same venue"));
        assert!(refused(&config(vec![id("TESTUSDT")]), &[]).contains("traded instrument"));
        let both = config(vec![id("OTHERUSDT"), id("OTHERUSDT")]);
        assert!(refused(&both, &given()).contains("named twice"));
        // An invalid bar the strategy would see is a data error.
        let mut invalid = given();
        invalid[0].bars[1].high = 1.0;
        assert!(matches!(
            run_with_references(&named, &own_bars(), &[], &[], &invalid),
            Err(EngineError::Core(_))
        ));
    }

    #[test]
    fn a_session_refuses_a_reference_bar_from_another_time_and_changes_nothing() {
        let session_config = SessionConfig {
            starting_cash: 1_000.0,
            slippage_bps: 0.0,
            fees: None,
            max_leverage: 1.0,
            account_currency: AccountCurrency::Quote,
        };
        let seen = Seen::default();
        let strategy = Box::new(SeesReference {
            reference: id("OTHERUSDT"),
            seen: seen.clone(),
        });
        let mut session =
            Session::new(strategy, instrument(), Timeframe::H1, session_config).unwrap();
        session.name_references(vec![id("OTHERUSDT")]);
        let refused = session.feed(bar(0, 100.0), None, &[], &[Some(bar(1, 50.0))], true);
        assert!(matches!(refused, Err(EngineError::InvalidReferences(_))));
        // One slot per reference, no more and no fewer.
        assert!(session.feed(bar(0, 100.0), None, &[], &[], true).is_err());
        assert!(session.bars().is_empty());
        session
            .feed(bar(0, 100.0), None, &[], &[Some(bar(0, 50.0))], true)
            .unwrap();
        session
            .feed(bar(1, 100.0), None, &[], &[None], false)
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1].2, Some(vec![Some(bar(0, 50.0)), None]));
    }

    // The testers and the battery, which build the registered test double `Follows` by name.

    /// Sixty hourly bars of a wandering path, from `seed`.
    fn wander(seed: u64, start: f64) -> Vec<Candle> {
        let mut rng = crate::rng::SplitMix64(seed);
        let mut close = start;
        (0..60)
            .map(|i| {
                close *= 1.0 + (rng.unit() - 0.5) * 0.04;
                bar(i, close)
            })
            .collect()
    }

    /// The reference of the tester tests: its own wander, with no bar at 30h.
    fn tester_reference() -> Vec<ReferenceBars> {
        let mut bars = wander(2, 50.0);
        bars.retain(|b| b.open_time != 30 * HOUR);
        vec![ReferenceBars {
            instrument: id("OTHERUSDT"),
            bars,
        }]
    }

    fn sweep_spec(from: Millis, to: Millis) -> crate::SweepSpec {
        crate::SweepSpec {
            base: BacktestConfig {
                from,
                to,
                warmup_from: None,
                close_at_end: true,
                ..config(vec![id("OTHERUSDT")])
            },
            axes: Vec::new(),
            mode: crate::SweepMode::Grid,
            objective: crate::Objective {
                metric: crate::Metric::TotalReturn,
                direction: crate::Direction::Maximize,
            },
            min_trades: 0,
        }
    }

    /// The tag `Follows` gives an order sent on the bar at `to`: the open time of the reference
    /// bar at each of `bars`' times from `from` to `to`, `-` for the gap.
    fn tag(bars: &[Candle], from: Millis, to: Millis) -> String {
        bars.iter()
            .filter(|b| (from..=to).contains(&b.open_time))
            .map(|b| {
                if b.open_time == 30 * HOUR {
                    "-".to_string()
                } else {
                    b.open_time.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn sweeps_out_of_sample_and_walk_forward_runs_give_every_run_the_reference_bars() {
        let bars = wander(1, 100.0);
        let refs = tester_reference();
        let spec = sweep_spec(0, 60 * HOUR);

        let swept = crate::sweep_with_references(&spec, &bars, &[], &[], &refs, None).unwrap();
        let best = swept.best.expect("one qualifying run");
        let last = best.orders.last().unwrap();
        assert_eq!(
            last.request.client_tag.as_deref(),
            Some(tag(&bars, 0, last.submitted_at).as_str())
        );
        assert_eq!(swept.data_fingerprint, best.data_fingerprint);
        // Without them the sweep stops with a clear error instead of running blind.
        let blind = crate::sweep(&spec, &bars, &[], &[], None).unwrap_err();
        assert!(blind.to_string().contains("no bars were given"), "{blind}");

        let oos = crate::OosSpec {
            sweep: spec.clone(),
            split: crate::Split::At(40 * HOUR),
        };
        let split = crate::out_of_sample_with_references(&oos, &bars, &[], &[], &refs, None);
        let out = split.unwrap().out_of_sample;
        // The out-of-sample run is warmed up on the in-sample bars, and sees their reference
        // bars from its first bar on.
        let first = &out.orders[0];
        assert_eq!(first.submitted_at, 40 * HOUR);
        assert_eq!(
            first.request.client_tag.as_deref(),
            Some(tag(&bars, 0, 40 * HOUR).as_str())
        );
        assert!(crate::out_of_sample(&oos, &bars, &[], &[], None).is_err());

        let wf = crate::WalkForwardSpec {
            sweep: spec.clone(),
            windows: crate::WindowSpec {
                train_bars: 24,
                test_bars: 12,
                step_bars: 12,
                mode: crate::WindowMode::Rolling,
            },
        };
        let result =
            crate::walk_forward_with_references(&wf, &bars, &[], &[], &refs, None).unwrap();
        assert_eq!(result.windows.len(), 3);
        for w in &result.windows {
            let first = &w.out_of_sample.orders[0];
            assert_eq!(first.submitted_at, w.test_from);
            assert_eq!(
                first.request.client_tag.as_deref(),
                Some(tag(&bars, w.train_from, w.test_from).as_str()),
                "the window testing from {}",
                w.test_from
            );
        }
        assert!(crate::walk_forward(&wf, &bars, &[], &[], None).is_err());
    }

    /// The luck test shuffles the traded bars and every reference with one permutation. With a
    /// reference identical to the traded bars, each copy's reference is then exactly the copy's
    /// traded bars, so each copy's best is the run on the bars `permute_bars` gives for that copy's
    /// seed, with those bars as the reference too. A reference left unshuffled, or shuffled in
    /// another order, makes `Follows` trade differently.
    #[test]
    fn the_luck_test_shuffles_each_reference_with_the_traded_bars_under_one_permutation() {
        let bars = wander(3, 100.0);
        let same = vec![ReferenceBars {
            instrument: id("OTHERUSDT"),
            bars: bars.clone(),
        }];
        let spec = crate::PermutationSpec {
            sweep: sweep_spec(0, 60 * HOUR),
            permutations: 6,
            seed: 11,
        };
        let result =
            crate::permutation_test_with_references(&spec, &bars, &[], &[], &same, None).unwrap();
        for (k, got) in result.permuted_best.iter().enumerate() {
            let seed = crate::rng::derive_seed(spec.seed, k as u64);
            let copy = crate::permute_bars(&bars, seed).unwrap();
            let copy_refs = vec![ReferenceBars {
                instrument: id("OTHERUSDT"),
                bars: copy.clone(),
            }];
            let want = run_with_references(&spec.sweep.base, &copy, &[], &[], &copy_refs)
                .unwrap()
                .metrics
                .total_return;
            assert_eq!(*got, want, "copy {k}");
        }
        // The copies differ from one another, so the check above has teeth.
        assert!(result.permuted_best.windows(2).any(|w| w[0] != w[1]));
        assert!(crate::permutation_test(&spec, &bars, &[], &[], None).is_err());
    }

    /// The battery gives every run the reference bars, and its saved record names them.
    #[test]
    fn the_battery_runs_with_its_references_and_its_record_names_them() {
        let entry = crate::find_strategy("test_sees_references").unwrap();
        let mut plan = entry.plan.clone();
        plan.period.to = 160 * 24 * HOUR;
        plan.evidence.luck.permutations = 4;
        plan.evidence.start_offsets = 0;
        let four_hours: Vec<Candle> = (0..960)
            .map(|i| Candle {
                open_time: i * 4 * HOUR,
                ..bar(0, 100.0 + 10.0 * ((i as f64) / 9.0).sin())
            })
            .collect();
        let reference: Vec<Candle> = four_hours
            .iter()
            .enumerate()
            .map(|(i, b)| Candle {
                open_time: b.open_time,
                ..bar(0, 50.0 + 5.0 * ((i as f64) / 5.0).cos())
            })
            .collect();
        let refs = vec![ReferenceBars {
            instrument: id("OTHERUSDT"),
            bars: reference,
        }];
        let instrument = instrument();
        let spec = crate::BatterySpec {
            strategy: "test_sees_references",
            plan: &plan,
            rule: &entry.rule,
            instrument: &instrument,
            seed: 5,
        };
        let battery =
            crate::run_battery_with_references(&spec, &four_hours, &[], &[], &refs, None).unwrap();
        assert!(
            battery.walk_forward.is_some(),
            "{:?}",
            battery.walk_forward_error
        );
        // The sweep and the walk-forward ran with the reference; the luck test runs only while
        // the verdict is open, which this losing test double never leaves it, so its shared
        // permutation is the tester test's to prove.
        assert!(battery.sweep.is_some());
        let base = &battery.walk_forward.as_ref().unwrap().spec.sweep.base;
        assert_eq!(base.references, vec![id("OTHERUSDT")]);
        let record =
            crate::PlanRun::with_references(&spec, battery, &four_hours, &[], &[], &refs).unwrap();
        assert_eq!(record.config.references, vec![id("OTHERUSDT")]);
        assert_eq!(
            record.data_fingerprint,
            crate::runner::config_fingerprint_with_references(
                &record.config,
                &four_hours,
                &[],
                &[],
                &refs
            )
            .unwrap()
        );
        // Without the reference bars the battery stops on its first run.
        let blind = crate::run_battery(&spec, &four_hours, &[], &[], None).unwrap_err();
        assert!(blind.to_string().contains("no reference"), "{blind}");
    }

    // The registered test double for the tester and the battery, which build strategies by name.

    /// Every registry-built strategy in these tests: on each traded bar after the first, buys one
    /// unit when the reference `OTHERUSDT` closed higher than on the bar before and sells one when
    /// it closed lower, and tags each order with the open times of every reference bar it has seen
    /// so far, `-` for a gap, so a test can read what each run saw.
    pub(crate) fn test_entries() -> Vec<StrategyEntry> {
        // Not through `find_strategy`, which lists these entries.
        let funding = crate::strategies::test_only::entries()
            .into_iter()
            .find(|e| e.name == "test_tells_funding")
            .expect("the funding test double");
        vec![StrategyEntry {
            name: "test_sees_references",
            description: "follows the reference instrument OTHERUSDT (tests only)",
            params: Vec::new(),
            build: |_| Ok(Box::new(Follows)),
            plan: funding.plan,
            rule: funding.rule,
        }]
    }

    struct Follows;

    impl Strategy for Follows {
        fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
            let Some(series) = ctx.reference(&id("OTHERUSDT")) else {
                ctx.fail("no reference OTHERUSDT");
                return;
            };
            let tag: Vec<String> = series
                .iter()
                .map(|s| s.map_or("-".into(), |b| b.open_time.to_string()))
                .collect();
            let n = series.len();
            if ctx.is_warming_up() || n < 2 {
                return;
            }
            let (Some(now), Some(before)) = (series[n - 1], series[n - 2]) else {
                return;
            };
            if now.close == before.close {
                return;
            }
            let side = if now.close > before.close {
                Side::Buy
            } else {
                Side::Sell
            };
            let _ = ctx.submit(OrderRequest {
                instrument: ctx.instrument().id.clone(),
                side,
                qty: 0.01,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: Some(tag.join(" ")),
            });
        }
    }
}
