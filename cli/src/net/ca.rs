//! Root CA generation/loading + on-the-fly per-host leaf minting + a cache of
//! per-host rustls `ServerConfig`s, plus the `net ca` management verbs
//! (`import`/`info`/`reset`).
//!
//! Modelled on hudsucker's `RcgenAuthority` (`certificate_authority/rcgen_authority.rs`):
//!   - One root CA, persisted to `~/.shadowdroid/net/ca.{crt,key}`, either
//!     generated once or **supplied by the user** via [`import_ca`], and installed
//!     into the device trust store by [crate::net::trust].
//!   - Each TLS interception mints a **leaf** cert for the SNI host, signed by
//!     the CA. We reuse the CA key as the leaf key (so rustls's `with_single_cert`
//!     gets a private key matching the leaf's SPKI) — simplest, fine for a MITM.
//!   - The fully-built `ServerConfig` is cached per host so repeat connections to
//!     the same origin don't re-mint.
//!
//! Everything downstream (daemon, `trust`, `check`, `stop --revoke-ca`) reads the
//! CA from the fixed `ca.{crt,key}` path, so importing a user CA there is all it
//! takes for the whole chain to sign + install *that* CA instead of ours.
//!
//! Footguns handled: explicit `aws_lc_rs`
//! `CryptoProvider` (no global install race); **http/1.1-only ALPN** so the
//! inner leg never negotiates h2 (we serve http1); `DnsName` SAN (CN alone is
//! rejected by modern clients) + `serverAuth` EKU.

use anyhow::{anyhow, bail, Context, Result};
use rcgen::string::Ia5String;
use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData, SanType,
    SerialNumber,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::net::paths;

/// Provenance markers written to `ca.source`.
pub const SOURCE_GENERATED: &str = "generated";
pub const SOURCE_IMPORTED: &str = "imported";

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
        Self::load_or_generate_from(&paths::ensure_net_dir()?)
    }

    /// Dir-scoped core of [`load_or_generate`] (the dir is always
    /// `paths::net_dir()` in production; parameterised for tests).
    fn load_or_generate_from(dir: &Path) -> Result<Arc<CertAuthority>> {
        let cert_path = ca_cert_in(dir);
        let key_path = ca_key_in(dir);

        let (cert_pem, key) = if cert_path.exists() && key_path.exists() {
            let key_pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("read {}", key_path.display()))?;
            let cert_pem = std::fs::read_to_string(&cert_path)
                .with_context(|| format!("read {}", cert_path.display()))?;
            let key = KeyPair::from_pem(&key_pem).map_err(|e| anyhow!("parse CA key: {e}"))?;
            (cert_pem, key)
        } else {
            generate_ca_files(dir)?
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
        // Advertise h2 + http/1.1 on the inner leg: the proxy serves the decrypted
        // connection with hyper's version-negotiating `auto` builder, so an app
        // that speaks HTTP/2 (most modern OkHttp/Cronet stacks) isn't downgraded.
        // Order matters — clients pick the first mutually-supported protocol.
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
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

// The CA lives as three siblings in the `net` dir. These take the dir explicitly
// (rather than reading `$HOME` through [`paths`]) so the management + generation
// logic is exercisable against a scratch directory in tests. In production the
// dir is always `paths::net_dir()`, so `ca_cert_in(net_dir()) == ca_cert_path()`.
fn ca_cert_in(dir: &Path) -> PathBuf {
    dir.join(paths::CA_CERT_FILE)
}
fn ca_key_in(dir: &Path) -> PathBuf {
    dir.join(paths::CA_KEY_FILE)
}
fn ca_source_in(dir: &Path) -> PathBuf {
    dir.join(paths::CA_SOURCE_FILE)
}

/// Generate a fresh ShadowDroid CA and persist cert + key + `generated` marker
/// into `dir`. Returns the material so the caller can build a [`CertAuthority`]
/// without re-reading it.
fn generate_ca_files(dir: &Path) -> Result<(String, KeyPair)> {
    let key = KeyPair::generate().map_err(|e| anyhow!("generate CA key: {e}"))?;
    let cert = ca_params()
        .self_signed(&key)
        .map_err(|e| anyhow!("self-sign CA: {e}"))?;
    let cert_pem = cert.pem();
    let cert_path = ca_cert_in(dir);
    std::fs::write(&cert_path, &cert_pem)
        .with_context(|| format!("write {}", cert_path.display()))?;
    write_private(&ca_key_in(dir), &key.serialize_pem())?;
    // Provenance is best-effort — a missing/stale marker is inferred by `ca info`.
    let _ = std::fs::write(ca_source_in(dir), SOURCE_GENERATED);
    Ok((cert_pem, key))
}

/// Unique-enough serial: time-seeded base + a monotonic counter, so concurrent
/// mints within one daemon never collide.
fn next_serial() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    base.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
        .max(1)
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

// ── `net ca` management (import / info / reset) ───────────────────────────────

/// A read-only description of the CA currently on disk — backs `net ca info` and
/// the confirmation payload of `net ca import` / `reset`.
#[derive(Debug, Clone, Serialize)]
pub struct CaInfo {
    /// `generated` | `imported` | `unknown` (a CA that predates provenance).
    pub source: String,
    pub subject: String,
    pub issuer: String,
    /// basicConstraints CA flag — must be true for the CA to sign leaves.
    pub is_ca: bool,
    pub self_signed: bool,
    pub not_before: String,
    pub not_after: String,
    pub expired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_algorithm: Option<String>,
    /// OpenSSL `subject_hash_old` — the filename Android keys the CA by in its
    /// trust store. `None` when `openssl` isn't on PATH (import still succeeds;
    /// `net trust` needs it to compute the install path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub android_hash: Option<String>,
    pub cert_path: String,
    pub key_path: String,
}

/// Install a **user-provided CA** as the proxy's signing CA.
///
/// `cert_src` is a PEM certificate; `key_src` its PEM private key. When `key_src`
/// is `None` the key is taken from `cert_src` (a combined PEM, e.g. mitmproxy's
/// `mitmproxy-ca.pem`). The cert+key replace `ca.{crt,key}`, so every downstream
/// consumer signs and installs *this* CA. Returns the resulting [`CaInfo`] and a
/// list of non-fatal warnings. The previous CA (if any) is moved aside to
/// `ca.{crt,key}.bak`.
///
/// Validation (all before anything is written): the cert parses and is a CA, the
/// key parses and its public key matches the cert, the cert is not expired, and
/// the full mint-a-leaf path succeeds with the pair.
pub fn import_ca(cert_src: &Path, key_src: Option<&Path>) -> Result<(CaInfo, Vec<String>)> {
    import_into(&paths::ensure_net_dir()?, cert_src, key_src)
}

fn import_into(
    net_dir: &Path,
    cert_src: &Path,
    key_src: Option<&Path>,
) -> Result<(CaInfo, Vec<String>)> {
    let cert_file = std::fs::read_to_string(cert_src)
        .with_context(|| format!("read certificate {}", cert_src.display()))?;
    let cert_blocks: Vec<PemBlock> = pem_blocks(&cert_file)
        .into_iter()
        .filter(|b| b.label == "CERTIFICATE")
        .collect();
    if cert_blocks.is_empty() {
        bail!("no CERTIFICATE block found in {}", cert_src.display());
    }

    // The key comes from --key, or from the cert file itself (combined PEM).
    let (key_text, key_src_display) = match key_src {
        Some(k) => (
            std::fs::read_to_string(k).with_context(|| format!("read key {}", k.display()))?,
            k.display().to_string(),
        ),
        None => (cert_file.clone(), cert_src.display().to_string()),
    };
    let key_block = pem_blocks(&key_text)
        .into_iter()
        .find(|b| b.label.ends_with("PRIVATE KEY"))
        .ok_or_else(|| match key_src {
            Some(_) => anyhow!("no PRIVATE KEY block found in {key_src_display}"),
            None => anyhow!(
                "no PRIVATE KEY block found in {key_src_display} — if the key is in a separate \
                 file, pass it with --key <path>"
            ),
        })?;

    // Normalise the key to PKCS#8 (rcgen/ring reject PKCS#1 / SEC1) and load it.
    let key_pem = normalize_key_pem(&key_block, &key_src_display)?;
    let key = KeyPair::from_pem(&key_pem)
        .map_err(|e| anyhow!("parse private key from {key_src_display}: {e}"))?;
    let key_spki = key.subject_public_key_info();

    // Parse the (first) cert and run the hard checks before touching disk.
    let clean_cert_pem: String = cert_blocks.iter().map(|b| b.text.as_str()).collect();
    let parsed = parse_cert(&cert_blocks[0].text)?;
    if !parsed.is_ca {
        bail!(
            "{} is not a CA certificate (basicConstraints CA:FALSE) — it cannot sign per-host \
             leaves. Provide your root or intermediate CA certificate.",
            cert_src.display()
        );
    }
    if key_spki != parsed.spki {
        bail!(
            "the private key does not match the certificate in {} (public keys differ). Check \
             that --cert and --key are a matching pair.",
            cert_src.display()
        );
    }
    if parsed.expired {
        bail!(
            "the CA certificate in {} expired on {} — devices will reject it. Provide a \
             currently-valid CA.",
            cert_src.display(),
            parsed.not_after
        );
    }

    // Prove the entire signing path works with this pair before persisting it.
    let ca = CertAuthority::build(&clean_cert_pem, key)
        .context("build a MITM CA from the provided cert+key")?;
    ca.server_config("import-selftest.shadowdroid")
        .context("mint a test leaf with the provided CA (the key may not support signing)")?;

    let mut warnings = Vec::new();
    if parsed.not_yet_valid {
        warnings.push(format!(
            "the CA is not valid until {} — devices will reject it until then.",
            parsed.not_before
        ));
    } else if parsed.days_to_expiry.is_some_and(|d| d < 30) {
        warnings.push(format!(
            "the CA expires in {} day(s) ({}).",
            parsed.days_to_expiry.unwrap_or(0),
            parsed.not_after
        ));
    }
    if !parsed.self_signed {
        warnings.push(
            "the provided cert is not self-signed (it looks like an intermediate); the device \
             must also trust its issuing root for leaves to validate."
                .to_string(),
        );
    }
    if cert_blocks.len() > 1 {
        warnings.push(format!(
            "{} certificates were found in {}; only the first is used as the signing CA.",
            cert_blocks.len(),
            cert_src.display()
        ));
    }

    // Commit: back up any existing CA, then write the new cert+key+marker.
    backup_in(net_dir)?;
    std::fs::write(ca_cert_in(net_dir), &clean_cert_pem).context("write ca.crt")?;
    write_private(&ca_key_in(net_dir), &key_pem)?;
    let _ = std::fs::write(ca_source_in(net_dir), SOURCE_IMPORTED);

    Ok((info_in(net_dir)?, warnings))
}

/// Describe the CA currently on disk. Errors if none exists yet.
pub fn ca_info() -> Result<CaInfo> {
    info_in(&paths::net_dir()?)
}

fn info_in(net_dir: &Path) -> Result<CaInfo> {
    let cert_path = ca_cert_in(net_dir);
    let key_path = ca_key_in(net_dir);
    if !cert_path.exists() {
        bail!(
            "no CA on disk yet at {} — one is created on the first `net start`/`net trust`, or \
             import your own with `net ca import --cert <file>`.",
            cert_path.display()
        );
    }
    let cert_pem = std::fs::read_to_string(&cert_path)
        .with_context(|| format!("read {}", cert_path.display()))?;
    let first = pem_blocks(&cert_pem)
        .into_iter()
        .find(|b| b.label == "CERTIFICATE")
        .ok_or_else(|| anyhow!("{} contains no CERTIFICATE block", cert_path.display()))?;
    let parsed = parse_cert(&first.text)?;
    Ok(CaInfo {
        source: read_source_in(net_dir, &parsed.subject),
        subject: parsed.subject,
        issuer: parsed.issuer,
        is_ca: parsed.is_ca,
        self_signed: parsed.self_signed,
        not_before: parsed.not_before,
        not_after: parsed.not_after,
        expired: parsed.expired,
        key_algorithm: parsed.key_algorithm,
        android_hash: crate::net::trust::ca_subject_hash_of(&cert_path).ok(),
        cert_path: cert_path.display().to_string(),
        key_path: key_path.display().to_string(),
    })
}

/// Discard the current CA (backing it up to `.bak`) and generate a fresh
/// ShadowDroid CA — the escape hatch after an import. Returns the new [`CaInfo`].
pub fn reset_ca() -> Result<CaInfo> {
    reset_in(&paths::ensure_net_dir()?)
}

fn reset_in(net_dir: &Path) -> Result<CaInfo> {
    backup_in(net_dir)?;
    // Files are gone, so this regenerates + records `generated` provenance.
    generate_ca_files(net_dir)?;
    info_in(net_dir)
}

/// Move any existing `ca.{crt,key,source}` aside to `<name>.bak`, so replacing
/// the CA never silently destroys a key the user might not have backed up.
fn backup_in(net_dir: &Path) -> Result<()> {
    for p in [
        ca_cert_in(net_dir),
        ca_key_in(net_dir),
        ca_source_in(net_dir),
    ] {
        if p.exists() {
            let _ = std::fs::rename(&p, bak_path(&p));
        }
    }
    Ok(())
}

fn bak_path(p: &Path) -> PathBuf {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.with_file_name(format!("{name}.bak"))
}

/// Read the provenance marker, inferring from our own generated subject when the
/// marker is absent (a CA created before provenance was recorded).
fn read_source_in(net_dir: &Path, subject: &str) -> String {
    if let Ok(s) = std::fs::read_to_string(ca_source_in(net_dir)) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if subject.contains("ShadowDroid MITM CA") {
        SOURCE_GENERATED.to_string()
    } else {
        "unknown".to_string()
    }
}

/// A single PEM block with its label (`CERTIFICATE`, `PRIVATE KEY`, …) and full
/// verbatim text (`-----BEGIN…-----` through `-----END…-----`, newline-terminated).
struct PemBlock {
    label: String,
    text: String,
}

/// Split a PEM string into labelled blocks, preserving each block's exact text so
/// it can be re-serialised losslessly. Tolerates surrounding comments/whitespace
/// and multiple blocks (cert chain + key in one file).
fn pem_blocks(s: &str) -> Vec<PemBlock> {
    let mut out = Vec::new();
    let mut lines = s.lines();
    while let Some(line) = lines.next() {
        let Some(label) = line
            .trim()
            .strip_prefix("-----BEGIN ")
            .and_then(|x| x.strip_suffix("-----"))
        else {
            continue;
        };
        let end = format!("-----END {label}-----");
        let mut text = String::new();
        text.push_str(line.trim());
        text.push('\n');
        let mut closed = false;
        for l in lines.by_ref() {
            text.push_str(l.trim());
            text.push('\n');
            if l.trim() == end {
                closed = true;
                break;
            }
        }
        if closed {
            out.push(PemBlock {
                label: label.to_string(),
                text,
            });
        }
    }
    out
}

/// Return a PKCS#8 PEM for a private-key block. PKCS#8 (`PRIVATE KEY`) passes
/// through; PKCS#1 (`RSA PRIVATE KEY`) / SEC1 (`EC PRIVATE KEY`) — which rcgen's
/// `ring` backend rejects — are converted via `openssl` when available, else the
/// error names the exact conversion command. Encrypted keys are rejected with a
/// decrypt hint.
fn normalize_key_pem(block: &PemBlock, src: &str) -> Result<String> {
    match block.label.as_str() {
        "PRIVATE KEY" => Ok(block.text.clone()),
        "ENCRYPTED PRIVATE KEY" => bail!(
            "the private key in {src} is passphrase-encrypted. Decrypt it first, e.g. \
             `openssl pkcs8 -in {src} -out ca-key.pem` (you'll be prompted for the passphrase), \
             then re-run with `--key ca-key.pem`."
        ),
        "RSA PRIVATE KEY" | "EC PRIVATE KEY" => openssl_to_pkcs8(&block.text).with_context(|| {
            format!(
                "the key in {src} is in a legacy ({}) format; converting it to PKCS#8 failed",
                block.label
            )
        }),
        other => bail!("unsupported private-key PEM label {other:?} in {src}"),
    }
}

/// Convert a legacy PKCS#1/SEC1 key PEM to PKCS#8 by piping it through
/// `openssl pkcs8 -topk8 -nocrypt`. Fails with an actionable message if openssl
/// isn't installed.
fn openssl_to_pkcs8(pem: &str) -> Result<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("openssl")
        .args(["pkcs8", "-topk8", "-nocrypt"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            anyhow!(
                "openssl is needed to convert this legacy key to PKCS#8 but could not be run \
                 ({e}). Convert it manually: `openssl pkcs8 -topk8 -nocrypt -in <key> -out \
                 ca-key.pem`, then re-run with `--key ca-key.pem`."
            )
        })?;
    child
        .stdin
        .take()
        .context("openssl stdin")?
        .write_all(pem.as_bytes())
        .context("write key to openssl")?;
    let out = child.wait_with_output().context("run openssl pkcs8")?;
    if !out.status.success() {
        bail!(
            "openssl could not convert the key: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Fields lifted from a parsed X.509 cert for validation + `ca info`.
struct ParsedCert {
    subject: String,
    issuer: String,
    is_ca: bool,
    self_signed: bool,
    not_before: String,
    not_after: String,
    expired: bool,
    not_yet_valid: bool,
    days_to_expiry: Option<i64>,
    /// The DER SubjectPublicKeyInfo — compared against the key's SPKI to prove
    /// the cert and key are a pair.
    spki: Vec<u8>,
    key_algorithm: Option<String>,
}

fn parse_cert(cert_pem_block: &str) -> Result<ParsedCert> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem_block.as_bytes())
        .map_err(|e| anyhow!("parse certificate PEM: {e}"))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| anyhow!("parse X.509 certificate: {e}"))?;

    let subject = cert.subject().to_string();
    let issuer = cert.issuer().to_string();
    let is_ca = cert
        .basic_constraints()
        .ok()
        .flatten()
        .map(|bc| bc.value.ca)
        .unwrap_or(false);
    let spki = cert.public_key().raw.to_vec();
    let key_algorithm = spki_algorithm_label(&cert.public_key().algorithm.algorithm.to_id_string());

    let v = cert.validity();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let not_after_ts = v.not_after.timestamp();
    let not_before_ts = v.not_before.timestamp();

    Ok(ParsedCert {
        self_signed: subject == issuer,
        subject,
        issuer,
        is_ca,
        not_before: v.not_before.to_string(),
        not_after: v.not_after.to_string(),
        expired: not_after_ts < now,
        not_yet_valid: not_before_ts > now,
        days_to_expiry: Some((not_after_ts - now) / 86_400),
        spki,
        key_algorithm,
    })
}

/// Map a public-key OID (dotted string) to a friendly algorithm name.
fn spki_algorithm_label(oid: &str) -> Option<String> {
    Some(
        match oid {
            "1.2.840.113549.1.1.1" => "RSA",
            "1.2.840.10045.2.1" => "EC",
            "1.3.101.112" => "Ed25519",
            "1.3.101.113" => "Ed448",
            other => other,
        }
        .to_string(),
    )
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
        assert_eq!(
            cfg1.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        // Second call for the same host is served from cache (same Arc).
        let cfg2 = ca.server_config("api.example.com").unwrap();
        assert!(Arc::ptr_eq(&cfg1, &cfg2));
        // A different host mints a distinct config.
        let cfg3 = ca.server_config("other.example.com").unwrap();
        assert!(!Arc::ptr_eq(&cfg1, &cfg3));
    }

    // ── `net ca` management ───────────────────────────────────────────────────

    /// A self-signed CA (cert PEM, PKCS#8 key PEM) minted via rcgen — no openssl,
    /// no filesystem, so these tests are hermetic. `KeyPair::generate` yields an
    /// ECDSA P-256 PKCS#8 key, which `normalize_key_pem` passes straight through.
    fn gen_ca_pem() -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let cert = ca_params().self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn pem_blocks_splits_labels_and_preserves_multiple() {
        let (cert, key) = gen_ca_pem();
        let combined = format!("# a comment\n{cert}\njunk between\n{key}\n");
        let blocks = pem_blocks(&combined);
        let labels: Vec<_> = blocks.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["CERTIFICATE", "PRIVATE KEY"]);
        // Round-trips: each block's captured text re-parses to itself.
        assert_eq!(pem_blocks(&blocks[0].text).len(), 1);
        // An unterminated block is ignored, not half-captured.
        assert!(pem_blocks("-----BEGIN CERTIFICATE-----\nabc\n").is_empty());
    }

    #[test]
    fn parse_cert_reads_ca_fields_and_flags_non_ca() {
        let (cert_pem, key_pem) = gen_ca_pem();
        let parsed = parse_cert(&cert_pem).unwrap();
        assert!(parsed.is_ca);
        assert!(parsed.self_signed);
        assert!(!parsed.expired);
        assert_eq!(parsed.key_algorithm.as_deref(), Some("EC"));
        // SPKI extracted from the cert equals the key's own SPKI.
        let key = KeyPair::from_pem(&key_pem).unwrap();
        assert_eq!(parsed.spki, key.subject_public_key_info());

        // A non-CA leaf is flagged.
        let leaf_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::ExplicitNoCa;
        params
            .distinguished_name
            .push(DnType::CommonName, "leaf.example");
        let leaf = params.self_signed(&leaf_key).unwrap();
        assert!(!parse_cert(&leaf.pem()).unwrap().is_ca);
    }

    #[test]
    fn spki_algorithm_labels_known_and_unknown() {
        assert_eq!(
            spki_algorithm_label("1.2.840.113549.1.1.1").as_deref(),
            Some("RSA")
        );
        assert_eq!(
            spki_algorithm_label("1.2.840.10045.2.1").as_deref(),
            Some("EC")
        );
        assert_eq!(
            spki_algorithm_label("1.3.101.112").as_deref(),
            Some("Ed25519")
        );
        assert_eq!(spki_algorithm_label("1.2.3.4").as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn bak_path_appends_suffix() {
        assert_eq!(
            bak_path(Path::new("/net/ca.crt")),
            Path::new("/net/ca.crt.bak")
        );
    }

    /// The live path and the dir-scoped helper must agree on the CA filename, or
    /// a generated CA and `net ca info` would look at different files.
    #[test]
    fn dir_helpers_match_live_paths() {
        let dir = paths::net_dir().unwrap();
        assert_eq!(ca_cert_in(&dir), paths::ca_cert_path().unwrap());
    }

    #[test]
    fn read_source_prefers_marker_then_infers() {
        let dir = tempfile::tempdir().unwrap();
        // No marker: inferred from our generated subject vs. anything else.
        assert_eq!(
            read_source_in(dir.path(), "CN=ShadowDroid MITM CA"),
            SOURCE_GENERATED
        );
        assert_eq!(read_source_in(dir.path(), "CN=My Corp Root"), "unknown");
        // Marker wins when present.
        write(dir.path(), paths::CA_SOURCE_FILE, "imported\n");
        assert_eq!(
            read_source_in(dir.path(), "CN=ShadowDroid MITM CA"),
            SOURCE_IMPORTED
        );
    }

    #[test]
    fn generate_then_info_reports_generated() {
        let dir = tempfile::tempdir().unwrap();
        generate_ca_files(dir.path()).unwrap();
        let info = info_in(dir.path()).unwrap();
        assert_eq!(info.source, SOURCE_GENERATED);
        assert!(info.subject.contains("ShadowDroid MITM CA"));
        assert!(info.is_ca && info.self_signed && !info.expired);
    }

    #[test]
    fn info_errors_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(info_in(dir.path()).is_err());
    }

    #[test]
    fn import_replaces_generated_and_backs_it_up() {
        let store = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        // Start from a generated CA so import has something to back up.
        generate_ca_files(store.path()).unwrap();
        let (cert, key) = gen_ca_pem();
        let cert_p = write(src.path(), "corp.crt", &cert);
        let key_p = write(src.path(), "corp.key", &key);

        let (info, warnings) = import_into(store.path(), &cert_p, Some(&key_p)).unwrap();
        assert_eq!(info.source, SOURCE_IMPORTED);
        assert!(info.is_ca);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        // The previous CA was preserved, not clobbered.
        assert!(bak_path(&ca_cert_in(store.path())).exists());
        assert!(bak_path(&ca_key_in(store.path())).exists());
        // The imported cert is now the live one, and it can build + mint.
        let live = std::fs::read_to_string(ca_cert_in(store.path())).unwrap();
        assert_eq!(live, cert);
        let ca = CertAuthority::load_or_generate_from(store.path()).unwrap();
        ca.server_config("post-import.example").unwrap();
    }

    #[test]
    fn import_accepts_combined_pem_without_key_flag() {
        let store = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let (cert, key) = gen_ca_pem();
        let combined = write(src.path(), "mitmproxy-ca.pem", &format!("{cert}{key}"));
        let (info, _) = import_into(store.path(), &combined, None).unwrap();
        assert_eq!(info.source, SOURCE_IMPORTED);
    }

    #[test]
    fn import_rejects_non_ca_cert() {
        let store = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::ExplicitNoCa;
        params.distinguished_name.push(DnType::CommonName, "leaf");
        let cert = params.self_signed(&key).unwrap();
        let cert_p = write(src.path(), "leaf.crt", &cert.pem());
        let key_p = write(src.path(), "leaf.key", &key.serialize_pem());
        let err = import_into(store.path(), &cert_p, Some(&key_p))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a CA certificate"), "{err}");
    }

    #[test]
    fn import_rejects_mismatched_key() {
        let store = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let (cert, _key_a) = gen_ca_pem();
        let (_cert_b, key_b) = gen_ca_pem();
        let cert_p = write(src.path(), "a.crt", &cert);
        let key_p = write(src.path(), "b.key", &key_b);
        let err = import_into(store.path(), &cert_p, Some(&key_p))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match the certificate"), "{err}");
    }

    #[test]
    fn import_rejects_expired_ca() {
        let store = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = ca_params();
        params.not_before = date_time_ymd(2000, 1, 1);
        params.not_after = date_time_ymd(2001, 1, 1);
        let cert = params.self_signed(&key).unwrap();
        let cert_p = write(src.path(), "old.crt", &cert.pem());
        let key_p = write(src.path(), "old.key", &key.serialize_pem());
        let err = import_into(store.path(), &cert_p, Some(&key_p))
            .unwrap_err()
            .to_string();
        assert!(err.contains("expired"), "{err}");
        // A rejected import must not touch the store.
        assert!(!ca_cert_in(store.path()).exists());
    }

    #[test]
    fn reset_restores_a_generated_ca() {
        let store = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let (cert, key) = gen_ca_pem();
        let cert_p = write(src.path(), "c.crt", &cert);
        let key_p = write(src.path(), "c.key", &key);
        import_into(store.path(), &cert_p, Some(&key_p)).unwrap();
        assert_eq!(info_in(store.path()).unwrap().source, SOURCE_IMPORTED);

        let info = reset_in(store.path()).unwrap();
        assert_eq!(info.source, SOURCE_GENERATED);
        assert!(info.subject.contains("ShadowDroid MITM CA"));
        // The imported CA was backed up on the way out.
        assert!(bak_path(&ca_cert_in(store.path())).exists());
    }

    #[test]
    fn normalize_key_rejects_encrypted_and_unknown() {
        let enc = PemBlock {
            label: "ENCRYPTED PRIVATE KEY".into(),
            text: String::new(),
        };
        let err = normalize_key_pem(&enc, "k.pem").unwrap_err().to_string();
        assert!(err.contains("passphrase-encrypted"), "{err}");

        let weird = PemBlock {
            label: "DSA PRIVATE KEY".into(),
            text: String::new(),
        };
        assert!(normalize_key_pem(&weird, "k.pem").is_err());
    }

    /// PKCS#1/SEC1 keys (openssl's legacy defaults) must be converted to PKCS#8;
    /// gated on openssl since CI images vary.
    #[test]
    fn normalize_key_converts_legacy_via_openssl() {
        if std::process::Command::new("openssl")
            .arg("version")
            .output()
            .is_err()
        {
            return;
        }
        // Produce a SEC1 EC key ("BEGIN EC PRIVATE KEY") and confirm it converts.
        let sec1 = std::process::Command::new("openssl")
            .args(["ecparam", "-name", "prime256v1", "-genkey", "-noout"])
            .output()
            .unwrap();
        let sec1 = String::from_utf8(sec1.stdout).unwrap();
        let block = pem_blocks(&sec1).pop().expect("one EC key block");
        assert_eq!(block.label, "EC PRIVATE KEY");
        let pkcs8 = normalize_key_pem(&block, "k.pem").unwrap();
        assert!(pkcs8.contains("BEGIN PRIVATE KEY"));
        // And rcgen can now load it.
        KeyPair::from_pem(&pkcs8).unwrap();
    }
}
