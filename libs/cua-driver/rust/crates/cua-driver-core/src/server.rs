//! Async MCP stdio server loop.

use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::{JoinError, JoinSet};
use tracing::{debug, error, warn};

use crate::tool::ToolRegistry;
use crate::{
    api::{ClientId, DriverController, ErrorCode, ErrorPhase, NativeError, PlatformDriver},
    protocol::{
        initialize_result, Request, Response, V2Command, V2Failure, V2HandshakeRequest,
        V2HandshakeResponse, V2ProtocolVersion, V2RequestEnvelope, V2ResponseBody,
        V2ResponseEnvelope, V2Success, V2_METHODS, V2_PROTOCOL_VERSION,
    },
};

const V2_MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
const V2_MAX_IN_FLIGHT_REQUESTS: usize = 128;
const V2_RESPONSE_QUEUE_CAPACITY: usize = 128;
const V2_IDLE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct V2ServerMetadata {
    pub driver_name: String,
    pub driver_version: String,
    pub build: String,
}

impl V2ServerMetadata {
    pub fn current(driver_name: impl Into<String>, build: impl Into<String>) -> Self {
        Self {
            driver_name: driver_name.into(),
            driver_version: env!("CARGO_PKG_VERSION").to_owned(),
            build: build.into(),
        }
    }
}

/// Run the MCP server, reading JSON-RPC lines from stdin and writing
/// responses to stdout. Exits when stdin reaches EOF or a fatal I/O
/// error occurs.
pub async fn run(registry: Arc<ToolRegistry>) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = tokio::io::BufWriter::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!(raw = trimmed, "→ request");

        let response = match serde_json::from_str::<Request>(trimmed) {
            Err(e) => {
                error!("JSON parse error: {e}");
                Response::parse_error()
            }
            Ok(req) if req.is_notification() => {
                // Notifications are silently dropped.
                continue;
            }
            Ok(req) => {
                let id = req.id.clone().unwrap_or(serde_json::Value::Null);
                handle_request(req, id, &registry).await
            }
        };

        let serialized = serde_json::to_string(&response)
            .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {e}"}}}}"#));
        debug!(raw = %serialized, "← response");

        writer.write_all(serialized.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

/// Run the native, background-only v2 protocol over stdio.
///
/// This is intentionally separate from MCP and the legacy tool registry. The
/// first non-empty line must be the version-range handshake; every later line
/// is one strict [`V2RequestEnvelope`]. One opaque client id is created for the
/// transport lifetime and all target state is destroyed before this function
/// returns, regardless of whether the stream ended cleanly or with an error.
pub async fn run_v2<P: PlatformDriver>(
    controller: Arc<DriverController<P>>,
    metadata: V2ServerMetadata,
) -> anyhow::Result<()> {
    run_v2_io(
        tokio::io::stdin(),
        tokio::io::stdout(),
        controller,
        metadata,
    )
    .await
}

/// I/O-generic v2 server used by the production stdio entrypoint and protocol
/// boundary tests. It has no access to `ToolRegistry` by construction.
pub async fn run_v2_io<R, W, P>(
    input: R,
    output: W,
    controller: Arc<DriverController<P>>,
    metadata: V2ServerMetadata,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    P: PlatformDriver,
{
    ensure_v2_method_inventory()?;
    let client_id = ClientId::new();
    let invalidation_task = controller.start_invalidation_loop();
    let idle_controller = Arc::clone(&controller);
    let idle_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(V2_IDLE_SWEEP_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = idle_controller.expire_idle_targets().await {
                tracing::error!(
                    code = ?error.code,
                    phase = ?error.phase,
                    "v2 target idle-expiry teardown failed"
                );
            }
        }
    });

    let serve_result = serve_v2_io(input, output, &controller, &client_id, &metadata).await;
    idle_task.abort();
    invalidation_task.abort();
    let _ = idle_task.await;
    let _ = invalidation_task.await;
    let cleanup_result = controller.close_connection(&client_id).await;

    match (serve_result, cleanup_result) {
        (Ok(()), Ok(count)) => {
            debug!(client_id = %client_id, targets_destroyed = count, "v2 connection closed");
            Ok(())
        }
        (Err(error), Ok(count)) => {
            debug!(client_id = %client_id, targets_destroyed = count, "v2 connection failed and closed");
            Err(error)
        }
        (Ok(()), Err(cleanup)) => Err(anyhow::anyhow!(
            "v2 connection cleanup failed (code={:?}, phase={:?}): {}",
            cleanup.code,
            cleanup.phase,
            cleanup.message
        )),
        (Err(error), Err(cleanup)) => {
            tracing::error!(
                client_id = %client_id,
                code = ?cleanup.code,
                phase = ?cleanup.phase,
                "v2 connection cleanup also failed"
            );
            Err(error)
        }
    }
}

async fn serve_v2_io<R, W, P>(
    input: R,
    output: W,
    controller: &Arc<DriverController<P>>,
    client_id: &ClientId,
    metadata: &V2ServerMetadata,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    P: PlatformDriver,
{
    let mut reader = BufReader::new(input);
    let (response_tx, response_rx) = mpsc::channel(V2_RESPONSE_QUEUE_CAPACITY);
    let mut writer_task = tokio::spawn(write_v2_responses(output, response_rx));
    let mut dispatch_tasks = JoinSet::new();
    let effectful_dispatch = Arc::new(Semaphore::new(1));

    let handshake_line = match read_v2_line(&mut reader).await {
        Ok(Some(line)) => line,
        Ok(None) => {
            return finish_v2_writer(response_tx, writer_task, Ok(())).await;
        }
        Err(error) => {
            return finish_v2_writer(response_tx, writer_task, Err(error)).await;
        }
    };
    let handshake = match serde_json::from_str::<V2HandshakeRequest>(&handshake_line) {
        Ok(handshake) => handshake,
        Err(error) => {
            let request_id = request_id_from_invalid_line(&handshake_line);
            let queued = queue_v2_failure(
                &response_tx,
                request_id,
                NativeError::invalid(format!("invalid v2 handshake: {error}")),
            )
            .await;
            return finish_v2_writer(response_tx, writer_task, queued).await;
        }
    };
    if let Err(protocol_error) = validate_handshake_range(&handshake) {
        let queued = queue_v2_failure(&response_tx, handshake.request_id, protocol_error).await;
        return finish_v2_writer(response_tx, writer_task, queued).await;
    }
    if let Err(error) = queue_v2_json(
        &response_tx,
        &V2HandshakeResponse {
            request_id: handshake.request_id,
            minimum_version: V2_PROTOCOL_VERSION,
            maximum_version: V2_PROTOCOL_VERSION,
            driver_name: metadata.driver_name.clone(),
            driver_version: metadata.driver_version.clone(),
            build: metadata.build.clone(),
            methods: V2_METHODS
                .iter()
                .map(|method| (*method).to_owned())
                .collect(),
        },
    )
    .await
    {
        return finish_v2_writer(response_tx, writer_task, Err(error)).await;
    }

    let mut serve_result = Ok(());
    let mut completed_writer_result = None;
    loop {
        tokio::select! {
            writer_result = &mut writer_task => {
                completed_writer_result = Some(flatten_writer_result(writer_result));
                break;
            }
            completed = dispatch_tasks.join_next(), if !dispatch_tasks.is_empty() => {
                if let Some(completed) = completed {
                    if let Err(error) = flatten_dispatch_result(completed) {
                        serve_result = Err(error);
                        break;
                    }
                }
            }
            line = read_v2_line(&mut reader) => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(error) => {
                        serve_result = Err(error);
                        break;
                    }
                };
                let request = match serde_json::from_str::<V2RequestEnvelope>(&line) {
                    Ok(request) => request,
                    Err(error) => {
                        if let Err(write_error) = queue_v2_failure(
                            &response_tx,
                            request_id_from_invalid_line(&line),
                            NativeError::invalid(format!("invalid v2 request envelope: {error}")),
                        )
                        .await
                        {
                            serve_result = Err(write_error);
                            break;
                        }
                        continue;
                    }
                };
                let request_id = request.request_id.clone();
                let method = request.command.method();
                if let Err(version_error) = request.validate_version() {
                    if let Err(write_error) =
                        queue_v2_failure(&response_tx, request_id, version_error).await
                    {
                        serve_result = Err(write_error);
                        break;
                    }
                    continue;
                }
                if dispatch_tasks.len() >= V2_MAX_IN_FLIGHT_REQUESTS {
                    let overload = NativeError::new(
                        ErrorCode::TargetBusy,
                        ErrorPhase::Preflight,
                        true,
                        "native v2 connection reached its in-flight request limit",
                    )
                    .with_detail("limit", V2_MAX_IN_FLIGHT_REQUESTS);
                    if let Err(write_error) =
                        queue_v2_failure(&response_tx, request_id, overload).await
                    {
                        serve_result = Err(write_error);
                        break;
                    }
                    continue;
                }
                let effectful_permit = if request.command.requires_serial_dispatch() {
                    match Arc::clone(&effectful_dispatch).try_acquire_owned() {
                        Ok(permit) => Some(permit),
                        Err(_) => {
                            let busy = NativeError::new(
                                ErrorCode::TargetBusy,
                                ErrorPhase::Preflight,
                                true,
                                "another launch or mutation is already active on this v2 connection",
                            )
                            .with_detail("method", method);
                            if let Err(write_error) =
                                queue_v2_failure(&response_tx, request_id, busy).await
                            {
                                serve_result = Err(write_error);
                                break;
                            }
                            continue;
                        }
                    }
                } else {
                    None
                };
                debug!(request_id, method, client_id = %client_id, "v2 request started");
                let task_controller = Arc::clone(controller);
                let task_client_id = client_id.clone();
                let task_response_tx = response_tx.clone();
                dispatch_tasks.spawn(async move {
                    // The owned permit covers the complete native dispatch and
                    // is dropped only after its typed result is constructed.
                    let _effectful_permit = effectful_permit;
                    let result =
                        dispatch_v2(&task_controller, &task_client_id, request.command).await;
                    match result {
                        Ok(result) => {
                            debug!(request_id, method, "v2 request completed");
                            queue_v2_json(
                                &task_response_tx,
                                &V2ResponseEnvelope {
                                    request_id,
                                    protocol_version: V2_PROTOCOL_VERSION,
                                    body: V2ResponseBody::Result(V2Success { result }),
                                },
                            )
                            .await
                        }
                        Err(error) => {
                            warn!(
                                request_id,
                                method,
                                code = ?error.code,
                                phase = ?error.phase,
                                retryable = error.retryable,
                                "v2 request failed"
                            );
                            queue_v2_failure(&task_response_tx, request_id, error).await
                        }
                    }
                });
            }
        }
    }

    // EOF ends intake, not in-flight native work. Drain every bounded request
    // before connection teardown so dropping the transport cannot release
    // target state underneath it.
    while let Some(completed) = dispatch_tasks.join_next().await {
        if let Err(error) = flatten_dispatch_result(completed) {
            if serve_result.is_ok() {
                serve_result = Err(error);
            } else {
                tracing::error!("v2 request task also failed while draining");
            }
        }
    }

    drop(response_tx);
    let writer_result = match completed_writer_result {
        Some(result) => result,
        None => flatten_writer_result(writer_task.await),
    };
    combine_v2_serve_results(serve_result, writer_result)
}

async fn write_v2_responses<W: AsyncWrite + Unpin>(
    output: W,
    mut responses: mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut writer = tokio::io::BufWriter::new(output);
    while let Some(serialized) = responses.recv().await {
        writer.write_all(&serialized).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

fn flatten_dispatch_result(result: Result<anyhow::Result<()>, JoinError>) -> anyhow::Result<()> {
    match result {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("v2 request task failed: {error}")),
    }
}

fn flatten_writer_result(result: Result<anyhow::Result<()>, JoinError>) -> anyhow::Result<()> {
    match result {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("v2 response writer task failed: {error}")),
    }
}

fn combine_v2_serve_results(
    serve_result: anyhow::Result<()>,
    writer_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (serve_result, writer_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(serve_error), Err(_writer_error)) => {
            tracing::error!("v2 response writer also failed after the serve loop failed");
            Err(serve_error)
        }
    }
}

async fn finish_v2_writer(
    response_tx: mpsc::Sender<Vec<u8>>,
    writer_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    serve_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    drop(response_tx);
    combine_v2_serve_results(serve_result, flatten_writer_result(writer_task.await))
}

async fn dispatch_v2<P: PlatformDriver>(
    controller: &DriverController<P>,
    client_id: &ClientId,
    command: V2Command,
) -> Result<serde_json::Value, NativeError> {
    let result = match command {
        V2Command::CheckReadiness(_) => serde_json::to_value(controller.check_readiness().await?),
        V2Command::GetCapabilities(_) => serde_json::to_value(controller.get_capabilities().await?),
        V2Command::ListApps(request) => serde_json::to_value(controller.list_apps(request).await?),
        V2Command::LaunchApp(request) => {
            serde_json::to_value(controller.launch_app(request).await?)
        }
        V2Command::ListWindows(request) => {
            serde_json::to_value(controller.list_windows(request).await?)
        }
        V2Command::GetWindow(request) => {
            serde_json::to_value(controller.get_window(request).await?)
        }
        V2Command::GetWindowState(request) => {
            serde_json::to_value(controller.get_window_state(client_id, request).await?)
        }
        V2Command::Click(command) => {
            serde_json::to_value(controller.click(client_id, command).await?)
        }
        V2Command::Drag(command) => {
            serde_json::to_value(controller.drag(client_id, command).await?)
        }
        V2Command::Scroll(command) => {
            serde_json::to_value(controller.scroll(client_id, command).await?)
        }
        V2Command::PressKey(command) => {
            serde_json::to_value(controller.press_key(client_id, command).await?)
        }
        V2Command::TypeText(command) => {
            serde_json::to_value(controller.type_text(client_id, command).await?)
        }
        V2Command::SetValue(command) => {
            serde_json::to_value(controller.set_value(client_id, command).await?)
        }
        V2Command::SelectText(command) => {
            serde_json::to_value(controller.select_text(client_id, command).await?)
        }
        V2Command::PerformSecondaryAction(command) => serde_json::to_value(
            controller
                .perform_secondary_action(client_id, command)
                .await?,
        ),
    };
    result.map_err(|error| {
        NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Verify,
            false,
            format!("failed to serialize typed v2 result: {error}"),
        )
    })
}

fn validate_handshake_range(request: &V2HandshakeRequest) -> Result<(), NativeError> {
    let ordered = request.minimum_version.major < request.maximum_version.major
        || (request.minimum_version.major == request.maximum_version.major
            && request.minimum_version.minor <= request.maximum_version.minor);
    let supported = version_at_least(V2_PROTOCOL_VERSION, request.minimum_version)
        && version_at_least(request.maximum_version, V2_PROTOCOL_VERSION);
    if ordered && supported {
        return Ok(());
    }
    Err(NativeError::new(
        ErrorCode::ProtocolMismatch,
        ErrorPhase::Validate,
        false,
        format!(
            "client protocol range {}.{} through {}.{} does not include driver protocol {}.{}",
            request.minimum_version.major,
            request.minimum_version.minor,
            request.maximum_version.major,
            request.maximum_version.minor,
            V2_PROTOCOL_VERSION.major,
            V2_PROTOCOL_VERSION.minor,
        ),
    ))
}

fn version_at_least(left: V2ProtocolVersion, right: V2ProtocolVersion) -> bool {
    (left.major, left.minor) >= (right.major, right.minor)
}

fn ensure_v2_method_inventory() -> anyhow::Result<()> {
    let mut methods = V2_METHODS.to_vec();
    methods.sort_unstable();
    if methods.windows(2).any(|pair| pair[0] == pair[1]) {
        anyhow::bail!("duplicate native v2 method in startup inventory");
    }
    if methods.len() != 15 {
        anyhow::bail!(
            "native v2 startup inventory has {} methods; expected 15",
            methods.len()
        );
    }
    Ok(())
}

async fn read_v2_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> anyhow::Result<Option<String>> {
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        if bytes > V2_MAX_LINE_BYTES {
            anyhow::bail!("v2 protocol line exceeded {V2_MAX_LINE_BYTES} bytes");
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_owned()));
        }
    }
}

fn request_id_from_invalid_line(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("request_id")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

async fn queue_v2_failure(
    responses: &mpsc::Sender<Vec<u8>>,
    request_id: String,
    error: NativeError,
) -> anyhow::Result<()> {
    queue_v2_json(
        responses,
        &V2ResponseEnvelope::<serde_json::Value> {
            request_id,
            protocol_version: V2_PROTOCOL_VERSION,
            body: V2ResponseBody::Error(V2Failure { error }),
        },
    )
    .await
}

async fn queue_v2_json<T: serde::Serialize>(
    responses: &mpsc::Sender<Vec<u8>>,
    value: &T,
) -> anyhow::Result<()> {
    let serialized = serde_json::to_vec(value)?;
    responses
        .send(serialized)
        .await
        .map_err(|_| anyhow::anyhow!("v2 response writer stopped before accepting a response"))
}

/// Dispatch one MCP JSON-RPC request against the registry (initialize /
/// tools/list / tools/call). Shared by the stdio loop above and the
/// daemon's HTTP transport (`cua-driver`'s `mcp_http`) so both speak the
/// exact same MCP semantics.
pub async fn handle_request(
    req: Request,
    id: serde_json::Value,
    registry: &Arc<ToolRegistry>,
) -> Response {
    match req.method.as_str() {
        "initialize" => Response::ok(id, initialize_result()),

        "tools/list" => Response::ok(id, registry.tools_list()),

        "tools/call" => match req.tool_call() {
            Err(e) => Response::error(id, -32602, format!("Invalid params: {e}")),
            Ok(call) => {
                let result = registry.invoke(&call.name, call.args).await;
                match serde_json::to_value(result) {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::error(id, -32603, format!("Serialize error: {e}")),
                }
            }
        },

        other => {
            warn!(method = other, "unknown method");
            Response::method_not_found(id, other)
        }
    }
}

#[cfg(test)]
mod v2_tests {
    use super::*;

    fn handshake(minimum: V2ProtocolVersion, maximum: V2ProtocolVersion) -> V2HandshakeRequest {
        V2HandshakeRequest {
            request_id: "handshake-1".to_owned(),
            minimum_version: minimum,
            maximum_version: maximum,
        }
    }

    #[test]
    fn native_method_inventory_is_complete_and_unique() {
        ensure_v2_method_inventory().expect("canonical v2 inventory");
        assert_eq!(
            V2_METHODS,
            [
                "driver.v2.check_readiness",
                "driver.v2.get_capabilities",
                "driver.v2.list_apps",
                "driver.v2.launch_app",
                "driver.v2.list_windows",
                "driver.v2.get_window",
                "driver.v2.get_window_state",
                "driver.v2.click",
                "driver.v2.drag",
                "driver.v2.scroll",
                "driver.v2.press_key",
                "driver.v2.type_text",
                "driver.v2.set_value",
                "driver.v2.select_text",
                "driver.v2.perform_secondary_action",
            ]
        );
    }

    #[test]
    fn handshake_accepts_only_a_range_containing_the_native_version() {
        assert!(validate_handshake_range(&handshake(
            V2ProtocolVersion { major: 2, minor: 0 },
            V2ProtocolVersion { major: 2, minor: 0 },
        ))
        .is_ok());

        for request in [
            handshake(
                V2ProtocolVersion { major: 3, minor: 0 },
                V2ProtocolVersion { major: 3, minor: 1 },
            ),
            handshake(
                V2ProtocolVersion { major: 1, minor: 0 },
                V2ProtocolVersion { major: 1, minor: 9 },
            ),
            handshake(
                V2ProtocolVersion { major: 2, minor: 1 },
                V2ProtocolVersion { major: 2, minor: 0 },
            ),
        ] {
            let error = validate_handshake_range(&request).expect_err("incompatible range");
            assert_eq!(error.code, ErrorCode::ProtocolMismatch);
            assert_eq!(error.phase, ErrorPhase::Validate);
            assert!(!error.retryable);
        }
    }
}
