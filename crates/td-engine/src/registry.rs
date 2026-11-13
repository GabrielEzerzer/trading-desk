use std::sync::OnceLock;

use crate::strategies::example_hashes as hash;
use crate::strategies::{donchian, gotobi, private, sma_cross};
use crate::{EngineError, ParamSpec, Params, Strategy, TestPlan, VerdictRule};

/// A strategy the tester and the API can list and build from parameters, with the plan the
/// battery tests it on and the rule its verdict is judged by.
#[derive(Debug, Clone)]
pub struct StrategyEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub params: Vec<ParamSpec>,
    /// Builds the strategy from resolved parameters (see [`Params::resolve`]).
    pub build: fn(&Params) -> Result<Box<dyn Strategy>, EngineError>,
    pub plan: TestPlan,
    pub rule: VerdictRule,
}

/// Every registered strategy: the examples, then the private strategies when their folder is
/// checked out (see build.rs).
pub fn strategies() -> Vec<StrategyEntry> {
    with_private(private::entries())
}

/// The examples, each with the hash of its source file. Registering an example is one line here.
fn examples() -> Vec<(StrategyEntry, &'static str)> {
    vec![
        (sma_cross::entry(), hash::SMA_CROSS),
        (donchian::entry(), hash::DONCHIAN),
        (gotobi::entry(), hash::GOTOBI),
    ]
}

/// The examples, then `private`.
fn with_private(private: Vec<StrategyEntry>) -> Vec<StrategyEntry> {
    let mut all: Vec<StrategyEntry> = examples().into_iter().map(|(e, _)| e).collect();
    all.extend(private);
    all
}

/// The hash of the registered strategy `name`'s source file, computed when the engine was built
/// (see build.rs): the first 16 hex digits of its SHA-256, read with LF line endings. Saved runs
/// and verdicts record it, so a verdict can tell that its strategy's code has changed since. `None`
/// for a name that is not registered.
pub fn code_hash(name: &str) -> Option<&'static str> {
    static HASHES: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        let private = private::entries()
            .into_iter()
            .zip(private::CODE_HASHES.iter().copied());
        examples()
            .into_iter()
            .chain(private)
            .map(|(entry, hash)| (entry.name, hash))
            .collect()
    });
    hashes.iter().find(|(n, _)| *n == name).map(|(_, h)| *h)
}

/// The registered strategy called `name`. In the crate's own tests it also finds the strategies
/// in `strategies::test_only`, which are broken on purpose and never listed.
pub fn find_strategy(name: &str) -> Option<StrategyEntry> {
    strategies()
        .into_iter()
        .chain(test_only())
        .find(|e| e.name == name)
}

#[cfg(test)]
fn test_only() -> Vec<StrategyEntry> {
    crate::strategies::test_only::entries()
}

#[cfg(not(test))]
fn test_only() -> Vec<StrategyEntry> {
    Vec::new()
}

/// Resolves `params` against the strategy's specs and builds it.
pub fn build_strategy(name: &str, params: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    let entry = find_strategy(name).ok_or_else(|| EngineError::UnknownStrategy(name.into()))?;
    (entry.build)(&params.resolve(&entry.params)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParamValue;

    #[test]
    fn every_strategy_builds_from_defaults() {
        for entry in strategies() {
            assert!(
                build_strategy(entry.name, &Params::new()).is_ok(),
                "{}",
                entry.name
            );
        }
    }

    /// Every strategy's plan and rule pass their checks and names are unique. With the private
    /// folder checked out this covers the private strategies too.
    #[test]
    fn every_plan_and_rule_is_valid_and_names_are_unique() {
        let all = strategies();
        for (i, entry) in all.iter().enumerate() {
            assert_eq!(entry.plan.validate(entry.name), Ok(()), "{}", entry.name);
            assert_eq!(entry.rule.validate(), Ok(()), "{}", entry.name);
            assert!(
                all[..i].iter().all(|e| e.name != entry.name),
                "{} is registered twice",
                entry.name
            );
        }
    }

    /// A private strategy file, compiled in through the same generated module as the private
    /// folder, is listed after the examples, builds and has a valid plan and rule.
    #[test]
    fn private_strategies_are_listed_after_the_examples() {
        let all = with_private(crate::strategies::fixture::entries());
        let names: Vec<&str> = all.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            ["sma_cross", "donchian_breakout", "gotobi", "fixture_hold"]
        );
        let fixture = &all[3];
        assert!((fixture.build)(&Params::new().resolve(&fixture.params).unwrap()).is_ok());
        // The full plan check looks the strategy up in the registry, which the fixture is not in.
        assert_eq!(fixture.plan.evidence.validate(), Ok(()));
        assert_eq!(fixture.rule.validate(), Ok(()));
        assert_eq!(fixture.rule.kind(), crate::RuleKind::HighRisk);
        // Without a private folder, which is how CI builds, only the examples are listed.
        assert_eq!(with_private(Vec::new()).len(), 3);
    }

    /// Every registered strategy has the hash of its own file, the examples' hashes differ, and a
    /// name that is not registered, such as a test-only strategy, has none.
    #[test]
    fn every_registered_strategy_has_the_hash_of_its_file() {
        let hashes: Vec<&str> = strategies()
            .iter()
            .map(|e| code_hash(e.name).unwrap_or_else(|| panic!("{} has no hash", e.name)))
            .collect();
        for h in &hashes {
            assert!(
                h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()),
                "{h}"
            );
        }
        assert_ne!(hashes[0], hashes[1]);
        assert_eq!(code_hash("sma_cross"), Some(hash::SMA_CROSS));
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/src/strategies/sma_cross.rs");
        assert_eq!(
            crate::private_strategies_build::source_hash(std::path::Path::new(file)).as_deref(),
            Ok(hash::SMA_CROSS)
        );
        assert_eq!(code_hash("nope"), None);
        // The fixture list carries one hash per entry, in the same order.
        assert_eq!(
            crate::strategies::fixture::CODE_HASHES.len(),
            crate::strategies::fixture::entries().len()
        );
    }

    #[test]
    fn unknown_names_and_bad_params_are_errors() {
        assert!(matches!(
            build_strategy("nope", &Params::new()),
            Err(EngineError::UnknownStrategy(_))
        ));
        let p = Params::new()
            .with("fast", ParamValue::Int(30))
            .with("slow", ParamValue::Int(10));
        assert!(matches!(
            build_strategy("sma_cross", &p),
            Err(EngineError::InvalidParam { .. })
        ));
    }
}
