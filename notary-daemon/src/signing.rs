//! The lair credentials and the opt-in this daemon reads from its environment.
//!
//! The daemon only reads (`read_predecessor_close`, `whoami`), but connecting is
//! not free: through lair a zome call is signed with the cell's own agent key
//! and nothing is written, while without it `ham` authorizes a throwaway signing
//! key by committing a capability grant to the notary's chain on EVERY connect,
//! so a restart loop writes to the chain of the agent whose signatures the whole
//! migration depends on.
//!
//! The decision itself is `ham`'s.

use anyhow::{Context, Result};
use ham::{CapGrantOptIn, LairCredentials, SigningPolicy};

use crate::config::var;

/// The lair keystore's IPC connection URL (`keystore.connection_url` in the
/// conductor config).
pub const LAIR_URL_VAR: &str = "MIGRATION_NOTARY_LAIR_URL";
/// The passphrase that unlocks that keystore.
pub const LAIR_PASSPHRASE_VAR: &str = "MIGRATION_NOTARY_LAIR_PASSPHRASE";
/// Permit the signing path that commits a capability grant per connect, for a
/// node with no lair to reach. Lair still wins wherever it resolves.
pub const ALLOW_CAP_GRANT_VAR: &str = "MIGRATION_NOTARY_ALLOW_CAP_GRANT_SIGNING";

/// Where a deployed droplet keeps the two lair values, named in the refusal so
/// an operator is told which files to look at, not just which variables are
/// missing.
const CONDUCTOR_CONFIG_PATH: &str = "/etc/holochain/conductor-config.yaml";
const PASSPHRASE_FILE_PATH: &str = "/var/lib/holochain/lair-passphrase";

/// Read the environment this daemon was started with.
pub fn from_env() -> Result<SigningPolicy> {
    resolve(
        var(LAIR_URL_VAR),
        var(LAIR_PASSPHRASE_VAR),
        var(ALLOW_CAP_GRANT_VAR),
    )
}

/// Hand `ham` the three raw values and name them on whatever it says back.
/// Pure, so the refusal is tested without mutating the process environment.
pub fn resolve(
    lair_url: Option<String>,
    lair_passphrase: Option<String>,
    allow_cap_grant: Option<String>,
) -> Result<SigningPolicy> {
    let opt_in =
        CapGrantOptIn::from_value(allow_cap_grant.as_deref()).context(ALLOW_CAP_GRANT_VAR)?;
    SigningPolicy::resolve(
        LairCredentials::Values {
            connection_url: lair_url,
            passphrase: lair_passphrase.map(String::into_bytes),
        },
        opt_in,
    )
    .with_context(operator_guidance)
}

/// What an operator needs at 3am that `ham` cannot say: which variables carry
/// the credentials on this service, where their values come from on a droplet,
/// and which variable permits the other path.
fn operator_guidance() -> String {
    format!(
        "{LAIR_URL_VAR} and {LAIR_PASSPHRASE_VAR} are how this daemon is given lair signing.\n\
         On a droplet both values sit with the conductor: keystore.connection_url in \
         {CONDUCTOR_CONFIG_PATH}, and the passphrase in {PASSPHRASE_FILE_PATH}. The automation \
         installer (setup-migration-notary.sh) reads them off the node and renders \
         them into this daemon's EnvironmentFile.\n\
         Set {ALLOW_CAP_GRANT_VAR}=1 only to permit the capability-grant path on a node with no \
         lair to reach. Lair wins wherever it resolves, so taking that path also means leaving \
         {LAIR_URL_VAR} and {LAIR_PASSPHRASE_VAR} unset."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "unix:///var/lib/holochain/lair/socket?k=abc123";

    fn ham_cfg() -> ham::HamConfig {
        ham::HamConfig::new(8800, 30000, "unyt")
    }

    #[test]
    fn lair_credentials_reach_hams_lair_signer() {
        let cfg = resolve(Some(URL.into()), Some("pass".into()), None)
            .unwrap()
            .apply(ham_cfg());
        let lair = cfg
            .lair
            .as_ref()
            .expect("lair credentials must configure ham's lair signer (the no-cap-grant path)");
        // The URL itself, not just "some lair": the keystore it reaches is the
        // one whose key the cell is signed with.
        assert_eq!(lair.connection_url.as_str(), URL);
    }

    #[test]
    fn no_lair_and_no_opt_in_refuses() {
        let err = format!("{:#}", resolve(None, None, None).unwrap_err());
        for expected in [
            LAIR_URL_VAR,
            LAIR_PASSPHRASE_VAR,
            ALLOW_CAP_GRANT_VAR,
            CONDUCTOR_CONFIG_PATH,
            PASSPHRASE_FILE_PATH,
        ] {
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn half_the_lair_credentials_refuses_too() {
        for (url, passphrase) in [
            (Some(URL.to_string()), None),
            (None, Some("pass".to_string())),
        ] {
            let err = format!(
                "{:#}",
                resolve(url.clone(), passphrase.clone(), None).unwrap_err()
            );
            // The refusal names both variables, and ham says which half is
            // missing, so the operator is not sent looking at both.
            assert!(err.contains(LAIR_URL_VAR), "{err}");
            assert!(err.contains(LAIR_PASSPHRASE_VAR), "{err}");
            let expected = if url.is_some() {
                "connection URL is set"
            } else {
                "passphrase is set"
            };
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn the_opt_in_selects_the_cap_grant_path() {
        let cfg = resolve(None, None, Some("1".into()))
            .unwrap()
            .apply(ham_cfg());
        assert!(
            cfg.lair.is_none(),
            "the opt-in must leave ham on the client-signing (cap grant) path"
        );
        assert!(
            cfg.allow_cap_grant_signing,
            "and it must reach ham's own flag, or ham refuses the connect"
        );
    }

    #[test]
    fn lair_wins_over_the_opt_in() {
        let cfg = resolve(Some(URL.into()), Some("pass".into()), Some("1".into()))
            .unwrap()
            .apply(ham_cfg());
        assert_eq!(
            cfg.lair.expect("lair signing").connection_url.as_str(),
            URL,
            "the opt-in permits the chain write where there is no other way, it does not ask \
             for one"
        );
        assert!(!cfg.allow_cap_grant_signing);
    }

    #[test]
    fn an_off_opt_in_still_requires_lair() {
        for raw in ["0", "false", "no", "OFF", ""] {
            assert!(
                resolve(None, None, Some(raw.into())).is_err(),
                "{raw}: an explicitly off opt-in is not permission to write to the chain"
            );
        }
    }

    #[test]
    fn an_unrecognized_opt_in_value_is_an_error_not_a_guess() {
        let err = format!(
            "{:#}",
            resolve(None, None, Some("maybe".into())).unwrap_err()
        );
        assert!(err.contains(ALLOW_CAP_GRANT_VAR), "{err}");
        assert!(err.contains("maybe"), "{err}");
    }

    #[test]
    fn a_malformed_lair_url_is_fatal_at_resolve_not_at_connect() {
        let err = format!(
            "{:#}",
            resolve(Some("not a url".into()), Some("pass".into()), None).unwrap_err()
        );
        assert!(err.contains(LAIR_URL_VAR), "{err}");
        assert!(err.contains("not a url"), "{err}");
    }

    #[test]
    fn the_debug_of_the_resolved_policy_never_renders_the_passphrase() {
        let policy = resolve(Some(URL.into()), Some("s3cr3t".into()), None).unwrap();
        // `Config` derives Debug and carries this, so a passphrase must not be
        // one `{:?}` away from the journal.
        let rendered = format!("{policy:?}");
        assert!(!rendered.contains("s3cr3t"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }
}
