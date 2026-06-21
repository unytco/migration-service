//! Headless server-agent migration driver. One binary, four supervised /
//! one-shot modes (`Status` · `CloseService` · `OpenService` · `Verify`) that
//! carry a stateful server agent's economic identity from an old alliance DNA
//! to its successor. Built on the same `ham` + published-`rave_engine`
//! foundation as the notary-daemon, with a mockable [`conductor::Conductor`]
//! seam so the policy / probe / verify state machines are unit-tested without a
//! live conductor.
//!
//! The lair-version-aware key carry across droplets is the automation shell's
//! job (`migrate-carry-key.sh`); this crate operates on an already-carried key,
//! installing the app FOR it on the new DNA — it only honors that contract.

pub mod close;
pub mod conductor;
pub mod config;
pub mod dna_errors;
pub mod fetch;
pub mod joining;
pub mod open;
pub mod policy;
pub mod probe;
pub mod state_file;
pub mod status;
pub mod verify;
