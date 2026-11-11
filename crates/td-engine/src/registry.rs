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

/// The examples, then `private`. Registering an example is one line here.
fn with_private(private: Vec<StrategyEntry>) -> Vec<StrategyEntry> {
    let mut all = vec![sma_cross::entry(), donchian::entry(), gotobi::entry()];
    all.extend(private);
    all
}

pub fn find_strategy(name: &str) -> Option<StrategyEntry> {
    strategies().into_iter().find(|e| e.name == name)
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
