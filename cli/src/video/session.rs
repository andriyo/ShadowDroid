//! Protected session-bundle manifest, timeline, and MP4 verification.

use super::backend::{Capabilities, DeviceMetadata};
use super::paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    #[serde(rename = "type")]
    pub artifact_type: String,
    pub schema_version: u32,
    pub session_id: String,
    pub state: String,
    pub device: DeviceMetadata,
    pub backend: BackendInfo,
    pub capture: CaptureInfo,
    pub artifacts: Vec<ArtifactInfo>,
    pub segments: Vec<Segment>,
    pub markers: Vec<Marker>,
    pub gaps: Vec<Gap>,
    pub warnings: Vec<String>,
    pub privacy: PrivacyInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackendInfo {
    pub name: String,
    pub version: Option<String>,
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureInfo {
    pub started_at: f64,
    pub stopped_at: Option<f64>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub elapsed_ms: u64,
    pub size: Option<VideoSize>,
    pub bit_rate_bps: Option<u32>,
    pub display_id: Option<u64>,
    pub bugreport: bool,
    pub segment_seconds: u32,
    pub split_on_rotation: bool,
    pub audio: AudioInfo,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct VideoSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AudioInfo {
    pub included: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactInfo {
    pub path: String,
    pub kind: String,
    pub complete: bool,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    pub redaction: String,
    pub potentially_sensitive: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Segment {
    pub index: u32,
    pub path: String,
    pub remote_path: String,
    pub state: String,
    pub started_at: f64,
    pub stopped_at: Option<f64>,
    pub elapsed_ms: u64,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub remote_cleaned: bool,
    #[serde(default)]
    pub sample_count: Option<u64>,
    #[serde(default)]
    pub media_duration_ms: Option<u64>,
    #[serde(default)]
    pub playable: bool,
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub timescale: Option<u32>,
    #[serde(default)]
    pub sample_entry_sha256: Option<String>,
    #[serde(default)]
    pub timing_basis: String,
    pub orientation: Option<u8>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Marker {
    pub label: String,
    pub ts: f64,
    pub elapsed_ms: u64,
    pub segment_index: Option<u32>,
    pub segment_elapsed_ms: Option<u64>,
    #[serde(default)]
    pub timing_basis: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Gap {
    pub after_segment: Option<u32>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub duration_ms: Option<u64>,
    pub reason: String,
    #[serde(default)]
    pub timing_basis: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PrivacyInfo {
    pub contains_sensitive_data: bool,
    pub encrypted: bool,
    pub video_redaction: String,
    pub metadata_redaction: String,
    pub host_directory_mode: String,
    pub host_file_mode: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub executable: String,
    pub remote_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActiveState {
    pub schema_version: u32,
    pub serial: String,
    pub startup_id: String,
    pub session_id: String,
    pub daemon_pid: u32,
    #[serde(default)]
    pub control_port: Option<u16>,
    pub bundle: PathBuf,
    pub remote_dir: String,
    pub state: String,
    pub started_at: f64,
    pub current_segment: Option<u32>,
    pub device_process: Option<DeviceProcessIdentity>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct Bundle {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub timeline_path: PathBuf,
    pub segments_dir: PathBuf,
}

impl Bundle {
    pub fn create(out: &Path) -> Result<Self> {
        if std::fs::symlink_metadata(out).is_ok() {
            return Err(crate::diagnostic::DiagnosticError::new(
                "video_destination_exists",
                "input",
                "video output path already exists; recordings never overwrite it",
            )
            .detail(json!({"out": out.display().to_string()}))
            .next_actions([
                "choose a new `--out <directory>`",
                "shadowdroid video status",
            ])
            .into());
        }
        if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create video output parent {}", parent.display()))?;
        }
        std::fs::create_dir(out)
            .with_context(|| format!("create video bundle {}", out.display()))?;
        paths::protect_dir(out)?;
        let root = out
            .canonicalize()
            .with_context(|| format!("resolve video bundle {}", out.display()))?;
        let segments_dir = root.join("segments");
        std::fs::create_dir(&segments_dir)
            .with_context(|| format!("create {}", segments_dir.display()))?;
        paths::protect_dir(&segments_dir)?;
        let manifest_path = root.join("manifest.json");
        let timeline_path = root.join("events.jsonl");
        create_private_file(&timeline_path)?;
        Ok(Self {
            root,
            manifest_path,
            timeline_path,
            segments_dir,
        })
    }

    pub fn open(root: PathBuf) -> Self {
        Self {
            manifest_path: root.join("manifest.json"),
            timeline_path: root.join("events.jsonl"),
            segments_dir: root.join("segments"),
            root,
        }
    }

    pub fn segment_path(&self, index: u32) -> PathBuf {
        self.segments_dir.join(format!("{index:06}.mp4"))
    }

    pub fn write_manifest(&self, manifest: &Manifest) -> Result<()> {
        write_json_private(&self.manifest_path, &serde_json::to_value(manifest)?)
    }

    pub fn append_event(&self, event: &Value) -> Result<()> {
        append_jsonl_private(&self.timeline_path, event)
    }
}

impl Manifest {
    pub fn new(
        session_id: String,
        device: DeviceMetadata,
        capabilities: Capabilities,
        capture: &super::CaptureArgs,
        parsed_size: Option<(u32, u32)>,
        redact: bool,
    ) -> Self {
        Self {
            artifact_type: "shadowdroid_video_session".into(),
            schema_version: SCHEMA_VERSION,
            session_id,
            state: "starting".into(),
            device,
            backend: BackendInfo {
                name: "screenrecord".into(),
                version: capabilities.version.clone(),
                capabilities,
            },
            capture: CaptureInfo {
                started_at: now_ts(),
                stopped_at: None,
                stop_reason: None,
                elapsed_ms: 0,
                size: parsed_size.map(|(width, height)| VideoSize { width, height }),
                bit_rate_bps: capture.bit_rate,
                display_id: capture.display_id,
                bugreport: capture.bugreport,
                segment_seconds: capture.segment_seconds,
                split_on_rotation: !capture.no_split_on_rotation,
                audio: AudioInfo {
                    included: false,
                    reason: "screenrecord_backend_video_only".into(),
                },
            },
            artifacts: vec![
                ArtifactInfo {
                    path: "video.mp4".into(),
                    kind: "video/mp4".into(),
                    complete: false,
                    bytes: None,
                    sha256: None,
                    redaction: "not_supported".into(),
                    potentially_sensitive: true,
                },
                ArtifactInfo {
                    path: "events.jsonl".into(),
                    kind: "application/x-ndjson".into(),
                    complete: false,
                    bytes: None,
                    sha256: None,
                    redaction: if redact {
                        "marker_labels_builtin_or_configured_v1".into()
                    } else {
                        "not_requested".into()
                    },
                    potentially_sensitive: true,
                },
            ],
            segments: Vec::new(),
            markers: Vec::new(),
            gaps: Vec::new(),
            warnings: vec![
                "screenrecord captures video only; device and microphone audio are not included"
                    .into(),
                "video pixels are not redacted and may contain credentials, notifications, or personal data"
                    .into(),
                "marker offsets and rollover gaps are host-observed estimates; segment media_duration_ms and sample_count describe encoded coverage"
                    .into(),
            ],
            privacy: PrivacyInfo {
                contains_sensitive_data: true,
                encrypted: false,
                video_redaction: "not_supported".into(),
                metadata_redaction: if redact {
                    "marker_labels_only".into()
                } else {
                    "not_requested".into()
                },
                host_directory_mode: paths::directory_mode_label().into(),
                host_file_mode: paths::file_mode_label().into(),
            },
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        elapsed_ms(self.capture.started_at, now_ts())
    }
}

pub fn now_ts() -> f64 {
    crate::events::now_ts()
}

pub fn elapsed_ms(started_at: f64, now: f64) -> u64 {
    ((now - started_at).max(0.0) * 1000.0).round() as u64
}

pub fn timeline_event(kind: &str, started_at: f64, mut fields: Value) -> Value {
    let ts = now_ts();
    let mut event = serde_json::Map::new();
    event.insert("type".into(), kind.into());
    event.insert("schema_version".into(), SCHEMA_VERSION.into());
    event.insert("ts".into(), json!(ts));
    event.insert("elapsed_ms".into(), elapsed_ms(started_at, ts).into());
    if let Value::Object(extra) = &mut fields {
        event.append(extra);
    }
    Value::Object(event)
}

pub fn write_active(serial: &crate::ids::Serial, state: &ActiveState) -> Result<()> {
    paths::ensure_dir()?;
    write_json_private(&paths::state(serial)?, &serde_json::to_value(state)?)
}

pub fn read_active(serial: &crate::ids::Serial) -> Result<Option<ActiveState>> {
    let path = paths::state(serial)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parse video state {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read video state {}", path.display())),
    }
}

pub fn write_json_private(path: &Path, value: &Value) -> Result<()> {
    crate::cmd::artifact::write_json(path, value)?;
    paths::protect_file(path)
}

fn create_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?
            .sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?
            .sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
    }
    paths::protect_file(path)
}

fn append_jsonl_private(path: &Path, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("open timeline {}", path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("append timeline {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync timeline {}", path.display()))?;
    paths::protect_file(path)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let digest = digest.finalize();
    let mut rendered = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(rendered, "{byte:02x}");
    }
    Ok(rendered)
}

/// Dependency-free validation of the top-level ISO BMFF structure. A cleanly
/// finalized screenrecord MP4 must contain bounded `ftyp`, `mdat`, and `moov`
/// boxes; an interrupted muxer commonly lacks `moov`.
pub fn validate_mp4(path: &Path) -> Result<()> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if len < 24 {
        bail!("MP4 is too small ({len} bytes)");
    }
    let mut offset = 0u64;
    let mut ftyp = false;
    let mut mdat = false;
    let mut moov = false;
    while offset + 8 <= len {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let short_size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
        let kind = &header[4..8];
        let (box_size, header_size) = match short_size {
            0 => (len - offset, 8),
            1 => {
                let mut extended = [0u8; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            value => (value, 8),
        };
        if box_size < header_size || offset.saturating_add(box_size) > len {
            bail!("invalid MP4 box at byte {offset}: size {box_size}, file length {len}");
        }
        match kind {
            b"ftyp" => ftyp = true,
            b"mdat" => mdat = true,
            b"moov" => moov = true,
            _ => {}
        }
        if box_size == 0 {
            break;
        }
        offset += box_size;
    }
    if !(ftyp && mdat && moov) {
        bail!("MP4 is not finalized (ftyp={ftyp}, mdat={mdat}, moov={moov})");
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct VideoTrackInfo {
    pub sample_count: u64,
    pub duration_ms: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    pub sample_entry_sha256: String,
}

impl VideoTrackInfo {
    pub fn playable(&self) -> bool {
        self.sample_count >= 2 && self.duration_ms > 0
    }
}

/// Inspect the finalized video track without depending on ffprobe. Android's
/// screenrecord may produce a structurally valid one-frame MP4 when the display
/// never changes; that is preserved as a segment, but must not be advertised as
/// a playable aggregate video.
pub fn inspect_mp4_video(path: &Path) -> Result<VideoTrackInfo> {
    validate_mp4(path)?;
    let bytes = read_moov(path)?;
    let moov = boxes(&bytes, 0, bytes.len())?
        .into_iter()
        .find(|item| item.kind == *b"moov")
        .ok_or_else(|| anyhow::anyhow!("MP4 has no moov box"))?;
    for trak in child_boxes(&bytes, moov)?
        .into_iter()
        .filter(|item| item.kind == *b"trak")
    {
        let Some(mdia) = child_boxes(&bytes, trak)?
            .into_iter()
            .find(|item| item.kind == *b"mdia")
        else {
            continue;
        };
        let mdia_children = child_boxes(&bytes, mdia)?;
        let Some(handler) = mdia_children
            .iter()
            .copied()
            .find(|item| item.kind == *b"hdlr")
        else {
            continue;
        };
        if read_slice(&bytes, handler.payload + 8, 4)? != b"vide" {
            continue;
        }
        let mdhd = mdia_children
            .iter()
            .copied()
            .find(|item| item.kind == *b"mdhd")
            .ok_or_else(|| anyhow::anyhow!("video track has no mdhd box"))?;
        let (timescale, header_duration) = parse_mdhd(&bytes, mdhd)?;
        let minf = mdia_children
            .iter()
            .copied()
            .find(|item| item.kind == *b"minf")
            .ok_or_else(|| anyhow::anyhow!("video track has no minf box"))?;
        let stbl = child_boxes(&bytes, minf)?
            .into_iter()
            .find(|item| item.kind == *b"stbl")
            .ok_or_else(|| anyhow::anyhow!("video track has no stbl box"))?;
        let sample_boxes = child_boxes(&bytes, stbl)?;
        let stsd = sample_boxes
            .iter()
            .copied()
            .find(|item| item.kind == *b"stsd")
            .ok_or_else(|| anyhow::anyhow!("video track has no stsd box"))?;
        let (codec, width, height, sample_entry_sha256) = parse_video_sample_entry(&bytes, stsd)?;
        let stsz_count = sample_boxes
            .iter()
            .copied()
            .find(|item| item.kind == *b"stsz")
            .map(|item| parse_stsz_count(&bytes, item))
            .transpose()?;
        let stts = sample_boxes
            .iter()
            .copied()
            .find(|item| item.kind == *b"stts")
            .map(|item| parse_stts(&bytes, item))
            .transpose()?;
        let sample_count = stsz_count
            .or_else(|| stts.map(|(count, _)| count))
            .unwrap_or_default();
        let duration_units = header_duration.max(stts.map(|(_, duration)| duration).unwrap_or(0));
        let duration_ms = if timescale == 0 {
            0
        } else {
            ((u128::from(duration_units) * 1000) / u128::from(timescale)).min(u128::from(u64::MAX))
                as u64
        };
        return Ok(VideoTrackInfo {
            sample_count,
            duration_ms,
            codec,
            width,
            height,
            timescale,
            sample_entry_sha256,
        });
    }
    bail!("MP4 has no video track")
}

fn read_moov(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut offset = 0u64;
    while offset + 8 <= len {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let short_size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
        let kind = &header[4..8];
        let (size, header_size) = match short_size {
            0 => (len - offset, 8u64),
            1 => {
                let mut extended = [0u8; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16u64)
            }
            value => (value, 8u64),
        };
        if size < header_size || offset.saturating_add(size) > len {
            bail!("invalid MP4 box at byte {offset}");
        }
        if kind == b"moov" {
            const MAX_MOOV_BYTES: u64 = 256 * 1024 * 1024;
            if size > MAX_MOOV_BYTES {
                bail!("MP4 moov box is unexpectedly large ({size} bytes)");
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; usize::try_from(size)?];
            file.read_exact(&mut bytes)?;
            return Ok(bytes);
        }
        offset += size;
    }
    bail!("MP4 has no moov box")
}

#[derive(Clone, Copy)]
struct BoxView {
    kind: [u8; 4],
    payload: usize,
    end: usize,
}

fn child_boxes(bytes: &[u8], parent: BoxView) -> Result<Vec<BoxView>> {
    boxes(bytes, parent.payload, parent.end)
}

fn boxes(bytes: &[u8], mut offset: usize, end: usize) -> Result<Vec<BoxView>> {
    let mut found = Vec::new();
    while offset.saturating_add(8) <= end {
        let short_size = read_u32(bytes, offset)? as u64;
        let kind: [u8; 4] = read_slice(bytes, offset + 4, 4)?
            .try_into()
            .expect("four-byte box type");
        let (size, header) = match short_size {
            0 => ((end - offset) as u64, 8usize),
            1 => (read_u64(bytes, offset + 8)?, 16usize),
            value => (value, 8usize),
        };
        let size = usize::try_from(size).context("MP4 box is too large for this host")?;
        let box_end = offset
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("MP4 box size overflow"))?;
        if size < header || box_end > end {
            bail!("invalid MP4 child box at byte {offset}");
        }
        found.push(BoxView {
            kind,
            payload: offset + header,
            end: box_end,
        });
        if box_end == offset {
            break;
        }
        offset = box_end;
    }
    Ok(found)
}

fn parse_mdhd(bytes: &[u8], item: BoxView) -> Result<(u32, u64)> {
    let version = *read_slice(bytes, item.payload, 1)?
        .first()
        .expect("one-byte slice");
    match version {
        0 => Ok((
            read_u32(bytes, item.payload + 12)?,
            u64::from(read_u32(bytes, item.payload + 16)?),
        )),
        1 => Ok((
            read_u32(bytes, item.payload + 20)?,
            read_u64(bytes, item.payload + 24)?,
        )),
        other => bail!("unsupported mdhd version {other}"),
    }
}

fn parse_stsz_count(bytes: &[u8], item: BoxView) -> Result<u64> {
    Ok(u64::from(read_u32(bytes, item.payload + 8)?))
}

fn parse_video_sample_entry(bytes: &[u8], item: BoxView) -> Result<(String, u32, u32, String)> {
    let entry_count = read_u32(bytes, item.payload + 4)?;
    if entry_count == 0 {
        bail!("video sample description is empty");
    }
    let entry_offset = item.payload + 8;
    let entry_size = usize::try_from(read_u32(bytes, entry_offset)?)?;
    let entry_end = entry_offset
        .checked_add(entry_size)
        .ok_or_else(|| anyhow::anyhow!("video sample entry size overflow"))?;
    if entry_size < 36 || entry_end > item.end {
        bail!("video sample entry is truncated");
    }
    let kind = read_slice(bytes, entry_offset + 4, 4)?;
    let codec = std::str::from_utf8(kind)
        .context("video codec is not ASCII")?
        .to_string();
    let payload = entry_offset + 8;
    let width = u32::from(read_u16(bytes, payload + 24)?);
    let height = u32::from(read_u16(bytes, payload + 26)?);
    if width == 0 || height == 0 {
        bail!("video sample entry has invalid dimensions {width}x{height}");
    }
    Ok((
        codec,
        width,
        height,
        sha256_bytes(read_slice(bytes, entry_offset, entry_size)?),
    ))
}

fn parse_stts(bytes: &[u8], item: BoxView) -> Result<(u64, u64)> {
    let entries = read_u32(bytes, item.payload + 4)? as usize;
    let mut sample_count = 0u64;
    let mut duration = 0u64;
    for index in 0..entries {
        let offset = item
            .payload
            .checked_add(8 + index.saturating_mul(8))
            .ok_or_else(|| anyhow::anyhow!("stts entry offset overflow"))?;
        let count = u64::from(read_u32(bytes, offset)?);
        let delta = u64::from(read_u32(bytes, offset + 4)?);
        sample_count = sample_count.saturating_add(count);
        duration = duration.saturating_add(count.saturating_mul(delta));
    }
    Ok((sample_count, duration))
}

fn read_slice(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("MP4 offset overflow"))?;
    bytes
        .get(offset..end)
        .ok_or_else(|| anyhow::anyhow!("truncated MP4 box payload"))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        read_slice(bytes, offset, 4)?
            .try_into()
            .expect("four-byte slice"),
    ))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        read_slice(bytes, offset, 2)?
            .try_into()
            .expect("two-byte slice"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(
        read_slice(bytes, offset, 8)?
            .try_into()
            .expect("eight-byte slice"),
    ))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

pub fn assemble_video(bundle: &Bundle, manifest: &mut Manifest) -> Result<Option<PathBuf>> {
    let output = bundle.root.join("video.mp4");
    if let Some(artifact) = manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == "video.mp4")
    {
        artifact.complete = false;
        artifact.bytes = None;
        artifact.sha256 = None;
    }
    let selected = manifest
        .segments
        .iter()
        .filter(|segment| segment.state == "complete" && segment.playable)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        if output.is_file() {
            std::fs::remove_file(&output)
                .with_context(|| format!("remove stale aggregate {}", output.display()))?;
        }
        return Ok(None);
    }
    let signature = (
        selected[0].codec.as_deref(),
        selected[0].width,
        selected[0].height,
        selected[0].timescale,
        selected[0].sample_entry_sha256.as_deref(),
    );
    if selected.iter().skip(1).any(|segment| {
        (
            segment.codec.as_deref(),
            segment.width,
            segment.height,
            segment.timescale,
            segment.sample_entry_sha256.as_deref(),
        ) != signature
    }) {
        manifest.warnings.push(
            "playable segments use incompatible codec or geometry; preserving them without video.mp4"
                .into(),
        );
        return Ok(None);
    }
    let completed = selected
        .iter()
        .map(|segment| bundle.root.join(&segment.path))
        .collect::<Vec<_>>();
    if completed.len() == 1 {
        std::fs::copy(&completed[0], &output)
            .with_context(|| format!("copy {} to {}", completed[0].display(), output.display()))?;
    } else {
        let list = bundle.root.join(".concat.txt");
        let mut contents = String::new();
        for path in &completed {
            let path = path
                .canonicalize()
                .with_context(|| format!("resolve {}", path.display()))?;
            let escaped = path.display().to_string().replace('\'', "'\\''");
            contents.push_str(&format!("file '{escaped}'\n"));
        }
        crate::cmd::artifact::write_bytes(&list, contents.as_bytes())?;
        paths::protect_file(&list)?;
        let temporary = bundle.root.join(".video.tmp.mp4");
        let command = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ])
            .arg(&list)
            .args(["-c", "copy", "-y"])
            .arg(&temporary)
            .output();
        let _ = std::fs::remove_file(&list);
        match command {
            Ok(result) if result.status.success() => {
                validate_mp4(&temporary)?;
                std::fs::rename(&temporary, &output)
                    .with_context(|| format!("publish assembled video {}", output.display()))?;
            }
            Ok(result) => {
                let _ = std::fs::remove_file(&temporary);
                manifest.warnings.push(format!(
                    "segments are complete, but ffmpeg assembly failed: {}",
                    String::from_utf8_lossy(&result.stderr).trim()
                ));
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                manifest.warnings.push(
                    "multiple segments are complete; install ffmpeg to assemble video.mp4".into(),
                );
                return Ok(None);
            }
            Err(error) => return Err(error).context("run ffmpeg segment assembly"),
        }
    }
    paths::protect_file(&output)?;
    let aggregate_info = inspect_mp4_video(&output)?;
    if !aggregate_info.playable() {
        bail!("assembled video has no playable duration");
    }
    let bytes = output.metadata()?.len();
    let sha256 = sha256_file(&output)?;
    if let Some(artifact) = manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == "video.mp4")
    {
        artifact.complete = true;
        artifact.bytes = Some(bytes);
        artifact.sha256 = Some(sha256);
    }
    Ok(Some(output))
}

pub fn manifest_from_bundle(root: &Path) -> Result<Manifest> {
    let path = root.join("manifest.json");
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn artifact_summary(root: &Path, manifest: &Manifest) -> Value {
    let video = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.path == "video.mp4" && artifact.complete)
        .map(|_| root.join("video.mp4").display().to_string());
    let bytes = manifest
        .segments
        .iter()
        .filter_map(|segment| segment.bytes)
        .sum::<u64>();
    json!({
        "session_id": manifest.session_id,
        "device": manifest.device.serial,
        "state": manifest.state,
        "bundle": root.display().to_string(),
        "manifest": root.join("manifest.json").display().to_string(),
        "timeline": root.join("events.jsonl").display().to_string(),
        "video": video,
        "segments": manifest.segments.iter().filter(|segment| segment.state == "complete").count(),
        "playable_segments": manifest.segments.iter().filter(|segment| segment.state == "complete" && segment.playable).count(),
        "playable": video.is_some(),
        "bytes": bytes,
        "elapsed_ms": manifest.capture.elapsed_ms,
        "warnings": manifest.warnings,
        "contains_sensitive_data": true,
        "potentially_sensitive": true,
        "encrypted": false,
        "redaction": {
            "metadata": manifest.privacy.metadata_redaction,
            "video_pixels": false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(payload.len() + 8);
        bytes.extend_from_slice(&u32::try_from(payload.len() + 8).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn synthetic_video(sample_count: u32, duration: u32) -> Vec<u8> {
        let mut mdhd = vec![0; 24];
        mdhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mdhd[16..20].copy_from_slice(&duration.to_be_bytes());
        let mut hdlr = vec![0; 12];
        hdlr[8..12].copy_from_slice(b"vide");

        let mut sample_entry = vec![0; 36];
        sample_entry[..4].copy_from_slice(&36u32.to_be_bytes());
        sample_entry[4..8].copy_from_slice(b"avc1");
        sample_entry[32..34].copy_from_slice(&1080u16.to_be_bytes());
        sample_entry[34..36].copy_from_slice(&1920u16.to_be_bytes());
        let mut stsd = vec![0; 8];
        stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&sample_entry);

        let mut stsz = vec![0; 12];
        stsz[8..12].copy_from_slice(&sample_count.to_be_bytes());
        let mut stts = vec![0; 16];
        stts[4..8].copy_from_slice(&1u32.to_be_bytes());
        stts[8..12].copy_from_slice(&sample_count.to_be_bytes());
        stts[12..16].copy_from_slice(
            &duration
                .checked_div(sample_count)
                .unwrap_or(0)
                .to_be_bytes(),
        );

        let mut stbl = Vec::new();
        stbl.extend(mp4_box(b"stsd", &stsd));
        stbl.extend(mp4_box(b"stsz", &stsz));
        stbl.extend(mp4_box(b"stts", &stts));
        let minf = mp4_box(b"minf", &mp4_box(b"stbl", &stbl));
        let mut mdia = Vec::new();
        mdia.extend(mp4_box(b"mdhd", &mdhd));
        mdia.extend(mp4_box(b"hdlr", &hdlr));
        mdia.extend(minf);
        let trak = mp4_box(b"trak", &mp4_box(b"mdia", &mdia));

        let mut bytes = mp4_box(b"ftyp", b"isom");
        bytes.extend(mp4_box(b"mdat", b"sample-data"));
        bytes.extend(mp4_box(b"moov", &trak));
        bytes
    }

    #[test]
    fn bundle_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Bundle::create(dir.path()).is_err());
    }

    #[test]
    fn timeline_is_valid_jsonl() {
        let parent = tempfile::tempdir().unwrap();
        let bundle = Bundle::create(&parent.path().join("session")).unwrap();
        bundle
            .append_event(&json!({"type": "video_marker", "label": "checkout"}))
            .unwrap();
        bundle
            .append_event(&json!({"type": "video_session_stop"}))
            .unwrap();
        let text = std::fs::read_to_string(bundle.timeline_path).unwrap();
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }

    #[test]
    fn elapsed_time_never_underflows() {
        assert_eq!(elapsed_ms(10.0, 9.0), 0);
        assert_eq!(elapsed_ms(10.0, 10.125), 125);
    }

    #[test]
    fn video_track_inspection_reports_media_truth() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("video.mp4");
        std::fs::write(&path, synthetic_video(3, 3000)).unwrap();
        let info = inspect_mp4_video(&path).unwrap();
        assert_eq!(info.sample_count, 3);
        assert_eq!(info.duration_ms, 3000);
        assert_eq!(info.codec, "avc1");
        assert_eq!((info.width, info.height), (1080, 1920));
        assert_eq!(info.timescale, 1000);
        assert_eq!(info.sample_entry_sha256.len(), 64);
        assert!(info.playable());

        std::fs::write(&path, synthetic_video(1, 0)).unwrap();
        assert!(!inspect_mp4_video(&path).unwrap().playable());
    }
}
