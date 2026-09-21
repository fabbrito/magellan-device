//! Sending a batch, and what the cloud's answer means for it.
//!
//! The status classes are the contract's, not this crate's invention: which answers let a batch
//! go and which keep it is a property of the seam, because getting it wrong loses readings on one
//! side and floods the cloud on the other.

use async_trait::async_trait;
use contract::{Batch, Manifest};

use std::time::Duration;

use anyhow::{Context, Result};

use crate::Token;

/// What an answer means for the batch that was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `2xx`. Committed; the device is done with it.
    Committed,
    /// A `4xx` the device cannot fix by trying again. Dropped, and the journal says why — this is
    /// the device having built something the cloud will never take.
    Rejected(u16),
    /// `401` or `403`. The batch stays: a rotated token is not worth the readings it would cost.
    Credential,
    /// `429`, `503`, `5xx`, or no answer at all. The batch stays and the device backs off; a
    /// duplicate is absorbed cloud-side, a gap is not.
    Unavailable,
}

impl Outcome {
    /// Whether the batch may leave the buffer.
    #[must_use]
    pub const fn releases_the_batch(self) -> bool {
        matches!(self, Self::Committed | Self::Rejected(_))
    }
}

/// What an HTTP status means. No answer at all is [`Outcome::Unavailable`], which is why this
/// takes a status rather than a result.
#[must_use]
pub fn classify(status: u16) -> Outcome {
    match status {
        200..=299 => Outcome::Committed,
        401 | 403 => Outcome::Credential,
        429 => Outcome::Unavailable,
        400..=499 => Outcome::Rejected(status),
        _ => Outcome::Unavailable,
    }
}

/// Where batches go. The runtime is written against this so the loop can be exercised without a
/// network, and so nothing in it names an HTTP client.
#[async_trait]
pub trait Cloud: Send + Sync {
    /// Declare what this device reads. Answers with the hash the cloud accepted.
    ///
    /// # Errors
    ///
    /// If the cloud refused it, or accepted bytes other than the ones sent.
    async fn declare(&self, manifest: &Manifest) -> Result<String, Declined>;

    /// Send one batch.
    async fn send(&self, batch: &Batch) -> Outcome;
}

/// Why declaring a manifest did not leave the device and cloud agreeing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declined {
    /// The cloud answered, and what it said means this.
    Answer(Outcome),
    /// The cloud accepted a manifest whose hash is not the hash of the bytes sent.
    ///
    /// Invariant 2 exists to catch exactly this: each side hashes the bytes it handled, so a
    /// mismatch means one side re-serialized. Retrying cannot fix it, and a batch naming the
    /// wrong hash would be rejected for as long as the device kept sending.
    Disagreement { sent: String, accepted: String },
}

/// The cloud over HTTPS.
#[derive(Debug)]
pub struct Http {
    client: reqwest::Client,
    base: String,
    device_id: String,
    token: Token,
}

impl Http {
    /// A client for `base`, which is the endpoint with any version prefix already on it.
    ///
    /// # Errors
    ///
    /// If the HTTP client cannot be built — a TLS backend that will not start, most likely.
    pub fn new(base: String, device_id: String, token: Token, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            // A request that never returns would hold the drain forever, and the buffer behind it.
            .timeout(timeout)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            client,
            base,
            device_id,
            token,
        })
    }

    fn url(&self, tail: &str) -> String {
        format!(
            "{}/devices/{}/{tail}",
            self.base.trim_end_matches('/'),
            self.device_id
        )
    }
}

/// The hash inside an `ETag`, without its quotes or a weak marker.
fn etag_hash(etag: &str) -> &str {
    etag.trim().trim_start_matches("W/").trim_matches('"')
}

#[async_trait]
impl Cloud for Http {
    async fn declare(&self, manifest: &Manifest) -> Result<String, Declined> {
        // Serialized once. The bytes that are hashed are the bytes that are sent, which is the
        // whole of invariant 2 — re-serializing to hash would be the bug it guards against.
        let bytes =
            serde_json::to_vec(manifest).map_err(|_| Declined::Answer(Outcome::Rejected(400)))?;
        let sent = contract::manifest_hash(&bytes);
        let response = self
            .client
            .put(self.url("manifest"))
            .bearer_auth(self.token.reveal())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| Declined::Answer(Outcome::Unavailable))?;
        let outcome = classify(response.status().as_u16());
        if outcome != Outcome::Committed {
            return Err(Declined::Answer(outcome));
        }
        let accepted = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|etag| etag_hash(etag).to_owned())
            .unwrap_or_default();
        if accepted != sent {
            return Err(Declined::Disagreement { sent, accepted });
        }
        Ok(sent)
    }

    async fn send(&self, batch: &Batch) -> Outcome {
        let Ok(bytes) = serde_json::to_vec(batch) else {
            return Outcome::Rejected(400);
        };
        match self
            .client
            .post(self.url("batches"))
            .bearer_auth(self.token.reveal())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
        {
            Ok(response) => classify(response.status().as_u16()),
            // No answer is not the same as a bad one: the batch may well have been committed, so
            // it stays and the duplicate is absorbed cloud-side.
            Err(_) => Outcome::Unavailable,
        }
    }
}

/// A fake cloud, for exercising the drain without a network.
///
/// Behind the `fake` feature, in the crate that owns the seam, so a test that needs one enables
/// it as a dev-dependency and nothing reachable from a release build has it.
///
/// It behaves; it does not record. What a test asserts on is what the device did with the answer
/// — whether the batch left the buffer — not how many times it was asked.
#[cfg(feature = "fake")]
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use contract::{Batch, Manifest};

    use super::{Cloud, Declined, Outcome};

    /// A cloud that answers from a script, then keeps answering the last thing forever.
    #[derive(Debug)]
    pub struct Fake {
        answers: Mutex<VecDeque<Outcome>>,
        then: Outcome,
    }

    impl Fake {
        /// A cloud that always answers the same way.
        #[must_use]
        pub fn always(outcome: Outcome) -> Self {
            Self {
                answers: Mutex::new(VecDeque::new()),
                then: outcome,
            }
        }

        /// A cloud that answers `script` in turn, then `then` from there on. This is how an
        /// outage is played: a run of `Unavailable`, then `Committed`.
        #[must_use]
        pub fn answering(script: Vec<Outcome>, then: Outcome) -> Self {
            Self {
                answers: Mutex::new(script.into()),
                then,
            }
        }
    }

    #[async_trait]
    impl Cloud for Fake {
        async fn declare(&self, manifest: &Manifest) -> Result<String, Declined> {
            // Behaves like a cloud that hashes the bytes it was handed, which is what makes a
            // disagreement a real finding rather than something the fake invented.
            serde_json::to_vec(manifest)
                .map(|bytes| contract::manifest_hash(&bytes))
                .map_err(|_| Declined::Answer(Outcome::Rejected(400)))
        }

        async fn send(&self, _batch: &Batch) -> Outcome {
            self.answers
                .lock()
                .map_or(None, |mut answers| answers.pop_front())
                .unwrap_or(self.then)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_committed_batch_leaves_the_buffer() {
        for status in [200, 201, 202, 204, 299] {
            assert_eq!(classify(status), Outcome::Committed);
            assert!(classify(status).releases_the_batch());
        }
    }

    #[test]
    fn a_rejected_batch_is_dropped_rather_than_retried_forever() {
        // The device built something the cloud will never take. Retrying is a loop that ends when
        // the buffer overflows, taking good readings with it.
        for status in [400, 404, 409, 413, 422] {
            assert_eq!(classify(status), Outcome::Rejected(status));
            assert!(classify(status).releases_the_batch());
        }
    }

    #[test]
    fn a_refused_credential_keeps_the_batch() {
        // Invariant: a rotated token must not cost the readings taken while it was stale.
        for status in [401, 403] {
            assert_eq!(classify(status), Outcome::Credential);
            assert!(!classify(status).releases_the_batch());
        }
    }

    #[test]
    fn a_cloud_that_cannot_commit_now_keeps_the_batch() {
        for status in [429, 500, 502, 503, 504] {
            assert_eq!(classify(status), Outcome::Unavailable);
            assert!(!classify(status).releases_the_batch());
        }
    }

    #[test]
    fn an_etag_is_read_however_the_cloud_quotes_it() {
        // Invariant 2 compares this against our own hash, so a quote left on turns every upload
        // into a disagreement and the device stops declaring anything.
        for etag in ["\"abc123\"", "abc123", "W/\"abc123\"", "  \"abc123\"  "] {
            assert_eq!(etag_hash(etag), "abc123", "{etag:?}");
        }
    }

    #[cfg(feature = "fake")]
    #[tokio::test]
    async fn the_fake_plays_its_script_then_holds_the_last_answer() {
        use super::fake::Fake;

        let batch = Batch {
            manifest_hash: "0".repeat(64),
            boot_id: "0123456789abcdef".to_owned(),
            seq: "1".to_owned(),
            readings: Vec::new(),
            heartbeat: None,
        };
        // An outage, then recovery: the shape the drain has to survive.
        let cloud = Fake::answering(
            vec![Outcome::Unavailable, Outcome::Unavailable],
            Outcome::Committed,
        );
        assert_eq!(cloud.send(&batch).await, Outcome::Unavailable);
        assert_eq!(cloud.send(&batch).await, Outcome::Unavailable);
        assert_eq!(cloud.send(&batch).await, Outcome::Committed);
        assert_eq!(cloud.send(&batch).await, Outcome::Committed);
    }

    #[test]
    fn the_two_credential_statuses_are_not_swept_up_with_the_other_4xx() {
        // They sit inside the 4xx range, so a range match written first would swallow them and
        // the device would drop a batch every time a token was rotated.
        assert_ne!(classify(401), Outcome::Rejected(401));
        assert_ne!(classify(403), Outcome::Rejected(403));
        assert_eq!(classify(402), Outcome::Rejected(402));
    }
}
