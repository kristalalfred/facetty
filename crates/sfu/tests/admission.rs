use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use facetty_proto::media::{ClientControl, ServerControl};
use facetty_proto::{ParticipantId, quic, token};
use facetty_sfu::Sfu;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::watch;
use tokio::time::timeout;

const SECRET: &[u8] = b"test-secret";
const ROOM: &str = "call";
const WAIT: Duration = Duration::from_secs(5);
const ADMISSION_DEADLINE: Duration = Duration::from_secs(12);

type InCall = Arc<Mutex<HashMap<ParticipantId, watch::Sender<()>>>>;

async fn start() -> (SocketAddr, InCall) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sfu = Arc::new(Sfu::bind("127.0.0.1:0".parse().unwrap(), SECRET).unwrap());
    let addr = sfu.local_addr().unwrap();
    let in_call = InCall::default();
    let roster = in_call.clone();
    tokio::spawn(async move {
        sfu.run(move |room, id| {
            let mut present = roster
                .lock()
                .unwrap()
                .get(&id)
                .filter(|_| room == ROOM)?
                .subscribe();
            Some(async move { while present.changed().await.is_ok() {} })
        })
        .await
    });
    (addr, in_call)
}

async fn connect(addr: SocketAddr, transport: quinn::TransportConfig) -> quinn::Connection {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![facetty_proto::ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);
    timeout(WAIT, endpoint.connect(addr, "facetty-sfu").unwrap())
        .await
        .unwrap()
        .unwrap()
}

async fn send_hello(
    conn: &quinn::Connection,
    participant: ParticipantId,
) -> (quinn::SendStream, quinn::RecvStream) {
    let expires_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let token = token::sign(
        SECRET,
        &token::Claims {
            room: ROOM.into(),
            participant,
            expires_unix,
        },
    );
    let (mut send, recv) = conn.open_bi().await.unwrap();
    quic::write_control(&mut send, &ClientControl::Hello { token })
        .await
        .unwrap();
    (send, recv)
}

struct Connected {
    conn: quinn::Connection,
    control: quinn::SendStream,
    replies: quinn::RecvStream,
    reply: ServerControl,
}

async fn hello(addr: SocketAddr, participant: ParticipantId) -> Connected {
    let conn = connect(addr, quinn::TransportConfig::default()).await;
    let (control, mut replies) = send_hello(&conn, participant).await;
    let reply = timeout(WAIT, quic::read_control(&mut replies))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    Connected {
        conn,
        control,
        replies,
        reply,
    }
}

impl Connected {
    async fn subscribe(&mut self, publisher: ParticipantId, rung: Option<u8>) {
        quic::write_control(
            &mut self.control,
            &ClientControl::Subscribe { publisher, rung },
        )
        .await
        .unwrap();
    }

    async fn until(&mut self, wanted: &ServerControl) {
        timeout(WAIT, async {
            loop {
                let message: ServerControl = quic::read_control(&mut self.replies)
                    .await
                    .unwrap()
                    .unwrap();
                if &message == wanted {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never got {wanted:?}"));
    }
}

#[tokio::test]
async fn a_valid_token_without_a_place_in_the_call_is_refused() {
    let (addr, _in_call) = start().await;
    let connected = hello(addr, 1).await;
    assert_eq!(
        connected.reply,
        ServerControl::Error {
            message: "not in this call".into()
        }
    );
}

#[tokio::test]
async fn leaving_the_call_closes_the_media_connection() {
    let (addr, in_call) = start().await;
    in_call.lock().unwrap().insert(2, watch::Sender::new(()));
    let connected = hello(addr, 2).await;
    assert_eq!(connected.reply, ServerControl::Welcome { participant: 2 });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(connected.conn.close_reason().is_none());

    in_call.lock().unwrap().remove(&2);
    let closed = timeout(WAIT, connected.conn.closed()).await.unwrap();
    let quinn::ConnectionError::ApplicationClosed(close) = closed else {
        panic!("expected the server to close the connection, got {closed}");
    };
    assert_eq!(&close.reason[..], b"left the call");
}

#[tokio::test]
async fn admission_has_one_deadline_even_for_clients_that_stall_it() {
    let (addr, _in_call) = start().await;
    let silent = async {
        let conn = connect(addr, quinn::TransportConfig::default()).await;
        timeout(ADMISSION_DEADLINE, conn.closed()).await
    };
    let starving = async {
        let mut transport = quinn::TransportConfig::default();
        transport.stream_receive_window(1u32.into());
        let conn = connect(addr, transport).await;
        let _streams = send_hello(&conn, 3).await;
        timeout(ADMISSION_DEADLINE, conn.closed()).await
    };
    let (silent, starving) = tokio::join!(silent, starving);
    assert!(
        silent.is_ok(),
        "a client that never says hello stays connected"
    );
    assert!(
        starving.is_ok(),
        "a client that never takes its rejection stays connected"
    );
}

#[tokio::test]
async fn departed_publishers_do_not_use_up_a_viewers_subscriptions() {
    let (addr, in_call) = start().await;
    in_call.lock().unwrap().insert(1, watch::Sender::new(()));
    let mut viewer = hello(addr, 1).await;
    for publisher in 2..=facetty_proto::signal::MAX_CALL_PARTICIPANTS as ParticipantId + 2 {
        in_call
            .lock()
            .unwrap()
            .insert(publisher, watch::Sender::new(()));
        let mut guest = hello(addr, publisher).await;
        viewer.subscribe(publisher, Some(0)).await;
        guest
            .until(&ServerControl::EncodeRungs { rungs: vec![0] })
            .await;
        in_call.lock().unwrap().remove(&publisher);
        timeout(WAIT, guest.conn.closed()).await.unwrap();
    }
}

#[tokio::test]
async fn every_new_viewer_gets_a_full_frame() {
    let (addr, in_call) = start().await;
    for id in 1..=3 {
        in_call.lock().unwrap().insert(id, watch::Sender::new(()));
    }
    let mut publisher = hello(addr, 1).await;
    let mut first = hello(addr, 2).await;
    let mut second = hello(addr, 3).await;
    first.subscribe(1, Some(0)).await;
    publisher.until(&ServerControl::Refresh { rung: 0 }).await;
    first.subscribe(1, None).await;
    second.subscribe(1, Some(0)).await;
    publisher.until(&ServerControl::Refresh { rung: 0 }).await;
}

#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
