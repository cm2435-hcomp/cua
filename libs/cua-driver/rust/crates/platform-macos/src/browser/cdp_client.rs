//! Legacy-compatible CDP client for Electron and other Chromium apps.
//! HTTP discovery stays compatible with the page tool; WebSocket calls use
//! cua-driver-core's shared event-aware `CdpConnection` transport.

use cua_driver_core::browser::cdp_ws::CdpConnection;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CdpPageTarget {
    pub port: u16,
    pub title: String,
    pub url: String,
    websocket_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CdpScrollDispatch {
    pub port: u16,
    pub title: String,
    pub viewport_x: f64,
    pub viewport_y: f64,
    pub viewport_origin_x: f64,
    pub viewport_origin_y: f64,
}

pub(crate) struct PreparedCdpScroll {
    session: CdpSession,
    target: CdpPageTarget,
    point: CdpViewportPoint,
}

impl PreparedCdpScroll {
    pub(crate) async fn revalidate(&mut self, screen_x: f64, screen_y: f64) -> anyhow::Result<()> {
        let (title, point) = read_exact_viewport(&mut self.session, screen_x, screen_y).await?;
        if title != self.target.title {
            anyhow::bail!(
                "CDP page title changed after preflight: expected {:?}, observed {:?}",
                self.target.title,
                title
            );
        }
        self.point = point;
        Ok(())
    }

    pub(crate) async fn dispatch(
        mut self,
        delta_x: f64,
        delta_y: f64,
    ) -> anyhow::Result<CdpScrollDispatch> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.session.call(
                "Input.synthesizeScrollGesture",
                serde_json::json!({
                    "x": self.point.x,
                    "y": self.point.y,
                    "xDistance": -delta_x,
                    "yDistance": -delta_y,
                    "speed": 800,
                    "gestureSourceType": "mouse",
                    "preventFling": true
                }),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("CDP scroll gesture timed out after 5s"))??;
        Ok(CdpScrollDispatch {
            port: self.target.port,
            title: self.target.title,
            viewport_x: self.point.x,
            viewport_y: self.point.y,
            viewport_origin_x: self.point.origin_x,
            viewport_origin_y: self.point.origin_y,
        })
    }
}

pub struct CdpClient;

impl CdpClient {
    /// Returns true if a CDP endpoint is listening on `port`.
    pub async fn is_available(port: u16) -> bool {
        http_get_json(port).await.is_ok()
    }

    /// Scan the given ports and return the first one that has a "page" target.
    pub async fn find_page_target(ports: &[u16]) -> Option<u16> {
        for &port in ports {
            if let Ok(json) = http_get_json(port).await {
                if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&json) {
                    if let Some(list) = arr.as_array() {
                        let has_page = list
                            .iter()
                            .any(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"));
                        if has_page {
                            return Some(port);
                        }
                    }
                }
            }
        }
        None
    }

    /// Find a live CDP endpoint already listening on one of `pid`'s open TCP
    /// ports (e.g. because the app was launched with a debugging port).
    /// Unlike `find_page_target`, this doesn't guess at a fixed port list —
    /// it only sees ports the process itself opened. Callers that need to
    /// *activate* an inspector that isn't already listening (e.g. Electron's
    /// SIGUSR1 trick) do that themselves before falling back to this.
    ///
    /// Reuses `find_page_target`'s validation (parses the `/json` body and
    /// requires an actual "page" target) rather than the bare TCP-connect
    /// check `is_available` does — a port that merely accepts a connection
    /// and closes it (any number of unrelated local services do this) would
    /// otherwise pass as "available" with an empty, non-CDP response.
    ///
    /// Note: this specifically requires the classic `/json` HTTP discovery
    /// to work, so it won't find a port only reachable via the browser-level
    /// `devtools/browser` endpoint (see `CdpSession::connect`) — callers that
    /// already know the port (e.g. the user enabled `chrome://inspect`
    /// manually) don't need this lookup at all.
    pub async fn find_port_for_pid(pid: i32) -> Option<u16> {
        let ports = listening_ports(pid).await;
        Self::find_page_target(&ports).await
    }

    /// Bind one classic CDP page to the exact title of the requested native
    /// window. Never fall back to a first page, URL guess, or sibling tab.
    pub(crate) async fn bind_exact_page_for_pid(
        pid: i32,
        exact_window_title: &str,
    ) -> anyhow::Result<CdpPageTarget> {
        if exact_window_title.trim().is_empty() {
            anyhow::bail!("the target native window has no exact title for CDP page binding");
        }
        let ports = listening_ports(pid).await;
        if ports.is_empty() {
            anyhow::bail!("target PID {pid} owns no listening TCP port");
        }
        let mut matches = Vec::new();
        for port in ports {
            let Ok(json) = http_get_json(port).await else {
                continue;
            };
            let Ok(port_matches) = exact_page_targets_from_json(port, &json, exact_window_title)
            else {
                continue;
            };
            matches.extend(port_matches);
        }
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => anyhow::bail!(
                "target PID {pid} has no classic CDP page whose title exactly matches {exact_window_title:?}"
            ),
            count => anyhow::bail!(
                "target PID {pid} has {count} classic CDP pages whose title exactly matches {exact_window_title:?}"
            ),
        }
    }

    pub(crate) async fn prepare_scroll_gesture(
        target: &CdpPageTarget,
        screen_x: f64,
        screen_y: f64,
    ) -> anyhow::Result<PreparedCdpScroll> {
        let mut session = CdpSession::connect_exact(target).await?;
        let (title, point) = read_exact_viewport(&mut session, screen_x, screen_y).await?;
        if title != target.title {
            anyhow::bail!(
                "CDP page title changed during preflight: expected {:?}, observed {:?}",
                target.title,
                title
            );
        }
        Ok(PreparedCdpScroll {
            session,
            target: target.clone(),
            point,
        })
    }

    /// Evaluate JavaScript via CDP Runtime.evaluate on the first page target.
    /// Always opens a fresh connection — used only by the Electron inspector
    /// path (SIGUSR1-activated V8 inspector), which doesn't show the
    /// "Allow remote debugging?" confirmation the Chrome-toggle path does,
    /// so there's no reason to route it through `CdpSessionCache`.
    pub async fn evaluate(javascript: &str, port: u16) -> anyhow::Result<String> {
        let mut session = CdpSession::connect(port, None).await?;
        do_evaluate(&mut session, javascript).await
    }
}

/// Caches one open `CdpSession` per port across calls, instead of opening a
/// fresh WebSocket connection every time.
///
/// This exists because Chrome's own "allow remote debugging" toggle
/// (`chrome://inspect/#remote-debugging`, the only way to reach a user's
/// real/default profile — see `resolve_cdp_port` in `tools/page.rs`) pops up
/// a live "Allow remote debugging?" confirmation on every *new* WebSocket
/// connection to the browser endpoint — confirmed by testing live,
/// repeatedly, against a real Chrome session: three separate
/// `insert_text`/`type_keystrokes` calls, each opening its own connection,
/// each got its own popup. Reusing one already-approved connection across
/// calls (re-attaching to a different tab on it via `Target.getTargets` +
/// `Target.attachToTarget` when needed, never opening a new socket) avoids
/// re-triggering that popup for anything but the very first call after a
/// fresh Chrome launch.
pub struct CdpSessionCache {
    sessions: AsyncMutex<HashMap<u16, Arc<AsyncMutex<CdpSession>>>>,
}

impl CdpSessionCache {
    pub fn new() -> Self {
        Self {
            sessions: AsyncMutex::new(HashMap::new()),
        }
    }

    async fn get_or_connect(
        &self,
        port: u16,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<Arc<AsyncMutex<CdpSession>>> {
        let mut map = self.sessions.lock().await;
        if let Some(existing) = map.get(&port) {
            return Ok(existing.clone());
        }
        let session = CdpSession::connect(port, target_url_contains).await?;
        let arc = Arc::new(AsyncMutex::new(session));
        map.insert(port, arc.clone());
        Ok(arc)
    }

    async fn evict(&self, port: u16) {
        self.sessions.lock().await.remove(&port);
    }

    /// Evaluate JavaScript against the unique page target selected by
    /// `target_url_contains`, reusing the cached browser connection. This
    /// avoids re-triggering Chrome's "Allow remote debugging?" confirmation
    /// for every targeted page action.
    pub async fn evaluate(
        &self,
        javascript: &str,
        port: u16,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<String> {
        let arc = self.get_or_connect(port, target_url_contains).await?;
        {
            let mut session = arc.lock().await;
            if session
                .ensure_target(port, target_url_contains)
                .await
                .is_ok()
            {
                return do_evaluate(&mut session, javascript).await;
            }
        }

        self.evict(port).await;
        let arc = self.get_or_connect(port, target_url_contains).await?;
        let mut session = arc.lock().await;
        session.ensure_target(port, target_url_contains).await?;
        do_evaluate(&mut session, javascript).await
    }

    /// Insert `text` at whatever currently holds DOM focus, via CDP's native
    /// `Input.insertText` — a single call, no synthesized keydown/char/keyup
    /// sequence. Chrome's renderer handles this the same way it handles an
    /// IME composition commit, which most rich-text editors (having to
    /// support CJK/IME input regardless) already treat as real input — so
    /// it lands more durably than a JS-level DOM write, at a fraction of the
    /// cost of `dispatch_keystrokes`. It does skip actual key events though,
    /// so page-level keydown/keyup handlers never fire; reach for
    /// `dispatch_keystrokes` instead if an editor's own keyboard shortcuts or
    /// per-keystroke validation need to see real keys.
    ///
    /// Caller must already have focused the target element.
    /// `target_url_contains`, when given, picks the page target whose URL
    /// contains this substring instead of just whichever tab is currently
    /// attached — a browser with more than one tab open has no other way to
    /// know which one the caller means (CDP target ids have no relation to
    /// a window_id).
    pub async fn insert_text(
        &self,
        text: &str,
        port: u16,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<()> {
        let arc = self.get_or_connect(port, target_url_contains).await?;
        let first_attempt = {
            let mut session = arc.lock().await;
            match session.ensure_target(port, target_url_contains).await {
                Ok(()) => do_insert_text(&mut session, text).await,
                Err(e) => Err(e),
            }
        };
        if first_attempt.is_ok() {
            return first_attempt;
        }

        // Stale/closed connection — evict and retry once against a fresh
        // session, so a dead cache entry self-heals instead of wedging
        // every future call.
        self.evict(port).await;
        let arc = self.get_or_connect(port, target_url_contains).await?;
        let mut session = arc.lock().await;
        session.ensure_target(port, target_url_contains).await?;
        do_insert_text(&mut session, text).await
    }

    /// Type `text` into whatever currently holds DOM focus, via per-character
    /// `Input.dispatchKeyEvent` (keyDown -> char -> keyUp). The `char`
    /// event's `text` field is what actually inserts the character;
    /// bracketing it with keyDown/keyUp gives the page's own keydown/keyup
    /// listeners a real event to see.
    ///
    /// This exists because rich-text editors (React/Draft.js-style
    /// contenteditable) reconcile their own state and can silently discard a
    /// one-shot DOM write (`execCommand`, `innerText =`) on the next render —
    /// a per-character keystroke stream is what those editors' own input
    /// pipeline actually observes as durable.
    ///
    /// Caller must already have focused the target element (e.g. via a prior
    /// click) — this dispatches to whatever element currently has focus, the
    /// same as a hardware keyboard would. `target_url_contains` — see
    /// `insert_text`.
    pub async fn dispatch_keystrokes(
        &self,
        text: &str,
        port: u16,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<()> {
        let arc = self.get_or_connect(port, target_url_contains).await?;
        let first_attempt = {
            let mut session = arc.lock().await;
            match session.ensure_target(port, target_url_contains).await {
                Ok(()) => do_dispatch_keystrokes(&mut session, text).await,
                Err(e) => Err(e),
            }
        };
        if first_attempt.is_ok() {
            return first_attempt;
        }

        self.evict(port).await;
        let arc = self.get_or_connect(port, target_url_contains).await?;
        let mut session = arc.lock().await;
        session.ensure_target(port, target_url_contains).await?;
        do_dispatch_keystrokes(&mut session, text).await
    }
}

impl Default for CdpSessionCache {
    fn default() -> Self {
        Self::new()
    }
}

async fn do_evaluate(session: &mut CdpSession, javascript: &str) -> anyhow::Result<String> {
    let obj = tokio::time::timeout(
        Duration::from_secs(10),
        session.call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": javascript,
                "returnByValue": true,
                "awaitPromise": true
            }),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("CDP evaluate timed out after 10s"))??;

    parse_cdp_result(&obj)
}

async fn do_insert_text(session: &mut CdpSession, text: &str) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(10),
        session.call("Input.insertText", serde_json::json!({ "text": text })),
    )
    .await
    .map_err(|_| anyhow::anyhow!("CDP insertText timed out after 10s"))??;
    Ok(())
}

async fn do_dispatch_keystrokes(session: &mut CdpSession, text: &str) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        for ch in text.chars() {
            let key = ch.to_string();
            for event_type in ["keyDown", "char", "keyUp"] {
                // Only the `char` event carries `text`/`unmodifiedText` —
                // that's what actually inserts the character. Chrome
                // treats a non-empty `text` on `keyDown` as ALSO
                // producing input, so setting it on both double-types
                // every character (confirmed: "hello" -> "hheelllloo").
                let mut params = serde_json::json!({
                    "type": event_type,
                    "key": key,
                });
                if event_type == "char" {
                    params["text"] = serde_json::json!(key);
                    params["unmodifiedText"] = serde_json::json!(key);
                }
                session.call("Input.dispatchKeyEvent", params).await?;
            }
            // Small gap between characters — a real keyboard doesn't emit
            // 36 events in the same tick, and some editors' autosave /
            // hashtag-parsing debounce keys off discrete keystrokes.
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("CDP keystroke dispatch timed out after 30s"))?
}

/// An open CDP connection scoped to one page target, plus whatever session
/// routing that required.
///
/// Two ways to reach a page:
/// - **Classic** — the target's own `webSocketDebuggerUrl` from `/json`.
///   Every message goes straight to that page; no session id needed. This is
///   what a normal `--remote-debugging-port` launch gives you.
/// - **Browser-attached** — connect to the browser-level
///   `ws://host:port/devtools/browser` endpoint instead, `Target.getTargets`
///   to find a page, then `Target.attachToTarget{flatten:true}` to obtain a
///   `sessionId` that has to ride along on every subsequent message. This is
///   the path for a CDP port that doesn't serve `/json` at all — observed
///   with newer Chrome's in-browser "allow remote debugging" toggle for an
///   already-running profile, which opens the browser endpoint without the
///   classic HTTP discovery endpoints. Re-attaching to a *different* target
///   on this same connection (via `ensure_target`) is what lets
///   `CdpSessionCache` switch tabs without opening a new socket.
struct CdpSession {
    connection: Arc<CdpConnection>,
    session_id: Option<String>,
    /// URL of whichever target we're currently attached to/connected to —
    /// used by `ensure_target` to decide whether a re-attach is needed.
    current_target_url: Option<String>,
}

impl CdpSession {
    async fn connect_exact(target: &CdpPageTarget) -> anyhow::Result<Self> {
        let connection = Arc::new(
            CdpConnection::connect(&target.websocket_url)
                .await
                .map_err(|error| anyhow::anyhow!("WebSocket connect failed: {error}"))?,
        );
        Ok(Self {
            connection,
            session_id: None,
            current_target_url: Some(target.url.clone()),
        })
    }

    async fn connect(port: u16, target_url_contains: Option<&str>) -> anyhow::Result<Self> {
        match ws_url_for_page_target(port, target_url_contains).await {
            Ok((ws_url, target_url)) => {
                let connection = Arc::new(
                    CdpConnection::connect(&ws_url)
                        .await
                        .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {e}"))?,
                );
                Ok(Self {
                    connection,
                    session_id: None,
                    current_target_url: Some(target_url),
                })
            }
            Err(page_discovery_err) => {
                Self::connect_via_browser_endpoint(port, target_url_contains, page_discovery_err)
                    .await
            }
        }
    }

    async fn connect_via_browser_endpoint(
        port: u16,
        target_url_contains: Option<&str>,
        page_discovery_err: anyhow::Error,
    ) -> anyhow::Result<Self> {
        let browser_ws_url = format!("ws://127.0.0.1:{port}/devtools/browser");
        let connection = Arc::new(CdpConnection::connect(&browser_ws_url).await.map_err(|e| {
            anyhow::anyhow!(
                "No /json page discovery on port {port} ({page_discovery_err}), and \
                 connecting to the browser-level endpoint also failed: {e}"
            )
        })?);
        let mut session = Self {
            connection,
            session_id: None,
            current_target_url: None,
        };
        session.attach_to_target(port, target_url_contains).await?;
        Ok(session)
    }

    /// Make sure we're attached to the tab `target_url_contains` selects
    /// (or, with no hint, whatever we're already attached to — anything is
    /// fine). Re-attaching on an already-open browser-endpoint connection
    /// via `Target.getTargets`/`Target.attachToTarget` does NOT reopen the
    /// socket, so it doesn't re-trigger Chrome's confirmation popup —
    /// that's the whole reason this exists instead of always reconnecting.
    async fn ensure_target(
        &mut self,
        port: u16,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<()> {
        if self.session_id.is_some() {
            // Flattened browser-endpoint mode: always re-resolve + reattach
            // on the SAME connection (Target.getTargets/attachToTarget,
            // never a new socket, so it's cheap and doesn't reprompt).
            //
            // Deliberately does NOT skip this when `current_target_url`
            // already matches the hint — a brand-new tab can carry the
            // exact same URL as a tab that was just closed (e.g. reopening
            // the same compose page), and the cache has no way to tell
            // "same URL" apart from "same still-alive target" without
            // asking Chrome. Confirmed live: skipping this on a URL-only
            // match kept sending Input.insertText into a closed target's
            // dead session — no error, text just never appeared anywhere.
            self.attach_to_target(port, target_url_contains).await
        } else {
            // Classic mode is tied to one page's own websocket URL — there's
            // no in-place retarget, but this mode never shows the popup
            // (it's the dedicated-automation-profile route), so a full
            // reconnect here is cheap and harmless.
            *self = Self::connect(port, target_url_contains).await?;
            Ok(())
        }
    }

    async fn attach_to_target(
        &mut self,
        port: u16,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<()> {
        let targets = self
            .call("Target.getTargets", serde_json::json!({}))
            .await?;
        let infos = targets["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let pages: Vec<&serde_json::Value> = infos
            .iter()
            .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
            .collect();
        let target = pick_target(&pages, target_url_contains).ok_or_else(|| {
            anyhow::anyhow!("Target.getTargets returned no page target on port {port}")
        })?;
        let target_id = target
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Target.getTargets entry missing targetId"))?
            .to_owned();
        let target_url = target
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let attach = self
            .call(
                "Target.attachToTarget",
                serde_json::json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = attach["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Target.attachToTarget did not return a sessionId"))?
            .to_owned();
        self.session_id = Some(session_id);
        self.current_target_url = Some(target_url);
        Ok(())
    }

    /// Send one command through the shared event-aware CDP demultiplexer.
    async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.connection
            .call(self.session_id.as_deref(), method, params)
            .await
    }
}

/// Resolve the websocket debugger URL (and the target's own URL) for the
/// page target `target_url_contains` selects, via the classic `/json` HTTP
/// discovery. Errors (not just returns None) so `CdpSession::connect` can
/// report exactly why the classic path failed if the browser-endpoint
/// fallback also fails.
async fn ws_url_for_page_target(
    port: u16,
    target_url_contains: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let json = http_get_json(port).await?;
    let targets: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| anyhow::anyhow!("CDP /json parse error: {e}"))?;

    let pages: Vec<&serde_json::Value> = targets
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                .collect()
        })
        .unwrap_or_default();

    let target = pick_target(&pages, target_url_contains)
        .ok_or_else(|| anyhow::anyhow!("No page target found on port {port}"))?;
    let ws_url = target
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("No page target with webSocketDebuggerUrl found on port {port}")
        })?
        .to_owned();
    let target_url = target
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    Ok((ws_url, target_url))
}

/// Pick the unique page target whose `url` contains `hint`
/// (case-insensitive), or the first page when no hint is given. Explicit
/// hints fail closed when zero or multiple pages match. Shared by both
/// discovery paths (classic `/json` and
/// `Target.getTargets`) since a browser with more than one tab open is
/// otherwise picked non-deterministically — CDP target ids carry no
/// relationship to the caller's `window_id`.
fn pick_target<'a>(
    pages: &[&'a serde_json::Value],
    hint: Option<&str>,
) -> Option<&'a serde_json::Value> {
    match hint {
        None => pages.first().copied(),
        Some(hint) => {
            let hint_lower = hint.to_ascii_lowercase();
            let mut matches = pages.iter().copied().filter(|target| {
                target
                    .get("url")
                    .and_then(|value| value.as_str())
                    .is_some_and(|url| url.to_ascii_lowercase().contains(&hint_lower))
            });
            let target = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            Some(target)
        }
    }
}

fn exact_page_targets_from_json(
    port: u16,
    json: &str,
    exact_title: &str,
) -> anyhow::Result<Vec<CdpPageTarget>> {
    let targets: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| anyhow::anyhow!("invalid CDP /json: {error}"))?;
    let array = targets
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("CDP /json is not an array"))?;
    Ok(array
        .iter()
        .filter(|target| {
            target.get("type").and_then(|value| value.as_str()) == Some("page")
                && target.get("title").and_then(|value| value.as_str()) == Some(exact_title)
        })
        .filter_map(|target| {
            Some(CdpPageTarget {
                port,
                title: target.get("title")?.as_str()?.to_owned(),
                url: target
                    .get("url")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned(),
                websocket_url: target.get("webSocketDebuggerUrl")?.as_str()?.to_owned(),
            })
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CdpViewportPoint {
    x: f64,
    y: f64,
    origin_x: f64,
    origin_y: f64,
}

async fn read_exact_viewport(
    session: &mut CdpSession,
    screen_x: f64,
    screen_y: f64,
) -> anyhow::Result<(String, CdpViewportPoint)> {
    let metrics = tokio::time::timeout(
        Duration::from_secs(5),
        session.call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "({title: document.title, screenX: window.screenX, screenY: window.screenY, outerWidth: window.outerWidth, outerHeight: window.outerHeight, innerWidth: window.innerWidth, innerHeight: window.innerHeight})",
                "returnByValue": true
            }),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("CDP viewport metrics timed out after 5s"))??;
    let value = metrics["result"]["value"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("CDP viewport metrics did not return an object"))?;
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("CDP viewport metrics did not return document.title"))?
        .to_owned();
    Ok((
        title,
        viewport_point_from_metrics(value, screen_x, screen_y)?,
    ))
}

fn viewport_point_from_metrics(
    metrics: &serde_json::Map<String, serde_json::Value>,
    screen_x: f64,
    screen_y: f64,
) -> anyhow::Result<CdpViewportPoint> {
    let number = |name: &'static str| {
        metrics
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .filter(|value| value.is_finite())
            .ok_or_else(|| anyhow::anyhow!("CDP viewport metric {name} is missing or non-finite"))
    };
    let window_x = number("screenX")?;
    let window_y = number("screenY")?;
    let outer_width = number("outerWidth")?;
    let outer_height = number("outerHeight")?;
    let inner_width = number("innerWidth")?;
    let inner_height = number("innerHeight")?;
    if inner_width <= 0.0
        || inner_height <= 0.0
        || outer_width < inner_width
        || outer_height < inner_height
    {
        anyhow::bail!(
            "CDP viewport metrics are inconsistent: outer=({outer_width},{outer_height}) inner=({inner_width},{inner_height})"
        );
    }

    let side_inset = ((outer_width - inner_width) / 2.0).max(0.0);
    let top_inset = (outer_height - inner_height - side_inset).max(0.0);
    let origin_x = window_x + side_inset;
    let origin_y = window_y + top_inset;
    let x = screen_x - origin_x;
    let y = screen_y - origin_y;
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= inner_width
        || y >= inner_height
    {
        anyhow::bail!(
            "captured point ({screen_x},{screen_y}) maps outside the live CDP viewport: origin=({origin_x},{origin_y}) point=({x},{y}) size=({inner_width},{inner_height})"
        );
    }
    Ok(CdpViewportPoint {
        x,
        y,
        origin_x,
        origin_y,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        exact_page_targets_from_json, pick_target, viewport_point_from_metrics, CdpPageTarget,
        CdpSessionCache, CdpViewportPoint,
    };
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    fn pages() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({ "type": "page", "url": "app://fixture/#window-a" }),
            serde_json::json!({ "type": "page", "url": "app://fixture/#window-b" }),
        ]
    }

    #[test]
    fn exact_page_binding_never_falls_back_to_another_title() {
        let json = serde_json::json!([
            {
                "type": "page",
                "title": "Other page",
                "url": "http://127.0.0.1/other",
                "webSocketDebuggerUrl": "ws://127.0.0.1/devtools/page/other"
            },
            {
                "type": "page",
                "title": "CUA exact target",
                "url": "http://127.0.0.1/target",
                "webSocketDebuggerUrl": "ws://127.0.0.1/devtools/page/target"
            }
        ])
        .to_string();
        assert_eq!(
            exact_page_targets_from_json(9222, &json, "CUA exact target").unwrap(),
            vec![CdpPageTarget {
                port: 9222,
                title: "CUA exact target".to_owned(),
                url: "http://127.0.0.1/target".to_owned(),
                websocket_url: "ws://127.0.0.1/devtools/page/target".to_owned(),
            }]
        );
        assert!(exact_page_targets_from_json(9222, &json, "Missing")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn captured_screen_point_converts_through_live_browser_chrome_metrics() {
        let metrics = serde_json::json!({
            "screenX": 100.0,
            "screenY": 40.0,
            "outerWidth": 1000.0,
            "outerHeight": 800.0,
            "innerWidth": 990.0,
            "innerHeight": 700.0
        });
        let point =
            viewport_point_from_metrics(metrics.as_object().unwrap(), 305.0, 235.0).unwrap();
        assert_eq!(
            point,
            CdpViewportPoint {
                x: 200.0,
                y: 100.0,
                origin_x: 105.0,
                origin_y: 135.0,
            }
        );
        assert!(viewport_point_from_metrics(metrics.as_object().unwrap(), 10.0, 10.0).is_err());
    }

    #[test]
    fn explicit_target_hint_selects_one_page() {
        let pages = pages();
        let refs = pages.iter().collect::<Vec<_>>();
        assert_eq!(
            pick_target(&refs, Some("#WINDOW-B")).and_then(|target| target["url"].as_str()),
            Some("app://fixture/#window-b")
        );
    }

    #[test]
    fn explicit_target_hint_never_falls_back() {
        let pages = pages();
        let refs = pages.iter().collect::<Vec<_>>();
        assert!(pick_target(&refs, Some("#missing")).is_none());
    }

    #[test]
    fn ambiguous_target_hint_fails_closed() {
        let pages = pages();
        let refs = pages.iter().collect::<Vec<_>>();
        assert!(pick_target(&refs, Some("app://fixture/")).is_none());
    }

    #[test]
    fn omitted_target_hint_keeps_legacy_first_page_behavior() {
        let pages = pages();
        let refs = pages.iter().collect::<Vec<_>>();
        assert_eq!(
            pick_target(&refs, None).and_then(|target| target["url"].as_str()),
            Some("app://fixture/#window-a")
        );
    }

    #[tokio::test]
    async fn targeted_evaluate_reuses_browser_websocket() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_by_server = accepted.clone();

        let server = tokio::spawn(async move {
            let (mut probe, _) = listener.accept().await.unwrap();
            accepted_by_server.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 1024];
            let _ = probe.read(&mut request).await.unwrap();
            probe
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();

            let (socket, _) = listener.accept().await.unwrap();
            accepted_by_server.fetch_add(1, Ordering::SeqCst);
            let mut websocket = accept_async(socket).await.unwrap();
            let mut attached = 0;
            let mut evaluated = 0;

            while evaluated < 2 {
                let message = websocket.next().await.unwrap().unwrap();
                let Message::Text(text) = message else {
                    continue;
                };
                let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                let id = request["id"].as_u64().unwrap();
                let method = request["method"].as_str().unwrap();
                let response = match method {
                    "Target.getTargets" => serde_json::json!({
                        "id": id,
                        "result": {
                            "targetInfos": [{
                                "targetId": "page-a",
                                "type": "page",
                                "url": "app://fixture/#window-a"
                            }]
                        }
                    }),
                    "Target.attachToTarget" => {
                        attached += 1;
                        serde_json::json!({
                            "id": id,
                            "result": { "sessionId": format!("session-{attached}") }
                        })
                    }
                    "Runtime.evaluate" => {
                        evaluated += 1;
                        serde_json::json!({
                            "id": id,
                            "sessionId": request["sessionId"],
                            "result": { "result": { "value": format!("value-{evaluated}") } }
                        })
                    }
                    other => panic!("unexpected method {other}"),
                };
                websocket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });

        let cache = CdpSessionCache::new();
        assert_eq!(
            cache.evaluate("1", port, Some("#window-a")).await.unwrap(),
            "value-1"
        );
        assert_eq!(
            cache.evaluate("2", port, Some("#window-a")).await.unwrap(),
            "value-2"
        );
        server.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn targeted_evaluate_does_not_replay_after_disconnect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_by_server = accepted.clone();

        let server = tokio::spawn(async move {
            let (mut probe, _) = listener.accept().await.unwrap();
            accepted_by_server.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 1024];
            let _ = probe.read(&mut request).await.unwrap();
            probe
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();

            let (socket, _) = listener.accept().await.unwrap();
            accepted_by_server.fetch_add(1, Ordering::SeqCst);
            let mut websocket = accept_async(socket).await.unwrap();

            loop {
                let message = websocket.next().await.unwrap().unwrap();
                let Message::Text(text) = message else {
                    continue;
                };
                let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                let id = request["id"].as_u64().unwrap();
                let method = request["method"].as_str().unwrap();
                let response = match method {
                    "Target.getTargets" => serde_json::json!({
                        "id": id,
                        "result": {
                            "targetInfos": [{
                                "targetId": "page-a",
                                "type": "page",
                                "url": "app://fixture/#window-a"
                            }]
                        }
                    }),
                    "Target.attachToTarget" => serde_json::json!({
                        "id": id,
                        "result": { "sessionId": "session-a" }
                    }),
                    "Runtime.evaluate" => break,
                    other => panic!("unexpected method {other}"),
                };
                websocket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
            websocket.close(None).await.unwrap();

            if let Ok(Ok((mut retry, _))) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
            {
                accepted_by_server.fetch_add(1, Ordering::SeqCst);
                let _ = retry.read(&mut request).await.unwrap();
            }
        });

        let cache = CdpSessionCache::new();
        assert!(cache
            .evaluate("sideEffect()", port, Some("#window-a"))
            .await
            .is_err());
        server.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }
}

fn parse_cdp_result(obj: &serde_json::Value) -> anyhow::Result<String> {
    if let Some(err) = obj.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("CDP error: {msg}");
    }
    let result = &obj["result"];
    if let Some(v) = result.get("value") {
        return Ok(match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => "null".to_owned(),
            other => other.to_string(),
        });
    }
    if let Some(desc) = result.get("description").and_then(|v| v.as_str()) {
        return Ok(desc.to_owned());
    }
    Ok("undefined".to_owned())
}

/// Return TCP listening ports for a process via `lsof`.
pub(super) async fn listening_ports(pid: i32) -> Vec<u16> {
    let out = tokio::process::Command::new("lsof")
        .args([
            // `-a` ANDs the selection criteria together — without it, lsof
            // ORs `-p` with `-iTCP -sTCP:LISTEN`, returning every listening
            // port on the whole system in addition to this pid's fds.
            "-a",
            "-p",
            &pid.to_string(),
            "-iTCP",
            "-sTCP:LISTEN",
            "-Fn",
            "-P",
        ])
        .output()
        .await;

    let Ok(out) = out else { return vec![] };
    let text = String::from_utf8_lossy(&out.stdout);

    let mut ports = Vec::new();
    for line in text.lines() {
        // Lines starting with 'n' contain the address e.g. n*:9222 or n127.0.0.1:9222
        let trimmed = line.trim();
        if !trimmed.starts_with('n') {
            continue;
        }
        if let Some(colon_pos) = trimmed.rfind(':') {
            let port_str = &trimmed[colon_pos + 1..];
            if let Ok(p) = port_str.parse::<u16>() {
                ports.push(p);
            }
        }
    }
    ports
}

async fn http_get_json(port: u16) -> anyhow::Result<String> {
    // The whole round-trip is timeout-guarded as one unit, not just the
    // connect — see the read loop below for why a bare `read_to_end` isn't
    // safe here.
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .map_err(|e| anyhow::anyhow!("TCP connect error: {e}"))?;

        let req =
            format!("GET /json HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await?;

        // Chrome's CDP HTTP server ignores our `Connection: close` request
        // and keeps the socket open, replying with `Content-Length` instead
        // — so `read_to_end` (which waits for the peer to close, i.e. EOF)
        // hangs forever. Read incrementally until the header block is
        // complete, parse `Content-Length`, then read exactly that many
        // body bytes.
        let mut buf = Vec::new();
        let header_end = loop {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                anyhow::bail!("connection closed before HTTP headers completed");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };

        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length: usize = headers
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);

        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        if content_length > 0 && body.len() > content_length {
            body.truncate(content_length);
        }
        Ok(String::from_utf8_lossy(&body).to_string())
    })
    .await
    .map_err(|_| anyhow::anyhow!("HTTP request to CDP port {port} timed out after 5s"))?
}
