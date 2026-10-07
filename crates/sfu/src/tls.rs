use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// A self-signed certificate made at startup. Clients learn its hash from the
/// signaling server and pin it, so no CA is involved.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub sha256_hex: String,
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let certified = rcgen::generate_simple_self_signed(vec!["bits-sfu".to_string()])?;
        let cert = certified.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
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
    tls.alpn_protocols = vec![bits_proto::ALPN.to_vec()];

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    let transport =
        Arc::get_mut(&mut config.transport).expect("transport config is not shared yet");
    transport
        .max_concurrent_uni_streams(1024u32.into())
        .max_idle_timeout(Some(Duration::from_secs(15).try_into()?))
        .keep_alive_interval(Some(Duration::from_secs(5)));
    Ok(config)
}
