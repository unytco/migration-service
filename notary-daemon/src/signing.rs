//! How this daemon's zome calls are signed, decided from the environment before
//! anything connects.
//!
//! The daemon only reads (`read_predecessor_close`, `whoami`), but connecting is
//! not free: through lair a zome call is signed with the cell's own agent key
//! and nothing is written, while without it `ham` authorizes a throwaway signing
//! key by committing a capability grant to the notary's chain on EVERY connect —
//! so a restart loop writes to the chain of the agent whose signatures the whole
//! migration depends on. Lair is therefore the only default, a daemon that
//! cannot get it refuses to start, and the chain-writing path is reachable only
//! by setting [`ALLOW_CAP_GRANT_VAR`].

use anyhow::{bail, Context, Result};

use crate::config::var;

/// The lair keystore's IPC connection URL (`keystore.connection_url` in the
/// conductor config).
pub const LAIR_URL_VAR: &str = "MIGRATION_NOTARY_LAIR_URL";
/// The passphrase that unlocks that keystore.
pub const LAIR_PASSPHRASE_VAR: &str = "MIGRATION_NOTARY_LAIR_PASSPHRASE";
/// Opt in to the signing path that commits a capability grant per connect.
pub const ALLOW_CAP_GRANT_VAR: &str = "MIGRATION_NOTARY_ALLOW_CAP_GRANT_SIGNING";

/// Where a deployed droplet keeps the two lair values, named in the refusal so
/// an operator is told which files to look at, not just which variables are
/// missing.
const CONDUCTOR_CONFIG_PATH: &str = "/etc/holochain/conductor-config.yaml";
const PASSPHRASE_FILE_PATH: &str = "/var/lib/holochain/lair-passphrase";

/// The signer every `ham` connection this daemon makes is built with.
#[derive(Clone)]
pub enum Signing {
    /// Sign with the cell's own agent key through lair. Commits nothing.
    Lair {
        connection_url: String,
        passphrase: String,
    },
    /// Authorize a throwaway signing key, committing one capability grant to
    /// the notary's chain per connect. Reachable only through the opt-in.
    CapGrant,
}

/// Hand-written: `Config` derives `Debug`, and a passphrase must not be one
/// `{:?}` away from the journal.
impl std::fmt::Debug for Signing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lair { connection_url, .. } => f
                .debug_struct("Lair")
                .field("connection_url", connection_url)
                .field("passphrase", &"<redacted>")
                .finish(),
            Self::CapGrant => f.write_str("CapGrant"),
        }
    }
}

impl Signing {
    /// Read the environment this daemon was started with.
    pub fn from_env() -> Result<Self> {
        Self::resolve(
            var(LAIR_URL_VAR),
            var(LAIR_PASSPHRASE_VAR),
            var(ALLOW_CAP_GRANT_VAR),
        )
    }

    /// Decide the signer from the three raw values. Pure, so the refusal is
    /// tested without mutating the process environment.
    ///
    /// The opt-in wins over present lair credentials: a deployed daemon always
    /// has both rendered into its EnvironmentFile, so an operator reaching for
    /// the escape hatch is asking for it on top of them, not instead of them.
    pub fn resolve(
        lair_url: Option<String>,
        lair_passphrase: Option<String>,
        allow_cap_grant: Option<String>,
    ) -> Result<Self> {
        if opted_in(allow_cap_grant)? {
            return Ok(Self::CapGrant);
        }
        match (lair_url, lair_passphrase) {
            (Some(connection_url), Some(passphrase)) => Ok(Self::Lair {
                connection_url,
                passphrase,
            }),
            (url, passphrase) => Err(refusal(url.is_some(), passphrase.is_some())),
        }
    }

    /// Build the `HamConfig` a connection is about to be made with.
    pub fn apply(&self, cfg: ham::HamConfig) -> Result<ham::HamConfig> {
        match self {
            Self::Lair {
                connection_url,
                passphrase,
            } => cfg
                .with_lair_signing(connection_url, passphrase.clone().into_bytes())
                .with_context(|| format!("{LAIR_URL_VAR} is not a usable lair connection URL")),
            Self::CapGrant => {
                tracing::warn!(
                    event = "signing.cap_grant",
                    opt_in = ALLOW_CAP_GRANT_VAR,
                    "signing WITHOUT lair: this connect commits a capability grant to the \
                     notary's chain"
                );
                Ok(cfg)
            }
        }
    }
}

/// The error a daemon without lair signing dies with. It carries everything an
/// operator needs at 3am: which variable was missing, what the consequence of
/// continuing would have been, where the values come from on a droplet, and the
/// one way to ask for the other path on purpose.
fn refusal(url_present: bool, passphrase_present: bool) -> anyhow::Error {
    anyhow::anyhow!(
        "refusing to connect: lair signing is required and unavailable ({LAIR_URL_VAR} is {}, \
         {LAIR_PASSPHRASE_VAR} is {}).\n\
         Without lair, connecting authorizes a throwaway signing key by committing a capability \
         grant to this notary's chain — a write from a daemon that is supposed to only read, on \
         every restart.\n\
         On a droplet both values sit with the conductor: keystore.connection_url in \
         {CONDUCTOR_CONFIG_PATH}, and the passphrase in {PASSPHRASE_FILE_PATH}. The automation \
         installer (setup-migration-notary.sh) reads them off the node and renders them into \
         this daemon's EnvironmentFile.\n\
         Set {ALLOW_CAP_GRANT_VAR}=1 only to allow that chain write deliberately.",
        set_or_unset(url_present),
        set_or_unset(passphrase_present),
    )
}

fn set_or_unset(present: bool) -> &'static str {
    if present {
        "set"
    } else {
        "unset"
    }
}

/// Parse the opt-in. Only explicit affirmatives enable it, and an unrecognized
/// value is an error rather than a silent "off": the variable is a request for
/// the path that writes to the chain, so it is never guessed at.
fn opted_in(raw: Option<String>) -> Result<bool> {
    let Some(value) = raw else {
        return Ok(false);
    };
    let value = value.trim();
    if ["1", "true", "yes", "on"]
        .iter()
        .any(|on| value.eq_ignore_ascii_case(on))
    {
        return Ok(true);
    }
    if ["", "0", "false", "no", "off"]
        .iter()
        .any(|off| value.eq_ignore_ascii_case(off))
    {
        return Ok(false);
    }
    bail!("{ALLOW_CAP_GRANT_VAR}: expected one of 1/true/yes/on or 0/false/no/off, got `{value}`")
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
        let signing = Signing::resolve(Some(URL.into()), Some("pass".into()), None).unwrap();
        let cfg = signing.apply(ham_cfg()).unwrap();
        assert!(
            cfg.lair.is_some(),
            "lair credentials must configure ham's lair signer (the no-cap-grant path)"
        );
    }

    #[test]
    fn no_lair_and_no_opt_in_refuses() {
        let err = Signing::resolve(None, None, None).unwrap_err().to_string();
        for expected in [LAIR_URL_VAR, LAIR_PASSPHRASE_VAR, ALLOW_CAP_GRANT_VAR] {
            assert!(err.contains(expected), "{err}");
        }
        assert!(err.contains(CONDUCTOR_CONFIG_PATH), "{err}");
        assert!(err.contains(PASSPHRASE_FILE_PATH), "{err}");
    }

    #[test]
    fn half_the_lair_credentials_refuses_too() {
        for (url, passphrase) in [
            (Some(URL.to_string()), None),
            (None, Some("pass".to_string())),
        ] {
            let err = Signing::resolve(url.clone(), passphrase.clone(), None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("refusing to connect"), "{err}");
            let expected = if url.is_some() {
                format!("{LAIR_URL_VAR} is set")
            } else {
                format!("{LAIR_URL_VAR} is unset")
            };
            assert!(err.contains(&expected), "{err}");
        }
    }

    #[test]
    fn the_opt_in_selects_the_cap_grant_path() {
        for raw in ["1", "true", "YES", "On"] {
            let signing = Signing::resolve(None, None, Some(raw.into())).unwrap();
            assert!(matches!(signing, Signing::CapGrant), "{raw}");
            let cfg = signing.apply(ham_cfg()).unwrap();
            assert!(
                cfg.lair.is_none(),
                "{raw}: the opt-in must leave ham on the client-signing (cap grant) path"
            );
        }
    }

    #[test]
    fn the_opt_in_wins_over_present_lair_credentials() {
        let signing =
            Signing::resolve(Some(URL.into()), Some("pass".into()), Some("1".into())).unwrap();
        assert!(signing.apply(ham_cfg()).unwrap().lair.is_none());
    }

    #[test]
    fn an_off_opt_in_still_requires_lair() {
        for raw in ["0", "false", "no", "OFF", ""] {
            assert!(
                Signing::resolve(None, None, Some(raw.into())).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_unrecognized_opt_in_value_is_an_error_not_a_guess() {
        let err = Signing::resolve(None, None, Some("maybe".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains(ALLOW_CAP_GRANT_VAR), "{err}");
        assert!(err.contains("maybe"), "{err}");
    }

    #[test]
    fn a_malformed_lair_url_errors_rather_than_signing_some_other_way() {
        let signing =
            Signing::resolve(Some("not a url".into()), Some("pass".into()), None).unwrap();
        assert!(signing.apply(ham_cfg()).is_err());
    }

    #[test]
    fn the_debug_of_lair_signing_never_renders_the_passphrase() {
        let signing = Signing::resolve(Some(URL.into()), Some("s3cr3t".into()), None).unwrap();
        let rendered = format!("{signing:?}");
        assert!(!rendered.contains("s3cr3t"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }
}
