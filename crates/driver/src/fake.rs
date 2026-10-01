//! Fakes for tests beside other seams (ADR 5).

use std::collections::BTreeMap;

use async_trait::async_trait;
use contract::{Metric, Reading};

use crate::{ReadError, Source};

/// A source declaring one gauge, answering every poll with the same value, or never.
#[derive(Debug)]
pub struct Fake {
    id: String,
    metrics: Vec<Metric>,
    value: Option<i64>,
}

impl Fake {
    /// Declares `key`, and reads `value` every poll.
    #[must_use]
    pub fn answering(id: &str, key: &str, value: i64) -> Self {
        Self::declaring(id, key, Some(value))
    }

    /// Declares `key`, and times out every poll: a dark source.
    #[must_use]
    pub fn silent(id: &str, key: &str) -> Self {
        Self::declaring(id, key, None)
    }

    fn declaring(id: &str, key: &str, value: Option<i64>) -> Self {
        Self {
            id: id.to_owned(),
            metrics: vec![Metric::Gauge {
                key: key.to_owned(),
                unit: Some("W".to_owned()),
                exponent: 0,
            }],
            value,
        }
    }
}

#[async_trait]
impl Source for Fake {
    fn id(&self) -> &str {
        &self.id
    }

    fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    async fn read(&mut self, timestamp_ms: u64) -> Result<Reading, ReadError> {
        let value = self.value.ok_or(ReadError::Timeout)?;
        let values = self
            .metrics
            .iter()
            .map(|metric| (metric.key().to_owned(), value))
            .collect::<BTreeMap<_, _>>();
        Ok(Reading {
            source: self.id.clone(),
            ts: timestamp_ms,
            values,
        })
    }
}
