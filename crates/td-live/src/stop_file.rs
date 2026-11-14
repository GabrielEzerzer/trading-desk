//! The session's stop state on disk: `stops.json` in the session folder, beside `config.json`.
//!
//! It holds the engine's [`StopState`] (the rules, the starting balance, the day's baseline and a
//! fired stop) and the stop guard's flatten record: the counter behind the flatten order ids and
//! what each flatten did. It is written whole, through a synced temporary file renamed over the
//! old one, whenever it changes: at creation, on a new day's baseline, when a stop fires, and
//! before every flatten order is sent.
//!
//! It is read back only through [`SavedStopState`] and [`StopState::restore`] against the rules
//! and starting balance in `config.json`. A missing, unreadable or refused file refuses the open:
//! the state is never rebuilt from nothing for a session that has run.

use std::path::Path;

use serde::{Deserialize, Serialize};
use td_engine::{SavedStopState, StopState};

use crate::session::{LiveError, SavedConfig, read_json, write_synced};

pub(crate) const STOP_FILE: &str = "stops.json";

/// How many flatten steps the record keeps, newest last.
const KEEP_STEPS: usize = 200;

/// The stop state and the flatten record, as the session holds them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StopRecord {
    pub state: StopState,
    pub flatten: Flatten,
}

/// What the guard's flattens did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Flatten {
    /// The number of the last flatten order id claimed. Each send claims the next number and saves
    /// it before the request goes out, so a crash and a retry never reuse an id.
    pub last_id: u64,
    /// Each step taken, newest last.
    pub steps: Vec<String>,
    /// How the last flatten ended; `None` before any has run.
    pub result: Option<FlattenResult>,
}

/// How a flatten ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FlattenResult {
    /// The venue position was flat and no order of the session rested.
    Flat,
    /// Its rounds ran out with something still open at the venue.
    NotFlat { left: String },
}

impl Flatten {
    pub fn step(&mut self, step: String) {
        if self.steps.len() == KEEP_STEPS {
            self.steps.remove(0);
        }
        self.steps.push(step);
    }
}

/// The file as it is written.
#[derive(Serialize)]
struct Out<'a> {
    stops: &'a StopState,
    flatten: &'a Flatten,
}

/// The file as it is read back, before the stop state is checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct In {
    stops: SavedStopState,
    flatten: Flatten,
}

impl StopRecord {
    pub fn new(state: StopState) -> StopRecord {
        StopRecord {
            state,
            flatten: Flatten::default(),
        }
    }

    /// Reads `stops.json` in `dir` and restores its stop state against the rules and starting
    /// balance in the session's config. Any failure is [`LiveError::StopFile`].
    pub fn load(dir: &Path, saved: &SavedConfig) -> Result<StopRecord, LiveError> {
        let path = dir.join(STOP_FILE);
        let refused = |reason: String| LiveError::StopFile {
            path: path.clone(),
            reason,
        };
        let read: In = read_json(&path).map_err(|e| refused(e.to_string()))?;
        let state = StopState::restore(read.stops, &saved.config.stops, saved.starting_balance)
            .map_err(|e| refused(e.to_string()))?;
        Ok(StopRecord {
            state,
            flatten: read.flatten,
        })
    }

    /// Writes `stops.json` in `dir`: a synced temporary file renamed over the old one.
    pub fn save(&self, dir: &Path) -> Result<(), LiveError> {
        let out = Out {
            stops: &self.state,
            flatten: &self.flatten,
        };
        let json = serde_json::to_vec_pretty(&out).map_err(|e| LiveError::StopFile {
            path: dir.join(STOP_FILE),
            reason: e.to_string(),
        })?;
        write_synced(dir, STOP_FILE, &json)
    }

    /// Whether a fired stop still has a flatten to finish.
    pub fn flatten_unfinished(&self) -> bool {
        self.state.fired().is_some() && self.flatten.result != Some(FlattenResult::Flat)
    }
}
