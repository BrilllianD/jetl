//! An in-process language server for tests, over `tokio::io::duplex`. It
//! answers `initialize` with the given capabilities and `shutdown` with
//! null by itself, and hands every message it receives to the test, which
//! can also send notifications, requests and responses of its own.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::client::Conn;
use super::transport::{read_frame, write_frame};

pub struct FakeServer {
    /// Every message from the client, in order.
    from_client: UnboundedReceiver<Value>,
    to_client: UnboundedSender<Value>,
}

/// A connection for `Client::start` and the server at its other end.
pub fn connect(capabilities: Value) -> (Conn, FakeServer) {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (server_read, mut server_write) = tokio::io::split(server_end);
    let (seen_tx, from_client) = mpsc::unbounded_channel();
    let (to_client, mut outbox) = mpsc::unbounded_channel::<Value>();
    let replies = to_client.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(server_read);
        while let Ok(Some(body)) = read_frame(&mut r).await {
            let msg: Value = serde_json::from_slice(&body).expect("client sent JSON");
            let id = msg.get("id").cloned();
            match (msg["method"].as_str(), id) {
                (Some("initialize"), Some(id)) => {
                    let _ = replies.send(json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": capabilities}}));
                }
                (Some("shutdown"), Some(id)) => {
                    let _ = replies.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
                }
                _ => {}
            }
            if seen_tx.send(msg).is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(msg) = outbox.recv().await {
            if write_frame(&mut server_write, &msg).await.is_err() {
                return;
            }
        }
    });
    (Conn { reader: Box::new(client_read), writer: Box::new(client_write), child: None }, FakeServer { from_client, to_client })
}

impl FakeServer {
    /// The next message from the client (5 s at most).
    pub async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.from_client.recv())
            .await
            .expect("a message in time")
            .expect("the client is connected")
    }

    /// Everything the client sent so far: a `test/ping` notification goes
    /// through the connection behind it, so unlike a `try_recv` loop this cannot miss
    /// a message still on its way.
    pub async fn sent_so_far(&mut self, client: &crate::lsp::client::Client) -> Vec<Value> {
        client.notify("test/ping", json!({}));
        let mut out = Vec::new();
        loop {
            let msg = self.next().await;
            if msg["method"] == "test/ping" {
                return out;
            }
            out.push(msg);
        }
    }

    /// The next message with `method`, skipping others.
    pub async fn recv(&mut self, method: &str) -> Value {
        loop {
            let msg = self.next().await;
            if msg["method"] == method {
                return msg;
            }
        }
    }

    /// The next answer to server request `id`, skipping other messages.
    pub async fn response(&mut self, id: i64) -> Value {
        loop {
            let msg = self.next().await;
            if msg.get("method").is_none() && msg["id"] == id {
                return msg;
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.to_client.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn request(&self, id: i64, method: &str, params: Value) {
        let _ = self.to_client.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    pub fn respond(&self, id: i64, result: Value) {
        let _ = self.to_client.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub fn respond_error(&self, id: i64, code: i64, message: &str) {
        let _ = self.to_client.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }
}
