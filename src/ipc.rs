//! Unix socket IPC between wl-menu daemon and CLI clients.

use serde::{Deserialize, Serialize};
use std::env;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

const LOG_PREFIX: &str = "wl-menu";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum IpcRequest {
    Open {
        #[serde(default = "default_menu")]
        menu: String,
    },
    Toggle {
        #[serde(default = "default_menu")]
        menu: String,
    },
    Close,
    Reload,
    Ping,
}

fn default_menu() -> String {
    "root".into()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

impl IpcResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            data: Some("ok".into()),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            data: None,
        }
    }
}

/// Default runtime socket path for the menu daemon.
pub fn socket_path() -> PathBuf {
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("wl-menu.sock")
}

/// Bind the daemon IPC socket, removing any stale file first.
pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path).map_err(|err| {
        eprintln!("{LOG_PREFIX}: ipc bind {} failed: {err}", path.display());
        err
    })
}

/// Read one JSON request line from a client stream.
pub fn read_request(stream: &mut UnixStream) -> std::io::Result<IpcRequest> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 4096 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ipc request too large",
            ));
        }
    }
    serde_json::from_slice(&buf).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid ipc json: {err}"),
        )
    })
}

/// Write a JSON response line to a client stream.
pub fn write_response(stream: &mut UnixStream, response: &IpcResponse) -> std::io::Result<()> {
    let line = serde_json::to_string(response).map_err(std::io::Error::other)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Send one IPC command to a running daemon and return its response.
pub fn client_call(path: &Path, request: &IpcRequest) -> Result<IpcResponse, String> {
    let mut stream = UnixStream::connect(path).map_err(|err| {
        format!("daemon not running ({}): {err}", path.display())
    })?;
    let line = serde_json::to_string(request).map_err(|err| err.to_string())?;
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|err| err.to_string())?;

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).map_err(|err| err.to_string())?;
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }
    serde_json::from_slice(&buf).map_err(|err| err.to_string())
}
