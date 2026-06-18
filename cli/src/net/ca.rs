//! Root CA generation/loading + on-the-fly per-host leaf minting + a cache of
//! per-host rustls `ServerConfig`s.
//!
//! Modelled on hudsucker's `RcgenAuthority` (`certificate_authority/rcgen_authority.rs`):
//!   - One root CA, persisted to `~/.shadowdroid/net/ca.{crt,key}`, generated
//!     once and installed into the device trust store by [crate::net::trust].
//!   - Each TLS interception mints a **leaf** cert for the SNI host, signed by
//!     the CA. We reuse the CA key as the leaf key (so rustls's `with_single_cert`
//!     gets a private key matching the leaf's SPKI) — simplest, fine for a MITM.
//!   - The fully-built `ServerConfig` is cached per host so repeat connections to
//!     the same origin don't re-mint.
//!
//! Footguns handled: explicit `aws_lc_rs`
//! `CryptoProvider` (no global install race); **http/1.1-only ALPN** so the
//! inner leg never negotiates h2 (we serve http1); `DnsName` SAN (CN alone is
//! rejected by modern clients) + `serverAuth` EKU.

use anyhow::{anyhow, Context, Result};
use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rcgen::string::Ia5String;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::net::paths;

/// The MITM certificate authority: signs per-host leaves and hands the proxy a
/// ready `ServerConfig` for each origin it intercepts.
pub struct CertAuthority {
    issuer: Issuer<'static, KeyPair>,
    /// The CA key, reused as every leaf's private key (DER/PKCS8).
    leaf_key: PrivateKeyDer<'static>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl CertAuthority {
    /// Load the CA from `~/.shadowdroid/net/ca.{crt,key}`, generating + persisting
    /// it on first use.
    pub fn load_or_generate() -> Result<Arc<CertAuthority>> {
        paths::ensure_net_dir()?;
        let cert_path = paths::ca_cert_path()?;
        let key_path = paths::ca_key_path()?;

        let (cert_pem, key) = if cert_path.exists() && key_path.exists() {
            let key_pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("read {}", key_path.display()))?;
            let cert_pem = std::fs::read_to_string(&cert_path)
                .with_context(|| format!("read {}", cert_path.display()))?;
            let key = KeyPair::from_pem(&key_pem).map_err(|e| anyhow!("parse CA key: {e}"))?;
            (cert_pem, key)
        } else {
            let key = KeyPair::generate().map_err(|e| anyhow!("generate CA key: {e}"))?;
            let cert = ca_params()
                .self_signed(&key)
                .map_err(|e| anyhow!("self-sign CA: {e}"))?;
            let cert_pem = cert.pem();
            std::fs::write(&cert_path, &cert_pem)
                .with_context(|| format!("write {}", cert_path.display()))?;
            write_private(&key_path, &key.serialize_pem())?;
            (cert_pem, key)
        };

        Self::build(&cert_pem, key)
    }

    fn build(cert_pem: &str, key: KeyPair) -> Result<Arc<CertAuthority>> {
        // Reuse the CA keypair as the leaf private key — compute the DER before
        // the keypair is moved into the Issuer.
        let leaf_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let issuer =
            Issuer::from_ca_cert_pem(cert_pem, key).map_err(|e| anyhow!("parse CA cert: {e}"))?;
        Ok(Arc::new(CertAuthority {
            issuer,
            leaf_key,
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
            cache: Mutex::new(HashMap::new()),
        }))
    }

    /// A rustls `ServerConfig` impersonating `host`, minting + caching as needed.
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>> {
        if let Some(cfg) = self.cache.lock().unwrap().get(host).cloned() {
            return Ok(cfg);
        }
        let leaf = self.mint_leaf(host)?;
        let mut cfg = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| anyhow!("rustls protocol versions: {e}"))?
            .with_no_client_auth()
            .with_single_cert(vec![leaf], self.leaf_key.clone_key())
            .map_err(|e| anyhow!("rustls server config for {host}: {e}"))?;
        // http/1.1 ONLY on the inner leg — we serve hyper http1, so advertising
        // h2 here would break any client that selects it.
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let cfg = Arc::new(cfg);
        self.cache
            .lock()
            .unwrap()
            .insert(host.to_string(), cfg.clone());
        Ok(cfg)
    }

    fn mint_leaf(&self, host: &str) -> Result<rustls::pki_types::CertificateDer<'static>> {
        let mut params = CertificateParams::default();
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2035, 1, 1);
        params.serial_number = Some(SerialNumber::from(next_serial()));

        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;

        // SAN is what modern clients actually validate; CN alone is ignored.
        params.subject_alt_names = vec![SanType::DnsName(
            Ia5String::try_from(host).map_err(|e| anyhow!("invalid SNI host {host:?}: {e}"))?,
        )];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        let cert = params
            .signed_by(self.issuer.key(), &self.issuer)
            .map_err(|e| anyhow!("sign leaf for {host}: {e}"))?;
        Ok(cert.der().clone())
    }
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2035, 1, 1);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "ShadowDroid MITM CA");
    dn.push(DnType::OrganizationName, "ShadowDroid");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

/// Unique-enough serial: time-seeded base + a monotonic counter, so concurrent
/// mints within one daemon never collide.
fn next_serial() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    base.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed)).max(1)
}

fn write_private(path: &Path, pem: &str) -> Result<()> {
    std::fs::write(path, pem).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the full crypto path (generate → build issuer → mint leaf →
    /// assemble ServerConfig) without touching the filesystem.
    #[test]
    fn generates_ca_and_mints_leaf() {
        let key = KeyPair::generate().unwrap();
        let cert = ca_params().self_signed(&key).unwrap();
        let ca = CertAuthority::build(&cert.pem(), key).unwrap();

        let cfg1 = ca.server_config("api.example.com").unwrap();
        assert_eq!(cfg1.alpn_protocols, vec![b"http/1.1".to_vec()]);
        // Second call for the same host is served from cache (same Arc).
        let cfg2 = ca.server_config("api.example.com").unwrap();
        assert!(Arc::ptr_eq(&cfg1, &cfg2));
        // A different host mints a distinct config.
        let cfg3 = ca.server_config("other.example.com").unwrap();
        assert!(!Arc::ptr_eq(&cfg1, &cfg3));
    }
}
