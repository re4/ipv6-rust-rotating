mod address_pool;
mod auth;
mod config;
mod http;
mod outbound;
mod server;

use std::{error::Error, io};

use config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("rotating_ipv6_proxy=info")),
        )
        .with_target(false)
        .compact()
        .init();

    let config =
        Config::from_env().map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    server::run(config).await?;
    Ok(())
}
