//! Direct QUIC sender to a leader's TPU.
//!
//! A hand-rolled client rather than `solana-tpu-client-next`: the experiment needs to know the
//! exact order and time at which each probe left this machine, and a pooling client is free to
//! batch, retry and reorder. Here one probe is one unidirectional stream, written in the order
//! given, and the timestamp is taken when the stream is finished.
//!
//! Protocol details match what validators expect: ALPN `solana-tpu`, a self-signed Ed25519
//! client certificate (an unstaked identity), server certificate not verified (validators use
//! self-signed certificates too), server name `connect`.

use anyhow::{anyhow, Context, Result};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Connection, Endpoint, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const ALPN: &[u8] = b"solana-tpu";

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
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
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub struct TpuSender {
    endpoint: Endpoint,
}

impl TpuSender {
    pub fn new() -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).context("client key")?;
        let mut params = rcgen::CertificateParams::new(vec!["solana".to_string()]).context("cert params")?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let cert = params.self_signed(&key).context("self-signed client cert")?;
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));

        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| anyhow!("tls versions: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification(provider)))
            .with_client_auth_cert(vec![cert_der], key_der)
            .map_err(|e| anyhow!("client auth cert: {e}"))?;
        tls.alpn_protocols = vec![ALPN.to_vec()];

        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
        transport.keep_alive_interval(Some(Duration::from_secs(1)));
        let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
        cfg.transport_config(Arc::new(transport));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(cfg);
        Ok(Self { endpoint })
    }

    pub async fn connect(&self, addr: &str) -> Result<Connection> {
        let sock: SocketAddr = addr.parse().with_context(|| format!("TPU address {addr}"))?;
        let connecting = self.endpoint.connect(sock, "connect")?;
        let conn = tokio::time::timeout(Duration::from_secs(3), connecting)
            .await
            .map_err(|_| anyhow!("QUIC handshake to {addr} timed out"))??;
        Ok(conn)
    }

    /// Write one transaction on its own stream; returns once the stream is finished locally.
    pub async fn send(conn: &Connection, wire: &[u8]) -> Result<()> {
        let mut stream = conn.open_uni().await?;
        stream.write_all(wire).await?;
        stream.finish()?;
        Ok(())
    }
}
