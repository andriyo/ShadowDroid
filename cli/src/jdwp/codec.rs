//! JDWP wire codec: packet framing plus IDSizes-aware readers and writers for
//! the primitive, id, location, and tagged-value encodings.
//!
//! Every multi-byte quantity is big-endian. Object, reference-type, method,
//! field, and frame ids have VM-chosen widths (`VirtualMachine.IDSizes`);
//! they are carried as `u64` here and narrowed/widened at the wire.

use super::protocol::{FLAG_REPLY, HEADER_LEN, tag};

/// Largest packet accepted from the VM. A JDWP reply for `AllClasses` on a
/// large app is a few MiB; anything far beyond that is a framing error.
pub const MAX_PACKET_LEN: usize = 64 * 1024 * 1024;

/// Widths of the five variable-size id kinds. Defaults to 8 bytes each, which
/// is what ART reports; the real values are read with `IDSizes` at attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdSizes {
    pub field: u8,
    pub method: u8,
    pub object: u8,
    pub reference_type: u8,
    pub frame: u8,
}

impl Default for IdSizes {
    fn default() -> Self {
        Self {
            field: 8,
            method: 8,
            object: 8,
            reference_type: 8,
            frame: 8,
        }
    }
}

impl IdSizes {
    /// Every width must be 1..=8 bytes for the `u64` carrier.
    pub fn validate(&self) -> Result<(), CodecError> {
        for (name, size) in [
            ("field", self.field),
            ("method", self.method),
            ("object", self.object),
            ("reference_type", self.reference_type),
            ("frame", self.frame),
        ] {
            if !(1..=8).contains(&size) {
                return Err(CodecError::Malformed(format!(
                    "unsupported {name} id size {size}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("packet truncated: needed {needed} more byte(s) at offset {offset}")]
    Truncated { offset: usize, needed: usize },
    #[error("malformed packet: {0}")]
    Malformed(String),
}

/// One framed JDWP packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub id: u32,
    pub kind: PacketKind,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    Command { command_set: u8, command: u8 },
    Reply { error: u16 },
}

impl Packet {
    pub fn command(id: u32, command_set: u8, command: u8, data: Vec<u8>) -> Self {
        Self {
            id,
            kind: PacketKind::Command {
                command_set,
                command,
            },
            data,
        }
    }

    #[cfg(test)]
    pub fn reply(id: u32, error: u16, data: Vec<u8>) -> Self {
        Self {
            id,
            kind: PacketKind::Reply { error },
            data,
        }
    }

    /// Wire bytes: header followed by data.
    pub fn encode(&self) -> Vec<u8> {
        let length = (HEADER_LEN + self.data.len()) as u32;
        let mut out = Vec::with_capacity(length as usize);
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&self.id.to_be_bytes());
        match self.kind {
            PacketKind::Command {
                command_set,
                command,
            } => {
                out.push(0);
                out.push(command_set);
                out.push(command);
            }
            PacketKind::Reply { error } => {
                out.push(FLAG_REPLY);
                out.extend_from_slice(&error.to_be_bytes());
            }
        }
        out.extend_from_slice(&self.data);
        out
    }

    /// Parse a header; returns the total packet length and a packet with an
    /// empty data buffer to be filled with `length - HEADER_LEN` bytes.
    pub fn decode_header(header: &[u8; HEADER_LEN]) -> Result<(usize, Packet), CodecError> {
        let length = u32::from_be_bytes(header[0..4].try_into().expect("4 bytes")) as usize;
        if length < HEADER_LEN {
            return Err(CodecError::Malformed(format!(
                "packet length {length} is shorter than the header"
            )));
        }
        if length > MAX_PACKET_LEN {
            return Err(CodecError::Malformed(format!(
                "packet length {length} exceeds the {MAX_PACKET_LEN}-byte limit"
            )));
        }
        let id = u32::from_be_bytes(header[4..8].try_into().expect("4 bytes"));
        let flags = header[8];
        let kind = if flags & FLAG_REPLY != 0 {
            PacketKind::Reply {
                error: u16::from_be_bytes([header[9], header[10]]),
            }
        } else {
            PacketKind::Command {
                command_set: header[9],
                command: header[10],
            }
        };
        Ok((
            length,
            Packet {
                id,
                kind,
                data: Vec::new(),
            },
        ))
    }

    /// Decode one complete packet from `bytes` (header + data).
    #[cfg(test)]
    pub fn decode(bytes: &[u8]) -> Result<Packet, CodecError> {
        if bytes.len() < HEADER_LEN {
            return Err(CodecError::Truncated {
                offset: bytes.len(),
                needed: HEADER_LEN - bytes.len(),
            });
        }
        let header: [u8; HEADER_LEN] = bytes[..HEADER_LEN].try_into().expect("header");
        let (length, mut packet) = Packet::decode_header(&header)?;
        if bytes.len() != length {
            return Err(CodecError::Malformed(format!(
                "packet length {length} does not match {} supplied bytes",
                bytes.len()
            )));
        }
        packet.data = bytes[HEADER_LEN..].to_vec();
        Ok(packet)
    }
}

/// A code location: class, method, and bytecode index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Location {
    pub type_tag: u8,
    pub class_id: u64,
    pub method_id: u64,
    pub index: u64,
}

impl Location {
    /// A native method frame: JDWP sends code index -1.
    pub fn is_native(&self) -> bool {
        self.index == u64::MAX
    }
}

/// A JDWP value. Object-like values keep their tag so strings, arrays,
/// threads, and class objects stay distinguishable without a round trip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Void,
    Boolean(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// `tag` is one of the object tags; `id == 0` is `null`.
    Object {
        tag: u8,
        id: u64,
    },
}

impl Value {
    pub fn tag(&self) -> u8 {
        match self {
            Value::Void => tag::VOID,
            Value::Boolean(_) => tag::BOOLEAN,
            Value::Byte(_) => tag::BYTE,
            Value::Char(_) => tag::CHAR,
            Value::Short(_) => tag::SHORT,
            Value::Int(_) => tag::INT,
            Value::Long(_) => tag::LONG,
            Value::Float(_) => tag::FLOAT,
            Value::Double(_) => tag::DOUBLE,
            Value::Object { tag, .. } => *tag,
        }
    }

    /// The object id, if this is a non-null reference.
    pub fn object_id(&self) -> Option<u64> {
        match self {
            Value::Object { id, .. } if *id != 0 => Some(*id),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Object { id: 0, .. })
    }
}

/// Appends JDWP-encoded fields to a packet body.
pub struct Writer {
    buf: Vec<u8>,
    sizes: IdSizes,
}

impl Writer {
    pub fn new(sizes: IdSizes) -> Self {
        Self {
            buf: Vec::new(),
            sizes,
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.u8(u8::from(value))
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn i64(&mut self, value: i64) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// JDWP string: 4-byte length + (modified) UTF-8 bytes.
    pub fn string(&mut self, value: &str) -> &mut Self {
        self.i32(value.len() as i32);
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    fn sized(&mut self, value: u64, size: u8) -> &mut Self {
        let bytes = value.to_be_bytes();
        self.buf.extend_from_slice(&bytes[8 - size as usize..]);
        self
    }

    pub fn object_id(&mut self, value: u64) -> &mut Self {
        self.sized(value, self.sizes.object)
    }

    pub fn reference_type_id(&mut self, value: u64) -> &mut Self {
        self.sized(value, self.sizes.reference_type)
    }

    pub fn method_id(&mut self, value: u64) -> &mut Self {
        self.sized(value, self.sizes.method)
    }

    pub fn field_id(&mut self, value: u64) -> &mut Self {
        self.sized(value, self.sizes.field)
    }

    pub fn frame_id(&mut self, value: u64) -> &mut Self {
        self.sized(value, self.sizes.frame)
    }

    pub fn location(&mut self, location: &Location) -> &mut Self {
        self.u8(location.type_tag)
            .reference_type_id(location.class_id)
            .method_id(location.method_id)
            .i64(location.index as i64)
    }

    /// A value without its tag (array regions of primitives, field writes).
    pub fn untagged_value(&mut self, value: &Value) -> &mut Self {
        match *value {
            Value::Void => self,
            Value::Boolean(v) => self.bool(v),
            Value::Byte(v) => self.u8(v as u8),
            Value::Char(v) => {
                self.buf.extend_from_slice(&v.to_be_bytes());
                self
            }
            Value::Short(v) => {
                self.buf.extend_from_slice(&v.to_be_bytes());
                self
            }
            Value::Int(v) => self.i32(v),
            Value::Long(v) => self.i64(v),
            Value::Float(v) => {
                self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
                self
            }
            Value::Double(v) => {
                self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
                self
            }
            Value::Object { id, .. } => self.object_id(id),
        }
    }

    pub fn tagged_value(&mut self, value: &Value) -> &mut Self {
        self.u8(value.tag());
        self.untagged_value(value)
    }
}

/// Reads JDWP-encoded fields from a packet body.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    sizes: IdSizes,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8], sizes: IdSizes) -> Self {
        Self {
            data,
            pos: 0,
            sizes,
        }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < n {
            return Err(CodecError::Truncated {
                offset: self.pos,
                needed: n - self.remaining(),
            });
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, CodecError> {
        Ok(self.u8()? != 0)
    }

    pub fn i32(&mut self) -> Result<i32, CodecError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().expect("4")))
    }

    pub fn i64(&mut self) -> Result<i64, CodecError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().expect("8")))
    }

    /// A non-negative count (`int` on the wire). A negative or absurd count
    /// is a framing error rather than a huge allocation.
    pub fn count(&mut self) -> Result<usize, CodecError> {
        let value = self.i32()?;
        let count = usize::try_from(value)
            .map_err(|_| CodecError::Malformed(format!("negative count {value}")))?;
        if count > self.remaining() {
            // Every element is at least one byte.
            return Err(CodecError::Malformed(format!(
                "count {count} exceeds the {} remaining byte(s)",
                self.remaining()
            )));
        }
        Ok(count)
    }

    pub fn string(&mut self) -> Result<String, CodecError> {
        let length = self.i32()?;
        let length = usize::try_from(length)
            .map_err(|_| CodecError::Malformed(format!("negative string length {length}")))?;
        let bytes = self.take(length)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn sized(&mut self, size: u8) -> Result<u64, CodecError> {
        let bytes = self.take(size as usize)?;
        Ok(bytes
            .iter()
            .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte)))
    }

    pub fn object_id(&mut self) -> Result<u64, CodecError> {
        self.sized(self.sizes.object)
    }

    pub fn reference_type_id(&mut self) -> Result<u64, CodecError> {
        self.sized(self.sizes.reference_type)
    }

    pub fn method_id(&mut self) -> Result<u64, CodecError> {
        self.sized(self.sizes.method)
    }

    pub fn field_id(&mut self) -> Result<u64, CodecError> {
        self.sized(self.sizes.field)
    }

    pub fn frame_id(&mut self) -> Result<u64, CodecError> {
        self.sized(self.sizes.frame)
    }

    pub fn location(&mut self) -> Result<Location, CodecError> {
        Ok(Location {
            type_tag: self.u8()?,
            class_id: self.reference_type_id()?,
            method_id: self.method_id()?,
            index: self.i64()? as u64,
        })
    }

    /// A value whose tag is known from context (array regions, `tag` byte
    /// already consumed).
    pub fn untagged_value(&mut self, value_tag: u8) -> Result<Value, CodecError> {
        Ok(match value_tag {
            tag::VOID => Value::Void,
            tag::BOOLEAN => Value::Boolean(self.bool()?),
            tag::BYTE => Value::Byte(self.u8()? as i8),
            tag::CHAR => Value::Char(u16::from_be_bytes(self.take(2)?.try_into().expect("2"))),
            tag::SHORT => Value::Short(i16::from_be_bytes(self.take(2)?.try_into().expect("2"))),
            tag::INT => Value::Int(self.i32()?),
            tag::LONG => Value::Long(self.i64()?),
            tag::FLOAT => Value::Float(f32::from_bits(u32::from_be_bytes(
                self.take(4)?.try_into().expect("4"),
            ))),
            tag::DOUBLE => Value::Double(f64::from_bits(u64::from_be_bytes(
                self.take(8)?.try_into().expect("8"),
            ))),
            other if tag::is_object(other) => Value::Object {
                tag: other,
                id: self.object_id()?,
            },
            other => {
                return Err(CodecError::Malformed(format!(
                    "unknown value tag {other:#04x}"
                )));
            }
        })
    }

    pub fn tagged_value(&mut self) -> Result<Value, CodecError> {
        let value_tag = self.u8()?;
        self.untagged_value(value_tag)
    }

    /// `tagged-objectID`: an object tag followed by an object id.
    pub fn tagged_object_id(&mut self) -> Result<Value, CodecError> {
        let value_tag = self.u8()?;
        if !tag::is_object(value_tag) {
            return Err(CodecError::Malformed(format!(
                "tag {value_tag:#04x} is not an object tag"
            )));
        }
        Ok(Value::Object {
            tag: value_tag,
            id: self.object_id()?,
        })
    }

    /// `ArrayReference.GetValues` region: element tag, count, then values that
    /// are untagged for primitives and tagged for references.
    pub fn array_region(&mut self) -> Result<Vec<Value>, CodecError> {
        let region_tag = self.u8()?;
        let count = self.i32()?;
        let count = usize::try_from(count)
            .map_err(|_| CodecError::Malformed(format!("negative array region count {count}")))?;
        // Every element takes at least one byte; a `void` region takes none
        // and would let a garbage count spin for billions of iterations.
        if region_tag == tag::VOID || count > self.remaining() {
            return Err(CodecError::Malformed(format!(
                "array region of {count} element(s) tagged {region_tag:#04x} with {} byte(s) left",
                self.remaining()
            )));
        }
        let mut values = Vec::with_capacity(count.min(self.remaining()));
        let primitive = !tag::is_object(region_tag);
        for _ in 0..count {
            values.push(if primitive {
                self.untagged_value(region_tag)?
            } else {
                self.tagged_value()?
            });
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minus_one_code_index_is_a_native_frame() {
        let native = Location {
            type_tag: 1,
            class_id: 1,
            method_id: 2,
            index: u64::MAX,
        };
        assert!(native.is_native());
        assert!(!Location { index: 0, ..native }.is_native());
    }
    use proptest::prelude::*;

    fn sizes_strategy() -> impl Strategy<Value = IdSizes> {
        (1u8..=8, 1u8..=8, 1u8..=8, 1u8..=8, 1u8..=8).prop_map(
            |(field, method, object, reference_type, frame)| IdSizes {
                field,
                method,
                object,
                reference_type,
                frame,
            },
        )
    }

    fn mask(value: u64, size: u8) -> u64 {
        if size >= 8 {
            value
        } else {
            value & ((1_u64 << (size as u32 * 8)) - 1)
        }
    }

    fn value_strategy() -> impl Strategy<Value = Value> {
        prop_oneof![
            any::<bool>().prop_map(Value::Boolean),
            any::<i8>().prop_map(Value::Byte),
            any::<u16>().prop_map(Value::Char),
            any::<i16>().prop_map(Value::Short),
            any::<i32>().prop_map(Value::Int),
            any::<i64>().prop_map(Value::Long),
            any::<u32>().prop_map(|bits| Value::Float(f32::from_bits(bits))),
            any::<u64>().prop_map(|bits| Value::Double(f64::from_bits(bits))),
            (
                prop::sample::select(vec![
                    tag::OBJECT,
                    tag::STRING,
                    tag::ARRAY,
                    tag::THREAD,
                    tag::THREAD_GROUP,
                    tag::CLASS_LOADER,
                    tag::CLASS_OBJECT
                ]),
                any::<u64>()
            )
                .prop_map(|(tag, id)| Value::Object { tag, id }),
            Just(Value::Void),
        ]
    }

    /// Bit-exact equality (NaN payloads included).
    fn same(left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
            _ => left == right,
        }
    }

    fn narrow(value: Value, sizes: IdSizes) -> Value {
        match value {
            Value::Object { tag, id } => Value::Object {
                tag,
                id: mask(id, sizes.object),
            },
            other => other,
        }
    }

    proptest! {
        #[test]
        fn packets_round_trip(
            id in any::<u32>(),
            reply in any::<bool>(),
            a in any::<u8>(),
            b in any::<u8>(),
            error in any::<u16>(),
            data in prop::collection::vec(any::<u8>(), 0..256),
        ) {
            let packet = if reply {
                Packet::reply(id, error, data)
            } else {
                Packet::command(id, a, b, data)
            };
            let bytes = packet.encode();
            prop_assert_eq!(bytes.len(), HEADER_LEN + packet.data.len());
            prop_assert_eq!(Packet::decode(&bytes).unwrap(), packet);
        }

        #[test]
        fn tagged_values_round_trip(sizes in sizes_strategy(), value in value_strategy()) {
            let value = narrow(value, sizes);
            let mut writer = Writer::new(sizes);
            writer.tagged_value(&value);
            let bytes = writer.into_bytes();
            let mut reader = Reader::new(&bytes, sizes);
            let decoded = reader.tagged_value().unwrap();
            prop_assert!(same(&decoded, &value), "{decoded:?} != {value:?}");
            prop_assert_eq!(reader.remaining(), 0);
        }

        #[test]
        fn ids_locations_and_strings_round_trip(
            sizes in sizes_strategy(),
            class_id in any::<u64>(),
            method_id in any::<u64>(),
            field_id in any::<u64>(),
            frame_id in any::<u64>(),
            index in any::<u64>(),
            type_tag in 1u8..=3,
            text in ".{0,40}",
        ) {
            let location = Location {
                type_tag,
                class_id: mask(class_id, sizes.reference_type),
                method_id: mask(method_id, sizes.method),
                index,
            };
            let mut writer = Writer::new(sizes);
            writer
                .location(&location)
                .field_id(mask(field_id, sizes.field))
                .frame_id(mask(frame_id, sizes.frame))
                .string(&text)
                .bool(true)
                .i32(-7);
            let bytes = writer.into_bytes();
            let mut reader = Reader::new(&bytes, sizes);
            prop_assert_eq!(reader.location().unwrap(), location);
            prop_assert_eq!(reader.field_id().unwrap(), mask(field_id, sizes.field));
            prop_assert_eq!(reader.frame_id().unwrap(), mask(frame_id, sizes.frame));
            prop_assert_eq!(reader.string().unwrap(), text);
            prop_assert!(reader.bool().unwrap());
            prop_assert_eq!(reader.i32().unwrap(), -7);
            prop_assert_eq!(reader.remaining(), 0);
        }

        #[test]
        fn primitive_array_regions_round_trip(sizes in sizes_strategy(), items in prop::collection::vec(any::<i32>(), 0..64)) {
            let mut writer = Writer::new(sizes);
            writer.u8(tag::INT).i32(items.len() as i32);
            for item in &items {
                writer.untagged_value(&Value::Int(*item));
            }
            let bytes = writer.into_bytes();
            let decoded = Reader::new(&bytes, sizes).array_region().unwrap();
            prop_assert_eq!(decoded, items.into_iter().map(Value::Int).collect::<Vec<_>>());
        }

        #[test]
        fn readers_never_panic_on_garbage(sizes in sizes_strategy(), data in prop::collection::vec(any::<u8>(), 0..128)) {
            let mut reader = Reader::new(&data, sizes);
            let _ = reader.tagged_value();
            let _ = reader.array_region();
            let _ = reader.string();
            let _ = reader.location();
            let _ = reader.count();
        }
    }

    #[test]
    fn object_array_regions_are_tagged_per_element() {
        let sizes = IdSizes::default();
        let mut writer = Writer::new(sizes);
        writer
            .u8(tag::OBJECT)
            .i32(2)
            .tagged_value(&Value::Object {
                tag: tag::STRING,
                id: 7,
            })
            .tagged_value(&Value::Object {
                tag: tag::OBJECT,
                id: 0,
            });
        let bytes = writer.into_bytes();
        let values = Reader::new(&bytes, sizes).array_region().unwrap();
        assert_eq!(values[0].object_id(), Some(7));
        assert!(values[1].is_null());
    }

    #[test]
    fn headers_reject_impossible_lengths() {
        let mut header = [0_u8; HEADER_LEN];
        header[3] = 5;
        assert!(Packet::decode_header(&header).is_err());
        header[0] = 0x7f;
        assert!(Packet::decode_header(&header).is_err());
        assert!(matches!(
            Packet::decode(&[0, 0, 0]),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn counts_and_strings_are_bounded_by_the_buffer() {
        let sizes = IdSizes::default();
        let bytes = 1000_i32.to_be_bytes();
        assert!(Reader::new(&bytes, sizes).count().is_err());
        let bytes = (-1_i32).to_be_bytes();
        assert!(Reader::new(&bytes, sizes).string().is_err());
        assert!(
            IdSizes {
                object: 9,
                ..IdSizes::default()
            }
            .validate()
            .is_err()
        );
        IdSizes::default().validate().unwrap();
    }
}
