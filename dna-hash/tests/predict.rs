//! Regression guard for the modifier → DNA-hash logic (backlog B85). A DNA hash
//! is deterministic over the integrity code + the DNA modifiers, so a fixed
//! bundle + seed + properties must always hash to the SAME value — a Holochain
//! pin bump that changes the bytes would flip these goldens and fail loudly here
//! rather than silently producing a wrong `to_dna_hash` on a live deploy.
//!
//! Fixture: `tests/fixtures/forum.happ` (role `forum`) — the smallest real hApp
//! bundle in the tree, copied in so the test is self-contained (it can't reach a
//! sibling submodule at build time). ~1.1 MB; embedded via `include_bytes!`.

use dna_hash::{hash_dna, load_dna_bundle, properties_serialized};
use holo_hash::DnaHashB64;
use holochain_types::web_app::{
    AppManifestLocation, WebAppBundle, WebAppManifest, WebAppManifestV0, WebUI,
};
use mr_bundle::{Bundle, ResourceBytes};

/// The committed fixture, embedded so cwd / checkout layout can't break the test.
const FORUM_HAPP: &[u8] = include_bytes!("fixtures/forum.happ");
const FORUM_ROLE: &str = "forum";

/// Fixed, arbitrary install-time modifiers for the always-on goldens. NOT the
/// real release values (those live in the `#[ignore]` known-answer test below) —
/// just a stable pair so the goldens are reproducible.
const FIXED_SEED: &str = "b85-regression-seed";
const FIXED_PROPS: &str =
    r#"{"progenitor_pubkey":"uhCAkbrzggNifw0v95IJGjQlfkOdXDaiUT9BM2JG2ZwFzUisFHiAM"}"#;

/// Golden hash of `forum.dna` at (`FIXED_SEED`, `FIXED_PROPS`), computed once by
/// this crate and hard-coded. Recompute (and update) only on a deliberate
/// Holochain pin bump.
const FORUM_GOLDEN: &str = "uhC0kUEgc8i27T3t_ApiP13S4VUSXUnJXmdaTVdthvSHbJwoAc9d2";

async fn forum_hash_from(bytes: &[u8], ext: &str) -> DnaHashB64 {
    let props = properties_serialized(FIXED_PROPS).expect("encode properties");
    let bundle = load_dna_bundle(bytes, ext, FORUM_ROLE)
        .await
        .expect("load DNA bundle");
    let hash = hash_dna(bundle, Some(FIXED_SEED.to_string()), Some(props))
        .await
        .expect("hash DNA");
    DnaHashB64::from(hash)
}

/// Pack a `.webhapp` wrapping the committed `.happ` (empty UI — never read by the
/// hash path), so the webhapp→inner-happ load branch is exercised with a real
/// bundle. Uses the same `mr_bundle` version `holochain_types` resolves.
fn pack_webhapp_wrapping(happ_bytes: &[u8]) -> Vec<u8> {
    let manifest = WebAppManifest::V0(WebAppManifestV0 {
        name: "b85-test-webhapp".to_string(),
        ui: WebUI {
            path: "ui.zip".to_string(),
        },
        happ: AppManifestLocation {
            path: "forum.happ".to_string(),
        },
    });
    let resources: Vec<(String, ResourceBytes)> = vec![
        ("ui.zip".to_string(), ResourceBytes::from(Vec::<u8>::new())),
        (
            "forum.happ".to_string(),
            ResourceBytes::from(happ_bytes.to_vec()),
        ),
    ];
    let bundle: Bundle<WebAppManifest> =
        Bundle::new(manifest, resources).expect("build web-app bundle");
    WebAppBundle::from(bundle)
        .pack()
        .expect("pack .webhapp")
        .to_vec()
}

#[tokio::test]
async fn forum_happ_hashes_to_the_golden() {
    let got = forum_hash_from(FORUM_HAPP, "happ").await.to_string();
    assert_eq!(
        got, FORUM_GOLDEN,
        "forum.dna hash drifted from the golden — a Holochain pin bump changed the \
         DNA bytes (recompute the golden ONLY if that change is intended)"
    );
}

#[tokio::test]
async fn webhapp_inner_happ_hashes_identically_to_the_bare_happ() {
    // A `.webhapp` wraps the `.happ`; its inner DNA must hash identically to the
    // bare `.happ` (locks in the B85 webhapp→inner-happ load branch).
    let webhapp = pack_webhapp_wrapping(FORUM_HAPP);
    let via_happ = forum_hash_from(FORUM_HAPP, "happ").await;
    let via_webhapp = forum_hash_from(&webhapp, "webhapp").await;
    assert_eq!(
        via_happ, via_webhapp,
        "a .webhapp's inner .happ must hash identically to the bare .happ"
    );
}

/// The REAL zero-fleet known answer for release v0.93.0: the alliance role of the
/// release `unyt.happ`, at the network seed + properties from
/// `automation/config/release.json`, hashes to the `to_dna_hash` the progenitor
/// deploy committed (`automation/config/progenitor/results/deploy-result.json`).
///
/// `#[ignore]` so CI passes WITHOUT the multi-MB release happ; run at release
/// time against the real bundle:
///   `B85_ALLIANCE_HAPP=/path/to/unyt.happ cargo test -- --ignored alliance`
#[tokio::test]
#[ignore = "requires the real release unyt.happ (multi-MB); run at release time"]
async fn alliance_known_answer_v0_93_0() {
    // From automation/config/release.json (v0.93.0).
    const SEED: &str = "bIibDddhRRadkATPPINJ2";
    const PROPS: &str = r#"{"progenitor_pubkey":"uhCAkbrzggNifw0v95IJGjQlfkOdXDaiUT9BM2JG2ZwFzUisFHiAM","joining_server_signer":"uhCAk_Jbtn_3RR-VCLPtJdhcQvVrpM7Vw5vHGog8_CwW5tO0_Cf37"}"#;
    // From automation/config/progenitor/results/deploy-result.json (alliance cell).
    const EXPECTED: &str = "uhC0kmXvAdsPwWnBbk_pJJTY6z1ud4cBQb0ngxj_KMWTPOQVlwDa0";

    let happ_path = std::env::var("B85_ALLIANCE_HAPP")
        .expect("set B85_ALLIANCE_HAPP to the real release unyt.happ");
    let bytes = std::fs::read(&happ_path).expect("read the real unyt.happ");
    let props = properties_serialized(PROPS).expect("encode properties");
    let bundle = load_dna_bundle(&bytes, "happ", "alliance")
        .await
        .expect("load alliance DNA");
    let hash = hash_dna(bundle, Some(SEED.to_string()), Some(props))
        .await
        .expect("hash alliance DNA");
    assert_eq!(DnaHashB64::from(hash).to_string(), EXPECTED);
}
