use anyhow::Result;
use migration_notary::{config::Config, serve};

fn version_requested() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
}

#[tokio::main]
async fn main() -> Result<()> {
    // Ahead of the logging setup, so the output is one line rather than JSON
    // records plus a version. The `<name> <version>` shape matches clap's, so
    // one check in the release workflow covers both of this repo's binaries.
    if version_requested() {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env()?;
    serve(cfg).await
}
