//! Fixed last-run bookkeeping of each producer in the existing index metadata store (EN-F52, PC-16).

use crate::SqliteStore;
use kaban_core::{IngestStore, Producer, Result, StoreError};
use serde_json::{Value, json};

/// Content-free producer outcome; never stores a supplied error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerRunOutcome {
    /// Completed.
    Ok,
    /// Producer failed while the other producers remain usable.
    Failed,
    /// Resource policy prevented the run.
    SkippedBudget,
}

impl ProducerRunOutcome {
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

/// JSON value for `index_meta.producer_run:<producer>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerRunRecord {
    /// Nonnegative Unix timestamp in seconds.
    pub at: i64,
    /// Fixed producer outcome.
    pub outcome: ProducerRunOutcome,
}

impl SqliteStore {
    /// Record a producer result without changing canonical records or deploying indexes.
    ///
    /// # Errors
    /// Rejects a negative timestamp; propagates metadata persistence errors.
    pub fn record_producer_run(
        &self,
        producer: Producer,
        at: i64,
        outcome: ProducerRunOutcome,
    ) -> Result<()> {
        if at < 0 {
            return Err(StoreError::InvalidName(
                "producer_run".to_owned(),
                "negative_timestamp",
            ));
        }
        let record = json!({ "at": at, "outcome": outcome.as_str() }).to_string();
        self.set_index_meta(&format!("producer_run:{}", producer.as_str()), &record)
    }

    /// Read a valid run; absent or malformed bookkeeping reads as no run.
    ///
    /// # Errors
    /// Propagates metadata read errors; never mutates metadata on a read.
    pub fn producer_run(&self, producer: Producer) -> Result<Option<ProducerRunRecord>> {
        let metadata = self.index_meta()?;
        let Some(value) = metadata.get(&format!("producer_run:{}", producer.as_str())) else {
            return Ok(None);
        };
        Ok(decode_run(value).filter(|run| run.at >= 0))
    }
}

fn decode_run(value: &str) -> Option<ProducerRunRecord> {
    let value: Value = serde_json::from_str(value).ok()?;
    let object = value.as_object()?;
    // Reject arbitrary supplemental text just like the API's strict ProducerRun DTO.
    if object.len() != 2 {
        return None;
    }
    let at = object.get("at")?.as_i64()?;
    let outcome = match object.get("outcome")?.as_str()? {
        "ok" => ProducerRunOutcome::Ok,
        "failed" => ProducerRunOutcome::Failed,
        "skipped_budget" => ProducerRunOutcome::SkippedBudget,
        _ => return None,
    };
    Some(ProducerRunRecord { at, outcome })
}
