//! The GUI's fake engine (gui/app/mock/engine.json, served by gui/app/shots/bridge-stub.js) must
//! answer with what the real engine would. The GUI tests cast it to the generated types, so the
//! TypeScript compiler never checks it; this test reads each part into the Rust type the real
//! route returns and writes it back, so a missing, renamed, extra or wrongly typed field fails.

use std::collections::BTreeMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use td_core::Instrument;
use td_engine::RunSummary;
use td_server::api::data::MonthCoverageInfo;
use td_server::api::runs::RunDetail;

const ENGINE_JSON: &str = include_str!("../../../gui/app/mock/engine.json");

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

/// Reads `engine.json`'s `key` as a `T` and checks it writes back the same.
fn round_trips<T: DeserializeOwned + Serialize>(key: &str) {
    let engine: Value = serde_json::from_str(ENGINE_JSON).expect("engine.json is JSON");
    let fixture = engine
        .get(key)
        .unwrap_or_else(|| panic!("engine.json has no {key:?}"))
        .clone();
    let parsed: T = serde_json::from_value(fixture.clone())
        .unwrap_or_else(|e| panic!("engine.json's {key:?} is not what the engine returns: {e}"));
    let written = numbers_as_f64(serde_json::to_value(&parsed).expect("serializes"));
    let mut differences = Vec::new();
    differ(key, &written, &numbers_as_f64(fixture), &mut differences);
    assert!(
        differences.is_empty(),
        "engine.json's {key:?} differs from what the engine would send:\n{}",
        differences.join("\n")
    );
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

#[test]
fn the_fake_engines_runs_details_instruments_and_coverage_match_the_real_types() {
    round_trips::<Vec<RunSummary>>("runs");
    round_trips::<BTreeMap<String, RunDetail>>("details");
    round_trips::<Vec<Instrument>>("instruments");
    round_trips::<BTreeMap<String, Vec<MonthCoverageInfo>>>("coverage");
}
