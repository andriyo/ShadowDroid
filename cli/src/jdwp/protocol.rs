//! JDWP constants: command sets, commands, event kinds, tags, and error codes,
//! as numbered by the Java Debug Wire Protocol specification. Only the subset
//! the standalone debugger speaks is named; unknown numbers still render
//! through [`command_name`] / [`error_name`] for diagnostics.

/// Handshake bytes both sides exchange before the first packet.
pub const HANDSHAKE: &[u8; 14] = b"JDWP-Handshake";
/// Packet header: length (4) + id (4) + flags (1) + command set/cmd or error (2).
pub const HEADER_LEN: usize = 11;
/// Reply packets carry this bit in the flags byte.
pub const FLAG_REPLY: u8 = 0x80;

pub mod set {
    pub const VIRTUAL_MACHINE: u8 = 1;
    pub const REFERENCE_TYPE: u8 = 2;
    pub const CLASS_TYPE: u8 = 3;
    pub const METHOD: u8 = 6;
    pub const OBJECT_REFERENCE: u8 = 9;
    pub const STRING_REFERENCE: u8 = 10;
    pub const THREAD_REFERENCE: u8 = 11;
    pub const ARRAY_REFERENCE: u8 = 13;
    pub const EVENT_REQUEST: u8 = 15;
    pub const STACK_FRAME: u8 = 16;
    pub const EVENT: u8 = 64;
    /// Android DDM chunks. ART may send these unsolicited; the debugger
    /// ignores them.
    pub const DDM: u8 = 199;
}

pub mod vm {
    pub const VERSION: u8 = 1;
    pub const CLASSES_BY_SIGNATURE: u8 = 2;
    pub const ALL_THREADS: u8 = 4;
    pub const DISPOSE: u8 = 6;
    pub const ID_SIZES: u8 = 7;
    pub const SUSPEND: u8 = 8;
    pub const RESUME: u8 = 9;
    pub const CREATE_STRING: u8 = 11;
    pub const CAPABILITIES_NEW: u8 = 17;
    pub const ALL_CLASSES_WITH_GENERIC: u8 = 20;
}

pub mod reference_type {
    pub const SIGNATURE: u8 = 1;
    pub const GET_VALUES: u8 = 6;
    pub const SOURCE_FILE: u8 = 7;
    pub const STATUS: u8 = 9;
    pub const SOURCE_DEBUG_EXTENSION: u8 = 12;
    pub const FIELDS_WITH_GENERIC: u8 = 14;
    pub const METHODS_WITH_GENERIC: u8 = 15;
}

pub mod class_type {
    pub const SUPERCLASS: u8 = 1;
    pub const INVOKE_METHOD: u8 = 3;
}

/// `options` bits of the InvokeMethod commands.
pub mod invoke {
    /// Only the invoking thread runs; every other thread stays suspended.
    pub const SINGLE_THREADED: i32 = 0x01;
}

pub mod method {
    pub const LINE_TABLE: u8 = 1;
    pub const BYTECODES: u8 = 3;
    pub const VARIABLE_TABLE_WITH_GENERIC: u8 = 5;
}

pub mod object_reference {
    pub const REFERENCE_TYPE: u8 = 1;
    pub const GET_VALUES: u8 = 2;
    pub const INVOKE_METHOD: u8 = 6;
    pub const DISABLE_COLLECTION: u8 = 7;
    pub const ENABLE_COLLECTION: u8 = 8;
    pub const IS_COLLECTED: u8 = 9;
}

pub mod string_reference {
    pub const VALUE: u8 = 1;
}

pub mod thread_reference {
    pub const NAME: u8 = 1;
    pub const SUSPEND: u8 = 2;
    pub const RESUME: u8 = 3;
    pub const STATUS: u8 = 4;
    pub const FRAMES: u8 = 6;
    pub const FRAME_COUNT: u8 = 7;
    pub const SUSPEND_COUNT: u8 = 12;
}

pub mod array_reference {
    pub const LENGTH: u8 = 1;
    pub const GET_VALUES: u8 = 2;
}

pub mod event_request {
    pub const SET: u8 = 1;
    pub const CLEAR: u8 = 2;
    pub const CLEAR_ALL_BREAKPOINTS: u8 = 3;
}

pub mod stack_frame {
    pub const GET_VALUES: u8 = 1;
    pub const THIS_OBJECT: u8 = 3;
}

pub mod event {
    pub const COMPOSITE: u8 = 100;
}

/// Event kinds (`EventKind` constants).
pub mod event_kind {
    pub const SINGLE_STEP: u8 = 1;
    pub const BREAKPOINT: u8 = 2;
    pub const EXCEPTION: u8 = 4;
    pub const THREAD_START: u8 = 6;
    pub const THREAD_DEATH: u8 = 7;
    pub const CLASS_PREPARE: u8 = 8;
    pub const CLASS_UNLOAD: u8 = 9;
    pub const FIELD_ACCESS: u8 = 20;
    pub const FIELD_MODIFICATION: u8 = 21;
    pub const METHOD_ENTRY: u8 = 40;
    pub const METHOD_EXIT: u8 = 41;
    pub const METHOD_EXIT_WITH_RETURN_VALUE: u8 = 42;
    pub const MONITOR_CONTENDED_ENTER: u8 = 43;
    pub const MONITOR_CONTENDED_ENTERED: u8 = 44;
    pub const MONITOR_WAIT: u8 = 45;
    pub const MONITOR_WAITED: u8 = 46;
    pub const VM_START: u8 = 90;
    pub const VM_DEATH: u8 = 99;
}

/// Event request modifier kinds.
pub mod modifier {
    pub const COUNT: u8 = 1;
    pub const THREAD_ONLY: u8 = 3;
    pub const CLASS_ONLY: u8 = 4;
    pub const CLASS_MATCH: u8 = 5;
    pub const CLASS_EXCLUDE: u8 = 6;
    pub const LOCATION_ONLY: u8 = 7;
    pub const EXCEPTION_ONLY: u8 = 8;
    pub const FIELD_ONLY: u8 = 9;
    pub const STEP: u8 = 10;
    pub const SOURCE_NAME_MATCH: u8 = 12;
}

pub mod suspend_policy {
    pub const NONE: u8 = 0;
    pub const EVENT_THREAD: u8 = 1;
    pub const ALL: u8 = 2;
}

pub mod step {
    pub const SIZE_LINE: i32 = 1;
    pub const DEPTH_INTO: i32 = 0;
    pub const DEPTH_OVER: i32 = 1;
    pub const DEPTH_OUT: i32 = 2;
}

/// `TypeTag` for reference types.
pub mod type_tag {
    pub const CLASS: u8 = 1;
    pub const INTERFACE: u8 = 2;
    pub const ARRAY: u8 = 3;
}

/// Value tags (`Tag` constants): the first byte of a tagged value.
pub mod tag {
    pub const ARRAY: u8 = b'[';
    pub const BYTE: u8 = b'B';
    pub const CHAR: u8 = b'C';
    pub const OBJECT: u8 = b'L';
    pub const FLOAT: u8 = b'F';
    pub const DOUBLE: u8 = b'D';
    pub const INT: u8 = b'I';
    pub const LONG: u8 = b'J';
    pub const SHORT: u8 = b'S';
    pub const VOID: u8 = b'V';
    pub const BOOLEAN: u8 = b'Z';
    pub const STRING: u8 = b's';
    pub const THREAD: u8 = b't';
    pub const THREAD_GROUP: u8 = b'g';
    pub const CLASS_LOADER: u8 = b'l';
    pub const CLASS_OBJECT: u8 = b'c';

    /// Tags whose payload is an object id.
    pub fn is_object(tag: u8) -> bool {
        matches!(
            tag,
            ARRAY | OBJECT | STRING | THREAD | THREAD_GROUP | CLASS_LOADER | CLASS_OBJECT
        )
    }

    /// The tag a JNI type signature's values carry (`I` → INT, `Lx;` → OBJECT).
    pub fn for_signature(signature: &str) -> u8 {
        match signature.as_bytes().first().copied() {
            Some(b'Z') => BOOLEAN,
            Some(b'B') => BYTE,
            Some(b'C') => CHAR,
            Some(b'S') => SHORT,
            Some(b'I') => INT,
            Some(b'J') => LONG,
            Some(b'F') => FLOAT,
            Some(b'D') => DOUBLE,
            Some(b'[') => ARRAY,
            Some(b'V') => VOID,
            _ => OBJECT,
        }
    }
}

pub mod thread_status {
    pub const ZOMBIE: i32 = 0;
    pub const RUNNING: i32 = 1;
    pub const SLEEPING: i32 = 2;
    pub const MONITOR: i32 = 3;
    pub const WAIT: i32 = 4;
    pub const SUSPEND_STATUS_SUSPENDED: i32 = 0x1;
}

pub mod class_status {
    pub const VERIFIED: i32 = 1;
    pub const PREPARED: i32 = 2;
    pub const INITIALIZED: i32 = 4;
    pub const ERROR: i32 = 8;
}

/// Error codes this backend reacts to explicitly.
pub mod error {
    pub const THREAD_NOT_SUSPENDED: u16 = 13;
    pub const INVALID_OBJECT: u16 = 20;
    pub const ABSENT_INFORMATION: u16 = 101;
    pub const VM_DEAD: u16 = 112;
}

/// Field/method modifier bit for `static`.
pub const ACC_STATIC: i32 = 0x0008;

/// Human name of a JDWP error code.
pub fn error_name(code: u16) -> &'static str {
    match code {
        0 => "NONE",
        10 => "INVALID_THREAD",
        11 => "INVALID_THREAD_GROUP",
        12 => "INVALID_PRIORITY",
        13 => "THREAD_NOT_SUSPENDED",
        14 => "THREAD_SUSPENDED",
        15 => "THREAD_NOT_ALIVE",
        20 => "INVALID_OBJECT",
        21 => "INVALID_CLASS",
        22 => "CLASS_NOT_PREPARED",
        23 => "INVALID_METHODID",
        24 => "INVALID_LOCATION",
        25 => "INVALID_FIELDID",
        30 => "INVALID_FRAMEID",
        31 => "NO_MORE_FRAMES",
        32 => "OPAQUE_FRAME",
        33 => "NOT_CURRENT_FRAME",
        34 => "TYPE_MISMATCH",
        35 => "INVALID_SLOT",
        40 => "DUPLICATE",
        41 => "NOT_FOUND",
        50 => "INVALID_MONITOR",
        51 => "NOT_MONITOR_OWNER",
        52 => "INTERRUPT",
        60 => "INVALID_CLASS_FORMAT",
        61 => "CIRCULAR_CLASS_DEFINITION",
        62 => "FAILS_VERIFICATION",
        63 => "ADD_METHOD_NOT_IMPLEMENTED",
        64 => "SCHEMA_CHANGE_NOT_IMPLEMENTED",
        65 => "INVALID_TYPESTATE",
        66 => "HIERARCHY_CHANGE_NOT_IMPLEMENTED",
        67 => "DELETE_METHOD_NOT_IMPLEMENTED",
        68 => "UNSUPPORTED_VERSION",
        69 => "NAMES_DONT_MATCH",
        70 => "CLASS_MODIFIERS_CHANGE_NOT_IMPLEMENTED",
        71 => "METHOD_MODIFIERS_CHANGE_NOT_IMPLEMENTED",
        99 => "NOT_IMPLEMENTED",
        100 => "NULL_POINTER",
        101 => "ABSENT_INFORMATION",
        102 => "INVALID_EVENT_TYPE",
        103 => "ILLEGAL_ARGUMENT",
        110 => "OUT_OF_MEMORY",
        111 => "ACCESS_DENIED",
        112 => "VM_DEAD",
        113 => "INTERNAL",
        115 => "UNATTACHED_THREAD",
        500 => "INVALID_TAG",
        502 => "ALREADY_INVOKING",
        503 => "INVALID_INDEX",
        504 => "INVALID_LENGTH",
        506 => "INVALID_STRING",
        507 => "INVALID_CLASS_LOADER",
        508 => "INVALID_ARRAY",
        509 => "TRANSPORT_LOAD",
        510 => "TRANSPORT_INIT",
        511 => "NATIVE_METHOD",
        512 => "INVALID_COUNT",
        _ => "UNKNOWN_ERROR",
    }
}

/// `Set.Command` name for diagnostics (`debugger_timeout` names the command).
pub fn command_name(command_set: u8, command: u8) -> String {
    let name = match (command_set, command) {
        (set::VIRTUAL_MACHINE, vm::VERSION) => "VirtualMachine.Version",
        (set::VIRTUAL_MACHINE, vm::CLASSES_BY_SIGNATURE) => "VirtualMachine.ClassesBySignature",
        (set::VIRTUAL_MACHINE, vm::ALL_THREADS) => "VirtualMachine.AllThreads",
        (set::VIRTUAL_MACHINE, vm::DISPOSE) => "VirtualMachine.Dispose",
        (set::VIRTUAL_MACHINE, vm::ID_SIZES) => "VirtualMachine.IDSizes",
        (set::VIRTUAL_MACHINE, vm::SUSPEND) => "VirtualMachine.Suspend",
        (set::VIRTUAL_MACHINE, vm::RESUME) => "VirtualMachine.Resume",
        (set::VIRTUAL_MACHINE, vm::CAPABILITIES_NEW) => "VirtualMachine.CapabilitiesNew",
        (set::VIRTUAL_MACHINE, vm::CREATE_STRING) => "VirtualMachine.CreateString",
        (set::CLASS_TYPE, class_type::INVOKE_METHOD) => "ClassType.InvokeMethod",
        (set::OBJECT_REFERENCE, object_reference::INVOKE_METHOD) => "ObjectReference.InvokeMethod",
        (set::VIRTUAL_MACHINE, vm::ALL_CLASSES_WITH_GENERIC) => {
            "VirtualMachine.AllClassesWithGeneric"
        }
        (set::REFERENCE_TYPE, reference_type::SIGNATURE) => "ReferenceType.Signature",
        (set::REFERENCE_TYPE, reference_type::GET_VALUES) => "ReferenceType.GetValues",
        (set::REFERENCE_TYPE, reference_type::SOURCE_FILE) => "ReferenceType.SourceFile",
        (set::REFERENCE_TYPE, reference_type::STATUS) => "ReferenceType.Status",
        (set::REFERENCE_TYPE, reference_type::SOURCE_DEBUG_EXTENSION) => {
            "ReferenceType.SourceDebugExtension"
        }
        (set::REFERENCE_TYPE, reference_type::FIELDS_WITH_GENERIC) => {
            "ReferenceType.FieldsWithGeneric"
        }
        (set::REFERENCE_TYPE, reference_type::METHODS_WITH_GENERIC) => {
            "ReferenceType.MethodsWithGeneric"
        }
        (set::CLASS_TYPE, class_type::SUPERCLASS) => "ClassType.Superclass",
        (set::METHOD, method::LINE_TABLE) => "Method.LineTable",
        (set::METHOD, method::BYTECODES) => "Method.Bytecodes",
        (set::METHOD, method::VARIABLE_TABLE_WITH_GENERIC) => "Method.VariableTableWithGeneric",
        (set::OBJECT_REFERENCE, object_reference::REFERENCE_TYPE) => {
            "ObjectReference.ReferenceType"
        }
        (set::OBJECT_REFERENCE, object_reference::GET_VALUES) => "ObjectReference.GetValues",
        (set::OBJECT_REFERENCE, object_reference::DISABLE_COLLECTION) => {
            "ObjectReference.DisableCollection"
        }
        (set::OBJECT_REFERENCE, object_reference::ENABLE_COLLECTION) => {
            "ObjectReference.EnableCollection"
        }
        (set::OBJECT_REFERENCE, object_reference::IS_COLLECTED) => "ObjectReference.IsCollected",
        (set::STRING_REFERENCE, string_reference::VALUE) => "StringReference.Value",
        (set::THREAD_REFERENCE, thread_reference::NAME) => "ThreadReference.Name",
        (set::THREAD_REFERENCE, thread_reference::SUSPEND) => "ThreadReference.Suspend",
        (set::THREAD_REFERENCE, thread_reference::RESUME) => "ThreadReference.Resume",
        (set::THREAD_REFERENCE, thread_reference::STATUS) => "ThreadReference.Status",
        (set::THREAD_REFERENCE, thread_reference::FRAMES) => "ThreadReference.Frames",
        (set::THREAD_REFERENCE, thread_reference::FRAME_COUNT) => "ThreadReference.FrameCount",
        (set::THREAD_REFERENCE, thread_reference::SUSPEND_COUNT) => "ThreadReference.SuspendCount",
        (set::ARRAY_REFERENCE, array_reference::LENGTH) => "ArrayReference.Length",
        (set::ARRAY_REFERENCE, array_reference::GET_VALUES) => "ArrayReference.GetValues",
        (set::EVENT_REQUEST, event_request::SET) => "EventRequest.Set",
        (set::EVENT_REQUEST, event_request::CLEAR) => "EventRequest.Clear",
        (set::EVENT_REQUEST, event_request::CLEAR_ALL_BREAKPOINTS) => {
            "EventRequest.ClearAllBreakpoints"
        }
        (set::STACK_FRAME, stack_frame::GET_VALUES) => "StackFrame.GetValues",
        (set::STACK_FRAME, stack_frame::THIS_OBJECT) => "StackFrame.ThisObject",
        (set::EVENT, event::COMPOSITE) => "Event.Composite",
        _ => return format!("{command_set}.{command}"),
    };
    name.to_string()
}

/// Name of an event kind for logs and the event JSON.
pub fn event_kind_name(kind: u8) -> &'static str {
    use event_kind::*;
    match kind {
        SINGLE_STEP => "single_step",
        BREAKPOINT => "breakpoint",
        EXCEPTION => "exception",
        THREAD_START => "thread_start",
        THREAD_DEATH => "thread_death",
        CLASS_PREPARE => "class_prepare",
        CLASS_UNLOAD => "class_unload",
        FIELD_ACCESS => "field_access",
        FIELD_MODIFICATION => "field_modification",
        METHOD_ENTRY => "method_entry",
        METHOD_EXIT => "method_exit",
        METHOD_EXIT_WITH_RETURN_VALUE => "method_exit_with_return_value",
        MONITOR_CONTENDED_ENTER => "monitor_contended_enter",
        MONITOR_CONTENDED_ENTERED => "monitor_contended_entered",
        MONITOR_WAIT => "monitor_wait",
        MONITOR_WAITED => "monitor_waited",
        VM_START => "vm_start",
        VM_DEATH => "vm_death",
        _ => "unknown",
    }
}

pub fn thread_status_name(status: i32) -> &'static str {
    match status {
        thread_status::ZOMBIE => "zombie",
        thread_status::RUNNING => "running",
        thread_status::SLEEPING => "sleeping",
        thread_status::MONITOR => "monitor",
        thread_status::WAIT => "wait",
        _ => "unknown",
    }
}

/// Class status bits rendered as names, e.g. `["verified", "prepared"]`.
pub fn class_status_names(status: i32) -> Vec<&'static str> {
    [
        (class_status::VERIFIED, "verified"),
        (class_status::PREPARED, "prepared"),
        (class_status::INITIALIZED, "initialized"),
        (class_status::ERROR, "error"),
    ]
    .into_iter()
    .filter(|(bit, _)| status & bit != 0)
    .map(|(_, name)| name)
    .collect()
}

/// `CapabilitiesNew` reply order; the remaining 11 booleans are reserved.
pub const CAPABILITY_NAMES: [&str; 21] = [
    "can_watch_field_modification",
    "can_watch_field_access",
    "can_get_bytecodes",
    "can_get_synthetic_attribute",
    "can_get_owned_monitor_info",
    "can_get_current_contended_monitor",
    "can_get_monitor_info",
    "can_redefine_classes",
    "can_add_method",
    "can_unrestrictedly_redefine_classes",
    "can_pop_frames",
    "can_use_instance_filters",
    "can_get_source_debug_extension",
    "can_request_vm_death_event",
    "can_set_default_stratum",
    "can_get_instance_info",
    "can_request_monitor_events",
    "can_get_monitor_frame_info",
    "can_use_source_name_filters",
    "can_get_constant_pool",
    "can_force_early_return",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cover_used_commands_and_errors() {
        assert_eq!(command_name(1, 7), "VirtualMachine.IDSizes");
        assert_eq!(command_name(15, 1), "EventRequest.Set");
        assert_eq!(command_name(42, 3), "42.3");
        assert_eq!(error_name(101), "ABSENT_INFORMATION");
        assert_eq!(error_name(9999), "UNKNOWN_ERROR");
        assert_eq!(event_kind_name(2), "breakpoint");
        assert_eq!(
            class_status_names(7),
            ["verified", "prepared", "initialized"]
        );
        assert!(tag::is_object(tag::STRING));
        assert!(!tag::is_object(tag::INT));
        assert_eq!(tag::for_signature("Ljava/lang/String;"), tag::OBJECT);
        assert_eq!(tag::for_signature("[I"), tag::ARRAY);
        assert_eq!(tag::for_signature("J"), tag::LONG);
        assert_eq!(thread_status_name(4), "wait");
    }
}
