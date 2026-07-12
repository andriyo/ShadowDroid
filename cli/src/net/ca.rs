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

use anyhow::{Context, Result, anyhow, bail};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PublicKeyData, SanType, SerialNumber, date_time_ymd,
};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ShadowDroidConfig;
use crate::ids::Serial;
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
    fingerprint: String,
    /// Shared lease held for the daemon lifetime. CA import/reset takes an
    /// exclusive non-blocking lease, so disk bytes cannot diverge from the
    /// signer while any daemon is using this managed CA.
    _ca_lease: Option<CaFileLock>,
}

impl CertAuthority {
    /// Load a CA from an explicit cert+key PEM pair, with **no** generation on
    /// miss. The detached daemon uses this: it is handed the already-resolved
    /// paths (via `--ca-cert`/`--ca-key`) and must never mint a new CA, since it
    /// runs without config context and can't know *which* CA the project wants.
    pub fn load_from_files(cert_path: &Path, key_path: &Path) -> Result<Arc<CertAuthority>> {
        let lock = managed_pair_dir(cert_path, key_path)
            .map(|dir| acquire_ca_lock(dir, false))
            .transpose()?;
        let key_pem = std::fs::read_to_string(key_path)
            .with_context(|| format!("read {}", key_path.display()))?;
        let cert_pem = std::fs::read_to_string(cert_path)
            .with_context(|| format!("read {}", cert_path.display()))?;
        let key = KeyPair::from_pem(&key_pem).map_err(|e| anyhow!("parse CA key: {e}"))?;
        Self::build_with_lease(&cert_pem, key, lock)
    }

    fn build(cert_pem: &str, key: KeyPair) -> Result<Arc<CertAuthority>> {
        Self::build_with_lease(cert_pem, key, None)
    }

    fn build_with_lease(
        cert_pem: &str,
        key: KeyPair,
        lease: Option<CaFileLock>,
    ) -> Result<Arc<CertAuthority>> {
        // Reuse the CA keypair as the leaf private key — compute the DER before
        // the keypair is moved into the Issuer.
        let leaf_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let issuer =
            Issuer::from_ca_cert_pem(cert_pem, key).map_err(|e| anyhow!("parse CA cert: {e}"))?;
        let fingerprint = fingerprint_bytes(cert_pem.as_bytes())?;
        Ok(Arc::new(CertAuthority {
            issuer,
            leaf_key,
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
            cache: Mutex::new(HashMap::new()),
            fingerprint,
            _ca_lease: lease,
        }))
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
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
        // Literal origins must use iPAddress SAN bytes, not a DNSName containing
        // textual digits/colons (which strict TLS clients reject).
        params.subject_alt_names = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => vec![SanType::IpAddress(ip)],
            Err(_) => vec![SanType::DnsName(
                Ia5String::try_from(host).map_err(|e| anyhow!("invalid SNI host {host:?}: {e}"))?,
            )],
        };
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

#[derive(Debug)]
struct CaFileLock {
    _file: File,
}

#[derive(Debug)]
pub struct CaReadLease {
    _lock: Option<CaFileLock>,
}

pub fn read_lease(ca: &CaPaths) -> Result<CaReadLease> {
    let lock = ca
        .dir
        .as_deref()
        .map(|dir| acquire_ca_lock(dir, false))
        .transpose()?;
    Ok(CaReadLease { _lock: lock })
}

fn open_ca_lock(dir: &Path) -> Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // The adjacent inode is the only cross-environment invariant for a given
    // CA directory. Project setup ignores `.ca.lock`, so it remains persistent
    // (unlinking a held lock is unsafe) without dirtying git status.
    let path = dir.join(".ca.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok((file, path))
}

fn acquire_ca_lock(dir: &Path, exclusive: bool) -> Result<CaFileLock> {
    let (file, path) = open_ca_lock(dir)?;
    if exclusive {
        file.lock()
    } else {
        file.lock_shared()
    }
    .with_context(|| format!("lock {}", path.display()))?;
    Ok(CaFileLock { _file: file })
}

fn try_acquire_ca_write_lock(dir: &Path) -> Result<CaFileLock> {
    let (file, path) = open_ca_lock(dir)?;
    match file.try_lock() {
        Ok(()) => Ok(CaFileLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(crate::diagnostic::DiagnosticError::new(
            "ca_in_use",
            "net",
            "the certificate authority is in use by a running network daemon",
        )
        .retryable(true)
        .detail(serde_json::json!({
            "ca_dir": dir.display().to_string(),
            "lock": path.display().to_string(),
        }))
        .next_actions([
            "stop every ShadowDroid net session using this project/global CA, then retry",
            "run `shadowdroid net status --json` for each connected device",
        ])
        .into()),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("lock {}", path.display()))
        }
    }
}

fn managed_pair_dir<'a>(cert_path: &'a Path, key_path: &Path) -> Option<&'a Path> {
    let dir = cert_path.parent()?;
    (cert_path.file_name()?.to_str()? == paths::CA_CERT_FILE
        && key_path.parent() == Some(dir)
        && key_path.file_name()?.to_str()? == paths::CA_KEY_FILE)
        .then_some(dir)
}

/// Generate a fresh ShadowDroid CA and persist cert + key + `generated` marker
/// into `dir`. Returns the material so the caller can build a [`CertAuthority`]
/// without re-reading it.
fn generate_ca_files_locked(dir: &Path) -> Result<(String, KeyPair)> {
    let key = KeyPair::generate().map_err(|e| anyhow!("generate CA key: {e}"))?;
    let cert = ca_params()
        .self_signed(&key)
        .map_err(|e| anyhow!("self-sign CA: {e}"))?;
    let cert_pem = cert.pem();
    replace_ca_files_locked(dir, &cert_pem, &key.serialize_pem(), SOURCE_GENERATED)?;
    Ok((cert_pem, key))
}

#[cfg(test)]
fn generate_ca_files(dir: &Path) -> Result<(String, KeyPair)> {
    let _lock = acquire_ca_lock(dir, true)?;
    generate_ca_files_locked(dir)
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

/// Stage one complete CA file in its destination directory. Private files get
/// their final mode before any key bytes are written, so a crash cannot leave a
/// briefly world-readable key behind. `NamedTempFile` also makes the final
/// persist an atomic same-directory rename.
fn stage_ca_file(dir: &Path, contents: &[u8], private: bool) -> Result<tempfile::NamedTempFile> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create temporary CA file in {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if private { 0o600 } else { 0o644 };
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("set temporary CA file mode to {mode:o}"))?;
    }
    #[cfg(not(unix))]
    let _ = private;
    temp.write_all(contents)
        .context("write temporary CA file")?;
    temp.as_file()
        .sync_all()
        .context("sync temporary CA file")?;
    Ok(temp)
}

/// Replace the live cert, private key, and provenance marker as one recoverable
/// transaction. Every new file is fully written and synced before the existing
/// set is moved to `.bak`. If any persist then fails, the previous set is
/// restored instead of leaving a mixed cert/key pair behind.
fn replace_ca_files_locked(dir: &Path, cert_pem: &str, key_pem: &str, source: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let cert_temp = stage_ca_file(dir, cert_pem.as_bytes(), false)?;
    let key_temp = stage_ca_file(dir, key_pem.as_bytes(), true)?;
    let source_temp = stage_ca_file(dir, source.as_bytes(), false)?;

    let cert_path = ca_cert_in(dir);
    let key_path = ca_key_in(dir);
    let source_path = ca_source_in(dir);
    backup_in(dir)?;

    let install = || -> Result<()> {
        cert_temp
            .persist(&cert_path)
            .map_err(|e| e.error)
            .with_context(|| format!("atomically install {}", cert_path.display()))?;
        key_temp
            .persist(&key_path)
            .map_err(|e| e.error)
            .with_context(|| format!("atomically install {}", key_path.display()))?;
        source_temp
            .persist(&source_path)
            .map_err(|e| e.error)
            .with_context(|| format!("atomically install {}", source_path.display()))?;
        sync_dir(dir)?;
        Ok(())
    };

    if let Err(error) = install() {
        let rollback = restore_backups(dir);
        return match rollback {
            Ok(()) => Err(error.context("install CA files; restored the previous CA")),
            Err(rollback_error) => Err(error.context(format!(
                "install CA files; restoring the previous CA also failed: {rollback_error:#}"
            ))),
        };
    }
    Ok(())
}

// ── CA source resolution (which CA does `net` sign + install?) ────────────────

/// The resolved location of the CA a `net` invocation should use, plus enough
/// provenance to report it and to decide whether it may be generated.
#[derive(Debug, Clone)]
pub struct CaPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// The directory the CA lives in, when ShadowDroid manages it (the global
    /// `net` dir or a project `.shadowdroid/`). `None` for an explicit config
    /// path pair, which ShadowDroid never generates into.
    pub dir: Option<PathBuf>,
    /// `explicit` (config path) | `project` (convention file) | `global`.
    pub origin: &'static str,
    /// Whether [`ensure_ca`] may mint the CA here when missing. True only for the
    /// global dir; an explicit pair must already exist, and a project CA is born
    /// only via `net ca reset/import --project`.
    pub generatable: bool,
}

/// Resolve which CA the proxy signs with and which CA `net trust` installs.
/// Order: (1) explicit `proxy.ca_cert`+`proxy.ca_key` from config, (2) a
/// per-project convention CA at `<project>/.shadowdroid/ca.{crt,key}` when both
/// files exist, (3) the global `~/.shadowdroid/net/ca.{crt,key}` (generated on
/// first use — today's behavior). `serial` is accepted for a future per-device
/// override but is not consulted yet.
pub fn resolve_ca(config: &ShadowDroidConfig, _serial: Option<&Serial>) -> Result<CaPaths> {
    let proxy = config.proxy.as_ref();
    match (
        proxy.and_then(|p| p.ca_cert.as_deref()),
        proxy.and_then(|p| p.ca_key.as_deref()),
    ) {
        (Some(cert), Some(key)) => {
            let cert = expand_required_path("proxy.ca_cert", cert)?;
            let key = expand_required_path("proxy.ca_key", key)?;
            if !cert.is_file() {
                bail!("proxy.ca_cert does not exist: {}", cert.display());
            }
            if !key.is_file() {
                bail!("proxy.ca_key does not exist: {}", key.display());
            }
            return Ok(CaPaths {
                cert,
                key,
                dir: None,
                origin: "explicit",
                generatable: false,
            });
        }
        (Some(_), None) => {
            bail!("proxy.ca_cert is set but proxy.ca_key is not — both are required")
        }
        (None, Some(_)) => {
            bail!("proxy.ca_key is set but proxy.ca_cert is not — both are required")
        }
        (None, None) => {}
    }

    if let Ok(dir) = crate::config::project_shadowdroid_dir()
        && let Some(project_ca) = resolve_project_ca(&dir)?
    {
        return Ok(project_ca);
    }

    let dir = paths::net_dir()?;
    Ok(CaPaths {
        cert: paths::ca_cert_path()?,
        key: paths::ca_key_path()?,
        dir: Some(dir),
        origin: "global",
        generatable: true,
    })
}

/// Resolve the conventional project CA without consulting process-global cwd.
/// Keeping this separate makes the incomplete-pair behavior deterministic and
/// testable: once either CA path exists, silently falling back to the global CA
/// would make the proxy sign with a different identity than the project owner
/// intended.
fn resolve_project_ca(dir: &Path) -> Result<Option<CaPaths>> {
    if !dir.is_dir() {
        return Ok(None);
    }

    let cert = ca_cert_in(dir);
    let key = ca_key_in(dir);
    let lock_path = dir.join(".ca.lock");
    // A config-only `.shadowdroid/` must not gain a lock artifact. Once CA
    // material (or its persistent lock from a transaction) exists, take a
    // shared lease and recheck the pair atomically.
    if !path_entry_exists(&cert)? && !path_entry_exists(&key)? && !lock_path.exists() {
        return Ok(None);
    }

    let _lock = acquire_ca_lock(dir, false)?;
    let cert_present = path_entry_exists(&cert)?;
    let key_present = path_entry_exists(&key)?;
    match (cert_present, key_present) {
        (false, false) => Ok(None),
        (true, true) if cert.is_file() && key.is_file() => Ok(Some(CaPaths {
            cert,
            key,
            dir: Some(dir.to_path_buf()),
            origin: "project",
            generatable: false,
        })),
        _ => bail!(
            "project CA is incomplete: expected regular files {} and {}, but found cert={} and \
             key={}. Restore the missing file, remove the stray CA material to use the global \
             CA, or run `shadowdroid net ca reset --project` / `shadowdroid net ca import \
             --project`.",
            cert.display(),
            key.display(),
            if cert.is_file() {
                "present"
            } else {
                "missing or not a regular file"
            },
            if key.is_file() {
                "present"
            } else {
                "missing or not a regular file"
            },
        ),
    }
}

/// `Path::exists` treats dangling symlinks as absent. For CA resolution they
/// are still project CA material and must produce an actionable error rather
/// than an unrelated fallback to the global CA.
fn path_entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

/// Ensure the resolved CA exists on disk, generating a fresh ShadowDroid CA when
/// permitted (the global dir). An explicit or project CA that is missing is a
/// hard error with an actionable message — we never silently fabricate a CA a
/// user pointed us at.
pub fn ensure_ca(ca: &CaPaths) -> Result<()> {
    if !ca.generatable {
        let _lock = ca
            .dir
            .as_deref()
            .map(|dir| acquire_ca_lock(dir, false))
            .transpose()?;
        if ca.cert.is_file() && ca.key.is_file() {
            return Ok(());
        }
        bail!(
            "the {} CA is missing: {} / {}. Import one with `net ca import` or generate a \
             project CA with `net ca reset --project`.",
            ca.origin,
            ca.cert.display(),
            ca.key.display()
        );
    }
    let dir = ca
        .dir
        .as_deref()
        .ok_or_else(|| anyhow!("a generatable CA must have a directory"))?;
    {
        let _read_lock = acquire_ca_lock(dir, false)?;
        if ca.cert.is_file() && ca.key.is_file() {
            return Ok(());
        }
    }
    let _lock = acquire_ca_lock(dir, true)?;
    if ca.cert.is_file() && ca.key.is_file() {
        return Ok(());
    }
    generate_ca_files_locked(dir)?;
    Ok(())
}

/// SHA-256 (hex) of the CA certificate in `cert_path`, normalised to DER so
/// cosmetically-different encodings of the same certificate hash identically.
/// Stable identity used to detect a changed CA on daemon reuse and to key the
/// per-serial verify-once trust cache.
pub fn fingerprint_of(cert_path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(cert_path).with_context(|| format!("read {}", cert_path.display()))?;
    fingerprint_bytes(&bytes)
}

fn fingerprint_bytes(bytes: &[u8]) -> Result<String> {
    let der = crate::net::trust::certificate_der(bytes)?;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&der);
    Ok(crate::release::hex_lower(&hasher.finalize()))
}

/// Expand a config path (`~/` allowed) and require it be absolute — a bare
/// relative path can't be resolved against the file that set it once configs are
/// merged, so it is rejected here and by `config validate`.
fn expand_required_path(field: &str, raw: &str) -> Result<PathBuf> {
    let expanded = crate::config::expand_config_path(&Some(raw.to_string()))
        .ok_or_else(|| anyhow!("{field} is empty"))?;
    if !expanded.is_absolute() {
        bail!("{field} must be an absolute path or start with `~/` (got {raw:?})");
    }
    Ok(expanded)
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
/// Directory-scoped: import into `net_dir` (a project `.shadowdroid/` or the
/// global net dir), which is created if missing so a first project CA can land.
pub fn import_into(
    net_dir: &Path,
    cert_src: &Path,
    key_src: Option<&Path>,
) -> Result<(CaInfo, Vec<String>)> {
    std::fs::create_dir_all(net_dir).with_context(|| format!("create {}", net_dir.display()))?;
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

    // Commit only after the complete replacement set has been staged and
    // synced. The helper preserves the previous set as `.bak` and restores it
    // if any final rename fails.
    let _lock = try_acquire_ca_write_lock(net_dir)?;
    replace_ca_files_locked(net_dir, &clean_cert_pem, &key_pem, SOURCE_IMPORTED)?;

    Ok((info_in_locked(net_dir)?, warnings))
}

/// Describe the CA in `net_dir`. Errors if none exists yet.
pub fn info_in(net_dir: &Path) -> Result<CaInfo> {
    let _lock = acquire_ca_lock(net_dir, false)?;
    info_in_locked(net_dir)
}

fn info_in_locked(net_dir: &Path) -> Result<CaInfo> {
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

/// Discard the CA in `net_dir` (backing it up to `.bak`) and generate a fresh
/// ShadowDroid CA — the escape hatch after an import, and how a project CA is
/// first minted (`net ca reset --project`). Returns the new [`CaInfo`].
pub fn reset_in(net_dir: &Path) -> Result<CaInfo> {
    let _lock = try_acquire_ca_write_lock(net_dir)?;
    // Generation stages the complete new set before backing up the live one.
    generate_ca_files_locked(net_dir)?;
    info_in_locked(net_dir)
}

/// The CA directory a `net ca` verb operates on. Explicit `--project`/`--global`
/// win; otherwise auto: the project `.shadowdroid/` when one already exists, else
/// the global net dir. Returns the dir and an origin label for the emit.
pub fn ca_scope_dir(project: bool, global: bool) -> Result<(PathBuf, &'static str)> {
    if global {
        return Ok((paths::net_dir()?, "global"));
    }
    if project {
        return Ok((crate::config::project_shadowdroid_dir()?, "project"));
    }
    let pdir = crate::config::project_shadowdroid_dir()?;
    if pdir.is_dir() {
        Ok((pdir, "project"))
    } else {
        Ok((paths::net_dir()?, "global"))
    }
}

/// Move any existing `ca.{crt,key,source}` aside to `<name>.bak`, so replacing
/// the CA never silently destroys a key the user might not have backed up.
fn backup_in(net_dir: &Path) -> Result<()> {
    backup_in_with_sync(net_dir, sync_dir)
}

fn backup_in_with_sync(net_dir: &Path, final_sync: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let mut moved = Vec::new();
    for path in [
        ca_cert_in(net_dir),
        ca_key_in(net_dir),
        ca_source_in(net_dir),
    ] {
        if !path.exists() {
            continue;
        }
        let backup = bak_path(&path);
        if backup.exists()
            && let Err(error) = std::fs::remove_file(&backup)
                .with_context(|| format!("remove stale CA backup {}", backup.display()))
        {
            return Err(backup_failure(error, net_dir, &moved));
        }
        if let Err(error) = std::fs::rename(&path, &backup) {
            let error = Err::<(), _>(error)
                .with_context(|| format!("back up {} to {}", path.display(), backup.display()))
                .unwrap_err();
            return Err(backup_failure(error, net_dir, &moved));
        }
        moved.push((path, backup));
    }
    if let Err(error) = final_sync(net_dir) {
        return Err(backup_failure(error, net_dir, &moved));
    }
    Ok(())
}

/// Restore exactly the live files moved by an incomplete [`backup_in`]. This is
/// intentionally narrower than [`restore_backups`], which also removes newly
/// installed files during replacement rollback.
fn restore_moved_backups(net_dir: &Path, moved: &[(PathBuf, PathBuf)]) -> Result<()> {
    let mut failures = Vec::new();
    for (original, backup) in moved.iter().rev() {
        if original.exists() {
            failures.push(format!(
                "refuse to overwrite unexpectedly recreated {}",
                original.display()
            ));
            continue;
        }
        if let Err(error) = std::fs::rename(backup, original) {
            failures.push(format!(
                "restore {} to {}: {error}",
                backup.display(),
                original.display()
            ));
        }
    }
    if let Err(error) = sync_dir(net_dir) {
        failures.push(error.to_string());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

fn backup_failure(
    error: anyhow::Error,
    net_dir: &Path,
    moved: &[(PathBuf, PathBuf)],
) -> anyhow::Error {
    if moved.is_empty() {
        return error;
    }
    match restore_moved_backups(net_dir, moved) {
        Ok(()) => anyhow!("back up CA files failed: {error:#}; restored all moved live files"),
        Err(rollback_error) => anyhow!(
            "back up CA files failed: {error:#}; restoring moved live files also failed: \
             {rollback_error:#}"
        ),
    }
}

/// Remove any partially-installed replacement and restore whatever live files
/// were moved to `.bak` by [`backup_in`]. Missing backups mean that file did not
/// exist before the transaction, so the partial replacement is simply removed.
fn restore_backups(net_dir: &Path) -> Result<()> {
    let mut failures = Vec::new();
    for path in [
        ca_cert_in(net_dir),
        ca_key_in(net_dir),
        ca_source_in(net_dir),
    ] {
        if path.exists()
            && let Err(error) = std::fs::remove_file(&path)
        {
            failures.push(format!("remove {}: {error}", path.display()));
            continue;
        }
        let backup = bak_path(&path);
        if backup.exists()
            && let Err(error) = std::fs::rename(&backup, &path)
        {
            failures.push(format!(
                "restore {} to {}: {error}",
                backup.display(),
                path.display()
            ));
        }
    }
    if let Err(error) = sync_dir(net_dir) {
        failures.push(error.to_string());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync CA directory {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
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

    #[test]
    fn literal_ip_leaf_uses_ip_address_san() {
        use x509_parser::extensions::GeneralName;

        let key = KeyPair::generate().unwrap();
        let cert = ca_params().self_signed(&key).unwrap();
        let ca = CertAuthority::build(&cert.pem(), key).unwrap();

        for (host, expected) in [
            ("127.0.0.1", std::net::IpAddr::from([127, 0, 0, 1])),
            ("::1", std::net::IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1])),
        ] {
            let der = ca.mint_leaf(host).unwrap();
            let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref()).unwrap();
            let san = cert.subject_alternative_name().unwrap().unwrap();
            let expected = match expected {
                std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
                std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
            };
            assert!(san.value.general_names.iter().any(|name| {
                matches!(name, GeneralName::IPAddress(bytes) if *bytes == expected.as_slice())
            }));
            assert!(
                san.value
                    .general_names
                    .iter()
                    .all(|name| !matches!(name, GeneralName::DNSName(_)))
            );
        }
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
    fn partial_project_ca_pair_is_an_actionable_error() {
        for (present, missing) in [
            (paths::CA_CERT_FILE, paths::CA_KEY_FILE),
            (paths::CA_KEY_FILE, paths::CA_CERT_FILE),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), present, "stray project CA material");

            let error = resolve_project_ca(dir.path()).unwrap_err().to_string();
            assert!(error.contains("project CA is incomplete"), "{error}");
            assert!(error.contains(present), "{error}");
            assert!(error.contains(missing), "{error}");
            assert!(error.contains("net ca reset --project"), "{error}");
        }
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

    #[cfg(unix)]
    #[test]
    fn generated_private_key_is_installed_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        generate_ca_files(dir.path()).unwrap();
        let mode = std::fs::metadata(ca_key_in(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn backup_errors_are_not_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cert = ca_cert_in(dir.path());
        std::fs::write(&cert, "live cert").unwrap();
        // A directory at the backup path cannot be removed with remove_file.
        // The live file must remain in place and the error must reach callers.
        std::fs::create_dir(bak_path(&cert)).unwrap();

        let error = backup_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("stale CA backup"), "{error}");
        assert_eq!(std::fs::read_to_string(cert).unwrap(), "live cert");
    }

    #[test]
    fn later_backup_error_restores_files_already_moved() {
        let dir = tempfile::tempdir().unwrap();
        let cert = ca_cert_in(dir.path());
        let key = ca_key_in(dir.path());
        std::fs::write(&cert, "live cert").unwrap();
        std::fs::write(&key, "live key").unwrap();
        // The cert is moved first. Failing while removing the key's stale
        // backup must put that cert back before returning the error.
        std::fs::create_dir(bak_path(&key)).unwrap();

        let error = backup_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("stale CA backup"), "{error}");
        assert!(error.contains("restored all moved live files"), "{error}");
        assert_eq!(std::fs::read_to_string(&cert).unwrap(), "live cert");
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "live key");
        assert!(!bak_path(&cert).exists());
    }

    #[test]
    fn final_backup_sync_error_restores_every_live_file() {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in [
            (ca_cert_in(dir.path()), "live cert"),
            (ca_key_in(dir.path()), "live key"),
            (ca_source_in(dir.path()), "live source"),
        ] {
            std::fs::write(path, contents).unwrap();
        }

        let error = backup_in_with_sync(dir.path(), |_| bail!("injected final sync failure"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("injected final sync failure"), "{error}");
        assert!(error.contains("restored all moved live files"), "{error}");
        for (path, contents) in [
            (ca_cert_in(dir.path()), "live cert"),
            (ca_key_in(dir.path()), "live key"),
            (ca_source_in(dir.path()), "live source"),
        ] {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
            assert!(!bak_path(&path).exists());
        }
    }

    #[test]
    fn backup_error_includes_rollback_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cert = ca_cert_in(dir.path());
        std::fs::write(&cert, "live cert").unwrap();

        let error = backup_in_with_sync(dir.path(), |_| {
            // Simulate an external writer recreating the live destination after
            // it was moved. Rollback refuses to clobber it and reports that
            // secondary failure alongside the triggering sync error.
            std::fs::write(&cert, "unexpected replacement").unwrap();
            bail!("injected final sync failure")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("injected final sync failure"), "{error}");
        assert!(
            error.contains("restoring moved live files also failed"),
            "{error}"
        );
        assert!(error.contains("refuse to overwrite"), "{error}");
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
        let ca =
            CertAuthority::load_from_files(&ca_cert_in(store.path()), &ca_key_in(store.path()))
                .unwrap();
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
#[test]
fn ca_lock_blocks_replacement_while_a_reader_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let reader = acquire_ca_lock(dir.path(), false).unwrap();
    assert!(dir.path().join(".ca.lock").exists());
    let busy = match try_acquire_ca_write_lock(dir.path()) {
        Ok(_) => panic!("exclusive mutation lock must not bypass a live reader"),
        Err(error) => error,
    };
    assert_eq!(crate::cli::error_code_of(&busy), "ca_in_use");
    let contender_dir = dir.path().to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel();
    let contender = std::thread::spawn(move || {
        let writer = acquire_ca_lock(&contender_dir, true).unwrap();
        sender.send(()).unwrap();
        drop(writer);
    });
    assert!(
        receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err()
    );
    drop(reader);
    receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    contender.join().unwrap();
}
