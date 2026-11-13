//! The GUI's fake engine (gui/app/mock/engine.json, served by gui/app/shots/bridge-stub.js) must
//! answer with what the real engine would. The GUI tests cast it to the generated types, so the
//! TypeScript compiler never checks it; this test reads each part into the Rust type the real
//! route returns and writes it back, so a missing, renamed, extra or wrongly typed field fails.

use std::collections::BTreeMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use td_core::Instrument;
use td_engine::{ParamKind, RuleKind, RunSummary, TestPlan, VerdictRule};
use td_server::api::data::MonthCoverageInfo;
use td_server::api::runs::RunDetail;
use td_server::api::strategy_infos;

const ENGINE_JSON: &str = include_str!("../../../gui/app/mock/engine.json");

fn engine() -> Value {
    serde_json::from_str(ENGINE_JSON).expect("engine.json is JSON")
}

/// `1` and `1.0` are the same number to the GUI; compare every number as an `f64`.
fn numbers_as_f64(value: Value) -> Value {
    match value {
        Value::Number(n) => n.as_f64().map_or(Value::Number(n), Value::from),
        Value::Array(items) => Value::Array(items.into_iter().map(numbers_as_f64).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, numbers_as_f64(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Reads `fake` as a `T` and writes it back, noting under `path` where it differs from what the
/// engine would send.
fn round_trip<T: DeserializeOwned + Serialize>(path: &str, fake: &Value, out: &mut Vec<String>) {
    match serde_json::from_value::<T>(fake.clone()) {
        Ok(parsed) => {
            let real = serde_json::to_value(&parsed).expect("serializes");
            differ(
                path,
                &numbers_as_f64(real),
                &numbers_as_f64(fake.clone()),
                out,
            );
        }
        Err(e) => out.push(format!("{path}: not what the engine sends: {e}")),
    }
}

/// Each path where the engine's value `real` and the fixture's `fake` differ.
fn differ(path: &str, real: &Value, fake: &Value, out: &mut Vec<String>) {
    match (real, fake) {
        (Value::Object(r), Value::Object(f)) => {
            for (k, rv) in r {
                match f.get(k) {
                    Some(fv) => differ(&format!("{path}.{k}"), rv, fv, out),
                    None => out.push(format!(
                        "{path}.{k}: the engine sends {rv}, the fixture has nothing"
                    )),
                }
            }
            for k in f.keys().filter(|k| !r.contains_key(*k)) {
                out.push(format!(
                    "{path}.{k}: the fixture has a field the engine does not send"
                ));
            }
        }
        (Value::Array(r), Value::Array(f)) if r.len() == f.len() => {
            for (i, (rv, fv)) in r.iter().zip(f).enumerate() {
                differ(&format!("{path}[{i}]"), rv, fv, out);
            }
        }
        _ if real != fake => out.push(format!(
            "{path}: the engine sends {real}, the fixture has {fake}"
        )),
        _ => {}
    }
}

/// The field names of a JSON object, sorted.
fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

fn assert_none(what: &str, differences: Vec<String>) {
    assert!(
        differences.is_empty(),
        "engine.json's {what} differs from what the engine would send:\n{}",
        differences.join("\n")
    );
}

#[test]
fn the_fake_engines_runs_details_instruments_and_coverage_match_the_real_types() {
    let engine = engine();
    let mut differences = Vec::new();
    round_trip::<Vec<RunSummary>>("runs", &engine["runs"], &mut differences);
    round_trip::<BTreeMap<String, RunDetail>>("details", &engine["details"], &mut differences);
    round_trip::<Vec<Instrument>>("instruments", &engine["instruments"], &mut differences);
    round_trip::<BTreeMap<String, Vec<MonthCoverageInfo>>>(
        "coverage",
        &engine["coverage"],
        &mut differences,
    );
    assert_none("runs, details, instruments or coverage", differences);
}

/// The fixture's strategies are made up for the screenshots (two of them are not registered), so
/// their values are not the registry's; their fields are checked instead. `StrategyInfo` and
/// `ParamSpec` are only ever sent, so their field names are compared with what `GET /strategies`
/// sends; the plan and the rule are read into their types and written back.
#[test]
fn the_fake_engines_strategies_have_the_engines_fields() {
    let real = serde_json::to_value(strategy_infos(td_engine::strategies())).expect("serializes");
    let real_strategy = &real[0];
    let real_param = &real_strategy["params"][0];
    assert!(real_param.is_object(), "the first strategy has no params");

    let engine = engine();
    let fixtures = engine["strategies"]
        .as_array()
        .expect("engine.json has a strategies list");
    assert!(!fixtures.is_empty(), "engine.json lists no strategies");
    let mut differences = Vec::new();
    for fake in fixtures {
        let path = format!("strategies.{}", fake["name"].as_str().unwrap_or("?"));
        if keys(fake) != keys(real_strategy) {
            differences.push(format!(
                "{path}: has fields {:?}, the engine sends {:?}",
                keys(fake),
                keys(real_strategy)
            ));
        }
        for (i, param) in fake["params"].as_array().into_iter().flatten().enumerate() {
            let path = format!("{path}.params[{i}]");
            if keys(param) != keys(real_param) {
                differences.push(format!(
                    "{path}: has fields {:?}, the engine sends {:?}",
                    keys(param),
                    keys(real_param)
                ));
            }
            let fits = match serde_json::from_value::<ParamKind>(param["kind"].clone()) {
                Ok(ParamKind::Int) => param["default"].is_i64(),
                Ok(ParamKind::Float) => param["default"].is_number(),
                Ok(ParamKind::Bool) => param["default"].is_boolean(),
                Err(_) => false,
            };
            if !fits {
                differences.push(format!(
                    "{path}: kind {} with default {}",
                    param["kind"], param["default"]
                ));
            }
        }
        round_trip::<TestPlan>(&format!("{path}.plan"), &fake["plan"], &mut differences);
        round_trip::<VerdictRule>(&format!("{path}.rule"), &fake["rule"], &mut differences);
        let kind = serde_json::from_value::<RuleKind>(fake["kind"].clone()).ok();
        let rule = serde_json::from_value::<VerdictRule>(fake["rule"].clone()).ok();
        if kind.is_none() || kind != rule.map(|r| r.kind()) {
            differences.push(format!(
                "{path}.kind: {} is not the rule's kind",
                fake["kind"]
            ));
        }
    }
    assert_none("strategies", differences);
}
