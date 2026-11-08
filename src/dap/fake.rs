//! An in-process debug adapter for tests, over `tokio::io::duplex`. It answers `initialize`, `launch` (or `attach`) and
//! `configurationDone` by itself, in the order real adapters use (`Order`), and hands every message it receives
//! to the test, which answers the rest and can send events and reverse requests of its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::lsp::client::Conn;
use crate::lsp::transport::{read_frame, write_frame};

/// How long `initialized` is held back: a client that does not wait for it is caught sending before it.
const HOLD: Duration = Duration::from_millis(100);
const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// js-debug: `initialized` right after the initialize response; launch answered after configurationDone.
    InitializedFirst,
    /// debugpy, lldb-dap, codelldb, gdb: `initialized` on launch; launch answered after configurationDone.
    LaunchDeferred,
    /// delve: `initialized` and the launch response together, before configurationDone.
    LaunchEarly,
}

pub const ORDERS: [Order; 3] = [Order::InitializedFirst, Order::LaunchDeferred, Order::LaunchEarly];

enum Out {
    Msg(Value),
    Close,
}

/// The adapter's side of the wire, shared with the tasks that answer by themselves.
#[derive(Clone)]
struct Wire {
    out: UnboundedSender<Out>,
    seq: Arc<AtomicI64>,
}

impl Wire {
    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    fn send(&self, mut msg: Value) {
        msg["seq"] = json!(self.next_seq());
        let _ = self.out.send(Out::Msg(msg));
    }

    fn response(&self, request: &Value, success: bool, message: Option<&str>, body: Value) {
        let mut msg = json!({
            "type": "response", "request_seq": request["seq"], "success": success, "command": request["command"], "body": body,
        });
        if let Some(m) = message {
            msg["message"] = json!(m);
        }
        self.send(msg);
    }

    fn event(&self, name: &str, body: Value) {
        self.send(json!({"type": "event", "event": name, "body": body}));
    }
}

/// `initialized` after `HOLD`, then the launch response when given (delve's "together").
fn release(wire: &Wire, initialized: &Arc<AtomicBool>, launch: Option<Value>) {
    let (wire, initialized) = (wire.clone(), Arc::clone(initialized));
    tokio::spawn(async move {
        tokio::time::sleep(HOLD).await;
        initialized.store(true, Ordering::SeqCst);
        wire.event("initialized", Value::Null);
        if let Some(launch) = launch {
            wire.response(&launch, true, None, Value::Null);
        }
    });
}

pub struct FakeAdapter {
    from_client: UnboundedReceiver<Value>,
    wire: Wire,
}

pub fn connect(capabilities: Value, order: Order) -> (Conn, FakeAdapter) {
    let (client_end, adapter_end) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (adapter_read, mut adapter_write) = tokio::io::split(adapter_end);
    let (seen, from_client) = mpsc::unbounded_channel();
    let (out, mut outbox) = mpsc::unbounded_channel();
    let wire = Wire { out, seq: Arc::new(AtomicI64::new(1)) };
    let capable = capabilities["supportsConfigurationDoneRequest"] == json!(true);
    let answers = wire.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(adapter_read);
        let initialized = Arc::new(AtomicBool::new(false));
        let mut waiting_launch: Option<Value> = None;
        while let Ok(Some(body)) = read_frame(&mut r).await {
            let msg: Value = serde_json::from_slice(&body).expect("client sent JSON");
            if msg["type"] == "request" {
                match msg["command"].as_str() {
                    Some("initialize") => {
                        answers.response(&msg, true, None, capabilities.clone());
                        if order == Order::InitializedFirst {
                            release(&answers, &initialized, None);
                        }
                    }
                    Some("launch" | "attach") => {
                        let early = order == Order::LaunchEarly;
                        if !early {
                            if capable {
                                waiting_launch = Some(msg.clone());
                            } else {
                                answers.response(&msg, true, None, Value::Null);
                            }
                        }
                        if order != Order::InitializedFirst {
                            release(&answers, &initialized, early.then(|| msg.clone()));
                        }
                    }
                    Some("configurationDone") => {
                        if initialized.load(Ordering::SeqCst) {
                            answers.response(&msg, true, None, Value::Null);
                            if let Some(launch) = waiting_launch.take() {
                                answers.response(&launch, true, None, Value::Null);
                            }
                        } else {
                            let text = "'configurationDone' is only expected after the 'initialized' event.";
                            answers.response(&msg, false, Some(text), Value::Null);
                        }
                    }
                    _ => {}
                }
            }
            if seen.send(msg).is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(out) = outbox.recv().await {
            match out {
                Out::Msg(msg) => {
                    if write_frame(&mut adapter_write, &msg).await.is_err() {
                        return;
                    }
                }
                Out::Close => {
                    let _ = adapter_write.shutdown().await;
                    return;
                }
            }
        }
    });
    (Conn { reader: Box::new(client_read), writer: Box::new(client_write), child: None }, FakeAdapter { from_client, wire })
}

impl FakeAdapter {
    /// The next message from the client, requests and responses alike. Panics after 5 s.
    pub async fn next(&mut self) -> Value {
        tokio::time::timeout(WAIT, self.from_client.recv()).await.expect("a message in time").expect("the client is connected")
    }

    /// The next request with that command, skipping everything else.
    pub async fn recv(&mut self, command: &str) -> Value {
        loop {
            let msg = self.next().await;
            if msg["type"] == "request" && msg["command"] == command {
                return msg;
            }
        }
    }

    /// The next client response answering `seq`.
    pub async fn response_to(&mut self, seq: i64) -> Value {
        loop {
            let msg = self.next().await;
            if msg["type"] == "response" && msg["request_seq"] == seq {
                return msg;
            }
        }
    }

    /// The client closed its stream. Panics after 5 s.
    pub async fn closed(&mut self) {
        tokio::time::timeout(WAIT, async { while self.from_client.recv().await.is_some() {} }).await.expect("the client closed in time");
    }

    pub fn respond(&self, request: &Value, body: Value) {
        self.wire.response(request, true, None, body);
    }

    pub fn respond_error(&self, request: &Value, message: &str) {
        self.wire.response(request, false, Some(message), Value::Null);
    }

    pub fn event(&self, name: &str, body: Value) {
        self.wire.event(name, body);
    }

    /// A reverse request; returns its seq.
    pub fn request(&self, command: &str, arguments: Value) -> i64 {
        let seq = self.wire.next_seq();
        let _ = self.wire.out.send(Out::Msg(json!({"seq": seq, "type": "request", "command": command, "arguments": arguments})));
        seq
    }

    /// Sent as is, no seq added.
    pub fn raw(&self, msg: Value) {
        let _ = self.wire.out.send(Out::Msg(msg));
    }

    /// Ends the adapter's output: the client reads EOF.
    pub fn close(&self) {
        let _ = self.wire.out.send(Out::Close);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWrite;

    use super::*;

    struct Raw {
        reader: BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
        writer: Box<dyn AsyncWrite + Send + Unpin>,
    }

    impl Raw {
        fn new(conn: Conn) -> Self {
            Self { reader: BufReader::new(conn.reader), writer: conn.writer }
        }

        async fn send(&mut self, seq: i64, command: &str) {
            write_frame(&mut self.writer, &json!({"seq": seq, "type": "request", "command": command, "arguments": {}})).await.unwrap();
        }

        async fn read(&mut self) -> Value {
            let body =
                tokio::time::timeout(WAIT, read_frame(&mut self.reader)).await.expect("a frame in time").unwrap().expect("not closed");
            serde_json::from_slice(&body).unwrap()
        }

        async fn silent(&mut self) {
            let got = tokio::time::timeout(3 * HOLD, read_frame(&mut self.reader)).await;
            assert!(got.is_err(), "unexpected frame: {got:?}");
        }
    }

    fn caps() -> Value {
        json!({"supportsConfigurationDoneRequest": true, "marker": 1})
    }

    #[tokio::test]
    async fn initialized_first_sends_it_after_the_response() {
        let (conn, _adapter) = connect(caps(), Order::InitializedFirst);
        let mut c = Raw::new(conn);
        c.send(1, "initialize").await;
        let resp = c.read().await;
        assert_eq!(
            (resp["type"].as_str(), resp["command"].as_str(), resp["success"].as_bool()),
            (Some("response"), Some("initialize"), Some(true))
        );
        assert_eq!(resp["body"], caps());
        let ev = c.read().await;
        assert_eq!((ev["type"].as_str(), ev["event"].as_str()), (Some("event"), Some("initialized")));
    }

    #[tokio::test]
    async fn launch_deferred_withholds_initialized_until_launch() {
        let (conn, _adapter) = connect(caps(), Order::LaunchDeferred);
        let mut c = Raw::new(conn);
        c.send(1, "initialize").await;
        assert_eq!(c.read().await["command"], "initialize");
        c.silent().await;
        c.send(2, "launch").await;
        let ev = c.read().await;
        assert_eq!(ev["event"], "initialized");
    }

    #[tokio::test]
    async fn launch_early_answers_launch_with_initialized() {
        let (conn, _adapter) = connect(caps(), Order::LaunchEarly);
        let mut c = Raw::new(conn);
        c.send(1, "initialize").await;
        c.read().await;
        c.send(2, "launch").await;
        assert_eq!(c.read().await["event"], "initialized");
        let launch = c.read().await;
        assert_eq!(
            (launch["command"].as_str(), launch["request_seq"].as_i64(), launch["success"].as_bool()),
            (Some("launch"), Some(2), Some(true))
        );
    }

    #[tokio::test]
    async fn launch_waits_for_configuration_done() {
        for order in [Order::InitializedFirst, Order::LaunchDeferred] {
            let (conn, _adapter) = connect(caps(), order);
            let mut c = Raw::new(conn);
            c.send(1, "initialize").await;
            c.read().await;
            c.send(2, "launch").await;
            loop {
                if c.read().await["event"] == "initialized" {
                    break;
                }
            }
            c.silent().await;
            c.send(3, "configurationDone").await;
            let done = c.read().await;
            assert_eq!((done["command"].as_str(), done["success"].as_bool()), (Some("configurationDone"), Some(true)), "{order:?}");
            let launch = c.read().await;
            assert_eq!((launch["command"].as_str(), launch["request_seq"].as_i64()), (Some("launch"), Some(2)), "{order:?}");
        }
    }

    #[tokio::test]
    async fn configuration_done_before_initialized_is_refused() {
        let (conn, _adapter) = connect(caps(), Order::InitializedFirst);
        let mut c = Raw::new(conn);
        c.send(1, "initialize").await;
        c.send(2, "configurationDone").await;
        c.read().await;
        let refused = c.read().await;
        assert_eq!((refused["command"].as_str(), refused["success"].as_bool()), (Some("configurationDone"), Some(false)));
        assert_eq!(refused["message"], "'configurationDone' is only expected after the 'initialized' event.");
    }

    #[tokio::test]
    async fn not_capable_answers_launch_at_once() {
        for order in [Order::InitializedFirst, Order::LaunchDeferred] {
            let (conn, _adapter) = connect(json!({}), order);
            let mut c = Raw::new(conn);
            c.send(1, "initialize").await;
            c.send(2, "launch").await;
            loop {
                let m = c.read().await;
                if m["command"] == "launch" {
                    assert_eq!(m["success"], true, "{order:?}");
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_side_messages_have_their_shape() {
        let (conn, mut adapter) = connect(caps(), Order::LaunchEarly);
        let mut c = Raw::new(conn);
        c.send(1, "initialize").await;
        let init = c.read().await;
        let req = json!({"seq": 9, "command": "next"});
        adapter.respond(&req, json!({"a": 1}));
        adapter.respond_error(&req, "boom");
        adapter.event("output", json!({"output": "hi"}));
        let seq = adapter.request("runInTerminal", json!({"x": 1}));
        let ok = c.read().await;
        assert_eq!(
            (ok["type"].as_str(), ok["request_seq"].as_i64(), ok["success"].as_bool(), ok["command"].as_str()),
            (Some("response"), Some(9), Some(true), Some("next"))
        );
        assert_eq!(ok["body"], json!({"a": 1}));
        let err = c.read().await;
        assert_eq!((err["success"].as_bool(), err["message"].as_str()), (Some(false), Some("boom")));
        let ev = c.read().await;
        assert_eq!((ev["type"].as_str(), ev["event"].as_str()), (Some("event"), Some("output")));
        assert_eq!(ev["body"], json!({"output": "hi"}));
        let rq = c.read().await;
        assert_eq!((rq["type"].as_str(), rq["command"].as_str(), rq["seq"].as_i64()), (Some("request"), Some("runInTerminal"), Some(seq)));
        assert_eq!(rq["arguments"], json!({"x": 1}));
        let seqs: Vec<i64> = [&init, &ok, &err, &ev, &rq].iter().map(|m| m["seq"].as_i64().unwrap()).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        // The client's own messages reach `next`, too.
        assert_eq!(adapter.next().await["command"], "initialize");
    }

    #[tokio::test]
    async fn close_ends_the_stream() {
        let (conn, adapter) = connect(caps(), Order::LaunchDeferred);
        let mut c = Raw::new(conn);
        adapter.close();
        let got = tokio::time::timeout(WAIT, read_frame(&mut c.reader)).await.expect("in time").unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn closed_sees_the_client_go() {
        let (conn, mut adapter) = connect(caps(), Order::LaunchDeferred);
        let mut c = Raw::new(conn);
        c.writer.shutdown().await.unwrap();
        drop(c);
        adapter.closed().await;
    }
}
