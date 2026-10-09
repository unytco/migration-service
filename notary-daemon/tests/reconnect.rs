mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use holo_hash::AgentPubKey;
use rave_engine::types::entries::migration::v0_2::AttestCloseResponse;
use tokio::sync::Notify;
use tokio::time::Instant;

use common::{attest_req, attested, healthz_req, send, TOKEN};
use migration_notary::conductor::{Conductor, Reconnecting};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What `Ham::ping` fails with once a holochain 0.7 conductor has closed its
/// connection by restarting.
const CLOSED: &str = "Failed to probe app_info: Websocket error: Websocket closed: No connection";
/// What a call in flight fails with when the connection under it drops.
const ANSWER_LOST: &str =
    "Failed to call zome: Websocket error: Websocket closed: ConnectionClosed";
const REFUSED: &str = "Failed to connect to admin interface: Websocket error: IO error: \
                       Connection refused (os error 111)";
const REQUEST_TIMEOUT: &str = "Failed to call zome: Websocket error: Timeout";
/// A signed call once the lair this connection signs through has restarted.
const LAIR_GONE: &str = r#"Failed to call zome: Unable to sign zome call: {"error":"BrokenPipe"}"#;
/// The conductor answering that its own keystore failed under a host fn.
const CONDUCTORS_KEYSTORE_FAILED: &str = r#"Failed to call zome: External API wire error: RibosomeError("KeystoreError: {\"error\":\"BrokenPipe\"}")"#;

/// A connection to it answers until the next restart, as a websocket does.
struct FakeConductor {
    restarts: AtomicU64,
    down: AtomicBool,
    connect_never_answers: AtomicBool,
    connect_takes_ms: AtomicU64,
    closes_new_connections: AtomicBool,
    times_out: AtomicBool,
    lair_restarts: AtomicU64,
    keystore_fails: AtomicBool,
    loses_next_answer: AtomicBool,
    holds_whoami: AtomicBool,
    release_whoami: Notify,
    connects: AtomicUsize,
    attestations_run: AtomicUsize,
}

impl FakeConductor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            restarts: AtomicU64::new(0),
            down: AtomicBool::new(false),
            connect_never_answers: AtomicBool::new(false),
            // Long enough that concurrent callers queue behind an attempt.
            connect_takes_ms: AtomicU64::new(100),
            closes_new_connections: AtomicBool::new(false),
            times_out: AtomicBool::new(false),
            lair_restarts: AtomicU64::new(0),
            keystore_fails: AtomicBool::new(false),
            loses_next_answer: AtomicBool::new(false),
            holds_whoami: AtomicBool::new(false),
            release_whoami: Notify::new(),
            connects: AtomicUsize::new(0),
            attestations_run: AtomicUsize::new(0),
        })
    }

    fn restart(&self) {
        self.restarts.fetch_add(1, SeqCst);
    }

    async fn connect(self: Arc<Self>) -> anyhow::Result<Connection> {
        self.connects.fetch_add(1, SeqCst);
        if self.connect_never_answers.load(SeqCst) {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_millis(self.connect_takes_ms.load(SeqCst))).await;
        if self.down.load(SeqCst) {
            anyhow::bail!(REFUSED);
        }
        let opened_at = self.restarts.load(SeqCst);
        if self.closes_new_connections.load(SeqCst) {
            self.restart();
        }
        Ok(Connection {
            opened_at,
            lair_at_open: self.lair_restarts.load(SeqCst),
            conductor: self,
        })
    }

    fn connects(&self) -> usize {
        self.connects.load(SeqCst)
    }

    fn attestations_run(&self) -> usize {
        self.attestations_run.load(SeqCst)
    }
}

struct Connection {
    conductor: Arc<FakeConductor>,
    opened_at: u64,
    lair_at_open: u64,
}

impl Connection {
    fn answer(&self) -> anyhow::Result<()> {
        if self.conductor.restarts.load(SeqCst) != self.opened_at {
            anyhow::bail!(CLOSED);
        }
        if self.conductor.times_out.load(SeqCst) {
            anyhow::bail!(REQUEST_TIMEOUT);
        }
        Ok(())
    }

    fn answer_signed(&self) -> anyhow::Result<()> {
        if self.conductor.lair_restarts.load(SeqCst) != self.lair_at_open {
            anyhow::bail!(LAIR_GONE);
        }
        if self.conductor.keystore_fails.load(SeqCst) {
            anyhow::bail!(CONDUCTORS_KEYSTORE_FAILED);
        }
        self.answer()
    }
}

#[async_trait]
impl Conductor for Connection {
    async fn ping(&self) -> anyhow::Result<()> {
        self.answer()
    }

    async fn notary_attest_close(&self, _: AgentPubKey) -> anyhow::Result<AttestCloseResponse> {
        self.answer_signed()?;
        self.conductor.attestations_run.fetch_add(1, SeqCst);
        if self.conductor.loses_next_answer.swap(false, SeqCst) {
            self.conductor.restart();
            anyhow::bail!(ANSWER_LOST);
        }
        Ok(attested())
    }

    async fn whoami(&self) -> anyhow::Result<AgentPubKey> {
        if self.conductor.holds_whoami.load(SeqCst) {
            self.conductor.release_whoami.notified().await;
        }
        self.answer_signed()?;
        Ok(AgentPubKey::from_raw_36(vec![9; 36]))
    }
}

async fn connected() -> (Arc<FakeConductor>, Arc<Reconnecting<Connection>>) {
    let conductor = FakeConductor::new();
    let first = conductor.clone().connect().await.unwrap();
    let reconnect = conductor.clone();
    let daemon = Reconnecting::new(first, CONNECT_TIMEOUT, move || reconnect.clone().connect());
    (conductor, Arc::new(daemon))
}

#[derive(Debug, Clone, Copy)]
enum Call {
    Ping,
    Whoami,
    Attest,
}

impl Call {
    async fn on(self, daemon: &Reconnecting<Connection>) -> anyhow::Result<()> {
        match self {
            Call::Ping => daemon.ping().await,
            Call::Whoami => daemon.whoami().await.map(drop),
            Call::Attest => daemon
                .notary_attest_close(AgentPubKey::from_raw_36(vec![1; 36]))
                .await
                .map(drop),
        }
    }
}

const EVERY_CALL: [Call; 3] = [Call::Ping, Call::Whoami, Call::Attest];

#[tokio::test(start_paused = true)]
async fn the_request_that_finds_the_connection_closed_fails_and_the_next_is_answered() {
    for first in EVERY_CALL {
        let (conductor, daemon) = connected().await;
        conductor.restart();

        let err = format!("{:#}", first.on(&daemon).await.unwrap_err());
        assert!(err.contains("Websocket closed"), "{first:?}: {err}");
        for call in EVERY_CALL {
            call.on(&daemon)
                .await
                .unwrap_or_else(|e| panic!("{first:?}, then {call:?}: {e:#}"));
        }
        assert_eq!(
            conductor.connects(),
            2,
            "{first:?}: one reconnect, kept for every call after it"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_whose_answer_was_lost_makes_one_zome_call_and_the_next_is_answered() {
    let (conductor, daemon) = connected().await;
    conductor.loses_next_answer.store(true, SeqCst);

    let (status, body) = send(daemon.clone(), attest_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(
        conductor.attestations_run(),
        1,
        "the call ran on the conductor before its answer was lost, so it is not run again"
    );

    let (status, body) = send(daemon.clone(), attest_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(conductor.attestations_run(), 2);
    assert_eq!(conductor.connects(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_replacement_that_closes_under_its_first_call_is_replaced_before_the_next_request() {
    let (conductor, daemon) = connected().await;
    conductor.down.store(true, SeqCst);
    conductor.restart();
    let (status, _) = send(daemon.clone(), attest_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(conductor.connects(), 2, "the reconnect failed");

    conductor.down.store(false, SeqCst);
    conductor.loses_next_answer.store(true, SeqCst);
    let (status, body) = send(daemon.clone(), attest_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(conductor.attestations_run(), 1, "one call, not run again");

    let (status, body) = send(daemon.clone(), attest_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(conductor.attestations_run(), 2);
    assert_eq!(conductor.connects(), 4);
}

#[tokio::test(start_paused = true)]
async fn while_the_conductor_is_down_each_request_tries_again_and_says_why_it_failed() {
    let (conductor, daemon) = connected().await;
    conductor.down.store(true, SeqCst);
    conductor.restart();

    let err = format!("{:#}", daemon.ping().await.unwrap_err());
    assert!(err.contains("Websocket closed"), "{err}");
    assert_eq!(conductor.connects(), 2);
    let err = format!("{:#}", daemon.ping().await.unwrap_err());
    assert!(err.contains("reconnecting to the conductor"), "{err}");
    assert!(err.contains("Connection refused"), "{err}");
    assert_eq!(conductor.connects(), 3);

    conductor.down.store(false, SeqCst);
    daemon
        .ping()
        .await
        .expect("answered once the conductor is back");
    daemon.ping().await.unwrap();
    assert_eq!(conductor.connects(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_leaves_the_connection_open_does_not_reconnect() {
    let (conductor, daemon) = connected().await;
    conductor.times_out.store(true, SeqCst);

    let err = format!("{:#}", daemon.whoami().await.unwrap_err());
    assert!(err.contains("Timeout"), "{err}");
    assert_eq!(
        conductor.connects(),
        1,
        "a slow call is not a closed connection, and a connect can write a capability grant"
    );
}

#[tokio::test(start_paused = true)]
async fn each_request_makes_at_most_one_reconnect() {
    let (conductor, daemon) = connected().await;
    conductor.closes_new_connections.store(true, SeqCst);
    conductor.restart();

    for expected_connects in [2, 3] {
        let answered = tokio::time::timeout(CONNECT_TIMEOUT * 10, daemon.ping())
            .await
            .expect("a request does not keep reconnecting");
        assert!(answered.is_err());
        assert_eq!(conductor.connects(), expected_connects);
    }
}

async fn pings_at_once(daemon: &Arc<Reconnecting<Connection>>) -> Vec<anyhow::Result<()>> {
    let calls: Vec<_> = (0..20)
        .map(|_| {
            let daemon = daemon.clone();
            tokio::spawn(async move { daemon.ping().await })
        })
        .collect();
    let mut answers = vec![];
    for call in calls {
        answers.push(call.await.unwrap());
    }
    answers
}

#[tokio::test(start_paused = true)]
async fn concurrent_requests_on_a_closed_connection_share_one_attempt_whatever_its_outcome() {
    for down in [false, true] {
        let (conductor, daemon) = connected().await;
        conductor.down.store(down, SeqCst);
        conductor.restart();

        for answered in pings_at_once(&daemon).await {
            let err = format!("{:#}", answered.unwrap_err());
            assert!(err.contains("Websocket closed"), "down: {down}: {err}");
        }
        assert_eq!(conductor.connects(), 2, "down: {down}");

        let next = daemon.ping().await;
        if down {
            let err = format!("{:#}", next.unwrap_err());
            assert!(err.contains("Connection refused"), "{err}");
        } else {
            next.expect("the next request uses the shared attempt's connection");
            assert_eq!(conductor.connects(), 2);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_requests_after_a_failed_reconnect_share_one_attempt_and_each_learns_its_outcome(
) {
    for comes_back in [false, true] {
        let (conductor, daemon) = connected().await;
        conductor.down.store(true, SeqCst);
        conductor.restart();
        daemon.ping().await.unwrap_err();
        assert_eq!(conductor.connects(), 2);
        conductor.down.store(!comes_back, SeqCst);

        for answered in pings_at_once(&daemon).await {
            match answered {
                Ok(()) => assert!(comes_back),
                Err(e) => {
                    let err = format!("{e:#}");
                    assert!(!comes_back, "{err}");
                    assert!(
                        err.contains("Connection refused"),
                        "every request sharing the attempt is told why it failed: {err}"
                    );
                }
            }
        }
        assert_eq!(conductor.connects(), 3, "comes back: {comes_back}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_that_hangs_gives_up_after_its_timeout() {
    let (conductor, daemon) = connected().await;
    conductor.connect_never_answers.store(true, SeqCst);
    conductor.restart();

    // The request that finds the connection closed, then one with no connection.
    for expected in ["Websocket closed", "took longer than 30s"] {
        let started = Instant::now();
        let answered = tokio::time::timeout(CONNECT_TIMEOUT * 10, daemon.ping())
            .await
            .expect("a hung reconnect must give up");
        let err = format!("{:#}", answered.unwrap_err());
        assert!(err.contains(expected), "{err}");
        assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
    }

    conductor.connect_never_answers.store(false, SeqCst);
    daemon.ping().await.expect("the next request tries again");
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_outlives_the_caller_that_gave_up_on_it() {
    let (conductor, daemon) = connected().await;
    conductor.connect_takes_ms.store(20_000, SeqCst);
    conductor.restart();

    assert!(
        tokio::time::timeout(Duration::from_secs(5), daemon.ping())
            .await
            .is_err(),
        "the router gives up on a call long before a slow connect finishes"
    );
    tokio::time::sleep(Duration::from_secs(20)).await;

    daemon.ping().await.unwrap();
    assert_eq!(
        conductor.connects(),
        2,
        "the abandoned attempt still landed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_in_flight_across_a_restart_fails_without_another_reconnect() {
    let (conductor, daemon) = connected().await;
    conductor.holds_whoami.store(true, SeqCst);
    let in_flight = tokio::spawn({
        let daemon = daemon.clone();
        async move { daemon.whoami().await }
    });
    tokio::time::sleep(Duration::from_millis(1)).await;

    conductor.restart();
    daemon.ping().await.unwrap_err();
    conductor.holds_whoami.store(false, SeqCst);
    conductor.release_whoami.notify_one();

    in_flight.await.unwrap().unwrap_err();
    assert_eq!(
        conductor.connects(),
        2,
        "the replacement another request made, not one more"
    );
    daemon.whoami().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn healthz_answers_again_after_a_restart_and_keeps_reconnect_causes_out_of_its_body() {
    let (conductor, daemon) = connected().await;
    assert_eq!(send(daemon.clone(), healthz_req()).await.0, StatusCode::OK);

    conductor.restart();
    assert_eq!(
        send(daemon.clone(), healthz_req()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(send(daemon.clone(), healthz_req()).await.0, StatusCode::OK);

    conductor.down.store(true, SeqCst);
    conductor.restart();
    send(daemon.clone(), healthz_req()).await;
    let (status, body) = send(daemon.clone(), healthz_req()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body["error"]["message"], "conductor unreachable: reconnecting to the conductor",
        "an unauthenticated endpoint names the failure, not the node's paths and ports"
    );

    conductor.down.store(false, SeqCst);
    assert_eq!(send(daemon.clone(), healthz_req()).await.0, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn a_lair_restart_under_a_running_conductor_fails_one_signed_request_then_reconnects() {
    let (conductor, daemon) = connected().await;
    conductor.lair_restarts.fetch_add(1, SeqCst);

    daemon.ping().await.unwrap();
    assert_eq!(
        conductor.connects(),
        1,
        "an unsigned call still answers on the old connection"
    );
    let err = format!("{:#}", daemon.whoami().await.unwrap_err());
    assert!(err.contains("BrokenPipe"), "{err}");
    for call in EVERY_CALL {
        call.on(&daemon)
            .await
            .unwrap_or_else(|e| panic!("{call:?}: {e:#}"));
    }
    assert_eq!(conductor.connects(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_conductors_own_keystore_failing_does_not_reconnect() {
    let (conductor, daemon) = connected().await;
    conductor.keystore_fails.store(true, SeqCst);

    let err = format!("{:#}", daemon.whoami().await.unwrap_err());
    assert!(err.contains("KeystoreError"), "{err}");
    assert_eq!(
        conductor.connects(),
        1,
        "the conductor answered, so the connection is good and a new one would not help"
    );
}
