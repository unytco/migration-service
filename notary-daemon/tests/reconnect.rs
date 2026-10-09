use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use holo_hash::AgentPubKey;
use http_body_util::BodyExt;
use rave_engine::types::entries::migration::v0_2::AttestCloseResponse;
use tokio::sync::Notify;
use tokio::time::Instant;
use tower::ServiceExt;

use migration_notary::conductor::{Conductor, Reconnecting};
use migration_notary::http::{router, AppState};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What `Ham::ping` fails with once a holochain 0.7 conductor has closed its
/// connection by restarting.
const CLOSED: &str = "Failed to probe app_info: Websocket error: Websocket closed: No connection";
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
    holds_whoami: AtomicBool,
    release_whoami: Notify,
    connects: AtomicUsize,
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
            holds_whoami: AtomicBool::new(false),
            release_whoami: Notify::new(),
            connects: AtomicUsize::new(0),
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
        Ok(AttestCloseResponse::NoCloseFound)
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
async fn a_restarted_conductor_is_answered_on_a_new_connection_whichever_call_comes_first() {
    for first in EVERY_CALL {
        let (conductor, daemon) = connected().await;
        conductor.restart();

        first
            .on(&daemon)
            .await
            .unwrap_or_else(|e| panic!("{first:?} after a restart: {e:#}"));
        for call in EVERY_CALL {
            call.on(&daemon).await.unwrap();
        }
        assert_eq!(
            conductor.connects(),
            2,
            "{first:?}: one reconnect, kept for every call after it"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn while_the_conductor_is_down_each_call_tries_again_and_says_why_it_failed() {
    let (conductor, daemon) = connected().await;
    conductor.down.store(true, SeqCst);
    conductor.restart();

    for _ in 0..2 {
        let err = format!("{:#}", daemon.ping().await.unwrap_err());
        assert!(err.contains("reconnecting to the conductor"), "{err}");
        assert!(err.contains("Connection refused"), "{err}");
    }
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
async fn a_call_is_run_again_once_even_if_the_new_connection_is_closed_too() {
    let (conductor, daemon) = connected().await;
    conductor.closes_new_connections.store(true, SeqCst);
    conductor.restart();

    let answered = tokio::time::timeout(CONNECT_TIMEOUT * 10, daemon.ping())
        .await
        .expect("a call is run again once, not until it succeeds");
    let err = format!("{:#}", answered.unwrap_err());
    assert!(err.contains("Websocket closed"), "{err}");
    assert_eq!(conductor.connects(), 2);
}

#[tokio::test(start_paused = true)]
async fn concurrent_calls_on_a_closed_connection_share_one_attempt_whatever_its_outcome() {
    for down in [false, true] {
        let (conductor, daemon) = connected().await;
        conductor.down.store(down, SeqCst);
        conductor.restart();

        let calls: Vec<_> = (0..20)
            .map(|_| {
                let daemon = daemon.clone();
                tokio::spawn(async move { daemon.ping().await })
            })
            .collect();
        for call in calls {
            match call.await.unwrap() {
                Ok(()) => assert!(!down),
                Err(e) => {
                    let err = format!("{e:#}");
                    assert!(down, "{err}");
                    assert!(
                        err.contains("Connection refused"),
                        "every caller sharing the attempt is told why it failed: {err}"
                    );
                }
            }
        }
        assert_eq!(conductor.connects(), 2, "down: {down}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_that_hangs_gives_up_after_its_timeout() {
    let (conductor, daemon) = connected().await;
    conductor.connect_never_answers.store(true, SeqCst);
    conductor.restart();

    let started = Instant::now();
    let answered = tokio::time::timeout(CONNECT_TIMEOUT * 10, daemon.ping())
        .await
        .expect("a hung reconnect must give up");
    let err = format!("{:#}", answered.unwrap_err());
    assert!(err.contains("took longer than 30s"), "{err}");
    assert_eq!(started.elapsed(), CONNECT_TIMEOUT);

    conductor.connect_never_answers.store(false, SeqCst);
    daemon.ping().await.expect("the next call tries again");
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
async fn a_call_in_flight_across_a_restart_finishes_on_the_replacement() {
    let (conductor, daemon) = connected().await;
    conductor.holds_whoami.store(true, SeqCst);
    let in_flight = tokio::spawn({
        let daemon = daemon.clone();
        async move { daemon.whoami().await }
    });
    tokio::time::sleep(Duration::from_millis(1)).await;

    conductor.restart();
    daemon.ping().await.unwrap();
    conductor.holds_whoami.store(false, SeqCst);
    conductor.release_whoami.notify_one();

    in_flight.await.unwrap().unwrap();
    assert_eq!(
        conductor.connects(),
        2,
        "the replacement another call made, not one more"
    );
}

async fn healthz(daemon: &Arc<Reconnecting<Connection>>) -> (StatusCode, serde_json::Value) {
    let app = router(AppState {
        conductor: daemon.clone(),
        bearer_token: Arc::new("token".into()),
    });
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test(start_paused = true)]
async fn healthz_answers_again_after_a_restart_and_keeps_the_cause_out_of_its_body() {
    let (conductor, daemon) = connected().await;
    assert_eq!(healthz(&daemon).await.0, StatusCode::OK);

    conductor.down.store(true, SeqCst);
    conductor.restart();
    let (status, body) = healthz(&daemon).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body["error"]["message"], "conductor unreachable: reconnecting to the conductor",
        "an unauthenticated endpoint names the failure, not the node's paths and ports"
    );

    conductor.down.store(false, SeqCst);
    assert_eq!(healthz(&daemon).await.0, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn a_lair_restart_under_a_running_conductor_is_answered_on_a_new_connection() {
    let (conductor, daemon) = connected().await;
    conductor.lair_restarts.fetch_add(1, SeqCst);

    daemon.ping().await.unwrap();
    assert_eq!(
        conductor.connects(),
        1,
        "an unsigned call still answers on the old connection"
    );
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
