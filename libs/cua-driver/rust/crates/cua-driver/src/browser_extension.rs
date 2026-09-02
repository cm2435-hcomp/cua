//! Explicit installation and native-messaging relay for existing Chrome profiles.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const HOST_NAME: &str = "com.hcompany.cua_driver";
pub const EXTENSION_ID: &str = "aaicokmghmaijchfjgiaohgidiimegkp";
const EXTENSION_MANIFEST: &str = include_str!("../browser-extension/manifest.json");
const EXTENSION_WORKER: &str = include_str!("../browser-extension/background.js");
const MAX_HOST_TO_EXTENSION_BYTES: usize = 1024 * 1024;
const MAX_EXTENSION_TO_HOST_BYTES: usize = 64 * 1024 * 1024;

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is unavailable"))
}

fn install_root() -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    return Ok(home_dir()?.join("Library/Application Support/Cua Driver/browser-extension"));
    #[cfg(target_os = "linux")]
    return Ok(std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config"))
        .join("cua-driver/browser-extension"));
    #[allow(unreachable_code)]
    Err(anyhow!(
        "the browser extension is supported only on macOS and Linux"
    ))
}

fn native_manifest_path() -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    return Ok(home_dir()?.join(format!(
        "Library/Application Support/Google/Chrome/NativeMessagingHosts/{HOST_NAME}.json"
    )));
    #[cfg(target_os = "linux")]
    return Ok(home_dir()?.join(format!(
        ".config/google-chrome/NativeMessagingHosts/{HOST_NAME}.json"
    )));
    #[allow(unreachable_code)]
    Err(anyhow!(
        "the browser extension is supported only on macOS and Linux"
    ))
}

fn write_atomic(path: &Path, bytes: &[u8], executable: bool) -> anyhow::Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            &temporary,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn install() -> anyhow::Result<Value> {
    let root = install_root()?;
    write_atomic(
        &root.join("manifest.json"),
        EXTENSION_MANIFEST.as_bytes(),
        false,
    )?;
    write_atomic(
        &root.join("background.js"),
        EXTENSION_WORKER.as_bytes(),
        false,
    )?;

    let executable = std::env::current_exe()?.canonicalize()?;
    let launcher = root.join("native-host");
    let script = format!(
        "#!/bin/sh\nexec {} --browser-extension-host \"$@\"\n",
        shell_quote(&executable.to_string_lossy())
    );
    write_atomic(&launcher, script.as_bytes(), true)?;
    let native_manifest = native_manifest_path()?;
    let manifest = serde_json::to_vec_pretty(&json!({
        "name": HOST_NAME,
        "description": "Cua Driver existing-profile browser bridge",
        "path": launcher,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    }))?;
    write_atomic(&native_manifest, &manifest, false)?;
    Ok(json!({
        "installed": true,
        "extension_id": EXTENSION_ID,
        "extension_path": root,
        "native_manifest": native_manifest,
        "next_action": "Load the extension_path once from chrome://extensions using Load unpacked.",
    }))
}

pub fn status() -> anyhow::Result<Value> {
    let root = install_root()?;
    let manifest = native_manifest_path()?;
    Ok(json!({
        "installed": root.join("manifest.json").is_file()
            && root.join("background.js").is_file()
            && manifest.is_file(),
        "extension_id": EXTENSION_ID,
        "extension_path": root,
        "native_manifest": manifest,
    }))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn run_early_if_requested() -> Option<i32> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match arguments.as_slice() {
        [flag, origin, ..] if flag == "--browser-extension-host" => run_host(origin).map(|_| None),
        [command, action] if command == "browser-extension" && action == "install" => {
            install().map(Some)
        }
        [command, action] if command == "browser-extension" && action == "status" => {
            status().map(Some)
        }
        [command, action] if command == "browser-extension" && action == "path" => {
            status().map(|value| {
                Some(json!({
                    "extension_id": value["extension_id"],
                    "extension_path": value["extension_path"],
                }))
            })
        }
        [command, ..] if command == "browser-extension" => Err(anyhow!(
            "usage: cua-driver browser-extension install|status|path"
        )),
        _ => return None,
    };
    match result {
        Ok(Some(value)) => {
            println!("{value}");
            Some(0)
        }
        Ok(None) => Some(0),
        Err(error) => {
            eprintln!("cua-driver browser extension: {error:#}");
            Some(1)
        }
    }
}

fn endpoint_path() -> PathBuf {
    #[cfg(unix)]
    let uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let uid = 0;
    std::env::temp_dir().join(format!(
        "cua-driver-browser-extension-{uid}-{}.json",
        std::process::id()
    ))
}

struct EndpointGuard {
    path: PathBuf,
    ws_url: String,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        let owned = fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value["ws_url"].as_str().map(str::to_owned))
            .as_deref()
            == Some(self.ws_url.as_str());
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn read_native(mut input: tokio::io::Stdin, tx: mpsc::Sender<Value>) -> anyhow::Result<()> {
    loop {
        let mut length = [0_u8; 4];
        if input.read_exact(&mut length).await.is_err() {
            return Ok(());
        }
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_EXTENSION_TO_HOST_BYTES {
            return Err(anyhow!("extension message exceeds 64 MiB"));
        }
        let mut payload = vec![0; length];
        input.read_exact(&mut payload).await?;
        tx.send(serde_json::from_slice(&payload)?).await?;
    }
}

async fn write_native(
    mut output: tokio::io::Stdout,
    mut rx: mpsc::Receiver<Value>,
) -> anyhow::Result<()> {
    while let Some(message) = rx.recv().await {
        let payload = serde_json::to_vec(&message)?;
        if payload.len() > MAX_HOST_TO_EXTENSION_BYTES {
            return Err(anyhow!("native-host request exceeds 1 MiB"));
        }
        output
            .write_all(&(payload.len() as u32).to_le_bytes())
            .await?;
        output.write_all(&payload).await?;
        output.flush().await?;
    }
    Ok(())
}

pub fn run_host(origin: &str) -> anyhow::Result<()> {
    let expected = format!("chrome-extension://{EXTENSION_ID}/");
    if origin != expected {
        return Err(anyhow!(
            "native host caller is not the installed Cua extension"
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_host_async());
    // Native messaging stdin is a blocking OS pipe. If the relay fails, do not
    // let Tokio's blocking-pool shutdown hide that failure behind an immortal
    // native-host process; Chrome owns and will close the pipe independently.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

async fn run_host_async() -> anyhow::Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let path = format!("/devtools/browser/{}", Uuid::new_v4());
    let ws_url = format!("ws://127.0.0.1:{port}{path}");
    let endpoint = endpoint_path();
    write_atomic(
        &endpoint,
        &serde_json::to_vec(&json!({
            "protocol_version": 1,
            "host_pid": std::process::id(),
            "ws_url": ws_url,
            "extension_id": EXTENSION_ID,
        }))?,
        false,
    )?;
    let _guard = EndpointGuard {
        path: endpoint,
        ws_url: ws_url.clone(),
    };

    let (native_in_tx, mut native_in_rx) = mpsc::channel::<Value>(32);
    let (native_out_tx, native_out_rx) = mpsc::channel::<Value>(32);
    tokio::spawn(read_native(tokio::io::stdin(), native_in_tx));
    tokio::spawn(write_native(tokio::io::stdout(), native_out_rx));
    native_out_tx.send(json!({ "op": "hello" })).await?;

    loop {
        let (stream, _) = listener.accept().await?;
        let expected_path = path.clone();
        let socket = match tokio_tungstenite::accept_hdr_async(stream, move |
            request: &tokio_tungstenite::tungstenite::handshake::server::Request,
            response: tokio_tungstenite::tungstenite::handshake::server::Response,
        | {
            if request.uri().path() == expected_path {
                Ok(response)
            } else {
                Err(tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(
                    Some("unknown relay path".to_owned()),
                ))
            }
        })
        .await
        {
            Ok(socket) => socket,
            // Endpoint discovery may probe loopback listeners with a plain
            // HTTP request. That request has no relay authority and must not
            // be able to tear down Chrome's long-lived native host.
            Err(_) => continue,
        };
        let (mut ws_out, mut ws_in) = socket.split();
        loop {
            tokio::select! {
                incoming = ws_in.next() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let message: Value = serde_json::from_str(&text)?;
                        native_out_tx.send(json!({ "op": "cdp", "message": message })).await?;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(bytes))) => ws_out.send(Message::Pong(bytes)).await?,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                },
                incoming = native_in_rx.recv() => match incoming {
                    Some(message) if message["op"] == "cdp" => {
                        ws_out.send(Message::Text(message["message"].to_string())).await?;
                    }
                    Some(_) => {}
                    None => return Ok(()),
                }
            }
        }
        native_out_tx
            .send(json!({ "op": "relay_disconnected" }))
            .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_manifest_has_the_stable_id_key() {
        let manifest: Value = serde_json::from_str(EXTENSION_MANIFEST).unwrap();
        assert_eq!(manifest["background"]["service_worker"], "background.js");
        assert_eq!(EXTENSION_ID.len(), 32);
    }
}
