//! JSON-RPC 2.0 over a Unix socket, newline-delimited.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Maximum accepted request line.
pub const MAX_LINE: usize = 64 * 1024;

/// JSON-RPC error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// Application error; `data.code` carries the spec code.
pub const APP_ERROR: i64 = -32000;

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// An error returned to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn spec(code: homing_format::ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: APP_ERROR,
            message: message.into(),
            data: Some(json!({ "code": code.as_str() })),
        }
    }
    pub fn plain(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    /// The spec error code string, if any.
    pub fn spec_code(&self) -> Option<&str> {
        self.data.as_ref()?.get("code")?.as_str()
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.spec_code() {
            Some(c) => write!(f, "{c}: {}", self.message),
            None => write!(f, "rpc error {}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for RpcError {}

pub fn response(id: Value, result: Result<Value, RpcError>) -> Value {
    match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    }
}

/// Blocking client (used by homingctl, the launcher and tests).
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Self> {
        let s = UnixStream::connect(socket)
            .with_context(|| format!("cannot connect to agent at {}", socket.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let writer = s.try_clone()?;
        Ok(Self {
            reader: BufReader::new(s),
            writer,
            next_id: 1,
        })
    }

    /// Call a method. Application errors come back as `Err` containing an [`RpcError`].
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp)? == 0 {
            return Err(anyhow!("agent closed the connection"));
        }
        let v: Value = serde_json::from_str(&resp).context("invalid response from agent")?;
        if let Some(e) = v.get("error") {
            let e: RpcError = serde_json::from_value(e.clone()).context("invalid error object")?;
            return Err(e.into());
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}
