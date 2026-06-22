//! The machine-readable progress file the report collector reads: it persists
//! atomically and round-trips its fields.

use migration_agent::state_file::{Phase, State, Step, VerifyReport};

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "migration-agent-state-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn round_trips_through_disk() {
    let path = tmp("roundtrip");
    let mut state =
        State::new(Phase::Open, Step::Verifying, "verifying").with_agent(Some("uhCAkAGENT".into()));
    state.old_chain_closed = true;
    state.new_chain_opened = true;
    state.package_fetchable = true;
    state.safe_to_teardown = true;
    state.signatures_collected = Some(3);
    state.signatures_threshold = Some(3);
    state.verify = Some(VerifyReport {
        balance_match: true,
        carry_forward_units_match: true,
        mismatches: vec![],
    });

    state.write(&path).unwrap();
    let read = State::read(&path).unwrap();

    assert_eq!(read.phase, Phase::Open);
    assert_eq!(read.step, Step::Verifying);
    assert_eq!(read.agent.as_deref(), Some("uhCAkAGENT"));
    assert!(read.old_chain_closed && read.new_chain_opened && read.package_fetchable);
    assert!(read.safe_to_teardown);
    assert_eq!(read.signatures_collected, Some(3));
    assert!(read.verify.as_ref().unwrap().passed());
    assert!(read.updated_at_us > 0, "the write stamps a timestamp");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_is_atomic_no_tmp_left_behind() {
    let path = tmp("atomic");
    let state = State::new(Phase::Close, Step::Probing, "probing");
    state.write(&path).unwrap();
    assert!(path.exists(), "the target file exists after write");
    let tmp_path = path.with_extension("json.tmp");
    assert!(
        !tmp_path.exists(),
        "the temp file is renamed away, not left behind"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn persisted_safe_to_teardown_reads_the_monotonic_signal() {
    // The authoritative teardown signal both the open service's idempotent path
    // and `status` read back (instead of re-running a live verify). A verified
    // open persisted `true`; an absent file or an un-verified record reads
    // `false` (the conservative default).
    let path = tmp("persisted-teardown");
    assert!(
        !State::persisted_safe_to_teardown(&path),
        "an absent state file ⇒ false (never verified)"
    );

    let mut s = State::new(Phase::Open, Step::CollectingSignatures, "mid-flight");
    s.new_chain_opened = true;
    s.safe_to_teardown = false;
    s.write(&path).unwrap();
    assert!(
        !State::persisted_safe_to_teardown(&path),
        "an un-verified record ⇒ false"
    );

    s.step = Step::Done;
    s.safe_to_teardown = true;
    s.write(&path).unwrap();
    assert!(
        State::persisted_safe_to_teardown(&path),
        "the open service's verified record ⇒ true"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_never_lowers_a_persisted_safe_to_teardown() {
    // The monotonic-latch write guard (fix 1): once `safe_to_teardown = true` is
    // on disk, NO later write may lower it — not a fresh `State{..false}` from a
    // cross-process restart, not the standalone `verify` command, not a `status`
    // probe. The guard read-modify-writes that one field defensively, so even a
    // caller that forgets to seed can never clobber the latch.
    let path = tmp("latch-write-guard");

    // A verified open persisted the latch.
    let mut verified = State::new(Phase::Open, Step::Done, "verified");
    verified.new_chain_opened = true;
    verified.old_chain_closed = true;
    verified.safe_to_teardown = true;
    verified.write(&path).unwrap();
    assert!(State::persisted_safe_to_teardown(&path));

    // A fresh record (default false) — as a restarted process / a status probe
    // would build — tries to overwrite. The on-disk latch must survive.
    let fresh = State::new(Phase::Status, Step::Probing, "fresh process, default false");
    assert!(!fresh.safe_to_teardown, "the fresh record itself is false");
    fresh.write(&path).unwrap();
    assert!(
        State::persisted_safe_to_teardown(&path),
        "a fresh false write must NOT lower the persisted true (monotonic latch)"
    );
    let reread = State::read(&path).unwrap();
    assert!(
        reread.safe_to_teardown,
        "the re-read record still carries the latched true"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_still_persists_true_when_disk_was_false() {
    // The guard only ever RAISES, never blocks a legitimate raise: writing `true`
    // over a prior `false` works (the open service's first verify-success write).
    let path = tmp("latch-raise");
    State::new(Phase::Open, Step::Probing, "not yet verified")
        .write(&path)
        .unwrap();
    assert!(!State::persisted_safe_to_teardown(&path));

    let mut done = State::new(Phase::Open, Step::Done, "verified");
    done.safe_to_teardown = true;
    done.write(&path).unwrap();
    assert!(
        State::persisted_safe_to_teardown(&path),
        "raising false→true must persist"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn seed_from_persisted_carries_the_monotonic_latch_and_verify() {
    // Fix 1's seed path: a FRESH `State` (as `open::run` / `verify::run` build at
    // start) seeded from a prior verified record picks up the monotonic
    // `safe_to_teardown = true` and the prior verify detail — so the in-memory
    // idempotent short-circuit (`AlreadyOpened` checks `state.safe_to_teardown`)
    // sees the same value the file holds, WITHOUT re-reading a just-clobbered file.
    let path = tmp("seed-from-persisted");
    let mut verified = State::new(Phase::Open, Step::Done, "verified");
    verified.new_chain_opened = true;
    verified.old_chain_closed = true;
    verified.safe_to_teardown = true;
    verified.verify = Some(VerifyReport {
        balance_match: true,
        carry_forward_units_match: true,
        mismatches: vec![],
    });
    verified.write(&path).unwrap();

    // A fresh record, exactly as a restarted open service starts.
    let mut fresh = State::new(Phase::Open, Step::Probing, "");
    assert!(!fresh.safe_to_teardown);
    assert!(fresh.verify.is_none());
    fresh.seed_from_persisted(&path);
    assert!(
        fresh.safe_to_teardown,
        "the seeded fresh record carries the prior monotonic latch (drives the no-fetch short-circuit)"
    );
    assert!(
        fresh.verify.as_ref().map(|v| v.passed()).unwrap_or(false),
        "the prior verify detail is carried forward by the seed"
    );

    // And an absent file is a harmless no-op (a first run has nothing to seed).
    let mut first = State::new(Phase::Open, Step::Probing, "");
    first.seed_from_persisted(&tmp("seed-absent"));
    assert!(!first.safe_to_teardown, "no prior file ⇒ nothing seeded");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_fails_closed_on_a_present_but_corrupt_state_file() {
    // B45: the latch read must tell a genuinely-ABSENT file from a PRESENT-but-
    // corrupt one. A corrupt file may hold a latched `true` that can't be parsed,
    // so a fresh `false` written over it would silently DROP that latch. Fail
    // closed: refuse the lowering write, leave the corrupt bytes intact, surface
    // the read error.
    let path = tmp("corrupt-fail-closed");
    // A present-but-unparseable state file (it held a latched `true` no reader can
    // now recover — exactly the bytes the latch must not let a fresh `false` drop).
    let corrupt = b"{ \"safe_to_teardown\": true, this is not valid json";
    std::fs::write(&path, corrupt).unwrap();

    // A fresh `status`-style record (default `safe_to_teardown = false`) tries to
    // overwrite the corrupt file. It MUST be refused, not silently clobber it.
    let fresh = State::new(Phase::Status, Step::Probing, "fresh process, default false");
    assert!(!fresh.safe_to_teardown);
    let err = fresh
        .write(&path)
        .expect_err("a lowering write over a corrupt file must fail closed, not succeed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing to lower safe_to_teardown") && msg.contains("unreadable"),
        "the read error is surfaced (fail closed): {msg}"
    );

    // The corrupt file is left EXACTLY as it was — no fresh `false` clobbered the
    // (unrecoverable but undropped) latch, and no temp file was left behind.
    assert_eq!(
        std::fs::read(&path).unwrap(),
        corrupt,
        "the corrupt file must be untouched — its latch was not dropped"
    );
    assert!(
        !path.with_extension("json.tmp").exists(),
        "no temp file is left behind by the refused write"
    );

    // RAISING the latch (`true`) over the same corrupt file IS allowed — a raise
    // can't lower anything and it repairs the corruption with a valid record.
    let mut done = State::new(Phase::Open, Step::Done, "verified — raises the latch");
    done.safe_to_teardown = true;
    done.write(&path)
        .expect("a `true` write repairs a corrupt file (raising can't drop a latch)");
    assert!(
        State::persisted_safe_to_teardown(&path),
        "the repaired file now carries the latched true"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn verify_report_passed_requires_all_fields() {
    let mut r = VerifyReport {
        balance_match: true,
        carry_forward_units_match: true,
        mismatches: vec![],
    };
    assert!(r.passed());
    r.carry_forward_units_match = false;
    assert!(!r.passed(), "any failed field fails the whole verify");
}
