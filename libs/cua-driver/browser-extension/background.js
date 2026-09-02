const HOST = "com.hcompany.cua_driver";
const PROTOCOL_VERSION = 1;

let nativePort;
let attached;

function reply(id, result, error) {
  const message = { id };
  if (error) message.error = { code: -32000, message: String(error) };
  else message.result = result ?? {};
  return message;
}

async function activePageTargets() {
  const [targets, tabs] = await Promise.all([
    chrome.debugger.getTargets(),
    chrome.tabs.query({ active: true }),
  ]);
  const active = new Set(tabs.filter(tab => tab.id !== undefined).map(tab => tab.id));
  return targets.filter(target =>
    target.type === "page" && target.tabId !== undefined && active.has(target.tabId));
}

async function targetForId(targetId) {
  const target = (await activePageTargets()).find(item => item.id === targetId);
  if (!target || target.tabId === undefined)
    throw new Error("the approved active browser target is no longer available");
  return target;
}

async function rootCommand(method, params) {
  if (method === "Target.getTargets") {
    const targets = await activePageTargets();
    return {
      targetInfos: targets.map(target => ({
        targetId: target.id,
        type: target.type,
        title: target.title ?? "",
        url: target.url ?? "",
        attached: target.attached,
      })),
    };
  }
  if (method === "Browser.getWindowForTarget") {
    const target = await targetForId(params?.targetId);
    const tab = await chrome.tabs.get(target.tabId);
    return { windowId: tab.windowId };
  }
  if (method === "Browser.getWindowBounds") {
    const window = await chrome.windows.get(params?.windowId);
    return {
      bounds: {
        left: window.left,
        top: window.top,
        width: window.width,
        height: window.height,
        windowState: window.state,
      },
    };
  }
  if (method === "Target.attachToTarget") {
    if (attached) throw new Error("a browser target is already attached");
    const target = await targetForId(params?.targetId);
    const tab = await chrome.tabs.get(target.tabId);
    if (!tab.active) throw new Error("the approved browser target is no longer active");
    await chrome.debugger.attach({ tabId: target.tabId }, "1.3");
    attached = {
      tabId: target.tabId,
      targetId: target.id,
      windowId: tab.windowId,
      sessionId: `cua-tab-${target.tabId}`,
      children: new Set(),
    };
    return { sessionId: attached.sessionId };
  }
  if (method === "Target.closeTarget") {
    const target = await targetForId(params?.targetId);
    await chrome.tabs.remove(target.tabId);
    return { success: true };
  }
  if (!attached) throw new Error(`browser-level command requires an attached tab: ${method}`);
  return chrome.debugger.sendCommand({ tabId: attached.tabId }, method, params ?? {});
}

async function sessionCommand(sessionId, method, params) {
  if (!attached) throw new Error("the approved browser target is not attached");
  let debuggee;
  if (sessionId === attached.sessionId) debuggee = { tabId: attached.tabId };
  else if (attached.children.has(sessionId)) debuggee = { tabId: attached.tabId, sessionId };
  else throw new Error("the CDP session is no longer attached to the approved tab");
  return chrome.debugger.sendCommand(debuggee, method, params ?? {});
}

async function handleCdp(message) {
  try {
    const result = message.sessionId
      ? await sessionCommand(message.sessionId, message.method, message.params)
      : await rootCommand(message.method, message.params);
    post({ op: "cdp", message: reply(message.id, result) });
  } catch (error) {
    post({ op: "cdp", message: reply(message.id, undefined, error?.message ?? error) });
  }
}

async function detach() {
  const tabId = attached?.tabId;
  attached = undefined;
  if (tabId !== undefined)
    await chrome.debugger.detach({ tabId }).catch(() => {});
}

function post(message) {
  if (nativePort) nativePort.postMessage(message);
}

function connect() {
  if (nativePort) return;
  try {
    nativePort = chrome.runtime.connectNative(HOST);
    nativePort.onMessage.addListener(message => {
      if (message.op === "hello")
        post({ event: "ready", protocol_version: PROTOCOL_VERSION });
      else if (message.op === "cdp")
        void handleCdp(message.message);
      else if (message.op === "relay_disconnected")
        void detach();
    });
    nativePort.onDisconnect.addListener(async () => {
      nativePort = undefined;
      // A dead controller loses all authority before an authority-free channel
      // is created; a later driver must perform exact binding again.
      await detach();
      setTimeout(connect, 1000);
    });
  } catch {
    nativePort = undefined;
  }
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (!attached || source.tabId !== attached.tabId) return;
  const child = params?.sessionId;
  if (method === "Target.attachedToTarget" && child) attached.children.add(child);
  if (method === "Target.detachedFromTarget" && child) attached.children.delete(child);
  const sessionId = source.sessionId ?? attached.sessionId;
  post({ op: "cdp", message: { method, params: params ?? {}, sessionId } });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  if (!attached || source.tabId !== attached.tabId) return;
  const sessionId = attached.sessionId;
  const targetId = attached.targetId;
  attached = undefined;
  post({
    op: "cdp",
    message: {
      method: "Target.detachedFromTarget",
      params: { sessionId, targetId, reason },
    },
  });
});

chrome.tabs.onRemoved.addListener(tabId => {
  if (attached?.tabId === tabId) attached = undefined;
});

chrome.runtime.onStartup.addListener(connect);
connect();
