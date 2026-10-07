use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bits_proto::signal::{MediaServer, SfuHeartbeat};
use bits_sfu::Sfu;
use clap::Parser;
use tracing::{info, warn};

/// Media server: forwards ASCII video and Opus audio between call participants.
#[derive(Parser)]
struct Args {
    #[arg(long, env = "BITS_SFU_LISTEN", default_value = "0.0.0.0:8741")]
    listen: SocketAddr,
    /// Address clients should dial, as `host:port`. Defaults to the listen
    /// port on whichever host clients used to reach the signaling server.
    #[arg(long, env = "BITS_SFU_PUBLIC_ADDR")]
    public_addr: Option<String>,
    /// Signaling server to register with.
    #[arg(long, env = "BITS_SIGNAL_URL", default_value = "http://127.0.0.1:8740")]
    signal: String,
    #[arg(long, env = "BITS_SECRET", default_value = bits_proto::token::DEV_SECRET, hide_default_value = true)]
    secret: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    if args.secret == bits_proto::token::DEV_SECRET {
        warn!("BITS_SECRET is not set; using the insecure development secret");
    }

    let sfu = Arc::new(Sfu::bind(args.listen, args.secret.as_bytes())?);
    let public_addr = args
        .public_addr
        .unwrap_or_else(|| format!(":{}", args.listen.port()));
    info!(listen = %sfu.local_addr()?, %public_addr, cert = sfu.cert_sha256(), "media server up");

    tokio::spawn(heartbeat(
        sfu.clone(),
        public_addr,
        args.signal,
        args.secret,
    ));
    sfu.run().await;
    Ok(())
}

async fn heartbeat(sfu: Arc<Sfu>, public_addr: String, signal: String, secret: String) {
    let client = reqwest::Client::new();
    let url = format!("{}/internal/sfu/heartbeat", signal.trim_end_matches('/'));
    let mut registered = false;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let beat = SfuHeartbeat {
            id: public_addr.clone(),
            media: MediaServer {
                addr: public_addr.clone(),
                cert_sha256: sfu.cert_sha256().to_string(),
            },
            connections: sfu.connections(),
        };
        let result = client
            .post(&url)
            .bearer_auth(&secret)
            .json(&beat)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match result {
            Ok(_) if !registered => {
                info!(%url, "registered with signaling server");
                registered = true;
            }
            Ok(_) => {}
            Err(e) => {
                if registered {
                    warn!("heartbeat failed: {e}");
                }
                registered = false;
            }
        }
    }
}
