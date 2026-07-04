use clap::Parser;
use tracing_subscriber::EnvFilter;
use usenet_streaming_server::{config::AppConfig, run};

#[derive(Parser)]
#[command(name = "usenet-streaming-server", version, about)]
struct Args {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,usenet_streaming_server=debug")),
        )
        .init();

    let args = Args::parse();
    let config = AppConfig::load(&args.config)?;
    run(config).await
}
