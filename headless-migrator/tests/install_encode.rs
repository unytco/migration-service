//! The install-with-`init_properties` encode — the headless half of HC-795. The
//! fetched migration package is placed as the migrating role's `init_properties`
//! on the `install_app` payload, byte-identically to what the DNA's `init`
//! decodes on the successor. `build_install_payload` is the pure builder that
//! `HamConductor::install_app` sends, so testing it here covers the encode (the
//! headline task-20 change) without a live conductor. A fresh, non-migrating
//! install carries no `init_properties`.

mod support;

use headless_migrator::conductor::{build_install_payload, InstallSpec};
use holochain_types::app::RoleSettings;
use holochain_types::prelude::{DnaModifiersOpt, SerializedBytes, YamlProperties};
use rave_engine::types::entries::migration::v0_1::MigrationInitRequest;
use rave_engine::types::CarryForwardUnits;

use support::{agent, migration_init_request, summary_state, unit_map};

fn alliance_spec(migration_package: Option<MigrationInitRequest>) -> InstallSpec {
    InstallSpec {
        app_id: "unyt".into(),
        role_name: "alliance".into(),
        agent_key: agent(7),
        happ_path: std::path::PathBuf::from("/tmp/unyt.happ"),
        network_seed: None,
        properties: None,
        membrane_proof: None,
        migration_package,
    }
}

/// The sole provisioned role's DNA modifiers override, as the payload carries it.
fn only_role_modifiers(spec: &InstallSpec) -> Option<DnaModifiersOpt<YamlProperties>> {
    let payload = build_install_payload(spec).expect("build install payload");
    let (_name, settings) = payload
        .roles_settings
        .expect("roles_settings present")
        .into_iter()
        .next()
        .expect("exactly one role");
    match settings {
        RoleSettings::Provisioned { modifiers, .. } => modifiers,
        _ => panic!("the migrating role must be Provisioned"),
    }
}

/// The sole provisioned role's `init_properties` bytes (the builder emits exactly
/// one Provisioned role); panics if the payload shape isn't that.
fn only_role_init_properties(spec: &InstallSpec) -> Option<SerializedBytes> {
    let payload = build_install_payload(spec).expect("build install payload");
    let (_name, settings) = payload
        .roles_settings
        .expect("roles_settings present")
        .into_iter()
        .next()
        .expect("exactly one role");
    match settings {
        RoleSettings::Provisioned {
            init_properties, ..
        } => init_properties.map(|ip| ip.0),
        _ => panic!("the migrating role must be Provisioned"),
    }
}

#[test]
fn migrating_install_carries_the_package_as_role_init_properties() {
    let pkg = migration_init_request(
        7,
        summary_state(unit_map(0, 10), CarryForwardUnits::new(), 1),
    );
    let encoded = only_role_init_properties(&alliance_spec(Some(pkg.clone())));
    let expected = SerializedBytes::try_from(&pkg).expect("encode package");
    assert_eq!(
        encoded,
        Some(expected),
        "the fetched package must encode into the role's init_properties byte-for-byte \
         (the exact bytes the DNA's init decodes)"
    );
}

#[test]
fn fresh_non_migrating_install_carries_no_init_properties() {
    assert_eq!(
        only_role_init_properties(&alliance_spec(None)),
        None,
        "a fresh, non-migrating install carries no init_properties"
    );
}

/// The network's DNA properties are hashed into the DNA hash, so the install
/// must carry them verbatim — the migrating cell lands on the network's DNA only
/// if its properties are byte-identical to every other agent's.
#[test]
fn install_carries_the_networks_dna_properties() {
    let props: YamlProperties = serde_json::from_str(
        r#"{"progenitor_pubkey":"uhCAkfake","joining_server_signer":"uhCAkalso"}"#,
    )
    .expect("parse properties");
    let mut spec = alliance_spec(None);
    spec.network_seed = Some("unyt-local-testnet-b".into());
    spec.properties = Some(props.clone());

    let modifiers = only_role_modifiers(&spec).expect("modifiers override present");
    assert_eq!(
        modifiers.network_seed.as_deref(),
        Some("unyt-local-testnet-b")
    );
    assert_eq!(
        modifiers.properties.map(SerializedBytes::try_from),
        Some(SerializedBytes::try_from(props)),
        "the role's properties must encode byte-for-byte as the network's — a \
         re-ordered or re-encoded map is a different DNA hash"
    );
}

/// The regression this guards: an override built from a seed alone used to carry
/// `properties: None`. The conductor only applies `Some` fields, so the real
/// clobber is emitting an override that CANNOT carry properties at all — with the
/// happ manifest declaring none (`properties: ~`), the cell then genesises on a
/// property-less DNA, a different hash from the network's, in its own empty DHT.
#[test]
fn install_never_emits_a_properties_nilling_modifier() {
    let mut seed_only = alliance_spec(None);
    seed_only.network_seed = Some("unyt-local-testnet-b".into());
    let modifiers = only_role_modifiers(&seed_only).expect("modifiers override present");
    assert!(
        modifiers.properties.is_none(),
        "with no properties to apply the override must leave the field unset, \
         never a value that overwrites the DNA's"
    );

    assert!(
        only_role_modifiers(&alliance_spec(None)).is_none(),
        "with neither modifier to apply, no override is sent at all"
    );
}
