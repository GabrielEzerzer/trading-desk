use crate::strategies::{donchian, sma_cross};
use crate::{EngineError, ParamSpec, Params, Strategy};

/// A strategy the tester and the API can list and build from parameters.
#[derive(Debug, Clone)]
pub struct StrategyEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub params: Vec<ParamSpec>,
    /// Builds the strategy from resolved parameters (see [`Params::resolve`]).
    pub build: fn(&Params) -> Result<Box<dyn Strategy>, EngineError>,
}

/// Every registered strategy.
pub fn strategies() -> Vec<StrategyEntry> {
    vec![sma_cross::entry(), donchian::entry()]
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
