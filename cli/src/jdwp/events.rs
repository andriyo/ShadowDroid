//! `Event.Composite` (command set 64, command 100) parsing.
//!
//! The VM batches every event that fired at one point into a composite packet
//! whose first byte is the strongest suspend policy among them. Each event's
//! layout depends on its kind, so an unknown kind ends parsing: the rest of
//! the packet cannot be framed. [`Composite::parse`] keeps the events read
//! before that point so the session can still resume what was suspended.

use serde_json::{Value as Json, json};

use super::codec::{CodecError, IdSizes, Location, Reader, Value};
use super::protocol::event_kind as kind;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    VmStart {
        request: i32,
        thread: u64,
    },
    SingleStep {
        request: i32,
        thread: u64,
        location: Location,
    },
    Breakpoint {
        request: i32,
        thread: u64,
        location: Location,
    },
    MethodEntry {
        request: i32,
        thread: u64,
        location: Location,
    },
    MethodExit {
        request: i32,
        thread: u64,
        location: Location,
        return_value: Option<Value>,
    },
    Monitor {
        kind: u8,
        request: i32,
        thread: u64,
        object: Value,
        location: Location,
    },
    Exception {
        request: i32,
        thread: u64,
        location: Location,
        exception: Value,
        catch_location: Option<Location>,
    },
    ThreadStart {
        request: i32,
        thread: u64,
    },
    ThreadDeath {
        request: i32,
        thread: u64,
    },
    ClassPrepare {
        request: i32,
        thread: u64,
        ref_type_tag: u8,
        type_id: u64,
        signature: String,
        status: i32,
    },
    ClassUnload {
        request: i32,
        signature: String,
    },
    Field {
        modification: bool,
        request: i32,
        thread: u64,
        location: Location,
        ref_type_tag: u8,
        type_id: u64,
        field_id: u64,
        object: Value,
        new_value: Option<Value>,
    },
    VmDeath {
        request: i32,
    },
}

impl Event {
    pub fn request_id(&self) -> i32 {
        match self {
            Event::VmStart { request, .. }
            | Event::SingleStep { request, .. }
            | Event::Breakpoint { request, .. }
            | Event::MethodEntry { request, .. }
            | Event::MethodExit { request, .. }
            | Event::Monitor { request, .. }
            | Event::Exception { request, .. }
            | Event::ThreadStart { request, .. }
            | Event::ThreadDeath { request, .. }
            | Event::ClassPrepare { request, .. }
            | Event::ClassUnload { request, .. }
            | Event::Field { request, .. }
            | Event::VmDeath { request } => *request,
        }
    }

    /// The thread the event happened on, when it has one.
    pub fn thread(&self) -> Option<u64> {
        match self {
            Event::VmStart { thread, .. }
            | Event::SingleStep { thread, .. }
            | Event::Breakpoint { thread, .. }
            | Event::MethodEntry { thread, .. }
            | Event::MethodExit { thread, .. }
            | Event::Monitor { thread, .. }
            | Event::Exception { thread, .. }
            | Event::ThreadStart { thread, .. }
            | Event::ThreadDeath { thread, .. }
            | Event::ClassPrepare { thread, .. }
            | Event::Field { thread, .. } => Some(*thread),
            Event::ClassUnload { .. } | Event::VmDeath { .. } => None,
        }
    }

    pub fn location(&self) -> Option<Location> {
        match self {
            Event::SingleStep { location, .. }
            | Event::Breakpoint { location, .. }
            | Event::MethodEntry { location, .. }
            | Event::MethodExit { location, .. }
            | Event::Monitor { location, .. }
            | Event::Exception { location, .. }
            | Event::Field { location, .. } => Some(*location),
            _ => None,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Event::VmStart { .. } => "vm_start",
            Event::SingleStep { .. } => "single_step",
            Event::Breakpoint { .. } => "breakpoint",
            Event::MethodEntry { .. } => "method_entry",
            Event::MethodExit { .. } => "method_exit",
            Event::Monitor { kind, .. } => super::protocol::event_kind_name(*kind),
            Event::Exception { .. } => "exception",
            Event::ThreadStart { .. } => "thread_start",
            Event::ThreadDeath { .. } => "thread_death",
            Event::ClassPrepare { .. } => "class_prepare",
            Event::ClassUnload { .. } => "class_unload",
            Event::Field {
                modification: true, ..
            } => "field_modification",
            Event::Field { .. } => "field_access",
            Event::VmDeath { .. } => "vm_death",
        }
    }

    /// Compact description for the daemon log and the `events` RPC.
    pub fn to_json(&self) -> Json {
        let mut value = json!({
            "kind": self.kind_name(),
            "request_id": self.request_id(),
            "thread": self.thread(),
            "location": self.location().map(location_json),
        });
        let extra = match self {
            Event::MethodExit {
                return_value: Some(v),
                ..
            } => json!({"return_value": format!("{v:?}")}),
            Event::Monitor { object, .. } => json!({"object": object.object_id()}),
            Event::Exception {
                exception,
                catch_location,
                ..
            } => json!({
                "exception": exception.object_id(),
                "caught": catch_location.is_some(),
                "catch_location": catch_location.map(location_json),
            }),
            Event::ClassPrepare {
                ref_type_tag,
                type_id,
                signature,
                status,
                ..
            } => json!({
                "ref_type_tag": ref_type_tag,
                "type_id": type_id,
                "signature": signature,
                "status": super::protocol::class_status_names(*status),
            }),
            Event::ClassUnload { signature, .. } => json!({"signature": signature}),
            Event::Field {
                ref_type_tag,
                type_id,
                field_id,
                object,
                new_value,
                ..
            } => json!({
                "ref_type_tag": ref_type_tag,
                "type_id": type_id,
                "field_id": field_id,
                "object": object.object_id(),
                "new_value": new_value.map(|v| format!("{v:?}")),
            }),
            _ => Json::Null,
        };
        if let (Json::Object(map), Json::Object(extra)) = (&mut value, extra) {
            map.extend(extra);
        }
        value
    }
}

pub fn location_json(location: Location) -> Json {
    json!({
        "type_tag": location.type_tag,
        "class_id": location.class_id,
        "method_id": location.method_id,
        "index": location.index,
    })
}

/// A parsed composite packet. `error` is set when parsing stopped early; the
/// events before it are still valid.
#[derive(Clone, Debug, PartialEq)]
pub struct Composite {
    pub suspend_policy: u8,
    pub events: Vec<Event>,
    pub error: Option<String>,
}

impl Composite {
    pub fn parse(data: &[u8], sizes: IdSizes) -> Result<Composite, CodecError> {
        let mut reader = Reader::new(data, sizes);
        let suspend_policy = reader.u8()?;
        let count = reader.count()?;
        let mut events = Vec::with_capacity(count);
        let mut error = None;
        for _ in 0..count {
            match parse_event(&mut reader) {
                Ok(event) => events.push(event),
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        Ok(Composite {
            suspend_policy,
            events,
            error,
        })
    }
}

fn parse_event(reader: &mut Reader<'_>) -> Result<Event, CodecError> {
    let event_kind = reader.u8()?;
    let request = reader.i32()?;
    Ok(match event_kind {
        kind::VM_START => Event::VmStart {
            request,
            thread: reader.object_id()?,
        },
        kind::SINGLE_STEP => Event::SingleStep {
            request,
            thread: reader.object_id()?,
            location: reader.location()?,
        },
        kind::BREAKPOINT => Event::Breakpoint {
            request,
            thread: reader.object_id()?,
            location: reader.location()?,
        },
        kind::METHOD_ENTRY => Event::MethodEntry {
            request,
            thread: reader.object_id()?,
            location: reader.location()?,
        },
        kind::METHOD_EXIT => Event::MethodExit {
            request,
            thread: reader.object_id()?,
            location: reader.location()?,
            return_value: None,
        },
        kind::METHOD_EXIT_WITH_RETURN_VALUE => Event::MethodExit {
            request,
            thread: reader.object_id()?,
            location: reader.location()?,
            return_value: Some(reader.tagged_value()?),
        },
        kind::MONITOR_CONTENDED_ENTER
        | kind::MONITOR_CONTENDED_ENTERED
        | kind::MONITOR_WAIT
        | kind::MONITOR_WAITED => {
            let thread = reader.object_id()?;
            let object = reader.tagged_object_id()?;
            let location = reader.location()?;
            match event_kind {
                kind::MONITOR_WAIT => {
                    reader.i64()?;
                }
                kind::MONITOR_WAITED => {
                    reader.bool()?;
                }
                _ => {}
            }
            Event::Monitor {
                kind: event_kind,
                request,
                thread,
                object,
                location,
            }
        }
        kind::EXCEPTION => {
            let thread = reader.object_id()?;
            let location = reader.location()?;
            let exception = reader.tagged_object_id()?;
            let catch_location = reader.location()?;
            Event::Exception {
                request,
                thread,
                location,
                exception,
                // An uncaught exception reports a zeroed catch location.
                catch_location: (catch_location.class_id != 0 || catch_location.method_id != 0)
                    .then_some(catch_location),
            }
        }
        kind::THREAD_START => Event::ThreadStart {
            request,
            thread: reader.object_id()?,
        },
        kind::THREAD_DEATH => Event::ThreadDeath {
            request,
            thread: reader.object_id()?,
        },
        kind::CLASS_PREPARE => Event::ClassPrepare {
            request,
            thread: reader.object_id()?,
            ref_type_tag: reader.u8()?,
            type_id: reader.reference_type_id()?,
            signature: reader.string()?,
            status: reader.i32()?,
        },
        kind::CLASS_UNLOAD => Event::ClassUnload {
            request,
            signature: reader.string()?,
        },
        kind::FIELD_ACCESS | kind::FIELD_MODIFICATION => {
            let modification = event_kind == kind::FIELD_MODIFICATION;
            let thread = reader.object_id()?;
            let location = reader.location()?;
            let ref_type_tag = reader.u8()?;
            let type_id = reader.reference_type_id()?;
            let field_id = reader.field_id()?;
            let object = reader.tagged_object_id()?;
            let new_value = if modification {
                Some(reader.tagged_value()?)
            } else {
                None
            };
            Event::Field {
                modification,
                request,
                thread,
                location,
                ref_type_tag,
                type_id,
                field_id,
                object,
                new_value,
            }
        }
        kind::VM_DEATH => Event::VmDeath { request },
        other => {
            return Err(CodecError::Malformed(format!(
                "unknown event kind {other} (request {request})"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jdwp::codec::Writer;
    use crate::jdwp::protocol::{suspend_policy, tag};

    fn loc(class_id: u64, method_id: u64, index: u64) -> Location {
        Location {
            type_tag: 1,
            class_id,
            method_id,
            index,
        }
    }

    #[test]
    fn parses_every_supported_event_kind() {
        let sizes = IdSizes::default();
        let mut w = Writer::new(sizes);
        w.u8(suspend_policy::ALL).i32(9);
        w.u8(kind::BREAKPOINT)
            .i32(3)
            .object_id(11)
            .location(&loc(1, 2, 3));
        w.u8(kind::SINGLE_STEP)
            .i32(4)
            .object_id(11)
            .location(&loc(1, 2, 4));
        w.u8(kind::EXCEPTION)
            .i32(5)
            .object_id(11)
            .location(&loc(1, 2, 5))
            .u8(tag::OBJECT)
            .object_id(77)
            .location(&loc(0, 0, 0));
        w.u8(kind::CLASS_PREPARE)
            .i32(6)
            .object_id(11)
            .u8(1)
            .reference_type_id(42)
            .string("Lcom/example/Foo;")
            .i32(7);
        w.u8(kind::THREAD_START).i32(0).object_id(12);
        w.u8(kind::THREAD_DEATH).i32(0).object_id(12);
        w.u8(kind::METHOD_ENTRY)
            .i32(8)
            .object_id(11)
            .location(&loc(1, 2, 0));
        w.u8(kind::FIELD_MODIFICATION)
            .i32(10)
            .object_id(11)
            .location(&loc(1, 2, 9))
            .u8(1)
            .reference_type_id(42)
            .field_id(5)
            .u8(tag::OBJECT)
            .object_id(99)
            .tagged_value(&Value::Int(3));
        w.u8(kind::VM_DEATH).i32(0);
        let composite = Composite::parse(&w.into_bytes(), sizes).unwrap();
        assert_eq!(composite.suspend_policy, suspend_policy::ALL);
        assert!(composite.error.is_none(), "{:?}", composite.error);
        assert_eq!(composite.events.len(), 9);
        assert_eq!(
            composite.events[0],
            Event::Breakpoint {
                request: 3,
                thread: 11,
                location: loc(1, 2, 3)
            }
        );
        match &composite.events[2] {
            Event::Exception {
                exception,
                catch_location,
                ..
            } => {
                assert_eq!(exception.object_id(), Some(77));
                assert!(catch_location.is_none(), "uncaught");
            }
            other => panic!("{other:?}"),
        }
        match &composite.events[3] {
            Event::ClassPrepare {
                signature, type_id, ..
            } => {
                assert_eq!(signature, "Lcom/example/Foo;");
                assert_eq!(*type_id, 42);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(composite.events[7].kind_name(), "field_modification");
        assert_eq!(composite.events[8], Event::VmDeath { request: 0 });
        let json = composite.events[3].to_json();
        assert_eq!(json["kind"], "class_prepare");
        assert_eq!(
            json["status"],
            json!(["verified", "prepared", "initialized"])
        );
    }

    #[test]
    fn an_unknown_kind_keeps_the_events_before_it() {
        let sizes = IdSizes::default();
        let mut w = Writer::new(sizes);
        w.u8(suspend_policy::EVENT_THREAD).i32(2);
        w.u8(kind::BREAKPOINT)
            .i32(1)
            .object_id(5)
            .location(&loc(1, 1, 1));
        w.u8(200).i32(1).object_id(5);
        let composite = Composite::parse(&w.into_bytes(), sizes).unwrap();
        assert_eq!(composite.events.len(), 1);
        assert!(composite.error.unwrap().contains("unknown event kind 200"));
        assert_eq!(composite.events[0].thread(), Some(5));
    }
}
