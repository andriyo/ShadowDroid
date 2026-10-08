//! A small, bounds-checked DEX reader for field access sites.
//!
//! JDWP cannot name the field an `iput`/`iget` instruction touches: its
//! operand is an index into the defining dex file's `field_ids`, and the
//! constant pool is not available over the wire. Device-confirmed:
//! Method.Bytecodes returns exactly the APK's dex code, so the APK's own dex
//! files resolve those indices. This module reads just enough of a dex file
//! (header, string/type/proto/field/method ids, class defs, class data, code
//! items) to list, for a `(class, field)`, every instruction that writes or
//! reads it, as `(class, method, signature, code index)`. Code indexes are
//! 16-bit units from the start of the method's instructions, the same unit
//! JDWP locations use.

use std::io::Read;

/// One instruction that reads or writes the watched field.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FieldSite {
    /// Declaring class of the method, as a JNI descriptor (`Lfoo/Bar;`).
    pub class: String,
    pub method: String,
    /// JNI method signature (`(I)V`).
    pub signature: String,
    /// Code index of the instruction (16-bit units).
    pub index: u64,
    /// `iput*`/`sput*` (true) or `iget*`/`sget*` (false).
    pub write: bool,
}

/// Opcodes that name a field: `iget*` 0x52–0x58, `iput*` 0x59–0x5f,
/// `sget*` 0x60–0x66, `sput*` 0x67–0x6d. Returns whether it writes.
pub fn field_opcode(opcode: u8) -> Option<bool> {
    match opcode {
        0x52..=0x58 | 0x60..=0x66 => Some(false),
        0x59..=0x5f | 0x67..=0x6d => Some(true),
        _ => None,
    }
}

/// Instruction width in code units for a Dalvik opcode (standard format
/// table, including ART's quickened opcodes).
pub fn width(opcode: u8) -> usize {
    match opcode {
        0x00..=0x01 | 0x04 | 0x07 | 0x0a..=0x12 | 0x1d | 0x1e | 0x21 | 0x27 | 0x28 => 1,
        0x02 | 0x05 | 0x08 | 0x13 | 0x15 | 0x16 | 0x19 | 0x1a | 0x1c | 0x1f | 0x20 | 0x22
        | 0x23 => 2,
        0x03 | 0x06 | 0x09 | 0x14 | 0x17 | 0x1b | 0x24..=0x26 | 0x2a..=0x2c => 3,
        0x18 => 5,
        0x29 | 0x2d..=0x3d => 2,
        0x3e..=0x43 => 1,
        0x44..=0x6d => 2,
        0x6e..=0x72 | 0x74..=0x78 => 3,
        0x73 | 0x79 | 0x7a => 1,
        0x7b..=0x8f => 1,
        0x90..=0xaf => 2,
        0xb0..=0xcf => 1,
        0xd0..=0xe2 => 2,
        0xe3..=0xf9 => 1,
        0xfa | 0xfb => 4,
        0xfc | 0xfd => 3,
        0xfe | 0xff => 2,
    }
}

/// Walk instructions in `units`, skipping the switch/array payload
/// pseudo-instructions; calls `visit(code index, units from there)`.
pub fn walk(units: &[u16], mut visit: impl FnMut(usize, &[u16])) {
    let mut pc = 0usize;
    while pc < units.len() {
        let unit = units[pc];
        let at = |offset: usize| units.get(pc + offset).copied().unwrap_or(0) as usize;
        let (payload, size) = match unit {
            // packed-switch, sparse-switch, fill-array-data payloads
            0x0100 => (true, at(1) * 2 + 4),
            0x0200 => (true, at(1) * 4 + 2),
            0x0300 => (true, (at(1) * (at(2) | at(3) << 16)).div_ceil(2) + 4),
            _ => (false, width((unit & 0xff) as u8)),
        };
        if !payload {
            visit(pc, &units[pc..]);
        }
        pc += size.max(1);
    }
}

/// Little-endian 16-bit code units of raw method code.
pub fn code_units(code: &[u8]) -> Vec<u16> {
    code.chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// `(code index, writes, field index)` of every field instruction.
pub fn field_instructions(units: &[u16]) -> Vec<(u64, bool, u32)> {
    let mut out = Vec::new();
    walk(units, |pc, rest| {
        if let Some(write) = field_opcode((rest[0] & 0xff) as u8)
            && let Some(field) = rest.get(1)
        {
            out.push((pc as u64, write, u32::from(*field)));
        }
    });
    out
}

/// A parsed dex file (borrowed bytes; everything is read lazily).
pub struct Dex<'a> {
    data: &'a [u8],
    strings: Section,
    types: Section,
    protos: Section,
    fields: Section,
    methods: Section,
    class_defs: Section,
}

#[derive(Clone, Copy)]
struct Section {
    size: u32,
    offset: u32,
}

/// Bound on code scanned per method (units); real methods are far smaller.
const MAX_INSNS: usize = 1 << 20;

impl<'a> Dex<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Dex<'a>, String> {
        if data.len() < 0x70 || &data[0..4] != b"dex\n" {
            return Err("not a dex file".into());
        }
        let section = |at: usize| -> Result<Section, String> {
            let size = u32_at(data, at).ok_or("truncated header")?;
            let offset = u32_at(data, at + 4).ok_or("truncated header")?;
            Ok(Section { size, offset })
        };
        Ok(Dex {
            data,
            strings: section(0x38)?,
            types: section(0x40)?,
            protos: section(0x48)?,
            fields: section(0x50)?,
            methods: section(0x58)?,
            class_defs: section(0x60)?,
        })
    }

    fn entry(&self, section: Section, index: u32, width: usize) -> Option<usize> {
        (index < section.size).then(|| section.offset as usize + index as usize * width)
    }

    pub fn string(&self, index: u32) -> Option<String> {
        let at = self.entry(self.strings, index, 4)?;
        let mut pos = u32_at(self.data, at)? as usize;
        let _utf16_len = uleb128(self.data, &mut pos)?;
        let rest = self.data.get(pos..)?;
        let end = rest.iter().position(|b| *b == 0)?;
        // MUTF-8: identifiers in practice are ASCII; decode lossily.
        Some(String::from_utf8_lossy(&rest[..end]).into_owned())
    }

    pub fn type_descriptor(&self, index: u32) -> Option<String> {
        let at = self.entry(self.types, index, 4)?;
        self.string(u32_at(self.data, at)?)
    }

    /// `(class descriptor, field name)` of field `index`.
    pub fn field(&self, index: u32) -> Option<(String, String)> {
        let at = self.entry(self.fields, index, 8)?;
        let class = u16_at(self.data, at)? as u32;
        let name = u32_at(self.data, at + 4)?;
        Some((self.type_descriptor(class)?, self.string(name)?))
    }

    /// `(class descriptor, name, signature)` of method `index`.
    pub fn method(&self, index: u32) -> Option<(String, String, String)> {
        let at = self.entry(self.methods, index, 8)?;
        let class = u16_at(self.data, at)? as u32;
        let proto = u16_at(self.data, at + 2)? as u32;
        let name = u32_at(self.data, at + 4)?;
        Some((
            self.type_descriptor(class)?,
            self.string(name)?,
            self.proto_signature(proto)?,
        ))
    }

    fn proto_signature(&self, index: u32) -> Option<String> {
        let at = self.entry(self.protos, index, 12)?;
        let return_type = u32_at(self.data, at + 4)?;
        let parameters = u32_at(self.data, at + 8)? as usize;
        let mut signature = String::from("(");
        if parameters != 0 {
            let count = u32_at(self.data, parameters)? as usize;
            for i in 0..count.min(256) {
                let type_index = u16_at(self.data, parameters + 4 + i * 2)? as u32;
                signature.push_str(&self.type_descriptor(type_index)?);
            }
        }
        signature.push(')');
        signature.push_str(&self.type_descriptor(return_type)?);
        Some(signature)
    }

    /// Class descriptors this file defines.
    #[cfg(test)]
    pub fn defined_classes(&self) -> Vec<String> {
        (0..self.class_defs.size.min(1 << 20))
            .filter_map(|i| {
                let at = self.entry(self.class_defs, i, 32)?;
                self.type_descriptor(u32_at(self.data, at)?)
            })
            .collect()
    }

    /// Every instruction in this file that reads or writes `field` of
    /// `class` (a descriptor). Empty when the file never references it.
    pub fn field_sites(&self, class: &str, field: &str) -> Vec<FieldSite> {
        let wanted: Vec<u32> = (0..self.fields.size)
            .filter(|i| {
                self.field(*i)
                    .is_some_and(|(c, n)| c == class && n == field)
            })
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut sites = Vec::new();
        for def in 0..self.class_defs.size.min(1 << 20) {
            let Some(at) = self.entry(self.class_defs, def, 32) else {
                break;
            };
            let Some(class_data) = u32_at(self.data, at + 24).filter(|o| *o != 0) else {
                continue;
            };
            for (method_index, code_off) in self.methods_with_code(class_data as usize) {
                let Some(units) = self.code(code_off) else {
                    continue;
                };
                let found: Vec<(u64, bool, u32)> = field_instructions(&units)
                    .into_iter()
                    .filter(|(_, _, f)| wanted.contains(f))
                    .collect();
                if found.is_empty() {
                    continue;
                }
                let Some((owner, name, signature)) = self.method(method_index) else {
                    continue;
                };
                for (index, write, _) in found {
                    sites.push(FieldSite {
                        class: owner.clone(),
                        method: name.clone(),
                        signature: signature.clone(),
                        index,
                        write,
                    });
                }
            }
        }
        sites
    }

    /// `(method index, code offset)` of the methods in a class_data item
    /// that have code.
    fn methods_with_code(&self, offset: usize) -> Vec<(u32, usize)> {
        let mut pos = offset;
        let mut read = || uleb128(self.data, &mut pos);
        let (Some(statics), Some(instances), Some(direct), Some(virtuals)) =
            (read(), read(), read(), read())
        else {
            return Vec::new();
        };
        let mut pos_fields = pos;
        for _ in 0..(statics as u64 + instances as u64).min(1 << 20) {
            if uleb128(self.data, &mut pos_fields).is_none()
                || uleb128(self.data, &mut pos_fields).is_none()
            {
                return Vec::new();
            }
        }
        let mut pos = pos_fields;
        let mut out = Vec::new();
        for count in [direct, virtuals] {
            let mut method = 0u32;
            for _ in 0..count.min(1 << 20) {
                let (Some(diff), Some(_access), Some(code)) = (
                    uleb128(self.data, &mut pos),
                    uleb128(self.data, &mut pos),
                    uleb128(self.data, &mut pos),
                ) else {
                    return out;
                };
                method = method.wrapping_add(diff);
                if code != 0 {
                    out.push((method, code as usize));
                }
            }
        }
        out
    }

    /// The instructions of a code_item.
    fn code(&self, offset: usize) -> Option<Vec<u16>> {
        let size = u32_at(self.data, offset + 12)? as usize;
        if size > MAX_INSNS {
            return None;
        }
        let bytes = self.data.get(offset + 16..offset + 16 + size * 2)?;
        Some(code_units(bytes))
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn uleb128(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *data.get(*pos)?;
        *pos += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// `classes.dex`, `classes2.dex`, … of an APK, in order.
pub fn apk_dex_files(apk: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(apk)).map_err(|e| format!("apk: {e}"))?;
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|name| {
            name.strip_prefix("classes")
                .and_then(|rest| rest.strip_suffix(".dex"))
                .is_some_and(|n| n.is_empty() || n.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect();
    names.sort_by_key(|name| {
        name.trim_start_matches("classes")
            .trim_end_matches(".dex")
            .parse::<u32>()
            .unwrap_or(1)
    });
    let mut out = Vec::new();
    for name in names {
        let mut entry = archive.by_name(&name).map_err(|e| format!("{name}: {e}"))?;
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{name}: {e}"))?;
        out.push(bytes);
    }
    Ok(out)
}

/// Field sites of `(class, field)` across dex files. A method's code index
/// refers to the dex file that defines its class, which is the file whose
/// class_defs hold it: each file is scanned with its own ids.
pub fn sites_in(files: &[Vec<u8>], class: &str, field: &str) -> Vec<FieldSite> {
    let mut sites = Vec::new();
    for file in files {
        if let Ok(dex) = Dex::parse(file) {
            for site in dex.field_sites(class, field) {
                if !sites.contains(&site) {
                    sites.push(site);
                }
            }
        }
    }
    sites
}

/// Read the dex files of `package` on `serial`: `pm path` (base and
/// splits), each APK pulled once into `~/.shadowdroid/debug/<serial>/
/// apk-cache/<sha256>.apk` (keyed by the device-side hash, so a reinstall
/// is pulled again), then its `classes*.dex`. `SHADOWDROID_JDWP_DEX` (a
/// `.dex` or `.apk` path) replaces the device for tests.
pub async fn load_app_dex(serial: &str, package: &str) -> Result<Vec<Vec<u8>>, String> {
    if let Some(path) = std::env::var_os("SHADOWDROID_JDWP_DEX") {
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.to_string_lossy()))?;
        return if bytes.starts_with(b"dex\n") {
            Ok(vec![bytes])
        } else {
            apk_dex_files(&bytes)
        };
    }
    let quoted = crate::config::quote_device_shell_arg(package);
    let listing = super::transport::shell_line(serial, &format!("pm path {quoted}"))
        .await
        .map_err(|e| format!("pm path {package}: {e:#}"))?;
    let apks: Vec<String> = listing
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(str::to_string)
        .collect();
    if apks.is_empty() {
        return Err(format!("pm path found no APK for {package}"));
    }
    let cache = super::paths::ensure_serial_dir(serial)
        .map_err(|e| format!("{e:#}"))?
        .join("apk-cache");
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let mut files = Vec::new();
    for remote in apks {
        let quoted = crate::config::quote_device_shell_arg(&remote);
        let hash = super::transport::shell_line(serial, &format!("sha256sum {quoted}"))
            .await
            .map_err(|e| format!("sha256sum {remote}: {e:#}"))?;
        let hash = hash
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("sha256sum {remote}: unexpected output"));
        }
        let local = cache.join(format!("{hash}.apk"));
        if !local.exists() {
            let partial = cache.join(format!("{hash}.apk.partial"));
            let serial = serial.to_string();
            let target = partial.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let mut file = std::fs::File::create(&target)?;
                crate::device::adb_wire::pull(&serial, &remote, &mut file)
            })
            .await
            .map_err(|e| format!("apk pull worker: {e}"))?
            .map_err(|e| format!("pulling the APK: {e:#}"))?;
            std::fs::rename(&partial, &local).map_err(|e| format!("{}: {e}", local.display()))?;
        }
        let bytes = std::fs::read(&local).map_err(|e| format!("{}: {e}", local.display()))?;
        files.extend(apk_dex_files(&bytes)?);
    }
    Ok(files)
}

/// `io.example.Foo` → `Lio/example/Foo;`.
pub fn descriptor(class: &str) -> String {
    format!("L{};", class.replace('.', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/dex/fields.dex");
    const COUNTER: &str = "Lio/example/app/Counter;";

    #[test]
    fn the_fixture_parses_ids_and_class_defs() {
        let dex = Dex::parse(FIXTURE).unwrap();
        assert_eq!(dex.field(0), Some((COUNTER.into(), "count".into())));
        assert_eq!(dex.field(1), Some((COUNTER.into(), "hits".into())));
        assert_eq!(dex.field(2), None);
        assert_eq!(
            dex.method(2),
            Some((
                "Lio/example/app/Ui;".into(),
                "onClick".into(),
                "(Lio/example/app/Counter;)V".into()
            ))
        );
        assert_eq!(dex.method(0).unwrap().2, "()V");
        assert_eq!(
            dex.defined_classes(),
            [COUNTER.to_string(), "Lio/example/app/Ui;".into()]
        );
        assert!(Dex::parse(b"PK\x03\x04 not a dex at all, far too short").is_err());
    }

    #[test]
    fn write_and_read_sites_are_found_in_every_class() {
        let dex = Dex::parse(FIXTURE).unwrap();
        let sites = dex.field_sites(COUNTER, "count");
        let summary: Vec<_> = sites
            .iter()
            .map(|s| (s.class.as_str(), s.method.as_str(), s.index, s.write))
            .collect();
        assert_eq!(
            summary,
            [
                (COUNTER, "bump", 0, false),
                (COUNTER, "bump", 4, true),
                // The payload after return-void (data unit 0x5952) is skipped.
                (COUNTER, "reset", 1, true),
                ("Lio/example/app/Ui;", "onClick", 0, false),
                ("Lio/example/app/Ui;", "onClick", 2, true),
            ]
        );
        let hits = dex.field_sites(COUNTER, "hits");
        assert_eq!(
            hits.iter()
                .map(|s| (s.method.as_str(), s.index, s.write))
                .collect::<Vec<_>>(),
            [("reset", 3, false), ("onClick", 4, true)]
        );
        assert!(dex.field_sites(COUNTER, "missing").is_empty());
        assert!(
            dex.field_sites("Lio/example/app/Other;", "count")
                .is_empty()
        );
    }

    #[test]
    fn the_decoder_follows_widths_and_skips_payloads() {
        // const/16 (2), iput (2), packed-switch payload (2 targets → 8),
        // sput (2), return-void (1).
        let units = [
            0x0013, 0x0007, 0x1059, 0x0003, 0x0100, 0x0002, 0, 0, 0, 0, 0, 0, 0x0067, 0x0004,
            0x000e,
        ];
        assert_eq!(field_instructions(&units), [(2, true, 3), (12, true, 4)]);
        // A truncated instruction never panics.
        assert!(field_instructions(&[0x0059]).is_empty());
        assert_eq!(field_opcode(0x5c), Some(true));
        assert_eq!(field_opcode(0x63), Some(false));
        assert_eq!(field_opcode(0x6e), None);
    }

    #[test]
    fn apks_yield_their_dex_files_in_order_and_garbage_is_rejected() {
        use std::io::Write;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, body) in [
                ("classes2.dex", b"second".as_slice()),
                ("AndroidManifest.xml", b"<m/>".as_slice()),
                ("classes.dex", FIXTURE),
                ("classes10.dex", b"tenth".as_slice()),
            ] {
                zip.start_file(name, stored).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        let files = apk_dex_files(buffer.get_ref()).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0], FIXTURE);
        assert_eq!(files[1], b"second");
        assert_eq!(files[2], b"tenth");
        // Non-dex members are ignored; unparsable dex files are skipped.
        assert_eq!(sites_in(&files, COUNTER, "count").len(), 5);
        assert!(apk_dex_files(b"not a zip").is_err());
        assert_eq!(descriptor("io.example.app.Counter"), COUNTER);
    }
}
