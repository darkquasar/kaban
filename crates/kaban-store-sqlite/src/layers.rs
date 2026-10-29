//! Fixed last-run bookkeeping in the existing index metadata store (EN-F52).

use crate::SqliteStore;
use kaban_core::{IngestStore, Layer, Result, StoreError};
use serde_json::{Value, json};

/// Content-free producer outcome; never stores a supplied error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerRunOutcome {
    /// Completed.
    Ok,
    /// Producer failed while sibling layers remain usable.
    Failed,
    /// Resource policy prevented the run.
    SkippedBudget,
}

impl LayerRunOutcome {
    /// Compatible fixed wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::SkippedBudget => "skipped_budget",
        }
    }
}

/// Compatible JSON value for `index_meta.layer_run:<layer>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerRunRecord {
    /// Nonnegative Unix timestamp in seconds.
    pub at: i64,
    /// Fixed producer outcome.
    pub outcome: LayerRunOutcome,
}

impl SqliteStore {
    /// Record a producer result without changing canonical records or deploying indexes.
    ///
    /// # Errors
    /// Rejects a negative timestamp; propagates metadata persistence errors.
    pub fn record_layer_run(&self, layer: Layer, at: i64, outcome: LayerRunOutcome) -> Result<()> {
        if at < 0 {
            return Err(StoreError::InvalidName(
                "layer_run".to_owned(),
                "negative_timestamp",
            ));
        }
        let record = json!({ "at": at, "outcome": outcome.as_str() }).to_string();
        self.set_index_meta(&format!("layer_run:{}", layer.as_str()), &record)
    }

    /// Read a valid run, tolerating absent/malformed bookkeeping and legacy integer values.
    ///
    /// # Errors
    /// Propagates metadata read errors; never mutates metadata on a read.
    pub fn layer_run(&self, layer: Layer) -> Result<Option<LayerRunRecord>> {
        let metadata = self.index_meta()?;
        let Some(value) = metadata.get(&format!("layer_run:{}", layer.as_str())) else {
            return Ok(None);
        };
        let run = decode_run(value).or_else(|| {
            value.parse::<i64>().ok().map(|at| LayerRunRecord {
                at,
                outcome: LayerRunOutcome::Ok,
            })
        });
        Ok(run.filter(|run| run.at >= 0))
    }
}

fn decode_run(value: &str) -> Option<LayerRunRecord> {
    let value: Value = serde_json::from_str(value).ok()?;
    let object = value.as_object()?;
    // Reject arbitrary supplemental text just like the API's strict LayerRun DTO.
    if object.len() != 2 {
        return None;
    }
    let at = object.get("at")?.as_i64()?;
    let outcome = match object.get("outcome")?.as_str()? {
        "ok" => LayerRunOutcome::Ok,
        "failed" => LayerRunOutcome::Failed,
        "skipped_budget" => LayerRunOutcome::SkippedBudget,
        _ => return None,
    };
    Some(LayerRunRecord { at, outcome })
}
