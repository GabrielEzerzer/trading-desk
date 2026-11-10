use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::EngineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    Int,
    Float,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    Bool(bool),
    Int(i64),
    Float(f64),
}

/// One tunable parameter of a strategy, as the tester and the API list it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    pub kind: ParamKind,
    pub default: ParamValue,
    /// Inclusive bounds for `Int` and `Float` parameters; `None` for `Bool` or no bound.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub description: &'static str,
}

/// Parameter values by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Params(BTreeMap<String, ParamValue>);

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, value: ParamValue) -> Self {
        self.set(name, value);
        self
    }

    pub fn set(&mut self, name: &str, value: ParamValue) {
        self.0.insert(name.to_string(), value);
    }

    pub fn get(&self, name: &str) -> Option<ParamValue> {
        self.0.get(name).copied()
    }

    /// Checks these values against `specs` and returns a full set: missing values take their
    /// default, an integer given for a float parameter is converted, and unknown names, wrong
    /// kinds, non-finite floats and values outside the bounds are errors.
    pub fn resolve(&self, specs: &[ParamSpec]) -> Result<Params, EngineError> {
        let fail = |name: &str, reason: String| EngineError::InvalidParam {
            name: name.to_string(),
            reason,
        };
        if let Some(name) = self.0.keys().find(|k| !specs.iter().any(|s| s.name == *k)) {
            return Err(fail(name, "unknown parameter".into()));
        }
        let mut out = Params::new();
        for spec in specs {
            let given = self.get(spec.name).unwrap_or(spec.default);
            let value = match (spec.kind, given) {
                (ParamKind::Int, ParamValue::Int(v)) => ParamValue::Int(v),
                (ParamKind::Float, ParamValue::Float(v)) => ParamValue::Float(v),
                (ParamKind::Float, ParamValue::Int(v)) => ParamValue::Float(v as f64),
                (ParamKind::Bool, ParamValue::Bool(v)) => ParamValue::Bool(v),
                (kind, _) => return Err(fail(spec.name, format!("expected {kind:?}"))),
            };
            let number = match value {
                ParamValue::Int(v) => Some(v as f64),
                ParamValue::Float(v) => Some(v),
                ParamValue::Bool(_) => None,
            };
            if let Some(n) = number {
                if !n.is_finite() {
                    return Err(fail(spec.name, format!("{n} is not a finite number")));
                }
                if spec.min.is_some_and(|min| n < min) || spec.max.is_some_and(|max| n > max) {
                    return Err(fail(
                        spec.name,
                        format!("{n} is outside {:?}..={:?}", spec.min, spec.max),
                    ));
                }
            }
            out.set(spec.name, value);
        }
        Ok(out)
    }

    pub fn int(&self, name: &str) -> Result<i64, EngineError> {
        match self.get(name) {
            Some(ParamValue::Int(v)) => Ok(v),
            _ => Err(missing(name, "Int")),
        }
    }

    pub fn float(&self, name: &str) -> Result<f64, EngineError> {
        match self.get(name) {
            Some(ParamValue::Float(v)) => Ok(v),
            Some(ParamValue::Int(v)) => Ok(v as f64),
            _ => Err(missing(name, "Float")),
        }
    }

    pub fn bool(&self, name: &str) -> Result<bool, EngineError> {
        match self.get(name) {
            Some(ParamValue::Bool(v)) => Ok(v),
            _ => Err(missing(name, "Bool")),
        }
    }

    /// An integer parameter as a count or length; it must be at least 1.
    pub fn len(&self, name: &str) -> Result<usize, EngineError> {
        let v = self.int(name)?;
        usize::try_from(v)
            .ok()
            .filter(|&n| n >= 1)
            .ok_or_else(|| EngineError::InvalidParam {
                name: name.to_string(),
                reason: format!("{v} must be at least 1"),
            })
    }
}

fn missing(name: &str, kind: &str) -> EngineError {
    EngineError::InvalidParam {
        name: name.to_string(),
        reason: format!("missing or not {kind}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs() -> Vec<ParamSpec> {
        vec![
            ParamSpec {
                name: "period",
                kind: ParamKind::Int,
                default: ParamValue::Int(10),
                min: Some(1.0),
                max: Some(100.0),
                description: "",
            },
            ParamSpec {
                name: "risk",
                kind: ParamKind::Float,
                default: ParamValue::Float(0.01),
                min: Some(0.0),
                max: Some(1.0),
                description: "",
            },
            ParamSpec {
                name: "short",
                kind: ParamKind::Bool,
                default: ParamValue::Bool(false),
                min: None,
                max: None,
                description: "",
            },
        ]
    }

    #[test]
    fn defaults_fill_missing_values() {
        let p = Params::new().resolve(&specs()).unwrap();
        assert_eq!(p.int("period"), Ok(10));
        assert_eq!(p.float("risk"), Ok(0.01));
        assert_eq!(p.bool("short"), Ok(false));
    }

    #[test]
    fn integers_convert_to_floats() {
        let p = Params::new()
            .with("risk", ParamValue::Int(1))
            .resolve(&specs())
            .unwrap();
        assert_eq!(p.get("risk"), Some(ParamValue::Float(1.0)));
    }

    #[test]
    fn bad_values_are_rejected() {
        let bad = [
            ("nope", ParamValue::Int(1)),
            ("period", ParamValue::Float(2.0)),
            ("period", ParamValue::Int(0)),
            ("risk", ParamValue::Float(f64::NAN)),
            ("risk", ParamValue::Float(1.5)),
            ("short", ParamValue::Int(1)),
        ];
        for (name, value) in bad {
            let err = Params::new().with(name, value).resolve(&specs());
            assert!(
                matches!(err, Err(EngineError::InvalidParam { .. })),
                "{name} {value:?}"
            );
        }
    }

    #[test]
    fn params_round_trip_through_json() {
        let p = Params::new()
            .with("period", ParamValue::Int(3))
            .with("risk", ParamValue::Float(0.5))
            .with("short", ParamValue::Bool(true));
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"period":3,"risk":0.5,"short":true}"#);
        assert_eq!(serde_json::from_str::<Params>(&json).unwrap(), p);
    }
}
