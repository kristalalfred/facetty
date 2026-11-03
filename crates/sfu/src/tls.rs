use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// Claude: browsers pin a certificate by hash only if it is valid for at most
/// 14 days, so each one starts a day back, to allow for clock skew, and lasts
/// 13 days.
const BACKDATE: time::Duration = time::Duration::days(1);
const LIFETIME: time::Duration = time::Duration::days(13);
pub const ROTATE_EVERY: Duration = Duration::from_secs(24 * 3600);

/// A self-signed certificate. Clients learn its hash from the signaling
/// server and pin it, so no CA is involved.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub sha256_hex: String,
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let mut params = rcgen::CertificateParams::new(vec!["facetty-sfu".to_string()])?;
        let now = OffsetDateTime::now_utc();
        params.not_before = now - BACKDATE;
        params.not_after = now - BACKDATE + LIFETIME;
        let signing_key = rcgen::KeyPair::generate()?;
        let cert = params.self_signed(&signing_key)?.der().clone();
        let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let sha256_hex = hex::encode(Sha256::digest(&cert));
        Ok(Self {
            cert,
            key,
            sha256_hex,
        })
    }
}

pub fn server_config(identity: &Identity) -> Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(
        vec![identity.cert.clone()],
        PrivateKeyDer::Pkcs8(identity.key.clone_key()),
    )?;
    tls.alpn_protocols = vec![
        facetty_proto::ALPN.to_vec(),
        web_transport_quinn::ALPN.as_bytes().to_vec(),
    ];

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config
        .max_incoming(256)
        .incoming_buffer_size(64 * 1024)
        .incoming_buffer_size_total(4 << 20);
    let transport =
        Arc::get_mut(&mut config.transport).expect("transport config is not shared yet");
    // Claude: a browser's HTTP/3 control and QPACK streams and its
    // WebTransport CONNECT stream count against these stream limits.
    transport
        .max_concurrent_uni_streams(16u32.into())
        .max_concurrent_bidi_streams(4u32.into())
        .receive_window((512u32 * 1024).into())
        .send_window(1 << 20)
        .datagram_receive_buffer_size(Some(16 * 1024))
        .datagram_send_buffer_size(64 * 1024)
        .max_idle_timeout(Some(Duration::from_secs(15).try_into()?))
        .keep_alive_interval(Some(Duration::from_secs(5)));
    Ok(config)
}
