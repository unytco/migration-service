//! The signing gate as a property of the BINARY, not of a function: the bug it
//! exists for was a service that connected — and wrote a capability grant to a
//! closed chain — before any of its probe-first logic could run. So these tests
//! spawn the real binary against a port they own, and read the connection
//! itself as the evidence.
//!
//! What they establish: without lair credentials the process exits non-zero and
//! never opens a TCP connection to the conductor's admin port, and with the
//! opt-in it does connect. What they cannot establish without a live conductor:
//! that the lair path commits no cap grant. That is `ham`'s documented behavior
//! (lair signing uses the cell's own agent key) and is asserted here only as
//! "ham is configured with a lair signer".

use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_headless-migrator");

/// A bound on every wait here: long enough for a process start plus a TCP
/// connect on a loaded CI box, short enough to fail rather than hang a job.
const DEADLINE: Duration = Duration::from_secs(60);
/// How often either wait re-checks; it leaves as soon as the condition lands.
const POLL: Duration = Duration::from_millis(50);

/// A listener standing in for the conductor's admin interface. Nothing ever
/// answers on it; the test only asks whether the service tried to connect.
fn admin_port_stub() -> (TcpListener, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("binding a stub admin port");
    listener
        .set_nonblocking(true)
        .expect("the accept probe must not block");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// A per-test working directory, so the child cannot pick up a `.env` that
/// happens to sit next to the crate (`dotenvy` searches upward from the cwd).
fn scratch_dir(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-signing-gate-{}-{}",
        name,
        std::process::id()
    ));
    std::fs::create_dir_all(&p).expect("creating the child's working directory");
    p
}

/// The close service, started with a cleared environment plus exactly the
/// variables named — so a lair credential in the developer's own environment
/// cannot make the refusal test pass for the wrong reason.
fn close_service(name: &str, admin_port: u16, extra: &[(&str, &str)]) -> Command {
    let dir = scratch_dir(name);
    let mut cmd = Command::new(BIN);
    cmd.arg("close-service")
        .current_dir(&dir)
        .env_clear()
        .env("HOLOCHAIN_ADMIN_PORT", admin_port.to_string())
        .env(
            "MIGRATION_AGENT_STATE_FILE",
            dir.join("state.json").to_string_lossy().to_string(),
        );
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd
}

/// Whether anything connected to the stub admin port.
fn was_connected_to(listener: &TcpListener) -> bool {
    match listener.accept() {
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::WouldBlock => false,
        Err(e) => panic!("accept probe failed: {e}"),
    }
}

fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Collect a refusal's output under [`DEADLINE`]. A plain `output()` would wait
/// as long as the child does, so a service that regressed into connecting and
/// retrying would hang this test instead of failing it.
fn wait_for_exit(mut child: Child, what: &str) -> Output {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if child.try_wait().expect("polling the child").is_some() {
            return child
                .wait_with_output()
                .expect("collecting the child's output");
        }
        if Instant::now() >= deadline {
            kill(child);
            panic!("{what} did not exit within {DEADLINE:?} — it is retrying a connection instead of refusing to start");
        }
        std::thread::sleep(POLL);
    }
}

#[test]
fn without_lair_signing_the_close_service_exits_instead_of_connecting() {
    let (listener, port) = admin_port_stub();

    let child = close_service("refusal", port, &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting the close service");
    let out = wait_for_exit(child, "the close service");

    assert!(
        !out.status.success(),
        "the service must not start without lair signing (exit was {:?})",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for expected in [
        "MIGRATION_AGENT_LAIR_URL",
        "MIGRATION_AGENT_LAIR_PASSPHRASE",
        "MIGRATION_AGENT_ALLOW_CAP_GRANT_SIGNING",
        "/etc/holochain/conductor-config.yaml",
        "/var/lib/holochain/lair-passphrase",
    ] {
        assert!(
            stderr.contains(expected),
            "the refusal must name {expected}; got:\n{stderr}"
        );
    }
    assert!(
        !was_connected_to(&listener),
        "the service connected to the conductor before dying — connecting IS the write \
         this refusal exists to prevent"
    );
}

#[test]
fn lair_credentials_let_the_close_service_connect() {
    // The other half of the gate: that `from_env` reads the two variables the
    // installer actually renders. Nothing else pins the wiring, so a service
    // that read the wrong name would refuse for the wrong reason and still look
    // correct. Signing is settled before the admin socket is dialled, so
    // reaching the stub port is the evidence the credentials were accepted.
    let (listener, port) = admin_port_stub();

    let mut child = close_service(
        "lair",
        port,
        &[
            (
                "MIGRATION_AGENT_LAIR_URL",
                "unix:///var/lib/holochain/lair/socket?k=abc123",
            ),
            ("MIGRATION_AGENT_LAIR_PASSPHRASE", "deadbeef"),
        ],
    )
    .stderr(Stdio::null())
    .stdout(Stdio::null())
    .spawn()
    .expect("starting the close service");

    let deadline = Instant::now() + DEADLINE;
    let connected = loop {
        if was_connected_to(&listener) {
            break true;
        }
        if let Some(status) = child.try_wait().expect("polling the close service") {
            panic!("the close service exited ({status:?}) instead of connecting: it did not accept the lair credentials");
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(POLL);
    };

    kill(child);
    assert!(
        connected,
        "with both lair variables set the close service must get past the refusal"
    );
}

#[test]
fn the_cap_grant_opt_in_lets_the_close_service_connect() {
    let (listener, port) = admin_port_stub();

    let mut child = close_service(
        "opt-in",
        port,
        &[("MIGRATION_AGENT_ALLOW_CAP_GRANT_SIGNING", "1")],
    )
    .stderr(Stdio::null())
    .stdout(Stdio::null())
    .spawn()
    .expect("starting the close service");

    let deadline = Instant::now() + DEADLINE;
    let connected = loop {
        if was_connected_to(&listener) {
            break true;
        }
        if let Some(status) = child.try_wait().expect("polling the close service") {
            panic!(
                "the opt-in service exited ({status:?}) instead of connecting — the escape \
                 hatch is decorative"
            );
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(POLL);
    };

    kill(child);
    assert!(
        connected,
        "with the opt-in set the service must take the cap-token path and connect"
    );
}

#[test]
fn a_dotenv_file_cannot_turn_the_cap_grant_path_back_on_for_the_close_service() {
    // The opt-in is the one variable that turns a chain write back on, so it
    // must come from the environment an operator set and from nothing else. A
    // `.env` beside the binary (or anywhere above its working directory) used to
    // be enough, because both binaries loaded one before reading their config.
    let (listener, port) = admin_port_stub();
    let dir = scratch_dir("dotenv");
    std::fs::write(
        dir.join(".env"),
        "MIGRATION_AGENT_ALLOW_CAP_GRANT_SIGNING=1\n",
    )
    .expect("planting a .env in the child's working directory");

    let child = close_service("dotenv", port, &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting the close service");
    let out = wait_for_exit(child, "the close service");

    assert!(
        !out.status.success(),
        "a .env must not satisfy the opt-in (exit was {:?})",
        out.status
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refusing to connect"),
        "the refusal must still fire; got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !was_connected_to(&listener),
        "a .env-supplied opt-in let the service connect"
    );
}
