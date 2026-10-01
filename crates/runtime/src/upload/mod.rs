//! Sending a batch, and what the cloud's answer means for it.
//!
//! The status classes are the contract's, not this crate's invention: which answers let a batch
//! go and which keep it is a property of the seam, because getting it wrong loses readings on one
//! side and floods the cloud on the other.

pub(crate) mod backoff;
pub(crate) mod drain;
#[cfg(feature = "fake")]
pub mod fake;

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use contract::{Batch, Encoded, Heartbeat};

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
    /// Whether the cloud may take it later: keep what was sent, back off, ask again. Otherwise the
    /// answer is final — a batch leaves the buffer, a manifest is taken or never will be.
    #[must_use]
    pub const fn retries(self) -> bool {
        matches!(self, Self::Credential | Self::Unavailable)
    }
}

/// What an HTTP status means. No answer at all is [`Outcome::Unavailable`], which is why this
/// takes a status rather than a result.
#[must_use]
pub const fn classify(status: u16) -> Outcome {
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
    async fn declare(&self, manifest: &Encoded) -> Result<String, Declined>;

    /// Send one batch.
    async fn send(&self, batch: &Batch) -> Outcome;

    /// Send one heartbeat. Its answer is only journalled: nothing is kept or retried on it.
    async fn beat(&self, heartbeat: &Heartbeat) -> Outcome;
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
    async fn declare(&self, manifest: &Encoded) -> Result<String, Declined> {
        let response = self
            .client
            .put(self.url("manifest"))
            .bearer_auth(self.token.reveal())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(manifest.bytes().to_vec())
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
        if accepted != manifest.hash() {
            return Err(Declined::Disagreement {
                sent: manifest.hash().to_owned(),
                accepted,
            });
        }
        Ok(accepted)
    }

    async fn send(&self, batch: &Batch) -> Outcome {
        let Ok(bytes) = serde_json::to_vec(batch) else {
            return Outcome::Rejected(400);
        };
        self.post("batches", bytes).await
    }

    async fn beat(&self, heartbeat: &Heartbeat) -> Outcome {
        let Ok(bytes) = serde_json::to_vec(heartbeat) else {
            return Outcome::Rejected(400);
        };
        self.post("heartbeats", bytes).await
    }
}

impl Http {
    async fn post(&self, tail: &str, bytes: Vec<u8>) -> Outcome {
        match self
            .client
            .post(self.url(tail))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_committed_batch_leaves_the_buffer() {
        for status in [200, 201, 202, 204, 299] {
            assert_eq!(classify(status), Outcome::Committed);
            assert!(!classify(status).retries());
        }
    }

    #[test]
    fn a_rejected_batch_is_dropped_rather_than_retried_forever() {
        // The device built something the cloud will never take. Retrying is a loop that ends when
        // the buffer overflows, taking good readings with it.
        for status in [400, 404, 409, 413, 422] {
            assert_eq!(classify(status), Outcome::Rejected(status));
            assert!(!classify(status).retries());
        }
    }

    #[test]
    fn a_refused_credential_keeps_the_batch() {
        // Invariant: a rotated token must not cost the readings taken while it was stale.
        for status in [401, 403] {
            assert_eq!(classify(status), Outcome::Credential);
            assert!(classify(status).retries());
        }
    }

    #[test]
    fn a_cloud_that_cannot_commit_now_keeps_the_batch() {
        for status in [429, 500, 502, 503, 504] {
            assert_eq!(classify(status), Outcome::Unavailable);
            assert!(classify(status).retries());
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

        let batch = batch("0".repeat(64));
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

    fn batch(manifest_hash: String) -> Batch {
        Batch {
            manifest_hash,
            boot_id: "0123456789abcdef".to_owned(),
            seq: "1".to_owned(),
            readings: Vec::new(),
        }
    }

    /// A body whose length is stated up front, not one streamed in chunks.
    fn assert_sized(head: &str) {
        assert!(head.contains("\r\ncontent-length: "), "{head}");
        assert!(!head.contains("\r\ntransfer-encoding: "), "{head}");
    }

    /// The head of one request to a loopback listener, answered `200` with `etag`.
    async fn request_head(answer_etag: &str, call: impl AsyncFnOnce(Http)) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let reply =
            format!("HTTP/1.1 200 OK\r\netag: \"{answer_etag}\"\r\ncontent-length: 0\r\n\r\n");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("a connection");
            let mut head = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.expect("a request");
                assert!(read > 0, "the request ended before its head did");
                head.extend_from_slice(&chunk[..read]);
            }
            stream.write_all(reply.as_bytes()).await.expect("an answer");
            String::from_utf8_lossy(&head).to_lowercase()
        });
        let http = Http::new(
            base,
            "device".to_owned(),
            Token::fixture("token"),
            Duration::from_secs(5),
        )
        .expect("a client");
        call(http).await;
        server.await.expect("the listener")
    }

    #[tokio::test]
    async fn every_upload_declares_its_length() {
        // The cloud answers `411` to a body without `content-length`, and a `4xx` drops the batch:
        // a streamed body would lose every reading it carried, silently.
        let manifest = contract::Manifest {
            tz: "UTC".to_owned(),
            sources: vec![contract::Source {
                id: "s".to_owned(),
                metrics: vec![contract::Metric::State {
                    key: "k".to_owned(),
                    state_labels: None,
                }],
            }],
        };
        let manifest = manifest.encode().expect("within the contract");
        let hash = manifest.hash().to_owned();
        let head = request_head(&hash, async |http| {
            assert_eq!(http.declare(&manifest).await, Ok(hash.clone()));
        })
        .await;
        assert_sized(&head);

        let batch = batch(hash.clone());
        let head = request_head(&hash, async |http| {
            assert_eq!(http.send(&batch).await, Outcome::Committed);
        })
        .await;
        assert_sized(&head);

        let heartbeat = Heartbeat::new("0123456789abcdef".to_owned(), 1, 0);
        let head = request_head(&hash, async |http| {
            assert_eq!(http.beat(&heartbeat).await, Outcome::Committed);
        })
        .await;
        assert_sized(&head);
        assert!(
            head.starts_with("post /devices/device/heartbeats "),
            "{head}"
        );
    }
}
