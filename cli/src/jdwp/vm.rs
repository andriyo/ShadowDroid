//! Typed JDWP commands over a [`Connection`]. One method per command the
//! backend uses; each encodes its request with the connection's IDSizes and
//! decodes the reply.

use std::sync::Arc;
use std::time::Duration;

use super::codec::{CodecError, IdSizes, Location, Reader, Value, Writer};
use super::conn::{Connection, JdwpError};
use super::protocol::{
    array_reference, class_type, event_request, method, modifier, object_reference, reference_type,
    set, stack_frame, string_reference, thread_reference, vm,
};

#[derive(Clone, Debug)]
pub struct VersionInfo {
    pub description: String,
    pub jdwp_major: i32,
    pub jdwp_minor: i32,
    pub vm_version: String,
    pub vm_name: String,
}

#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub type_id: u64,
    pub signature: String,
}

#[derive(Clone, Debug)]
pub struct MethodInfo {
    pub method_id: u64,
    pub name: String,
    pub signature: String,
    pub mod_bits: i32,
}

impl MethodInfo {
    pub fn is_static(&self) -> bool {
        self.mod_bits & super::protocol::ACC_STATIC != 0
    }
}

/// An InvokeMethod reply: the returned value, or the thrown exception.
#[derive(Clone, Copy, Debug)]
pub struct InvokeResult {
    pub value: Value,
    /// Non-null when the invoked method threw.
    pub exception: Value,
}

#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub field_id: u64,
    pub name: String,
    pub signature: String,
    pub mod_bits: i32,
}

#[derive(Clone, Debug, Default)]
pub struct LineTable {
    /// `(code_index, line_number)` in reply order.
    pub lines: Vec<(u64, i32)>,
}

impl LineTable {
    /// The source line executing at `index`: the entry with the greatest code
    /// index not after it.
    pub fn line_at(&self, index: u64) -> Option<i32> {
        self.lines
            .iter()
            .filter(|(code, _)| *code <= index)
            .max_by_key(|(code, _)| *code)
            .map(|(_, line)| *line)
    }

    /// The first code index for `line`, if this method has code on it.
    pub fn first_index_of(&self, line: i32) -> Option<u64> {
        self.lines
            .iter()
            .filter(|(_, l)| *l == line)
            .map(|(code, _)| *code)
            .min()
    }
}

#[derive(Clone, Debug)]
pub struct Variable {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    pub length: u32,
    pub slot: i32,
}

impl Variable {
    /// Whether the variable is in scope at `index`.
    pub fn visible_at(&self, index: u64) -> bool {
        index >= self.code_index && index < self.code_index + u64::from(self.length)
    }
}

/// One event-request modifier. The full JDWP set used by phase 1 is
/// encodable; the backend combines them per request kind.
#[derive(Clone, Debug)]
pub enum Modifier {
    Count(i32),
    #[allow(dead_code)] // P1b: thread-scoped requests
    ThreadOnly(u64),
    #[allow(dead_code)] // P1b: exception class filters
    ClassOnly(u64),
    ClassMatch(String),
    ClassExclude(String),
    LocationOnly(Location),
    ExceptionOnly {
        exception: u64,
        caught: bool,
        uncaught: bool,
    },
    Step {
        thread: u64,
        size: i32,
        depth: i32,
    },
    SourceNameMatch(String),
}

impl Modifier {
    fn encode(&self, w: &mut Writer) {
        match self {
            Modifier::Count(count) => {
                w.u8(modifier::COUNT).i32(*count);
            }
            Modifier::ThreadOnly(thread) => {
                w.u8(modifier::THREAD_ONLY).object_id(*thread);
            }
            Modifier::ClassOnly(class) => {
                w.u8(modifier::CLASS_ONLY).reference_type_id(*class);
            }
            Modifier::ClassMatch(pattern) => {
                w.u8(modifier::CLASS_MATCH).string(pattern);
            }
            Modifier::ClassExclude(pattern) => {
                w.u8(modifier::CLASS_EXCLUDE).string(pattern);
            }
            Modifier::LocationOnly(location) => {
                w.u8(modifier::LOCATION_ONLY).location(location);
            }
            Modifier::ExceptionOnly {
                exception,
                caught,
                uncaught,
            } => {
                w.u8(modifier::EXCEPTION_ONLY)
                    .reference_type_id(*exception)
                    .bool(*caught)
                    .bool(*uncaught);
            }
            Modifier::Step {
                thread,
                size,
                depth,
            } => {
                w.u8(modifier::STEP)
                    .object_id(*thread)
                    .i32(*size)
                    .i32(*depth);
            }
            Modifier::SourceNameMatch(pattern) => {
                w.u8(modifier::SOURCE_NAME_MATCH).string(pattern);
            }
        }
    }
}

/// Encode an `EventRequest.Set` body (shared with tests and the fake VM).
pub fn encode_event_request(
    sizes: IdSizes,
    event_kind: u8,
    suspend_policy: u8,
    modifiers: &[Modifier],
) -> Vec<u8> {
    let mut w = Writer::new(sizes);
    w.u8(event_kind)
        .u8(suspend_policy)
        .i32(modifiers.len() as i32);
    for m in modifiers {
        m.encode(&mut w);
    }
    w.into_bytes()
}

/// Typed client. Cheap to clone; all clones share the connection.
#[derive(Clone)]
pub struct Jdwp {
    conn: Arc<Connection>,
    timeout: Duration,
}

fn decode<T>(command: &str, body: impl FnOnce() -> Result<T, CodecError>) -> Result<T, JdwpError> {
    body().map_err(|source| JdwpError::Codec {
        command: command.to_string(),
        source,
    })
}

impl Jdwp {
    pub fn new(conn: Arc<Connection>, timeout: Duration) -> Self {
        Self { conn, timeout }
    }

    pub fn sizes(&self) -> IdSizes {
        self.conn.sizes()
    }

    fn writer(&self) -> Writer {
        Writer::new(self.sizes())
    }

    async fn call(
        &self,
        command_set: u8,
        command: u8,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, JdwpError> {
        self.conn
            .request(command_set, command, body, self.timeout)
            .await
    }

    async fn call_with(
        &self,
        command_set: u8,
        command: u8,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, JdwpError> {
        self.conn.request(command_set, command, body, timeout).await
    }

    // ── VirtualMachine ────────────────────────────────────────────────

    pub async fn id_sizes(&self, timeout: Duration) -> Result<IdSizes, JdwpError> {
        let data = self
            .call_with(set::VIRTUAL_MACHINE, vm::ID_SIZES, vec![], timeout)
            .await?;
        let sizes = decode("VirtualMachine.IDSizes", || {
            let mut r = Reader::new(&data, IdSizes::default());
            let sizes = IdSizes {
                field: r.i32()? as u8,
                method: r.i32()? as u8,
                object: r.i32()? as u8,
                reference_type: r.i32()? as u8,
                frame: r.i32()? as u8,
            };
            sizes.validate()?;
            Ok(sizes)
        })?;
        self.conn.set_sizes(sizes);
        Ok(sizes)
    }

    pub async fn version(&self) -> Result<VersionInfo, JdwpError> {
        let data = self.call(set::VIRTUAL_MACHINE, vm::VERSION, vec![]).await?;
        decode("VirtualMachine.Version", || {
            let mut r = Reader::new(&data, self.sizes());
            Ok(VersionInfo {
                description: r.string()?,
                jdwp_major: r.i32()?,
                jdwp_minor: r.i32()?,
                vm_version: r.string()?,
                vm_name: r.string()?,
            })
        })
    }

    /// The 32 capability booleans in reply order.
    pub async fn capabilities_new(&self) -> Result<Vec<bool>, JdwpError> {
        let data = self
            .call(set::VIRTUAL_MACHINE, vm::CAPABILITIES_NEW, vec![])
            .await?;
        decode("VirtualMachine.CapabilitiesNew", || {
            let mut r = Reader::new(&data, self.sizes());
            let mut out = Vec::with_capacity(32);
            while r.remaining() > 0 && out.len() < 32 {
                out.push(r.bool()?);
            }
            Ok(out)
        })
    }

    pub async fn classes_by_signature(&self, signature: &str) -> Result<Vec<ClassInfo>, JdwpError> {
        let mut w = self.writer();
        w.string(signature);
        let data = self
            .call(
                set::VIRTUAL_MACHINE,
                vm::CLASSES_BY_SIGNATURE,
                w.into_bytes(),
            )
            .await?;
        decode("VirtualMachine.ClassesBySignature", || {
            let mut r = Reader::new(&data, self.sizes());
            let count = r.count()?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let _ref_type_tag = r.u8()?;
                let type_id = r.reference_type_id()?;
                let _status = r.i32()?;
                out.push(ClassInfo {
                    type_id,
                    signature: signature.to_string(),
                });
            }
            Ok(out)
        })
    }

    /// Every loaded class. Large on ART (the boot classpath is included), so
    /// this gets a longer deadline than ordinary commands.
    pub async fn all_classes(&self) -> Result<Vec<ClassInfo>, JdwpError> {
        let data = self
            .call_with(
                set::VIRTUAL_MACHINE,
                vm::ALL_CLASSES_WITH_GENERIC,
                vec![],
                self.timeout.max(Duration::from_secs(30)),
            )
            .await?;
        decode("VirtualMachine.AllClassesWithGeneric", || {
            let mut r = Reader::new(&data, self.sizes());
            let count = r.count()?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let _ref_type_tag = r.u8()?;
                let type_id = r.reference_type_id()?;
                let signature = r.string()?;
                let _generic = r.string()?;
                let _status = r.i32()?;
                out.push(ClassInfo { type_id, signature });
            }
            Ok(out)
        })
    }

    pub async fn all_threads(&self) -> Result<Vec<u64>, JdwpError> {
        let data = self
            .call(set::VIRTUAL_MACHINE, vm::ALL_THREADS, vec![])
            .await?;
        decode("VirtualMachine.AllThreads", || {
            let mut r = Reader::new(&data, self.sizes());
            let count = r.count()?;
            (0..count).map(|_| r.object_id()).collect()
        })
    }

    pub async fn suspend(&self) -> Result<(), JdwpError> {
        self.call(set::VIRTUAL_MACHINE, vm::SUSPEND, vec![])
            .await
            .map(drop)
    }

    pub async fn resume(&self) -> Result<(), JdwpError> {
        self.call(set::VIRTUAL_MACHINE, vm::RESUME, vec![])
            .await
            .map(drop)
    }

    /// Dispose clears every event request and resumes the VM as many times as
    /// the debugger suspended it.
    pub async fn dispose(&self, timeout: Duration) -> Result<(), JdwpError> {
        self.call_with(set::VIRTUAL_MACHINE, vm::DISPOSE, vec![], timeout)
            .await
            .map(drop)
    }

    pub fn dispose_nowait(&self) {
        self.conn
            .send_nowait(set::VIRTUAL_MACHINE, vm::DISPOSE, vec![]);
    }

    // ── ReferenceType / ClassType ─────────────────────────────────────

    fn ref_body(&self, type_id: u64) -> Vec<u8> {
        let mut w = self.writer();
        w.reference_type_id(type_id);
        w.into_bytes()
    }

    pub async fn signature(&self, type_id: u64) -> Result<String, JdwpError> {
        let data = self
            .call(
                set::REFERENCE_TYPE,
                reference_type::SIGNATURE,
                self.ref_body(type_id),
            )
            .await?;
        decode("ReferenceType.Signature", || {
            Reader::new(&data, self.sizes()).string()
        })
    }

    /// `None` when the class has no SourceFile attribute (ABSENT_INFORMATION).
    pub async fn source_file(&self, type_id: u64) -> Result<Option<String>, JdwpError> {
        match self
            .call(
                set::REFERENCE_TYPE,
                reference_type::SOURCE_FILE,
                self.ref_body(type_id),
            )
            .await
        {
            Ok(data) => decode("ReferenceType.SourceFile", || {
                Reader::new(&data, self.sizes()).string().map(Some)
            }),
            Err(error) if error.vm_code() == Some(super::protocol::error::ABSENT_INFORMATION) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// The JSR-45 SMAP, when the class carries one (Kotlin inline functions).
    #[allow(dead_code)] // P1b: SMAP inline-body resolution
    pub async fn source_debug_extension(&self, type_id: u64) -> Result<Option<String>, JdwpError> {
        match self
            .call(
                set::REFERENCE_TYPE,
                reference_type::SOURCE_DEBUG_EXTENSION,
                self.ref_body(type_id),
            )
            .await
        {
            Ok(data) => decode("ReferenceType.SourceDebugExtension", || {
                Reader::new(&data, self.sizes()).string().map(Some)
            }),
            Err(error)
                if matches!(
                    error.vm_code(),
                    Some(super::protocol::error::ABSENT_INFORMATION) | Some(99)
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    #[allow(dead_code)] // P1b: `debug status` class readiness
    pub async fn class_status(&self, type_id: u64) -> Result<i32, JdwpError> {
        let data = self
            .call(
                set::REFERENCE_TYPE,
                reference_type::STATUS,
                self.ref_body(type_id),
            )
            .await?;
        decode("ReferenceType.Status", || {
            Reader::new(&data, self.sizes()).i32()
        })
    }

    pub async fn methods(&self, type_id: u64) -> Result<Vec<MethodInfo>, JdwpError> {
        let data = self
            .call(
                set::REFERENCE_TYPE,
                reference_type::METHODS_WITH_GENERIC,
                self.ref_body(type_id),
            )
            .await?;
        decode("ReferenceType.MethodsWithGeneric", || {
            let mut r = Reader::new(&data, self.sizes());
            let count = r.count()?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let method_id = r.method_id()?;
                let name = r.string()?;
                let signature = r.string()?;
                let _generic = r.string()?;
                let mod_bits = r.i32()?;
                out.push(MethodInfo {
                    method_id,
                    name,
                    signature,
                    mod_bits,
                });
            }
            Ok(out)
        })
    }

    pub async fn fields(&self, type_id: u64) -> Result<Vec<FieldInfo>, JdwpError> {
        let data = self
            .call(
                set::REFERENCE_TYPE,
                reference_type::FIELDS_WITH_GENERIC,
                self.ref_body(type_id),
            )
            .await?;
        decode("ReferenceType.FieldsWithGeneric", || {
            let mut r = Reader::new(&data, self.sizes());
            let count = r.count()?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let field_id = r.field_id()?;
                let name = r.string()?;
                let signature = r.string()?;
                let _generic = r.string()?;
                let mod_bits = r.i32()?;
                out.push(FieldInfo {
                    field_id,
                    name,
                    signature,
                    mod_bits,
                });
            }
            Ok(out)
        })
    }

    /// Static field values.
    pub async fn static_values(
        &self,
        type_id: u64,
        fields: &[u64],
    ) -> Result<Vec<Value>, JdwpError> {
        let mut w = self.writer();
        w.reference_type_id(type_id).i32(fields.len() as i32);
        for field in fields {
            w.field_id(*field);
        }
        let data = self
            .call(
                set::REFERENCE_TYPE,
                reference_type::GET_VALUES,
                w.into_bytes(),
            )
            .await?;
        decode("ReferenceType.GetValues", || {
            read_values(&data, self.sizes())
        })
    }

    /// `None` for `java.lang.Object` and interfaces.
    pub async fn superclass(&self, class_id: u64) -> Result<Option<u64>, JdwpError> {
        let mut w = self.writer();
        w.reference_type_id(class_id);
        let data = self
            .call(set::CLASS_TYPE, class_type::SUPERCLASS, w.into_bytes())
            .await?;
        decode("ClassType.Superclass", || {
            let id = Reader::new(&data, self.sizes()).reference_type_id()?;
            Ok((id != 0).then_some(id))
        })
    }

    // ── Method ────────────────────────────────────────────────────────

    fn method_body(&self, type_id: u64, method_id: u64) -> Vec<u8> {
        let mut w = self.writer();
        w.reference_type_id(type_id).method_id(method_id);
        w.into_bytes()
    }

    /// Empty for native/abstract methods (ABSENT_INFORMATION / NATIVE_METHOD).
    pub async fn line_table(&self, type_id: u64, method_id: u64) -> Result<LineTable, JdwpError> {
        match self
            .call(
                set::METHOD,
                method::LINE_TABLE,
                self.method_body(type_id, method_id),
            )
            .await
        {
            Ok(data) => decode("Method.LineTable", || {
                let mut r = Reader::new(&data, self.sizes());
                let _start = r.i64()?;
                let _end = r.i64()?;
                let count = r.count()?;
                let mut lines = Vec::with_capacity(count);
                for _ in 0..count {
                    lines.push((r.i64()? as u64, r.i32()?));
                }
                Ok(LineTable { lines })
            }),
            Err(error) if matches!(error.vm_code(), Some(101) | Some(511)) => {
                Ok(LineTable::default())
            }
            Err(error) => Err(error),
        }
    }

    /// Empty when the method has no local-variable table.
    pub async fn variable_table(
        &self,
        type_id: u64,
        method_id: u64,
    ) -> Result<Vec<Variable>, JdwpError> {
        match self
            .call(
                set::METHOD,
                method::VARIABLE_TABLE_WITH_GENERIC,
                self.method_body(type_id, method_id),
            )
            .await
        {
            Ok(data) => decode("Method.VariableTableWithGeneric", || {
                let mut r = Reader::new(&data, self.sizes());
                let _arg_count = r.i32()?;
                let count = r.count()?;
                let mut out = Vec::with_capacity(count);
                for _ in 0..count {
                    let code_index = r.i64()? as u64;
                    let name = r.string()?;
                    let signature = r.string()?;
                    let _generic = r.string()?;
                    let length = r.i32()?.max(0) as u32;
                    let slot = r.i32()?;
                    out.push(Variable {
                        code_index,
                        name,
                        signature,
                        length,
                        slot,
                    });
                }
                Ok(out)
            }),
            Err(error) if matches!(error.vm_code(), Some(101) | Some(511)) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    // ── ObjectReference / StringReference / ArrayReference ────────────

    fn object_body(&self, object_id: u64) -> Vec<u8> {
        let mut w = self.writer();
        w.object_id(object_id);
        w.into_bytes()
    }

    /// `(ref_type_tag, type_id)` of an object's runtime class.
    pub async fn object_type(&self, object_id: u64) -> Result<(u8, u64), JdwpError> {
        let data = self
            .call(
                set::OBJECT_REFERENCE,
                object_reference::REFERENCE_TYPE,
                self.object_body(object_id),
            )
            .await?;
        decode("ObjectReference.ReferenceType", || {
            let mut r = Reader::new(&data, self.sizes());
            Ok((r.u8()?, r.reference_type_id()?))
        })
    }

    pub async fn object_values(
        &self,
        object_id: u64,
        fields: &[u64],
    ) -> Result<Vec<Value>, JdwpError> {
        let mut w = self.writer();
        w.object_id(object_id).i32(fields.len() as i32);
        for field in fields {
            w.field_id(*field);
        }
        let data = self
            .call(
                set::OBJECT_REFERENCE,
                object_reference::GET_VALUES,
                w.into_bytes(),
            )
            .await?;
        decode("ObjectReference.GetValues", || {
            read_values(&data, self.sizes())
        })
    }

    pub async fn disable_collection(&self, object_id: u64) -> Result<(), JdwpError> {
        self.call(
            set::OBJECT_REFERENCE,
            object_reference::DISABLE_COLLECTION,
            self.object_body(object_id),
        )
        .await
        .map(drop)
    }

    pub async fn enable_collection(&self, object_id: u64) -> Result<(), JdwpError> {
        self.call(
            set::OBJECT_REFERENCE,
            object_reference::ENABLE_COLLECTION,
            self.object_body(object_id),
        )
        .await
        .map(drop)
    }

    pub async fn is_collected(&self, object_id: u64) -> Result<bool, JdwpError> {
        let data = self
            .call(
                set::OBJECT_REFERENCE,
                object_reference::IS_COLLECTED,
                self.object_body(object_id),
            )
            .await?;
        decode("ObjectReference.IsCollected", || {
            Reader::new(&data, self.sizes()).bool()
        })
    }

    pub async fn string_value(&self, object_id: u64) -> Result<String, JdwpError> {
        let data = self
            .call(
                set::STRING_REFERENCE,
                string_reference::VALUE,
                self.object_body(object_id),
            )
            .await?;
        decode("StringReference.Value", || {
            Reader::new(&data, self.sizes()).string()
        })
    }

    pub async fn array_length(&self, array_id: u64) -> Result<i32, JdwpError> {
        let data = self
            .call(
                set::ARRAY_REFERENCE,
                array_reference::LENGTH,
                self.object_body(array_id),
            )
            .await?;
        decode("ArrayReference.Length", || {
            Reader::new(&data, self.sizes()).i32()
        })
    }

    pub async fn array_values(
        &self,
        array_id: u64,
        first: i32,
        length: i32,
    ) -> Result<Vec<Value>, JdwpError> {
        let mut w = self.writer();
        w.object_id(array_id).i32(first).i32(length);
        let data = self
            .call(
                set::ARRAY_REFERENCE,
                array_reference::GET_VALUES,
                w.into_bytes(),
            )
            .await?;
        decode("ArrayReference.GetValues", || {
            Reader::new(&data, self.sizes()).array_region()
        })
    }

    // ── ThreadReference ───────────────────────────────────────────────

    pub async fn thread_name(&self, thread: u64) -> Result<String, JdwpError> {
        let data = self
            .call(
                set::THREAD_REFERENCE,
                thread_reference::NAME,
                self.object_body(thread),
            )
            .await?;
        decode("ThreadReference.Name", || {
            Reader::new(&data, self.sizes()).string()
        })
    }

    #[allow(dead_code)] // P1b: suspend-policy `thread`
    pub async fn thread_suspend(&self, thread: u64) -> Result<(), JdwpError> {
        self.call(
            set::THREAD_REFERENCE,
            thread_reference::SUSPEND,
            self.object_body(thread),
        )
        .await
        .map(drop)
    }

    pub async fn thread_resume(&self, thread: u64) -> Result<(), JdwpError> {
        self.call(
            set::THREAD_REFERENCE,
            thread_reference::RESUME,
            self.object_body(thread),
        )
        .await
        .map(drop)
    }

    /// `(thread_status, suspend_status)`.
    pub async fn thread_status(&self, thread: u64) -> Result<(i32, i32), JdwpError> {
        let data = self
            .call(
                set::THREAD_REFERENCE,
                thread_reference::STATUS,
                self.object_body(thread),
            )
            .await?;
        decode("ThreadReference.Status", || {
            let mut r = Reader::new(&data, self.sizes());
            Ok((r.i32()?, r.i32()?))
        })
    }

    pub async fn suspend_count(&self, thread: u64) -> Result<i32, JdwpError> {
        let data = self
            .call(
                set::THREAD_REFERENCE,
                thread_reference::SUSPEND_COUNT,
                self.object_body(thread),
            )
            .await?;
        decode("ThreadReference.SuspendCount", || {
            Reader::new(&data, self.sizes()).i32()
        })
    }

    pub async fn frame_count(&self, thread: u64) -> Result<i32, JdwpError> {
        let data = self
            .call(
                set::THREAD_REFERENCE,
                thread_reference::FRAME_COUNT,
                self.object_body(thread),
            )
            .await?;
        decode("ThreadReference.FrameCount", || {
            Reader::new(&data, self.sizes()).i32()
        })
    }

    /// `(frame_id, location)` for frames `[start, start + length)`; a length
    /// of -1 means all remaining frames.
    pub async fn frames(
        &self,
        thread: u64,
        start: i32,
        length: i32,
    ) -> Result<Vec<(u64, Location)>, JdwpError> {
        let mut w = self.writer();
        w.object_id(thread).i32(start).i32(length);
        let data = self
            .call(
                set::THREAD_REFERENCE,
                thread_reference::FRAMES,
                w.into_bytes(),
            )
            .await?;
        decode("ThreadReference.Frames", || {
            let mut r = Reader::new(&data, self.sizes());
            let count = r.count()?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                out.push((r.frame_id()?, r.location()?));
            }
            Ok(out)
        })
    }

    // ── StackFrame ────────────────────────────────────────────────────

    /// Values of local `slots` (`(slot, signature tag)`) in a frame.
    pub async fn frame_values(
        &self,
        thread: u64,
        frame: u64,
        slots: &[(i32, u8)],
    ) -> Result<Vec<Value>, JdwpError> {
        let mut w = self.writer();
        w.object_id(thread).frame_id(frame).i32(slots.len() as i32);
        for (slot, tag) in slots {
            w.i32(*slot).u8(*tag);
        }
        let data = self
            .call(set::STACK_FRAME, stack_frame::GET_VALUES, w.into_bytes())
            .await?;
        decode("StackFrame.GetValues", || read_values(&data, self.sizes()))
    }

    /// `this` of a frame; `None` in static and native methods.
    pub async fn this_object(&self, thread: u64, frame: u64) -> Result<Option<Value>, JdwpError> {
        let mut w = self.writer();
        w.object_id(thread).frame_id(frame);
        let data = self
            .call(set::STACK_FRAME, stack_frame::THIS_OBJECT, w.into_bytes())
            .await?;
        decode("StackFrame.ThisObject", || {
            let value = Reader::new(&data, self.sizes()).tagged_object_id()?;
            Ok((!value.is_null()).then_some(value))
        })
    }

    // ── Invocation (`--invoke`) ───────────────────────────────────────

    /// A `java.lang.String` in the debuggee (for string arguments).
    pub async fn create_string(&self, value: &str) -> Result<u64, JdwpError> {
        let mut w = self.writer();
        w.string(value);
        let data = self
            .call(set::VIRTUAL_MACHINE, vm::CREATE_STRING, w.into_bytes())
            .await?;
        decode("VirtualMachine.CreateString", || {
            Reader::new(&data, self.sizes()).object_id()
        })
    }

    /// ObjectReference.InvokeMethod on `thread`, single-threaded, bounded
    /// by `timeout`.
    #[allow(clippy::too_many_arguments)]
    pub async fn invoke_instance(
        &self,
        object: u64,
        thread: u64,
        class_id: u64,
        method_id: u64,
        args: &[Value],
        timeout: Duration,
    ) -> Result<InvokeResult, JdwpError> {
        let mut w = self.writer();
        w.object_id(object)
            .object_id(thread)
            .reference_type_id(class_id)
            .method_id(method_id)
            .i32(args.len() as i32);
        for arg in args {
            w.tagged_value(arg);
        }
        w.i32(super::protocol::invoke::SINGLE_THREADED);
        let data = self
            .call_with(
                set::OBJECT_REFERENCE,
                object_reference::INVOKE_METHOD,
                w.into_bytes(),
                timeout,
            )
            .await?;
        decode("ObjectReference.InvokeMethod", || {
            read_invoke(&data, self.sizes())
        })
    }

    /// ClassType.InvokeMethod (a static method) on `thread`.
    pub async fn invoke_static(
        &self,
        class_id: u64,
        thread: u64,
        method_id: u64,
        args: &[Value],
        timeout: Duration,
    ) -> Result<InvokeResult, JdwpError> {
        let mut w = self.writer();
        w.reference_type_id(class_id)
            .object_id(thread)
            .method_id(method_id)
            .i32(args.len() as i32);
        for arg in args {
            w.tagged_value(arg);
        }
        w.i32(super::protocol::invoke::SINGLE_THREADED);
        let data = self
            .call_with(
                set::CLASS_TYPE,
                class_type::INVOKE_METHOD,
                w.into_bytes(),
                timeout,
            )
            .await?;
        decode("ClassType.InvokeMethod", || {
            read_invoke(&data, self.sizes())
        })
    }

    // ── EventRequest ──────────────────────────────────────────────────

    pub async fn set_event(
        &self,
        event_kind: u8,
        suspend_policy: u8,
        modifiers: &[Modifier],
    ) -> Result<i32, JdwpError> {
        let body = encode_event_request(self.sizes(), event_kind, suspend_policy, modifiers);
        let data = self
            .call(set::EVENT_REQUEST, event_request::SET, body)
            .await?;
        decode("EventRequest.Set", || {
            Reader::new(&data, self.sizes()).i32()
        })
    }

    pub async fn clear_event(&self, event_kind: u8, request_id: i32) -> Result<(), JdwpError> {
        let mut w = self.writer();
        w.u8(event_kind).i32(request_id);
        self.call(set::EVENT_REQUEST, event_request::CLEAR, w.into_bytes())
            .await
            .map(drop)
    }

    #[allow(dead_code)] // P1b: `debug break clear`
    pub async fn clear_all_breakpoints(&self) -> Result<(), JdwpError> {
        self.call(
            set::EVENT_REQUEST,
            event_request::CLEAR_ALL_BREAKPOINTS,
            vec![],
        )
        .await
        .map(drop)
    }
}

fn read_invoke(data: &[u8], sizes: IdSizes) -> Result<InvokeResult, CodecError> {
    let mut r = Reader::new(data, sizes);
    Ok(InvokeResult {
        value: r.tagged_value()?,
        exception: r.tagged_object_id()?,
    })
}

fn read_values(data: &[u8], sizes: IdSizes) -> Result<Vec<Value>, CodecError> {
    let mut r = Reader::new(data, sizes);
    let count = r.count()?;
    (0..count).map(|_| r.tagged_value()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_tables_answer_line_at_and_first_index() {
        let table = LineTable {
            lines: vec![(0, 10), (8, 11), (16, 12), (24, 11)],
        };
        assert_eq!(table.line_at(0), Some(10));
        assert_eq!(table.line_at(9), Some(11));
        assert_eq!(table.line_at(30), Some(11));
        assert_eq!(table.first_index_of(11), Some(8));
        assert_eq!(table.first_index_of(99), None);
        assert_eq!(LineTable::default().line_at(3), None);
    }

    #[test]
    fn variable_scope_is_half_open() {
        let variable = Variable {
            code_index: 4,
            name: "x".into(),
            signature: "I".into(),
            length: 6,
            slot: 1,
        };
        assert!(!variable.visible_at(3));
        assert!(variable.visible_at(4));
        assert!(variable.visible_at(9));
        assert!(!variable.visible_at(10));
    }

    #[test]
    fn event_requests_encode_every_modifier() {
        let sizes = IdSizes::default();
        let body = encode_event_request(
            sizes,
            2,
            2,
            &[
                Modifier::Count(1),
                Modifier::ThreadOnly(5),
                Modifier::ClassOnly(6),
                Modifier::ClassMatch("a.*".into()),
                Modifier::ClassExclude("java.*".into()),
                Modifier::LocationOnly(Location {
                    type_tag: 1,
                    class_id: 2,
                    method_id: 3,
                    index: 4,
                }),
                Modifier::ExceptionOnly {
                    exception: 0,
                    caught: true,
                    uncaught: false,
                },
                Modifier::Step {
                    thread: 9,
                    size: 1,
                    depth: 1,
                },
                Modifier::SourceNameMatch("Foo.kt".into()),
            ],
        );
        let mut r = Reader::new(&body, sizes);
        assert_eq!(
            (r.u8().unwrap(), r.u8().unwrap(), r.i32().unwrap()),
            (2, 2, 9)
        );
        assert_eq!((r.u8().unwrap(), r.i32().unwrap()), (modifier::COUNT, 1));
        assert_eq!(
            (r.u8().unwrap(), r.object_id().unwrap()),
            (modifier::THREAD_ONLY, 5)
        );
        assert_eq!(
            (r.u8().unwrap(), r.reference_type_id().unwrap()),
            (modifier::CLASS_ONLY, 6)
        );
        assert_eq!(
            (r.u8().unwrap(), r.string().unwrap()),
            (modifier::CLASS_MATCH, "a.*".into())
        );
        assert_eq!(
            (r.u8().unwrap(), r.string().unwrap()),
            (modifier::CLASS_EXCLUDE, "java.*".into())
        );
        assert_eq!(r.u8().unwrap(), modifier::LOCATION_ONLY);
        assert_eq!(r.location().unwrap().index, 4);
        assert_eq!(r.u8().unwrap(), modifier::EXCEPTION_ONLY);
        assert_eq!(r.reference_type_id().unwrap(), 0);
        assert!(r.bool().unwrap());
        assert!(!r.bool().unwrap());
        assert_eq!(r.u8().unwrap(), modifier::STEP);
        assert_eq!(
            (r.object_id().unwrap(), r.i32().unwrap(), r.i32().unwrap()),
            (9, 1, 1)
        );
        assert_eq!(
            (r.u8().unwrap(), r.string().unwrap()),
            (modifier::SOURCE_NAME_MATCH, "Foo.kt".into())
        );
        assert_eq!(r.remaining(), 0);
    }
}
