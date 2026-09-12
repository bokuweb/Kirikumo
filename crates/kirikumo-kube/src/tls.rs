//! TLS for the connections `ureq` does not make.
//!
//! `ureq` builds its own TLS from the kubeconfig for every request, and a
//! WebSocket is not a request: it is an upgrade `ureq` cannot make, opened
//! with `tungstenite` over a socket of ours. So the same three facts a
//! kubeconfig states about a cluster — its roots, the client's certificate,
//! and whether to verify at all — are turned into a `rustls` configuration
//! here, once per connection.
//!
//! The provider is `ring`, named explicitly rather than taken as the process
//! default, because `ureq` uses `ring` and two providers compiled into one
//! binary is exactly the situation `rustls` refuses to guess in.

use crate::error::{Error, Result};
use crate::kubeconfig::ClusterAccess;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::Arc;

/// A `rustls` client configuration for a cluster.
pub fn client_config(
    access: &ClusterAccess,
    client_cert: Option<&(Vec<u8>, Vec<u8>)>,
) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Transport(format!("tls: {error}")))?;

    let builder = if access.insecure {
        // The reader said so in the kubeconfig, and the window says so in
        // its sidebar. This is the same decision `ureq` is handed.
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification { provider }))
    } else {
        let mut roots = RootCertStore::empty();
        if access.roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        for pem in &access.roots {
            for certificate in CertificateDer::pem_slice_iter(pem) {
                let certificate = certificate
                    .map_err(|error| Error::Credentials(format!("a root certificate: {error}")))?;
                roots
                    .add(certificate)
                    .map_err(|error| Error::Credentials(format!("a root certificate: {error}")))?;
            }
        }
        builder.with_root_certificates(roots)
    };

    let config = match client_cert {
        Some((chain, key)) => {
            let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain)
                .collect::<std::result::Result<_, _>>()
                .map_err(|error| Error::Credentials(format!("the client certificate: {error}")))?;
            let key = PrivateKeyDer::from_pem_slice(key)
                .map_err(|error| Error::Credentials(format!("the client key: {error}")))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|error| Error::Credentials(format!("the client certificate: {error}")))?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// A verifier that accepts any certificate.
///
/// What `insecure-skip-tls-verify: true` means, and nothing more: the
/// signatures on the handshake are still checked, so the connection is still
/// to *someone* who holds the key the certificate names — it is only the
/// question of who that is that goes unasked.
#[derive(Debug)]
struct NoVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMethod;

    fn access(insecure: bool, roots: Vec<Vec<u8>>) -> ClusterAccess {
        ClusterAccess {
            server: "https://127.0.0.1:6443".into(),
            roots,
            insecure,
            server_name: None,
            auth: AuthMethod::Anonymous,
            namespace: None,
        }
    }

    #[test]
    fn an_insecure_context_builds_a_config_that_asks_no_questions() {
        assert!(client_config(&access(true, Vec::new()), None).is_ok());
    }

    #[test]
    fn a_context_with_no_roots_trusts_the_public_ones() {
        assert!(client_config(&access(false, Vec::new()), None).is_ok());
    }

    #[test]
    fn a_root_that_is_not_a_certificate_is_refused_with_a_reason() {
        let bad = access(
            false,
            vec![b"-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n".to_vec()],
        );
        assert!(matches!(
            client_config(&bad, None),
            Err(Error::Credentials(_))
        ));
    }

    #[test]
    fn a_client_key_that_is_not_a_key_is_refused_with_a_reason() {
        let cert = (
            b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".to_vec(),
            b"nope".to_vec(),
        );
        assert!(matches!(
            client_config(&access(true, Vec::new()), Some(&cert)),
            Err(Error::Credentials(_))
        ));
    }
}
