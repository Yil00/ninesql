//! TLS with rustls only (ADR-0011, I5): `disable`, `require`, `verify-ca`, `verify-full`,
//! `server_name` for tunnel endpoints, client certificates.
//!
//! Trust anchors follow libpq's `sslrootcert` semantics for `TlsConfig::ca_file`:
//! - `verify-ca` requires a CA file and trusts **only** it (a public CA vouches for any host it
//!   signed, so chain-only verification against the platform store would accept any server
//!   with a publicly trusted certificate);
//! - `verify-full` with a CA file trusts only that file; without one, the platform trust store.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use ninesql_driver_api::{DriverError, DriverResult, TlsConfig, TlsMode};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_cert_signed_by_trust_anchor;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};

/// What the session needs to open TLS connections: the main one and the cancel connections,
/// which use the exact same settings (docs/08, L5).
#[derive(Clone)]
pub(crate) struct TlsSetup {
    pub(crate) config: Arc<ClientConfig>,
    pub(crate) server_name: ServerName<'static>,
}

impl fmt::Debug for TlsSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsSetup")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err(context: &str, e: impl fmt::Display) -> DriverError {
    DriverError::Tls(format!("{context}: {e}"))
}

/// Builds the TLS setup for `cfg`, or `None` for [`TlsMode::Disable`]. `host` is the endpoint
/// host, used for verification unless `cfg.server_name` is set (SSH tunnel endpoints).
pub(crate) fn setup(cfg: &TlsConfig, host: &str) -> DriverResult<Option<TlsSetup>> {
    let mode = cfg.mode;
    if mode == TlsMode::Disable {
        return Ok(None);
    }
    let name = cfg.server_name.as_deref().unwrap_or(host);
    let server_name = server_name(name)?;
    let provider = provider();
    let verifier: Arc<dyn ServerCertVerifier> = match mode {
        TlsMode::Require => Arc::new(NoVerification {
            algorithms: provider.signature_verification_algorithms,
        }),
        TlsMode::VerifyCa => {
            if cfg.ca_file.is_none() {
                return Err(DriverError::Tls(
                    "verify-ca needs the CA file of the server's certificate authority (it \
                     trusts only that file); use verify-full to rely on the system trust store"
                        .into(),
                ));
            }
            Arc::new(ChainOnly {
                roots: Arc::new(roots(cfg)?),
                algorithms: provider.signature_verification_algorithms,
            })
        }
        TlsMode::VerifyFull => {
            let roots = Arc::new(roots(cfg)?);
            WebPkiServerVerifier::builder_with_provider(roots, Arc::clone(&provider))
                .build()
                .map_err(|e| tls_err("cannot build the certificate verifier", e))?
        }
        // `TlsMode` is non-exhaustive: an unknown mode fails closed.
        _ => {
            return Err(DriverError::InvalidParams(format!(
                "TLS mode {mode:?} is not supported by the pgwire driver"
            )));
        }
    };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err("TLS configuration", e))?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match &cfg.client_certificate {
        None => builder.with_no_client_auth(),
        Some(cc) => {
            if cc.key_passphrase.is_some() {
                return Err(DriverError::InvalidParams(
                    "encrypted client certificate keys are not supported by the pgwire driver \
                     yet; use an unencrypted key file readable only by you"
                        .into(),
                ));
            }
            let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cc.cert_file)
                .map_err(|e| tls_err("cannot read the client certificate", e))?
                .collect::<Result<_, _>>()
                .map_err(|e| tls_err("cannot read the client certificate", e))?;
            if chain.is_empty() {
                return Err(DriverError::Tls(
                    "the client certificate file holds no certificate".into(),
                ));
            }
            let key = PrivateKeyDer::from_pem_file(&cc.key_file)
                .map_err(|e| tls_err("cannot read the client key (PEM, unencrypted)", e))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| tls_err("invalid client certificate or key", e))?
        }
    };
    Ok(Some(TlsSetup {
        config: Arc::new(config),
        server_name,
    }))
}

fn server_name(name: &str) -> DriverResult<ServerName<'static>> {
    if let Ok(ip) = name.parse::<IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(name.to_owned())
        .map_err(|_| DriverError::InvalidParams(format!("invalid TLS server name `{name}`")))
}

/// Trust anchors: the connection's CA file only when set, the platform trust store otherwise.
fn roots(cfg: &TlsConfig) -> DriverResult<RootCertStore> {
    let mut store = RootCertStore::empty();
    if cfg.ca_file.is_none() {
        let native = rustls_native_certs::load_native_certs();
        if !native.errors.is_empty() {
            tracing::debug!(
                errors = native.errors.len(),
                "some platform CA certificates could not be loaded"
            );
        }
        let (added, ignored) = store.add_parsable_certificates(native.certs);
        tracing::debug!(added, ignored, "platform trust store loaded");
        if store.is_empty() {
            return Err(DriverError::Tls(
                "the system trust store is empty; set the server's CA file".into(),
            ));
        }
    }
    if let Some(path) = &cfg.ca_file {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
            .map_err(|e| tls_err("cannot read the CA file", e))?
            .collect::<Result<_, _>>()
            .map_err(|e| tls_err("cannot read the CA file", e))?;
        if certs.is_empty() {
            return Err(DriverError::Tls(
                "the CA file holds no PEM certificate".into(),
            ));
        }
        for cert in certs {
            store
                .add(cert)
                .map_err(|e| tls_err("invalid certificate in the CA file", e))?;
        }
    }
    Ok(store)
}

/// `verify-ca`: the chain is verified against the CA file; the host name is not.
#[derive(Debug)]
struct ChainOnly {
    roots: Arc<RootCertStore>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Chain only, explicitly (not inferred from the order of rustls' checks).
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// `require`: encrypted, certificate not verified (flagged as a downgrade, I5). Handshake
/// signatures are still checked, so the server proves it holds the key of the certificate it
/// sent.
#[derive(Debug)]
struct NoVerification {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for NoVerification {
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
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_has_no_setup_and_names_parse() {
        let mut cfg = TlsConfig::default();
        cfg.mode = TlsMode::Disable;
        assert!(setup(&cfg, "db.example.test").unwrap().is_none());
        assert!(matches!(
            server_name("127.0.0.1").unwrap(),
            ServerName::IpAddress(_)
        ));
        assert!(matches!(
            server_name("db.example.test").unwrap(),
            ServerName::DnsName(_)
        ));
        assert!(server_name("not a name").is_err());
    }

    /// A CA file of one certificate taken from the platform store (skipped when empty).
    fn one_platform_ca() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
        let cert = rustls_native_certs::load_native_certs()
            .certs
            .into_iter()
            .next()?;
        let b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(cert.as_ref())
        };
        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, pem).unwrap();
        Some((dir, path))
    }

    #[test]
    fn ca_file_replaces_the_platform_store() {
        let Some((_dir, path)) = one_platform_ca() else {
            return;
        };
        let mut cfg = TlsConfig::default();
        cfg.ca_file = Some(path);
        assert_eq!(roots(&cfg).unwrap().len(), 1, "only the CA file is trusted");
        cfg.ca_file = None;
        assert!(
            roots(&cfg).unwrap().len() > 1,
            "platform store without a CA file"
        );
    }

    #[test]
    fn verify_ca_requires_a_ca_file() {
        let mut cfg = TlsConfig::default();
        cfg.mode = TlsMode::VerifyCa;
        assert!(matches!(
            setup(&cfg, "db.example.test"),
            Err(DriverError::Tls(_))
        ));
    }

    #[test]
    fn modes_build_and_server_name_overrides_host() {
        for mode in [TlsMode::Require, TlsMode::VerifyFull] {
            let mut cfg = TlsConfig::default();
            cfg.mode = mode;
            cfg.server_name = Some("db.example.test".into());
            let s = setup(&cfg, "127.0.0.1").unwrap().unwrap();
            assert_eq!(
                s.server_name,
                ServerName::try_from("db.example.test").unwrap()
            );
        }
    }

    #[test]
    fn bad_ca_file_is_a_tls_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, "not a certificate").unwrap();
        let mut cfg = TlsConfig::default();
        cfg.ca_file = Some(path);
        assert!(matches!(
            setup(&cfg, "db.example.test"),
            Err(DriverError::Tls(_))
        ));
    }
}
