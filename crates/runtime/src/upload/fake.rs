//! A fake cloud, for exercising the drain without a network.
//!
//! Behind the `fake` feature, in the crate that owns the seam, so a test that needs one enables
//! it as a dev-dependency and nothing reachable from a release build has it.
//!
//! It behaves; it does not record. What a test asserts on is what the device did with the answer
//! — whether the batch left the buffer — not how many times it was asked.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use contract::{Batch, Encoded, Heartbeat};

use crate::upload::{Cloud, Declined, Outcome};

/// A cloud that answers from a script, then keeps answering the last thing forever. It takes
/// every manifest unless told otherwise with [`Fake::declaring`].
#[derive(Debug)]
pub struct Fake {
    answers: Mutex<VecDeque<Outcome>>,
    then: Outcome,
    declares: Mutex<VecDeque<Outcome>>,
    declared: Outcome,
}

impl Fake {
    /// A cloud that always answers the same way.
    #[must_use]
    pub fn always(outcome: Outcome) -> Self {
        Self::answering(Vec::new(), outcome)
    }

    /// A cloud that answers `script` in turn, then `then` from there on. This is how an
    /// outage is played: a run of `Unavailable`, then `Committed`.
    #[must_use]
    pub fn answering(script: Vec<Outcome>, then: Outcome) -> Self {
        Self {
            answers: Mutex::new(script.into()),
            then,
            declares: Mutex::new(VecDeque::new()),
            declared: Outcome::Committed,
        }
    }

    /// Answer a declaration `script` in turn, then `then`. `Committed` takes the manifest;
    /// anything else declines it with that answer.
    #[must_use]
    pub fn declaring(self, script: Vec<Outcome>, then: Outcome) -> Self {
        Self {
            declares: Mutex::new(script.into()),
            declared: then,
            ..self
        }
    }
}

#[async_trait]
impl Cloud for Fake {
    async fn declare(&self, manifest: &Encoded) -> Result<String, Declined> {
        let answer = self
            .declares
            .lock()
            .map_or(None, |mut declares| declares.pop_front())
            .unwrap_or(self.declared);
        if answer != Outcome::Committed {
            return Err(Declined::Answer(answer));
        }
        // Behaves like a cloud that hashes the bytes it was handed, which is what makes a
        // disagreement a real finding rather than something the fake invented.
        Ok(contract::manifest_hash(manifest.bytes()))
    }

    async fn send(&self, _batch: &Batch) -> Outcome {
        self.answers
            .lock()
            .map_or(None, |mut answers| answers.pop_front())
            .unwrap_or(self.then)
    }

    /// Answered as a batch finally is: a cloud down for one is down for both.
    async fn beat(&self, _heartbeat: &Heartbeat) -> Outcome {
        self.then
    }
}
