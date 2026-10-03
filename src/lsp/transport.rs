//! JSON-RPC 2.0 over a byte stream with `Content-Length` framing, the way
//! language servers speak on stdio. Generic over `AsyncRead`/`AsyncWrite`,
//! so tests run a fake server over `tokio::io::duplex`. A reader task turns
//! frames into [`Incoming`] messages; a writer task sends [`Outgoing`] ones.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// A frame larger than this is taken for a broken stream.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// A request id: ours are numbers, a server may use strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(i64),
    Str(String),
}

/// The error of a failed request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ResponseError {
    /// `MethodNotFound`, the answer to server requests jetl does not know.
    pub const METHOD_NOT_FOUND: i64 = -32601;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// A message from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Request { id: RequestId, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: RequestId, result: Result<Value, ResponseError> },
}

/// The fields of any JSON-RPC message; which are present tells the kind.
#[derive(Deserialize)]
struct Raw {
    id: Option<RequestId>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<ResponseError>,
}

impl Incoming {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let raw: Raw = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        match (raw.id, raw.method) {
            (Some(id), Some(method)) => Ok(Incoming::Request { id, method, params: raw.params }),
            (None, Some(method)) => Ok(Incoming::Notification { method, params: raw.params }),
            (Some(id), None) => {
                let result = match raw.error {
                    Some(e) => Err(e),
                    None => Ok(raw.result.unwrap_or(Value::Null)),
                };
                Ok(Incoming::Response { id, result })
            }
            (None, None) => Err("neither a method nor an id".into()),
        }
    }
}

/// A message to the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Request { id: i64, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: RequestId, result: Result<Value, ResponseError> },
}

impl Outgoing {
    pub fn notification(method: &str, params: Value) -> Self {
        Outgoing::Notification { method: method.into(), params }
    }

    /// The message as JSON; null params (`shutdown`, `exit`) are left out,
    /// as JSON-RPC wants.
    pub fn to_json(&self) -> Value {
        let mut msg = match self {
            Outgoing::Request { id, method, params } => json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            Outgoing::Notification { method, params } => json!({"jsonrpc": "2.0", "method": method, "params": params}),
            Outgoing::Response { id, result: Ok(v) } => return json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Outgoing::Response { id, result: Err(e) } => return json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        if msg["params"].is_null()
            && let Some(m) = msg.as_object_mut()
        {
            m.remove("params");
        }
        msg
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Outgoing::Request { method, .. } | Outgoing::Notification { method, .. } => Some(method),
            Outgoing::Response { .. } => None,
        }
    }
}

/// Read one frame's body. `None` is the end of the stream between frames;
/// an end inside a frame is an error.
pub async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len: Option<usize> = None;
    let mut line = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line).await? == 0 {
            return if first { Ok(None) } else { Err(io::ErrorKind::UnexpectedEof.into()) };
        }
        first = false;
        let text = std::str::from_utf8(&line).map_err(|_| invalid("header is not UTF-8"))?.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break;
        }
        let (name, value) = text.split_once(':').ok_or_else(|| invalid("malformed header"))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            len = Some(value.trim().parse().map_err(|_| invalid("bad Content-Length"))?);
        }
    }
    let len = len.ok_or_else(|| invalid("no Content-Length"))?;
    if len > MAX_BODY {
        return Err(invalid("frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Write one message as a frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    w.write_all(&frame).await?;
    w.flush().await
}

/// Read frames until the stream ends or breaks; a body that does not parse
/// is logged and skipped. Dropping `tx` tells the client the server is gone.
pub fn spawn_reader<R: AsyncRead + Send + Unpin + 'static>(name: String, r: R, tx: UnboundedSender<Incoming>) {
    tokio::spawn(async move {
        let mut r = BufReader::new(r);
        loop {
            match read_frame(&mut r).await {
                Ok(Some(body)) => match Incoming::parse(&body) {
                    Ok(msg) => {
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!("{name}: unreadable message: {e}"),
                },
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!("{name}: reading from the server: {e}");
                    return;
                }
            }
        }
    });
}

/// Write messages in order until `rx` closes or the stream breaks.
pub fn spawn_writer<W: AsyncWrite + Send + Unpin + 'static>(name: String, mut w: W, mut rx: UnboundedReceiver<Outgoing>) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_frame(&mut w, &msg.to_json()).await {
                tracing::warn!("{name}: writing to the server: {e}");
                return;
            }
        }
        let _ = w.shutdown().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_with_split_reads_and_non_ascii_bodies() {
        let msgs = [
            Outgoing::Request { id: 1, method: "initialize".into(), params: json!({"rootUri": "file:///tmp/日本"}) },
            Outgoing::notification("textDocument/didChange", json!({"text": "a😀b\r\nü"})),
            Outgoing::Response { id: RequestId::Str("x".into()), result: Err(ResponseError::new(-32601, "nope")) },
            Outgoing::Response { id: RequestId::Num(7), result: Ok(Value::Null) },
            Outgoing::notification("exit", Value::Null),
        ];
        let mut bytes = Vec::new();
        for m in &msgs {
            write_frame(&mut bytes, &m.to_json()).await.unwrap();
        }
        // Content-Length counts bytes, not chars.
        let body = serde_json::to_vec(&msgs[1].to_json()).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains(&format!("Content-Length: {}\r\n\r\n", body.len())));
        // Delivered a few bytes at a time, splitting headers, bodies and
        // multi-byte characters.
        let (client, mut server) = tokio::io::duplex(7);
        let feed = tokio::spawn(async move {
            for chunk in bytes.chunks(3) {
                server.write_all(chunk).await.unwrap();
            }
        });
        let mut r = BufReader::new(client);
        let mut got = Vec::new();
        while let Some(body) = read_frame(&mut r).await.unwrap() {
            got.push(Incoming::parse(&body).unwrap());
        }
        feed.await.unwrap();
        assert_eq!(
            got,
            [
                Incoming::Request { id: RequestId::Num(1), method: "initialize".into(), params: json!({"rootUri": "file:///tmp/日本"}) },
                Incoming::Notification { method: "textDocument/didChange".into(), params: json!({"text": "a😀b\r\nü"}) },
                Incoming::Response { id: RequestId::Str("x".into()), result: Err(ResponseError::new(-32601, "nope")) },
                Incoming::Response { id: RequestId::Num(7), result: Ok(Value::Null) },
                Incoming::Notification { method: "exit".into(), params: Value::Null },
            ]
        );
        assert_eq!(msgs[4].to_json(), json!({"jsonrpc": "2.0", "method": "exit"}), "no null params");
    }

    #[tokio::test]
    async fn bad_frames_are_errors() {
        let read = |s: &'static [u8]| async move { read_frame(&mut BufReader::new(s)).await };
        assert!(read(b"").await.unwrap().is_none(), "end between frames");
        assert_eq!(read(b"Content-Length: 10\r\n\r\n{}").await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read(b"Content-Type: x\r\n\r\n{}").await.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read(b"Content-Length: x\r\n\r\n").await.unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Header names are case-insensitive; other headers are skipped.
        let body = read(b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}").await.unwrap();
        assert_eq!(body.as_deref(), Some(&b"{}"[..]));
        assert!(Incoming::parse(b"{}").is_err());
        assert!(Incoming::parse(b"not json").is_err());
    }
}
