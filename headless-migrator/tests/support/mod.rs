//! Shared test scaffolding: a mock [`Conductor`] whose every method is scripted
//! and whose calls are recorded, plus fixture builders for the `rave_engine`
//! wire types. Lets the probe / close / open state machines run with no live
//! conductor — the notary-daemon's mocked-seam pattern, extended to the
//! admin-lifecycle surface.
//!
//! Each integration test file is its own crate and uses a different subset of
//! these helpers, so unused-in-one-crate is expected — silence it here rather
//! than per item.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use headless_migrator::conductor::{AppPresence, Conductor, InstallSpec};
use holo_hash::{ActionHash, AgentPubKey, DnaHash};
use rave_engine::types::entries::migration::v0_1::{
    AgreementCarryForward, CommittedClose, MigrationInitRequest, NotarySignature,
    PrepareCloseResponse, SignClosingResponse, SignRequest, SummaryState, SummaryStatePayload,
    SummaryTx,
};
use rave_engine::types::ledger::CarryForwardUnits;
use rave_engine::types::ledger::Ledger;
use rave_engine::types::units::UnitMap;
use zfuel::fuel::ZFuel;

/// Every interaction the mock records, so a test can assert ordering (e.g.
/// `drop_off_fees` precedes `prepare_closing_summary`).
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Ping,
    GetLedger,
    DropOffFees,
    /// `target` is the close-binding DNA passed in, so a test can assert the
    /// configured `to_dna` reaches `prepare_closing_summary`.
    PrepareClosingSummary {
        target: holo_hash::DnaHash,
    },
    RequestClosingSignature,
    CloseAgentChain,
    GetMigrationCloseState,
    VerifyIfMigrated,
    AppPresence,
    InstallApp,
}

/// A fully scripted mock conductor. Each field is either a fixed value or a
/// queue consumed per call; absent scripts return a sensible default or error.
#[derive(Default)]
pub struct MockConductor {
    pub calls: Mutex<Vec<Call>>,
    pub ledger: Mutex<Option<Ledger>>,
    pub drop_fees: Mutex<Option<anyhow::Result<String>>>,
    pub prepare: Mutex<Option<anyhow::Result<PrepareCloseResponse>>>,
    pub sign_responses: Mutex<VecDeque<anyhow::Result<SignClosingResponse>>>,
    pub close_result: Mutex<Option<anyhow::Result<ActionHash>>>,
    pub close_state: Mutex<VecDeque<anyhow::Result<CommittedClose>>>,
    pub verify_migrated: Mutex<VecDeque<anyhow::Result<bool>>>,
    pub presence: Mutex<VecDeque<anyhow::Result<AppPresence>>>,
    pub install_result: Mutex<VecDeque<anyhow::Result<()>>>,
}

impl MockConductor {
    pub fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn pop<T>(q: &Mutex<VecDeque<anyhow::Result<T>>>, what: &str) -> anyhow::Result<T> {
        q.lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("mock: no scripted response for {what}"))
    }
}

#[async_trait]
impl Conductor for MockConductor {
    async fn ping(&self) -> anyhow::Result<()> {
        self.record(Call::Ping);
        Ok(())
    }

    async fn get_ledger(&self) -> anyhow::Result<Ledger> {
        self.record(Call::GetLedger);
        self.ledger
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("mock: no ledger scripted"))
    }

    async fn drop_off_fees(&self) -> anyhow::Result<String> {
        self.record(Call::DropOffFees);
        self.drop_fees
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Ok("No fees owed".into()))
    }

    async fn prepare_closing_summary(
        &self,
        target: holo_hash::DnaHash,
    ) -> anyhow::Result<PrepareCloseResponse> {
        self.record(Call::PrepareClosingSummary { target });
        self.prepare
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Err(anyhow::anyhow!("mock: no prepare scripted")))
    }

    async fn request_closing_signature(
        &self,
        _req: SignRequest,
    ) -> anyhow::Result<SignClosingResponse> {
        self.record(Call::RequestClosingSignature);
        Self::pop(&self.sign_responses, "request_closing_signature")
    }

    async fn close_agent_chain(
        &self,
        _payload: SummaryStatePayload,
        _notary_signatures: Vec<NotarySignature>,
    ) -> anyhow::Result<ActionHash> {
        self.record(Call::CloseAgentChain);
        self.close_result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Ok(action_hash(9)))
    }

    async fn get_migration_close_state(&self) -> anyhow::Result<CommittedClose> {
        self.record(Call::GetMigrationCloseState);
        Self::pop(&self.close_state, "get_migration_close_state")
    }

    async fn verify_if_migrated(&self) -> anyhow::Result<bool> {
        self.record(Call::VerifyIfMigrated);
        Self::pop(&self.verify_migrated, "verify_if_migrated")
    }

    async fn app_presence(&self, _app_id: &str) -> anyhow::Result<AppPresence> {
        self.record(Call::AppPresence);
        Self::pop(&self.presence, "app_presence")
    }

    async fn install_app(&self, _spec: &InstallSpec) -> anyhow::Result<()> {
        self.record(Call::InstallApp);
        Self::pop(&self.install_result, "install_app")
    }
}

// ── Fixture builders ─────────────────────────────────────────────────────

pub fn action_hash(seed: u8) -> ActionHash {
    ActionHash::from_raw_36(vec![seed; 36])
}

pub fn agent(seed: u8) -> AgentPubKey {
    AgentPubKey::from_raw_36(vec![seed; 36])
}

pub fn dna(seed: u8) -> DnaHash {
    DnaHash::from_raw_36(vec![seed; 36])
}

/// A `DnaHashB64` from a seed, for the router-facing params (`from_dna` / `to_dna`).
pub fn dna_b64(seed: u8) -> holo_hash::DnaHashB64 {
    holo_hash::DnaHashB64::from(dna(seed))
}

/// A ledger with the given balance/CFU and zero fees.
pub fn ledger(balance: UnitMap, cfu: CarryForwardUnits, fees_owed: ZFuel) -> Ledger {
    Ledger {
        balance,
        carry_forward_units: cfu,
        fees_owed,
        proposed_balance: UnitMap::new(),
    }
}

/// A `SummaryState` with the given closing balance / CFU and `n` agreement
/// carry-forward entries.
pub fn summary_state(
    closing_balance: UnitMap,
    closing_cfu: CarryForwardUnits,
    agreements: usize,
) -> SummaryState {
    SummaryState {
        opening_balance: UnitMap::new(),
        opening_carry_forward_units: CarryForwardUnits::new(),
        closing_balance,
        closing_carry_forward_units: closing_cfu,
        summary_tx: SummaryTx {
            proposals: vec![],
            commitments: vec![],
            accepts: vec![],
            receipts: vec![],
            rejects: vec![],
            reclaims: vec![],
            spend_links: vec![],
        },
        agreement_carry_forward: (0..agreements)
            .map(|i| AgreementCarryForward {
                smart_agreement_hash: action_hash(100 + i as u8),
                last_execution_action_hash: action_hash(150 + i as u8),
                carryover: serde_json::json!({ "i": i }),
            })
            .collect(),
    }
}

pub fn payload(agent_seed: u8, closing: SummaryState) -> SummaryStatePayload {
    SummaryStatePayload {
        agent_pubkey: agent(agent_seed),
        source_dna_hash: dna(1),
        target_dna_hash: dna(2),
        closing_state: closing,
        chain_top: action_hash(2),
    }
}

pub fn prepare_response(
    agent_seed: u8,
    closing: SummaryState,
    notaries: Vec<AgentPubKey>,
    threshold: u32,
) -> PrepareCloseResponse {
    PrepareCloseResponse {
        payload: payload(agent_seed, closing),
        closing_notaries: notaries,
        closing_threshold: threshold,
    }
}

pub fn committed_close(agent_seed: u8, closing: SummaryState) -> CommittedClose {
    CommittedClose {
        payload: payload(agent_seed, closing),
        notary_signatures: vec![NotarySignature {
            notary: agent(50),
            signature: hdi::prelude::Signature([1u8; 64]),
        }],
        close_action: action_hash(6),
    }
}

pub fn migration_init_request(agent_seed: u8, closing: SummaryState) -> MigrationInitRequest {
    MigrationInitRequest {
        payload: payload(agent_seed, closing),
        notary_signatures: vec![NotarySignature {
            notary: agent(50),
            signature: hdi::prelude::Signature([1u8; 64]),
        }],
        close_action: action_hash(6),
    }
}

/// A `UnitMap` holding `amount` ZFuel at unit index `unit`. `UnitMap::from`
/// takes `(unit_index, amount_str)` tuples (the key is the unit index number,
/// the value is the ZFuel amount parsed from its string form).
pub fn unit_map(unit: u32, amount: u32) -> UnitMap {
    UnitMap::from(vec![(unit, amount_leaked(amount))])
}

/// Leak the small amount string so `UnitMap::from`'s `&'static str` value bound
/// can be built from a number. Test-only, bounded (a handful of fixtures).
fn amount_leaked(amount: u32) -> &'static str {
    Box::leak(amount.to_string().into_boxed_str())
}
