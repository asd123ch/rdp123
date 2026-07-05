//! TLS transport for RDP's trust-on-first-use certificate model.
//!
//! RDP servers commonly use self-signed certificates, so certificate-chain and
//! hostname validation are deferred to the public-key pin in `session`. The TLS
//! CertificateVerify signature is still mandatory: accepting it without
//! verification would let an attacker replay a pinned public certificate
//! without possessing its private key.

use std::io;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio_rustls::client::TlsStream;
use x509_cert::der::Decode as _;

#[derive(Debug)]
struct TofuVerifier {
    supported: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Identity is checked by the public-key pin before CredSSP sends
        // credentials. Handshake signatures are verified below.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, signature, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, signature, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

pub async fn upgrade<S>(stream: S, server_name: &str) -> io::Result<(TlsStream<S>, Vec<u8>)>
where
    S: Unpin + AsyncRead + AsyncWrite,
{
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let verifier = TofuVerifier {
        supported: provider.signature_verification_algorithms,
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    // CredSSP does not support TLS session resumption.
    config.resumption = rustls::client::Resumption::disabled();

    let server_name = ServerName::try_from(server_name.to_owned()).map_err(io::Error::other)?;
    let mut tls_stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(server_name, stream)
        .await?;
    tls_stream.flush().await?;

    let cert_der = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| io::Error::other("peer certificate is missing"))?;
    let cert = x509_cert::Certificate::from_der(cert_der.as_ref()).map_err(io::Error::other)?;
    let public_key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| io::Error::other("server certificate public key is not byte-aligned"))?
        .to_vec();

    Ok((tls_stream, public_key))
}
