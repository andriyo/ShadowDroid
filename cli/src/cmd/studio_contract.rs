//! Host-side Android Studio plugin bridge contract.
//!
//! Keep this focused on values that the CLI sends to the ShadowDroid Android
//! Studio plugin. Response payload fields can stay near the code that renders
//! or consumes them.

pub(crate) const DEFAULT_URL: &str = "http://127.0.0.1:50576";
pub(crate) const REGISTRY_FILE: &str = "studio-debugger.json";

pub(crate) mod route {
    pub(crate) const STATUS: &str = "/v1/status";
    pub(crate) const SESSIONS: &str = "/v1/sessions";
    pub(crate) const SESSION_CONTROL: &str = "/v1/session/control";
    pub(crate) const SESSION_STACK: &str = "/v1/session/stack";
    pub(crate) const SESSION_THREADS: &str = "/v1/session/threads";
    pub(crate) const SESSION_VARIABLES: &str = "/v1/session/variables";
    pub(crate) const SESSION_EVALUATE: &str = "/v1/session/evaluate";
    pub(crate) const SESSION_INSPECT: &str = "/v1/session/inspect";
    pub(crate) const SESSION_COROUTINES: &str = "/v1/session/coroutines";
    pub(crate) const SESSION_COROUTINES_THREADS: &str = "/v1/session/coroutines/threads";
    pub(crate) const SESSION_COROUTINES_CONTINUATION: &str = "/v1/session/coroutines/continuation";
    pub(crate) const SESSION_COROUTINES_FLOW: &str = "/v1/session/coroutines/flow";
    pub(crate) const WATCHES: &str = "/v1/watches";
    pub(crate) const WATCHES_ADD: &str = "/v1/watches/add";
    pub(crate) const WATCHES_REMOVE: &str = "/v1/watches/remove";
    pub(crate) const WATCHES_CLEAR: &str = "/v1/watches/clear";
    pub(crate) const CLIENTS: &str = "/v1/clients";
    pub(crate) const BREAKPOINTS: &str = "/v1/breakpoints";
    pub(crate) const BREAKPOINT_LINE: &str = "/v1/breakpoints/line";
    pub(crate) const BREAKPOINT_EXCEPTION: &str = "/v1/breakpoints/exception";
    pub(crate) const BREAKPOINT_METHOD: &str = "/v1/breakpoints/method";
    pub(crate) const BREAKPOINT_FIELD: &str = "/v1/breakpoints/field";
    pub(crate) const BREAKPOINT_UPDATE: &str = "/v1/breakpoints/update";
    pub(crate) const BREAKPOINT_REMOVE: &str = "/v1/breakpoints/remove";
    pub(crate) const ATTACH: &str = "/v1/attach";
    pub(crate) const LAYOUT_SNAPSHOT: &str = "/v1/layout/snapshot";
    pub(crate) const LAYOUT_RECOMPOSITIONS: &str = "/v1/layout/recompositions";
    pub(crate) const LAYOUT_SOURCE: &str = "/v1/layout/source";
}

pub(crate) mod query {
    pub(crate) const ACCESS: &str = "access";
    pub(crate) const ACTION: &str = "action";
    pub(crate) const BOUNDS: &str = "bounds";
    pub(crate) const CAUGHT: &str = "caught";
    pub(crate) const CLASS: &str = "class";
    pub(crate) const CLEAR_CONDITION: &str = "clear_condition";
    pub(crate) const CLEAR_LOG_EXPRESSION: &str = "clear_log_expression";
    pub(crate) const CONDITION: &str = "condition";
    pub(crate) const CONFIGURATION: &str = "configuration";
    pub(crate) const DEBUGGER: &str = "debugger";
    pub(crate) const DEPTH: &str = "depth";
    pub(crate) const DESC: &str = "desc";
    pub(crate) const DEVICE: &str = "device";
    pub(crate) const DIALOG: &str = "dialog";
    pub(crate) const DRAW_ID: &str = "draw_id";
    pub(crate) const ENABLED: &str = "enabled";
    pub(crate) const ENTRY: &str = "entry";
    pub(crate) const EXCEPTION: &str = "exception";
    pub(crate) const EXIT: &str = "exit";
    pub(crate) const EXPRESSION: &str = "expression";
    pub(crate) const FIELD: &str = "field";
    pub(crate) const FILE: &str = "file";
    pub(crate) const FRAME: &str = "frame";
    pub(crate) const HANDLE: &str = "handle";
    pub(crate) const ID: &str = "id";
    pub(crate) const LINE: &str = "line";
    pub(crate) const LIMIT: &str = "limit";
    pub(crate) const LOG_EXPRESSION: &str = "log_expression";
    pub(crate) const LOG_MESSAGE: &str = "log_message";
    pub(crate) const LOG_STACK: &str = "log_stack";
    pub(crate) const MAX_ARRAY_ITEMS: &str = "max_array_items";
    pub(crate) const MAX_FIELDS: &str = "max_fields";
    pub(crate) const METHOD: &str = "method";
    pub(crate) const MODE: &str = "mode";
    pub(crate) const MODIFICATION: &str = "modification";
    pub(crate) const NAME: &str = "name";
    pub(crate) const PACKAGE: &str = "package";
    pub(crate) const PASS_COUNT: &str = "pass_count";
    pub(crate) const PATH: &str = "path";
    pub(crate) const PID: &str = "pid";
    pub(crate) const PROJECT: &str = "project";
    pub(crate) const RESET: &str = "reset";
    pub(crate) const RID: &str = "rid";
    pub(crate) const SESSION: &str = "session";
    pub(crate) const SUSPEND: &str = "suspend";
    pub(crate) const TEMPORARY: &str = "temporary";
    pub(crate) const TEXT: &str = "text";
    pub(crate) const THREAD: &str = "thread";
    pub(crate) const TIMEOUT_MS: &str = "timeout_ms";
    pub(crate) const UNCAUGHT: &str = "uncaught";
}

pub(crate) mod session_action {
    pub(crate) const PAUSE: &str = "pause";
    pub(crate) const RESUME: &str = "resume";
    pub(crate) const STEP_INTO: &str = "step_into";
    pub(crate) const STEP_OUT: &str = "step_out";
    pub(crate) const STEP_OVER: &str = "step_over";
    pub(crate) const STOP: &str = "stop";
}

pub(crate) mod value {
    pub(crate) const LAYOUT_RECOMPOSITIONS: &str = "layout_recompositions";
    pub(crate) const LAYOUT_SNAPSHOT: &str = "layout_snapshot";
    pub(crate) const LAYOUT_SOURCE: &str = "layout_source";
}
