//! Fleet-free rail test for the open service: with the conductor injected
//! (B2's `open::run_with` + `MockConnector`), drive the whole supervised loop to
//! GD-wait exhaustion against a mock — no live conductor — and assert the
//! persisted state + returned error carry the actionable CONFIG-FAULT diagnosis
//! (B1), NOT a raw genesis error.

mod support;

use std::sync::Arc;
use std::time::Duration;

use headless_migrator::conductor::AppPresence;
use headless_migrator::config::{Config, OpenConfig};
use headless_migrator::open::{self, OpenParams};
use headless_migrator::policy::PolicyOpts;
use headless_migrator::state_file::{State, Step};
use holochain_types::prelude::CellId;
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn bind_local() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

/// Answer one request. `false` once the listener can no longer accept, which
/// ends the serving loop driving it.
async fn answer(listener: &TcpListener, status_line: &str, body: &str) -> bool {
    let Ok((mut socket, _)) = listener.accept().await else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let _ = socket.read(&mut buf).await;
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
    true
}

/// Serve `script` in order, one response per request, then close. (Mirrors
/// `tests/fetch.rs`'s; each test crate is standalone, so it carries its own.)
async fn serve(script: Vec<(&'static str, &'static str)>) -> String {
    let (listener, url) = bind_local().await;
    tokio::spawn(async move {
        for (status_line, body) in script {
            if !answer(&listener, status_line, body).await {
                return;
            }
        }
    });
    url
}

async fn one_shot_server(status_line: &'static str, body: &'static str) -> String {
    serve(vec![(status_line, body)]).await
}

/// Serve `script` in order and start it again, for a run with no last pass.
async fn cycling_server(script: Vec<(&'static str, &'static str)>) -> String {
    let (listener, url) = bind_local().await;
    tokio::spawn(async move {
        'serving: loop {
            for (status_line, body) in &script {
                if !answer(&listener, status_line, body).await {
                    break 'serving;
                }
            }
        }
    });
    url
}

async fn endless_server(status_line: &'static str, body: &'static str) -> String {
    cycling_server(vec![(status_line, body)]).await
}

fn tmp_state(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-open-service-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// A watchdog, not a deadline to beat: everything awaited here is local and
/// takes milliseconds, so only something that never happens trips it.
const WATCHDOG: Duration = Duration::from_secs(30);

const KEEP_RETRYING_WINDOW: Duration = Duration::from_millis(150);

/// Watch the state file until `accepts` takes the record. On expiry the `Err`
/// carries the last record read, so a failure says what was there.
async fn persisted_state_reaches(
    state_file: &std::path::Path,
    accepts: impl Fn(&State) -> bool,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    let mut last = "no state file written yet".to_string();
    while tokio::time::Instant::now() < deadline {
        match State::read(state_file) {
            Ok(state) if accepts(&state) => return Ok(()),
            Ok(state) => last = format!("{:?}: {}", state.step, state.message),
            Err(e) => last = format!("{e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    Err(last)
}

/// A `Config` with snappy retries; conductor ports are irrelevant (the mock
/// connector is injected, so nothing dials them).
fn cfg(state_file: std::path::PathBuf) -> Config {
    Config {
        admin_port: 1,
        app_port: 1,
        app_id: "unyt".into(),
        role_name: "alliance".into(),
        request_timeout_secs: 1,
        state_file,
        retry_initial: Duration::from_millis(1),
        retry_max: Duration::from_millis(2),
        policy: PolicyOpts {
            request_timeout: Duration::from_secs(1),
            state_mismatch_retries: 1,
            retry_initial: Duration::from_millis(1),
            retry_max: Duration::from_millis(2),
        },
        to_dna: None,
    }
}

fn never_shutdown() -> ham::ShutdownRx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    rx
}

/// A valid `MigrationInitRequest` body so the router fetch succeeds (letting the
/// loop reach the init/verify call). Built the same way `tests/fetch.rs` does —
/// the type is wire-decoded, so the JSON is assembled by field.
fn package_body() -> &'static str {
    let body = serde_json::json!({
        "payload": payload(3, summary_state(unit_map(0, 5), CarryForwardUnits::new(), 0)),
        "notary_signatures": [],
        "close_action": action_hash(6),
    })
    .to_string();
    Box::leak(body.into_boxed_str())
}

#[tokio::test]
async fn gd_wait_exhaustion_reports_a_config_fault_not_a_raw_genesis_error() {
    let state_file = tmp_state("gd-exhaust");

    // The new server already has the app installed but not yet verified, so the
    // loop skips install (no joining-service call) and goes straight to fetch →
    // connect ham → drive init. `init` reports the successor GD is not in effect
    // on every pass.
    let mock = Arc::new(MockConductor::default());
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    mock.verify_migrated
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(
            "wasm error: No Global Definition found"
        )));

    // The router hands back a valid package so the one fetch succeeds.
    let router = one_shot_server("200 OK", package_body()).await;

    // A happ bundle must exist for `assert_happ_path` (its contents don't matter
    // — install is skipped on the Installed path).
    let happ = tmp_state("dummy-happ");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: "http://127.0.0.1:1".into(),
        network_seed: None,
        joining_service_happ_id: "v0.99.0".into(),
        // Zero budget: the FIRST too-early exhausts immediately (single pass,
        // single fetch), so the one-shot router suffices.
        gd_wait_timeout: Duration::ZERO,
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    let mut sd = never_shutdown();
    let err = open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd)
        .await
        .expect_err("an exhausted GD wait must fail the open service")
        .to_string();

    // The returned error is the config-fault diagnosis, carrying both DNAs and
    // the raw init cause as a trailing detail — NOT the old raw-genesis bail.
    assert!(
        err.contains("check the successor DNA hash / registry"),
        "error leads with the config-fault diagnosis: {err}"
    );
    assert!(
        err.contains(&params.from_dna.to_string()) && err.contains(&params.to_dna.to_string()),
        "error carries both DNAs: {err}"
    );
    assert!(
        err.contains("No Global Definition found"),
        "error keeps the raw init cause: {err}"
    );
    assert!(
        !err.contains("gave up waiting for the successor GD"),
        "must not be the old raw-genesis bail: {err}"
    );

    // The persisted state file (which the automation rail cats out) carries the
    // same diagnosis, at Step::Failed.
    let state = State::read(&state_file).unwrap();
    assert_eq!(state.step, Step::Failed);
    assert!(
        state
            .message
            .contains("check the successor DNA hash / registry"),
        "persisted message is the config-fault diagnosis: {}",
        state.message
    );
    assert!(
        state.message.contains("No Global Definition found"),
        "persisted message keeps the raw init cause: {}",
        state.message
    );

    // The init error surfaced via the mock (drove init), confirming the loop
    // reached the verify call rather than failing earlier.
    assert!(
        mock.calls().contains(&Call::VerifyIfMigrated),
        "the loop drove init via verify_if_migrated: {:?}",
        mock.calls()
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

/// A node an earlier (pre-fix) install put on the WRONG DNA takes the
/// already-installed path, so the install-time target check never sees it. Without
/// a check here it would spend the whole GD budget on a definition that can never
/// resolve, then blame the GD. The loop reads the installed cell's DNA (admin-only,
/// no zome call) and hard-stops immediately, naming both hashes and the three
/// inputs that decide them.
#[tokio::test]
async fn an_already_installed_app_on_the_wrong_dna_hard_stops_immediately() {
    let state_file = tmp_state("wrong-dna");

    let mock = Arc::new(MockConductor::default());
    // TWO scripted passes, though a working guard stops on the first: with the
    // guard removed the loop falls through to the (unroutable) router and retries,
    // and the second entry keeps that retry from panicking on an exhausted mock —
    // so this test then fails on its own assertions, naming what broke, rather
    // than on an unrelated "no scripted response" panic.
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    // Installed on dna(9) for the right agent; the migration target below is dna(2).
    *mock.installed_cell.lock().unwrap() = Some(CellId::new(dna(9), agent(3)));

    let happ = tmp_state("dummy-happ-wrong-dna");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: "http://127.0.0.1:1".into(),
        network_seed: None,
        joining_service_happ_id: "v0.99.0".into(),
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        // An unroutable router: the check must fire BEFORE any package fetch, so
        // this is never reached.
        router_url: "http://127.0.0.1:1".into(),
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    let mut sd = never_shutdown();
    let err = open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd)
        .await
        .expect_err("an app on the wrong DNA must hard-stop the open service")
        .to_string();

    assert!(
        err.contains(&dna_b64(9).to_string()) && err.contains(&dna_b64(2).to_string()),
        "the hard stop names the installed DNA and the target: {err}"
    );
    assert!(
        err.contains("alone on an empty DHT"),
        "the hard stop explains the consequence: {err}"
    );

    // It stopped at the DNA check — never drove init, never fetched.
    assert!(
        !mock.calls().contains(&Call::VerifyIfMigrated),
        "the loop must not drive init on a cell that is on the wrong DNA: {:?}",
        mock.calls()
    );

    let state = State::read(&state_file).unwrap();
    assert_eq!(state.step, Step::Failed);

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

/// The bridging node runs TWO migrating apps off one shared carried lair, each
/// with its own `agent_key` — the shape where a config slip lands a cell on the
/// RIGHT DNA under the WRONG agent. The DNA check alone waves that through; the
/// carried chain then never opens, and it resurfaces as the misleading
/// "init_properties were not applied". The guard compares the whole CellId.
#[tokio::test]
async fn an_installed_app_for_the_wrong_agent_hard_stops() {
    let state_file = tmp_state("wrong-agent");

    let mock = Arc::new(MockConductor::default());
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    // Right DNA (the target below is dna(2)), but installed for agent(8) while
    // this migration carries agent(3).
    *mock.installed_cell.lock().unwrap() = Some(CellId::new(dna(2), agent(8)));

    let happ = tmp_state("dummy-happ-wrong-agent");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: "http://127.0.0.1:1".into(),
        network_seed: None,
        joining_service_happ_id: "v0.99.0".into(),
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: "http://127.0.0.1:1".into(),
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    let mut sd = never_shutdown();
    let err = open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd)
        .await
        .expect_err("an app installed for the wrong agent must hard-stop")
        .to_string();

    assert!(
        err.contains("WRONG key"),
        "the hard stop names the crossed-key fault rather than a DNA mismatch: {err}"
    );
    assert!(
        err.contains("agent_key"),
        "the hard stop points at the `.migrate.apps[]` agent_key config: {err}"
    );
    assert!(
        !mock.calls().contains(&Call::VerifyIfMigrated),
        "the loop must not drive init for the wrong agent: {:?}",
        mock.calls()
    );
    // The guard genuinely ran (rather than the run failing earlier).
    assert!(
        mock.calls().contains(&Call::InstalledCellId),
        "the loop read the installed cell: {:?}",
        mock.calls()
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

/// The rail proof that a permanent joining fault ends the run: an unregistered
/// happ_id gets the same 400 on every pass, so the whole loop must return.
#[tokio::test]
async fn an_unregistered_joining_happ_id_ends_the_run_instead_of_retrying() {
    let state_file = tmp_state("unknown-network");

    // Nothing installed, so the pass reaches the install and its join.
    let mock = Arc::new(MockConductor::default());
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Absent));

    let router = one_shot_server("200 OK", package_body()).await;
    let joining = one_shot_server(
        "400 Bad Request",
        r#"{"error":{"code":"unknown_network","message":"network is not registered with this service"}}"#,
    )
    .await;

    let happ = tmp_state("dummy-happ-unknown-network");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: joining,
        network_seed: None,
        // The local-testnet shape of this id.
        joining_service_happ_id: "unyt".into(),
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    let mut sd = never_shutdown();
    let err = tokio::time::timeout(
        WATCHDOG,
        open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd),
    )
    .await
    .expect("the open service must return, not keep retrying a refusal")
    .expect_err("a joining service that will never provision must end the run")
    .to_string();

    assert!(
        err.contains("unknown_network") && err.contains("not registered with this service"),
        "the failure carries the joining service's own reason: {err}"
    );
    assert!(
        err.contains("MIGRATION_AGENT_JOINING_SERVICE_HAPP_ID") && err.contains("unyt"),
        "the failure names the config that has to change: {err}"
    );

    // Without the join's modifiers the app would land on the wrong network's DNA.
    assert!(
        !mock.calls().contains(&Call::InstallApp),
        "no install without a provision: {:?}",
        mock.calls()
    );

    let state = State::read(&state_file).unwrap();
    assert_eq!(state.step, Step::Failed);
    assert!(
        state.message.contains("unknown_network"),
        "the persisted message carries the reason the rail cats out: {}",
        state.message
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

/// The other half at rail level. Without it, a change making every joining
/// failure terminal would pass every other test in this file.
#[tokio::test]
async fn a_joining_service_outage_keeps_the_run_waiting() {
    let state_file = tmp_state("joining-outage");

    // Every fixture answers for as long as it is asked, so what ends this run is
    // the test, never the script running out.
    let mock = Arc::new(MockConductor::default());
    *mock.presence_after_script.lock().unwrap() = Some(AppPresence::Absent);

    let router = endless_server("200 OK", package_body()).await;
    let joining = endless_server(
        "503 Service Unavailable",
        r#"{"error":{"code":"service_unavailable","message":"Auth service check failed"}}"#,
    )
    .await;

    let happ = tmp_state("dummy-happ-joining-outage");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    // Long enough that the windows below hold a handful of passes, not hundreds.
    let mut cfg = cfg(state_file.clone());
    cfg.retry_initial = Duration::from_millis(50);
    cfg.retry_max = Duration::from_millis(50);
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: joining,
        network_seed: None,
        joining_service_happ_id: "unyt".into(),
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    // The sender stays in scope: dropping it closes the channel, which the loop
    // reads as a shutdown and returns on, hiding whether it would have retried.
    let (_shutdown_tx, mut sd) = tokio::sync::watch::channel(false);
    let mut run = std::pin::pin!(open::run_with(
        &connector,
        &EchoSigner,
        &cfg,
        &open_cfg,
        &params,
        &mut sd
    ));

    // Half one: the outage reaches the state file, raced against the run's own
    // return. The run returning first is the regression.
    tokio::select! {
        outcome = &mut run => panic!(
            "the run must still be retrying the outage, not have returned: {:?}",
            outcome.map_err(|e| e.to_string())
        ),
        reached = persisted_state_reaches(&state_file, |s| {
            s.step != Step::Failed && s.message.contains("service_unavailable")
        }) => reached.unwrap_or_else(|last| panic!(
            "the persisted message must come to name what the run is waiting on, last read {last}"
        )),
    }

    // Half two: it keeps retrying past that. An absence, so a slow runner can
    // only make this window quieter.
    let returned = tokio::select! {
        outcome = &mut run => Some(outcome.map_err(|e| e.to_string())),
        _ = tokio::time::sleep(KEEP_RETRYING_WINDOW) => None,
    };
    assert!(
        returned.is_none(),
        "the run must go on retrying the outage, not return: {returned:?}"
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

#[tokio::test]
async fn a_membrane_proof_that_is_not_base64_ends_the_run() {
    let state_file = tmp_state("bad-proof");

    let mock = Arc::new(MockConductor::default());
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Absent));

    let router = one_shot_server("200 OK", package_body()).await;
    let joining = serve(vec![
        ("200 OK", r#"{"session":"s1","status":"ready"}"#),
        (
            "200 OK",
            r#"{"roles":{"alliance":{"membrane_proof":"not base64 !!"}}}"#,
        ),
    ])
    .await;

    let happ = tmp_state("dummy-happ-bad-proof");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: joining,
        network_seed: None,
        joining_service_happ_id: "unyt".into(),
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    let mut sd = never_shutdown();
    let err = tokio::time::timeout(
        WATCHDOG,
        open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd),
    )
    .await
    .expect("the open service must return, not keep retrying a proof it cannot decode")
    .expect_err("an undecodable membrane proof must end the run")
    .to_string();

    assert!(
        err.contains("not valid base64"),
        "the failure says what is wrong with the proof: {err}"
    );
    assert!(
        err.contains("alliance"),
        "the failure names the role it was served for: {err}"
    );
    assert!(
        !mock.calls().contains(&Call::InstallApp),
        "no install without a decodable proof: {:?}",
        mock.calls()
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

/// Upstream's own answer to a re-join by a key it has already admitted
/// (`joining-service/src/app.ts`).
const ALREADY_JOINED_409: (&str, &str) = (
    "409 Conflict",
    r#"{"error":{"code":"agent_already_joined","message":"This agent key has already completed joining this network. Use POST /v1/reconnect instead."}}"#,
);

/// The full recovery a re-entered install meets: refused, reconnected,
/// provisioned, carrying the modifiers that decide the cell's DNA hash.
fn already_joined_then_reconnected() -> Vec<(&'static str, &'static str)> {
    vec![
        ALREADY_JOINED_409,
        (
            "200 OK",
            r#"{"linker_urls":[],"http_gateways":[],"session":"s_reconnected"}"#,
        ),
        (
            "200 OK",
            r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y=","dna_modifiers":{"network_seed":"unyt-recovered"}}}}"#,
        ),
    ]
}

/// The pass boundary the bug lives on: an install that fails on the successor
/// GD leaves the app uninstalled, so the next pass re-enters `install` and joins
/// with a key that has already joined, meeting that same 409 every time. Two
/// passes, ended by the second install's own verdict rather than by a clock, so
/// what it proves is that the recovery REPEATS.
#[tokio::test]
async fn a_re_entered_install_reconnects_on_every_pass() {
    let state_file = tmp_state("already-joined");

    let mock = Arc::new(MockConductor::default());
    *mock.presence_after_script.lock().unwrap() = Some(AppPresence::Absent);
    // Pass one fails the way the budget is there for, leaving the app
    // uninstalled and the loop going round. Pass two ends the run on a verdict of
    // its own, so the test stops where it means to instead of on a timer.
    mock.install_result.lock().unwrap().extend([
        Err(anyhow::anyhow!("wasm error: No Global Definition found")),
        Err(anyhow::anyhow!(
            "Guest(\"[MIGERR:MIG_KEY_MISMATCH] the carried key is not the notarized agent\")"
        )),
    ]);

    let router = endless_server("200 OK", package_body()).await;
    let joining = cycling_server(already_joined_then_reconnected()).await;

    let happ = tmp_state("dummy-happ-already-joined");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: joining,
        network_seed: None,
        joining_service_happ_id: "v0.99.0".into(),
        // Ample, so this run ends on a verdict and not on the budget.
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    // The sender stays in scope: dropping it reads as a shutdown, and this test's
    // point is the SECOND pass. `never_shutdown` drops it, so it suits one pass.
    let (_shutdown_tx, mut sd) = tokio::sync::watch::channel(false);
    let err = tokio::time::timeout(
        WATCHDOG,
        open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd),
    )
    .await
    .expect("the open service must return, not hang")
    .expect_err("the second install returns an unrecoverable verdict")
    .to_string();

    // The decisive one. Refused at the join, the run stops before the FIRST
    // install; recovering only once, it stops before the second.
    let specs = mock.install_specs.lock().unwrap().clone();
    assert_eq!(
        specs.len(),
        2,
        "every pass must recover its own provision and install with it: {:?}",
        mock.calls()
    );
    // And what the recovery yielded is what the install used: counting installs
    // would pass for a recovery that handed back nothing.
    for (pass, spec) in specs.iter().enumerate() {
        assert_eq!(
            spec.membrane_proof.as_deref(),
            Some(b"proof".as_slice()),
            "pass {pass} installed without the reconnected proof"
        );
        assert_eq!(
            spec.network_seed.as_deref(),
            Some("unyt-recovered"),
            "pass {pass} installed without the reconnected network seed"
        );
    }
    assert!(
        !err.contains("will not provision the carried key"),
        "a key that has already joined is not a refusal to provision: {err}"
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

/// The recovery inherits the classification rather than escaping it: a reconnect
/// the service refuses is as final as a refused join.
#[tokio::test]
async fn a_reconnect_the_joining_service_refuses_ends_the_run() {
    let state_file = tmp_state("reconnect-refused");

    // The mock answers every pass, so a regression that retries fails on this
    // test's own assertions rather than on an exhausted mock.
    let mock = Arc::new(MockConductor::default());
    *mock.presence_after_script.lock().unwrap() = Some(AppPresence::Absent);

    let router = endless_server("200 OK", package_body()).await;
    let joining = serve(vec![
        ALREADY_JOINED_409,
        (
            "400 Bad Request",
            r#"{"error":{"code":"invalid_signature","message":"Signature does not verify against agent key"}}"#,
        ),
    ])
    .await;

    let happ = tmp_state("dummy-happ-reconnect-refused");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: joining,
        network_seed: None,
        joining_service_happ_id: "v0.99.0".into(),
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
    };

    let mut sd = never_shutdown();
    let err = tokio::time::timeout(
        WATCHDOG,
        open::run_with(&connector, &EchoSigner, &cfg, &open_cfg, &params, &mut sd),
    )
    .await
    .expect("the open service must return, not keep retrying a refusal")
    .expect_err("a refused reconnect leaves no way to provision")
    .to_string();

    assert!(
        err.contains("invalid_signature"),
        "the failure carries the joining service's own reason: {err}"
    );
    assert!(
        !mock.calls().contains(&Call::InstallApp),
        "no install without a provision: {:?}",
        mock.calls()
    );

    let state = State::read(&state_file).unwrap();
    assert_eq!(state.step, Step::Failed);

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}
