const HOST = "com.hcompany.cua_driver_extension_spike";
const PROTOCOL_VERSION = 1;

let nativePort;
let attached;

function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

async function digest(bytes) {
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(hash), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function decodeBase64(value) {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

async function tabMetadata(tabId) {
  const tab = await chrome.tabs.get(tabId);
  const window = await chrome.windows.get(tab.windowId);
  return {
    tab_id: tab.id,
    chrome_window_id: tab.windowId,
    active: tab.active,
    origin: originOf(tab.url),
    window: {
      left: window.left,
      top: window.top,
      width: window.width,
      height: window.height,
      state: window.state,
    },
  };
}

async function activeTab() {
  const tabs = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  if (tabs.length !== 1 || tabs[0].id === undefined) {
    throw new Error(`expected one active tab, found ${tabs.length}`);
  }
  return tabMetadata(tabs[0].id);
}

async function documentIdentity(tabId) {
  const [result, tab] = await Promise.all([
    chrome.debugger.sendCommand({ tabId }, "Page.getFrameTree"),
    chrome.tabs.get(tabId),
  ]);
  const frame = result.frameTree?.frame;
  if (!frame?.id || !frame.loaderId || !tab.url) {
    throw new Error("the attached tab has no complete top-frame identity");
  }
  return { frame_id: frame.id, loader_id: frame.loaderId, url: tab.url };
}

async function attach(message) {
  if (attached) {
    throw new Error("a tab is already attached");
  }
  const tab = await tabMetadata(message.tab_id);
  if (!tab.active) {
    throw new Error("the approved tab is no longer active in its Chrome window");
  }
  if (tab.chrome_window_id !== message.chrome_window_id) {
    throw new Error("the approved tab moved to another Chrome window");
  }
  if (tab.origin !== message.expected_origin) {
    throw new Error("the approved tab origin changed before attachment");
  }
  await chrome.debugger.attach({ tabId: tab.tab_id }, "1.3");
  attached = {
    tab_id: tab.tab_id,
    chrome_window_id: tab.chrome_window_id,
    origin: tab.origin,
    generation: 1,
  };
  try {
    // The document token is checked synchronously before each operation because
    // Chrome can suppress tab and debugger navigation events while attached.
    await chrome.debugger.sendCommand({ tabId: tab.tab_id }, "Page.enable");
    attached.document = await documentIdentity(tab.tab_id);
  } catch (error) {
    attached = undefined;
    await chrome.debugger.detach({ tabId: tab.tab_id }).catch(() => {});
    throw error;
  }
  return { ...tab, generation: attached.generation };
}

async function proveAttached(message) {
  if (!attached || attached.tab_id !== message.tab_id) {
    throw new Error("the requested tab is not attached");
  }
  const tab = await tabMetadata(message.tab_id);
  if (
    tab.chrome_window_id !== attached.chrome_window_id ||
    tab.origin !== attached.origin
  ) {
    throw new Error("the attached tab identity changed");
  }
  const document = await documentIdentity(message.tab_id);
  if (
    document.frame_id !== attached.document.frame_id ||
    document.loader_id !== attached.document.loader_id ||
    document.url !== attached.document.url
  ) {
    attached.generation += 1;
    attached.document = document;
    post({
      event: "generation_changed",
      tab_id: attached.tab_id,
      generation: attached.generation,
    });
  }
  if (attached.generation !== message.generation) {
    throw new Error("the attached document generation changed");
  }
  return tab;
}

async function cdp(message) {
  await proveAttached(message);
  let params;
  if (message.method === "DOMSnapshot.captureSnapshot") {
    params = { computedStyles: [], includeDOMRects: true };
  } else if (message.method === "Page.captureScreenshot") {
    params = { format: "png", fromSurface: true, captureBeyondViewport: false };
  } else {
    throw new Error(`CDP method is not allowed: ${message.method}`);
  }

  const result = await chrome.debugger.sendCommand(
    { tabId: message.tab_id },
    message.method,
    params,
  );
  if (message.method === "Page.captureScreenshot") {
    const bytes = decodeBase64(result.data);
    return {
      method: message.method,
      byte_length: bytes.byteLength,
      sha256: await digest(bytes),
    };
  }

  const bytes = new TextEncoder().encode(JSON.stringify(result));
  const documents = result.documents ?? [];
  return {
    method: message.method,
    document_count: documents.length,
    node_count: documents.reduce(
      (count, document) => count + (document.nodes?.nodeName?.length ?? 0),
      0,
    ),
    byte_length: bytes.byteLength,
    sha256: await digest(bytes),
  };
}

async function detach(message) {
  if (!attached || attached.tab_id !== message.tab_id) {
    throw new Error("the requested tab is not attached");
  }
  await chrome.debugger.detach({ tabId: message.tab_id });
  attached = undefined;
  return { detached: true };
}

async function dispatch(message) {
  switch (message.op) {
    case "active_tab":
      return activeTab();
    case "attach":
      return attach(message);
    case "cdp":
      return cdp(message);
    case "detach":
      return detach(message);
    default:
      throw new Error(`unsupported operation: ${message.op}`);
  }
}

function post(message) {
  if (nativePort) {
    nativePort.postMessage(message);
  }
}

function connect() {
  if (nativePort) {
    return;
  }
  try {
    nativePort = chrome.runtime.connectNative(HOST);
    nativePort.onMessage.addListener((message) => {
      Promise.resolve(dispatch(message)).then(
        (result) => post({ id: message.id, ok: true, result }),
        (error) =>
          post({
            id: message.id,
            ok: false,
            error: String(error?.message ?? error),
          }),
      );
    });
    nativePort.onDisconnect.addListener(async () => {
      nativePort = undefined;
      if (attached) {
        // Losing the controller must not strand the user's tab under debugger
        // control or let a later bridge silently inherit its authority.
        const tabId = attached.tab_id;
        attached = undefined;
        await chrome.debugger.detach({ tabId }).catch(() => {});
      }
      setTimeout(connect, 1000);
    });
    post({ event: "hello", protocol_version: PROTOCOL_VERSION });
  } catch {
    nativePort = undefined;
  }
}

chrome.debugger.onDetach.addListener((source, reason) => {
  if (attached && source.tabId === attached.tab_id) {
    const tabId = attached.tab_id;
    attached = undefined;
    post({ event: "detached", tab_id: tabId, reason });
  }
});

chrome.tabs.onRemoved.addListener((tabId) => {
  if (attached && tabId === attached.tab_id) {
    attached = undefined;
    post({ event: "tab_closed", tab_id: tabId });
  }
});

// MV3 workers are not guaranteed to run merely because Chrome restarted.
// Registering this event gives a restored profile an explicit channel wake-up.
chrome.runtime.onStartup.addListener(connect);
connect();
