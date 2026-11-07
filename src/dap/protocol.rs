//! DAP wire types. Hand-written serde: every optional field defaults, unknown fields are ignored, `null` counts
//! as absent, and adapter-specific values stay `serde_json::Value`. Lines and columns are as on the wire.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

/// `#[serde(default)]` alone does not accept an explicit `null`; this does.
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A typed body, or its default when it does not parse: a strange body never ends a session. An absent body
/// (`Null`) is legal on some responses and gives the default without a warning.
pub fn body_of<T: DeserializeOwned + Default>(what: &str, body: Value) -> T {
    if body.is_null() {
        return T::default();
    }
    match serde_json::from_value(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("dap: {what} body: {e}");
            T::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response(Response),
    Event(Event),
    Request(ReverseRequest),
}

/// `request_seq` is snake_case on the wire, so no `rename_all` here.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Response {
    pub request_seq: i64,
    pub success: bool,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReverseRequest {
    pub seq: i64,
    pub command: String,
    #[serde(default)]
    pub arguments: Value,
}

impl Incoming {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let parsed = match v.get("type").and_then(Value::as_str) {
            Some("response") => serde_json::from_value(v).map(Incoming::Response),
            Some("event") => serde_json::from_value(v).map(Incoming::Event),
            Some("request") => serde_json::from_value(v).map(Incoming::Request),
            Some(other) => return Err(format!("unknown message type {other}")),
            None => return Err("message without a type".into()),
        };
        parsed.map_err(|e| e.to_string())
    }
}

impl Response {
    /// What to show for a failed request: the message, else the body's error format, else a fallback.
    pub fn error_text(&self) -> String {
        self.message
            .as_deref()
            .filter(|m| !m.is_empty())
            .or_else(|| self.body.pointer("/error/format").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} failed", self.command))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Request { seq: i64, command: String, arguments: Value },
    Response { seq: i64, request_seq: i64, success: bool, command: String, message: Option<String> },
}

impl Outgoing {
    pub fn to_json(&self) -> Value {
        match self {
            Outgoing::Request { seq, command, arguments } => {
                let mut m = Map::new();
                m.insert("seq".into(), json!(seq));
                m.insert("type".into(), json!("request"));
                m.insert("command".into(), json!(command));
                if !arguments.is_null() {
                    m.insert("arguments".into(), arguments.clone());
                }
                Value::Object(m)
            }
            Outgoing::Response { seq, request_seq, success, command, message } => {
                let mut m = Map::new();
                m.insert("seq".into(), json!(seq));
                m.insert("type".into(), json!("response"));
                m.insert("request_seq".into(), json!(request_seq));
                m.insert("success".into(), json!(success));
                m.insert("command".into(), json!(command));
                if let Some(message) = message {
                    m.insert("message".into(), json!(message));
                }
                Value::Object(m)
            }
        }
    }

    pub fn command(&self) -> &str {
        match self {
            Outgoing::Request { command, .. } | Outgoing::Response { command, .. } => command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(deserialize_with = "nullable")]
    pub supports_configuration_done_request: bool,
    #[serde(deserialize_with = "nullable")]
    pub supports_terminate_request: bool,
    /// debugpy spells it `supportsTerminateDebuggee`.
    #[serde(alias = "supportsTerminateDebuggee", deserialize_with = "nullable")]
    pub support_terminate_debuggee: bool,
    #[serde(deserialize_with = "nullable")]
    pub supports_variable_paging: bool,
    #[serde(deserialize_with = "nullable")]
    pub exception_breakpoint_filters: Vec<ExceptionFilter>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExceptionFilter {
    #[serde(deserialize_with = "nullable")]
    pub filter: String,
    #[serde(deserialize_with = "nullable")]
    pub label: String,
    #[serde(deserialize_with = "nullable")]
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Source {
    pub name: Option<String>,
    pub path: Option<String>,
    pub source_reference: Option<i64>,
}

/// Lines are as on the wire, 1-based. A pending breakpoint has no `line`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    pub verified: bool,
    pub line: Option<i64>,
    pub message: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StoppedEvent {
    #[serde(deserialize_with = "nullable")]
    pub reason: String,
    pub thread_id: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    pub all_threads_stopped: bool,
    #[serde(deserialize_with = "nullable")]
    pub hit_breakpoint_ids: Vec<i64>,
    pub text: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputEvent {
    pub category: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BreakpointEvent {
    #[serde(deserialize_with = "nullable")]
    pub reason: String,
    #[serde(deserialize_with = "nullable")]
    pub breakpoint: Breakpoint,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExitedEvent {
    #[serde(deserialize_with = "nullable")]
    pub exit_code: i64,
}

/// The adapter started or attached to the program.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct ProcessEvent {
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(rename = "systemProcessId", default)]
    pub system_process_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Thread {
    #[serde(deserialize_with = "nullable")]
    pub id: i64,
    #[serde(deserialize_with = "nullable")]
    pub name: String,
}

/// Lines are as on the wire, 1-based. Frame id 0 is valid; `line` 0 means no source; `column` can be 0.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StackFrame {
    #[serde(deserialize_with = "nullable")]
    pub id: i64,
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    pub source: Option<Source>,
    #[serde(deserialize_with = "nullable")]
    pub line: i64,
    #[serde(deserialize_with = "nullable")]
    pub column: i64,
}

/// The body of a `stackTrace` response.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StackTraceBody {
    #[serde(deserialize_with = "nullable")]
    pub stack_frames: Vec<StackFrame>,
}

/// The body of a `threads` response.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadsBody {
    #[serde(deserialize_with = "nullable")]
    pub threads: Vec<Thread>,
}

/// A scope of a frame (`Locals`, `Arguments`, ...).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Scope {
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub presentation_hint: String,
    #[serde(deserialize_with = "nullable")]
    pub variables_reference: i64,
    #[serde(deserialize_with = "nullable")]
    pub expensive: bool,
}

/// A variable. A reference of 0 or less is a leaf. `indexed_variables` and `named_variables` are hints for the child count.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Variable {
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub value: String,
    #[serde(rename = "type", deserialize_with = "nullable")]
    pub ty: String,
    #[serde(deserialize_with = "nullable")]
    pub variables_reference: i64,
    #[serde(deserialize_with = "nullable")]
    pub indexed_variables: i64,
    #[serde(deserialize_with = "nullable")]
    pub named_variables: i64,
}

/// The body of a `scopes` response.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScopesBody {
    #[serde(deserialize_with = "nullable")]
    pub scopes: Vec<Scope>,
}

/// The body of a `variables` response.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VariablesBody {
    #[serde(deserialize_with = "nullable")]
    pub variables: Vec<Variable>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_and_variable_read_gdbs_shapes() {
        let s: ScopesBody = body_of(
            "t",
            json!({"scopes":[
                {"name":"Locals","presentationHint":"locals","variablesReference":1,"namedVariables":8,"expensive":false},
                {"name":"Registers","presentationHint":"registers","variablesReference":2,"namedVariables":59,"expensive":false},
                {"name":"X","presentationHint":null,"variablesReference":null,"expensive":null}]}),
        );
        assert_eq!(s.scopes.len(), 3);
        assert_eq!(
            s.scopes[0],
            Scope { name: "Locals".into(), presentation_hint: "locals".into(), variables_reference: 1, expensive: false }
        );
        assert_eq!(s.scopes[1].presentation_hint, "registers");
        assert_eq!(s.scopes[2], Scope { name: "X".into(), ..Scope::default() }, "nulls give defaults");
        let v: VariablesBody = body_of(
            "t",
            json!({"variables":[
                {"variablesReference":0,"name":"a","value":"1","type":"int"},
                {"variablesReference":3,"name":"p","value":"","namedVariables":4,"type":"struct point"},
                {"variablesReference":4,"name":"arr","value":"","indexedVariables":3,"type":"int [3]"},
                {"name":"n","value":null,"type":null,"variablesReference":null,"indexedVariables":null,"namedVariables":null}]}),
        );
        let [a, p, arr, n] = v.variables.as_slice() else { panic!("{:?}", v.variables) };
        assert_eq!((a.name.as_str(), a.value.as_str(), a.ty.as_str(), a.variables_reference), ("a", "1", "int", 0));
        assert_eq!((p.value.as_str(), p.ty.as_str(), p.variables_reference, p.indexed_variables), ("", "struct point", 3, 0));
        assert_eq!((p.named_variables, arr.named_variables, n.named_variables), (4, 0, 0), "named is parsed, null is 0");
        assert_eq!((arr.ty.as_str(), arr.indexed_variables), ("int [3]", 3));
        assert_eq!(*n, Variable { name: "n".into(), ..Variable::default() });
        assert!(body_of::<ScopesBody>("t", json!({"scopes":null})).scopes.is_empty());
        assert!(body_of::<VariablesBody>("t", Value::Null).variables.is_empty());
    }

    fn parse(v: Value) -> Result<Incoming, String> {
        Incoming::parse(v.to_string().as_bytes())
    }

    #[test]
    fn stack_trace_body_parses_frame_id_zero_and_column_zero() {
        let b: StackTraceBody =
            body_of("t", json!({"stackFrames":[{"id":0,"name":"main","line":9,"column":0,"source":{"path":"/x/demo.c"}}]}));
        assert_eq!(b.stack_frames.len(), 1);
        let f = &b.stack_frames[0];
        assert_eq!((f.id, f.name.as_str(), f.line, f.column), (0, "main", 9, 0));
        assert_eq!(f.source.as_ref().and_then(|s| s.path.as_deref()), Some("/x/demo.c"));
        assert!(body_of::<StackTraceBody>("t", json!({"stackFrames":null})).stack_frames.is_empty());
        let t: ThreadsBody = body_of("t", json!({"threads":[{"id":4},{"id":9,"name":"x"}]}));
        assert_eq!(t.threads.iter().map(|t| t.id).collect::<Vec<_>>(), vec![4, 9]);
    }

    #[test]
    fn parse_matches_on_type() {
        let r = parse(json!({"type":"response","seq":1,"request_seq":2,"success":true,"command":"next"})).unwrap();
        assert!(matches!(r, Incoming::Response(_)));
        let e = parse(json!({"type":"event","seq":1,"event":"initialized"})).unwrap();
        assert!(matches!(e, Incoming::Event(Event { ref event, .. }) if event == "initialized"));
        let q = parse(json!({"type":"request","seq":5,"command":"runInTerminal","arguments":{"a":1}})).unwrap();
        assert_eq!(q, Incoming::Request(ReverseRequest { seq: 5, command: "runInTerminal".into(), arguments: json!({"a":1}) }));
        assert!(parse(json!({"type":"x"})).unwrap_err().contains('x'));
        assert!(parse(json!({"seq":1})).is_err());
        assert!(Incoming::parse(b"not json").is_err());
    }

    #[test]
    fn response_uses_snake_request_seq() {
        let Incoming::Response(r) = parse(json!({"type":"response","seq":9,"request_seq":4,"success":true,"command":"next"})).unwrap()
        else {
            panic!("not a response")
        };
        assert_eq!(r.request_seq, 4);
        assert_eq!(r.message, None);
        assert_eq!(r.body, Value::Null);
        assert!(parse(json!({"type":"response","seq":9,"requestSeq":4,"success":true,"command":"next"})).is_err());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let r = parse(json!({"type":"response","request_seq":1,"success":true,"command":"c","extra":1})).unwrap();
        assert!(matches!(r, Incoming::Response(_)));
        let e = parse(json!({"type":"event","event":"x","extra":[1]})).unwrap();
        assert!(matches!(e, Incoming::Event(_)));
        let c: Capabilities = body_of("t", json!({"supportsTerminateRequest":true,"extra":{"a":1}}));
        assert!(c.supports_terminate_request);
    }

    #[test]
    fn capabilities_absent_means_unsupported() {
        assert_eq!(body_of::<Capabilities>("t", json!({})), Capabilities::default());
        assert_eq!(body_of::<Capabilities>("t", json!("garbage")), Capabilities::default());
        assert_eq!(body_of::<Capabilities>("t", Value::Null), Capabilities::default());
    }

    #[test]
    fn capabilities_read_camel_case_and_filters() {
        let c: Capabilities = body_of(
            "t",
            json!({
                "supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true,
                "supportTerminateDebuggee": true, "supportsVariablePaging": true,
                "exceptionBreakpointFilters": [{"filter":"a","label":"A","default":true}, {"filter":"b","label":"B"}],
            }),
        );
        assert!(c.supports_configuration_done_request && c.supports_terminate_request);
        assert!(c.support_terminate_debuggee && c.supports_variable_paging);
        assert_eq!(
            c.exception_breakpoint_filters,
            vec![
                ExceptionFilter { filter: "a".into(), label: "A".into(), default: true },
                ExceptionFilter { filter: "b".into(), label: "B".into(), default: false },
            ]
        );
    }

    #[test]
    fn debugpy_spelling_of_terminate_debuggee() {
        assert!(body_of::<Capabilities>("t", json!({"supportsTerminateDebuggee": true})).support_terminate_debuggee);
        assert!(body_of::<Capabilities>("t", json!({"supportTerminateDebuggee": true})).support_terminate_debuggee);
    }

    #[test]
    fn explicit_null_counts_as_absent() {
        let c: Capabilities = body_of("t", json!({"exceptionBreakpointFilters":null,"supportsTerminateRequest":null}));
        assert_eq!(c, Capabilities::default());
        // A null field must not discard its neighbours: the body would otherwise fall back to the default.
        let c: Capabilities = body_of("t", json!({"exceptionBreakpointFilters":null,"supportsConfigurationDoneRequest":true}));
        assert!(c.supports_configuration_done_request && c.exception_breakpoint_filters.is_empty());
        let s: StoppedEvent = body_of("t", json!({"reason":null,"hitBreakpointIds":null}));
        assert_eq!(s, StoppedEvent::default());
    }

    #[test]
    fn stopped_event_as_gdb_sends_it() {
        let s: StoppedEvent = body_of("t", json!({"threadId":1,"allThreadsStopped":true,"hitBreakpointIds":[1],"reason":"breakpoint"}));
        assert_eq!(
            s,
            StoppedEvent {
                reason: "breakpoint".into(),
                thread_id: Some(1),
                all_threads_stopped: true,
                hit_breakpoint_ids: vec![1],
                text: None,
                description: None
            }
        );
    }

    #[test]
    fn stopped_event_without_thread_id() {
        let s: StoppedEvent = body_of("t", json!({"reason":"pause"}));
        assert_eq!(s.thread_id, None);
        assert_eq!(s.reason, "pause");
    }

    #[test]
    fn frame_as_gdb_sends_it() {
        let f: StackFrame = body_of("t", json!({"id":0,"name":"f","line":3,"column":0,"source":{"name":"t.c","path":"/t.c"}}));
        assert_eq!((f.id, f.column, f.line), (0, 0, 3));
        assert_eq!(f.source.unwrap().path.as_deref(), Some("/t.c"));
    }

    #[test]
    fn frame_without_source() {
        let f: StackFrame = body_of("t", json!({"id":2,"name":"g"}));
        assert_eq!((f.source, f.line, f.column), (None, 0, 0));
    }

    #[test]
    fn source_with_reference_and_no_path() {
        let s: Source = body_of("t", json!({"name":"x","sourceReference":7}));
        assert_eq!((s.path, s.source_reference), (None, Some(7)));
    }

    #[test]
    fn pending_breakpoint_as_gdb_sends_it() {
        let b: Breakpoint = body_of("t", json!({"id":1,"verified":false,"reason":"pending"}));
        assert_eq!((b.id, b.verified, b.line, b.reason.as_deref()), (Some(1), false, None, Some("pending")));
    }

    #[test]
    fn breakpoint_changed_event() {
        let e: BreakpointEvent = body_of("t", json!({"reason":"changed","breakpoint":{"id":1,"verified":true,"line":3}}));
        assert_eq!(e.reason, "changed");
        assert_eq!(e.breakpoint, Breakpoint { id: Some(1), verified: true, line: Some(3), message: None, reason: None });
    }

    #[test]
    fn output_keeps_emoji() {
        let o: OutputEvent = body_of("t", json!({"category":"stdout","output":"x=3 \u{1F600}\n"}));
        assert_eq!(o.output, "x=3 \u{1F600}\n");
        assert_eq!(o.category.as_deref(), Some("stdout"));
        let o: OutputEvent = body_of("t", json!({"output":"a"}));
        assert_eq!(o.category, None);
    }

    #[test]
    fn exited_event() {
        assert_eq!(body_of::<ExitedEvent>("t", json!({"exitCode":3})).exit_code, 3);
        assert_eq!(body_of::<ExitedEvent>("t", json!({})).exit_code, 0);
    }

    #[test]
    fn request_to_json_leaves_out_null_arguments() {
        let r = Outgoing::Request { seq: 3, command: "configurationDone".into(), arguments: Value::Null };
        assert_eq!(r.to_json(), json!({"seq":3,"type":"request","command":"configurationDone"}));
        let r = Outgoing::Request { seq: 4, command: "next".into(), arguments: json!({"threadId":1}) };
        assert_eq!(r.to_json(), json!({"seq":4,"type":"request","command":"next","arguments":{"threadId":1}}));
        assert_eq!(r.command(), "next");
    }

    #[test]
    fn response_to_json_shape() {
        let r = Outgoing::Response { seq: 8, request_seq: 77, success: false, command: "runInTerminal".into(), message: None };
        assert_eq!(r.to_json(), json!({"seq":8,"type":"response","request_seq":77,"success":false,"command":"runInTerminal"}));
        let r = Outgoing::Response {
            seq: 8,
            request_seq: 77,
            success: false,
            command: "runInTerminal".into(),
            message: Some("not supported".into()),
        };
        assert_eq!(r.to_json()["message"], json!("not supported"));
        assert_eq!(r.command(), "runInTerminal");
    }

    #[test]
    fn error_text_prefers_message() {
        let mk = |message: Option<&str>, body: Value| Response {
            request_seq: 1,
            success: false,
            command: "launch".into(),
            message: message.map(str::to_string),
            body,
        };
        let body = json!({"error":{"id":1,"format":"fmt text"}});
        assert_eq!(mk(Some("short"), body.clone()).error_text(), "short");
        assert_eq!(mk(Some(""), body.clone()).error_text(), "fmt text");
        assert_eq!(mk(None, body).error_text(), "fmt text");
        assert_eq!(mk(None, Value::Null).error_text(), "launch failed");
    }
}
