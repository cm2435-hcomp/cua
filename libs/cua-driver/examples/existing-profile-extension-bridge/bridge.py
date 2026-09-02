#!/usr/bin/env python3
"""Disposable native-messaging bridge for the existing-profile CDP spike."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import selectors
import shlex
import socket
import stat
import struct
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


HOST_NAME = "com.hcompany.cua_driver_extension_spike"
MAX_MESSAGE_BYTES = 1024 * 1024
ALLOWED_OPS = {"active_tab", "attach", "cdp", "detach"}
EXTENSION_ID = re.compile(r"^[a-p]{32}$")


def config_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library/Application Support/Cua Driver Extension Spike"
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "cua-driver-extension-spike"


def manifest_path() -> Path:
    if sys.platform == "darwin":
        root = Path.home() / "Library/Application Support/Google/Chrome/NativeMessagingHosts"
    elif sys.platform.startswith("linux"):
        root = Path(os.environ.get("CHROME_CONFIG_HOME", Path.home() / ".config/google-chrome")) / "NativeMessagingHosts"
    else:
        raise SystemExit("the spike supports only macOS and Linux")
    return root / f"{HOST_NAME}.json"


def socket_path() -> Path:
    return Path(tempfile.gettempdir()) / f"cua-driver-extension-spike-{os.getuid()}.sock"


def atomic_json(path: Path, value: dict[str, Any], mode: int = 0o600) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    temporary.chmod(mode)
    temporary.replace(path)


def install(extension_id: str) -> None:
    if not EXTENSION_ID.fullmatch(extension_id):
        raise SystemExit("extension id must be 32 lowercase letters in the range a-p")
    directory = config_dir()
    directory.mkdir(parents=True, exist_ok=True)
    directory.chmod(0o700)
    bridge = Path(__file__).resolve()
    launcher = directory / "native-host"
    launcher.write_text(
        "#!/bin/sh\nexec "
        + shlex.quote(sys.executable)
        + " "
        + shlex.quote(str(bridge))
        + ' "$@"\n',
        encoding="utf-8",
    )
    launcher.chmod(0o700)
    origin = f"chrome-extension://{extension_id}/"
    atomic_json(
        directory / "config.json",
        {"extension_origin": origin, "socket_path": str(socket_path())},
    )
    atomic_json(
        manifest_path(),
        {
            "name": HOST_NAME,
            "description": "Cua Driver existing-profile extension spike",
            "path": str(launcher),
            "type": "stdio",
            "allowed_origins": [origin],
        },
    )
    print(json.dumps({"installed": True, "manifest": str(manifest_path()), "extension_origin": origin}))


def uninstall() -> None:
    for path in (manifest_path(), config_dir() / "config.json", config_dir() / "native-host"):
        try:
            path.unlink()
        except FileNotFoundError:
            pass
    remove_socket(socket_path())
    try:
        config_dir().rmdir()
    except OSError:
        pass
    print(json.dumps({"installed": False}))


def load_config() -> dict[str, Any]:
    value = json.loads((config_dir() / "config.json").read_text(encoding="utf-8"))
    if set(value) != {"extension_origin", "socket_path"}:
        raise RuntimeError("invalid bridge config")
    return value


def encode_native(message: dict[str, Any]) -> bytes:
    payload = json.dumps(message, separators=(",", ":")).encode("utf-8")
    if len(payload) > MAX_MESSAGE_BYTES:
        raise ValueError("native message exceeds 1 MiB")
    return struct.pack("=I", len(payload)) + payload


def decode_native(buffer: bytearray) -> list[dict[str, Any]]:
    messages: list[dict[str, Any]] = []
    while len(buffer) >= 4:
        length = struct.unpack("=I", buffer[:4])[0]
        if length > MAX_MESSAGE_BYTES:
            raise ValueError("native message exceeds 1 MiB")
        if len(buffer) < 4 + length:
            break
        payload = bytes(buffer[4 : 4 + length])
        del buffer[: 4 + length]
        value = json.loads(payload)
        if not isinstance(value, dict):
            raise ValueError("native message must be an object")
        messages.append(value)
    return messages


def remove_socket(path: Path) -> None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return
    if info.st_uid != os.getuid() or not stat.S_ISSOCK(info.st_mode):
        raise RuntimeError(f"refusing to replace non-owned socket path: {path}")
    path.unlink()


def send_line(client: socket.socket, message: dict[str, Any]) -> None:
    payload = json.dumps(message, separators=(",", ":")).encode("utf-8") + b"\n"
    if len(payload) > MAX_MESSAGE_BYTES:
        raise ValueError("control message exceeds 1 MiB")
    client.sendall(payload)


def host(origin: str) -> None:
    config = load_config()
    if origin != config["extension_origin"]:
        raise RuntimeError("native host caller origin does not match the installed extension")
    path = Path(config["socket_path"])
    remove_socket(path)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(path))
    path.chmod(0o600)
    server.listen(1)
    server.setblocking(False)

    selector = selectors.DefaultSelector()
    selector.register(sys.stdin.buffer, selectors.EVENT_READ, "native")
    selector.register(server, selectors.EVENT_READ, "server")
    native_buffer = bytearray()
    client: socket.socket | None = None
    client_buffer = bytearray()
    hello: dict[str, Any] | None = None

    try:
        while True:
            for key, _ in selector.select():
                if key.data == "server":
                    candidate, _ = server.accept()
                    if client is not None:
                        send_line(candidate, {"event": "busy"})
                        candidate.close()
                        continue
                    client = candidate
                    client.setblocking(False)
                    selector.register(client, selectors.EVENT_READ, "client")
                    if hello is not None:
                        send_line(client, hello)
                elif key.data == "native":
                    chunk = os.read(sys.stdin.fileno(), 65536)
                    if not chunk:
                        return
                    native_buffer.extend(chunk)
                    for message in decode_native(native_buffer):
                        if message.get("event") == "hello":
                            hello = message
                        if client is not None:
                            send_line(client, message)
                else:
                    assert client is not None
                    chunk = client.recv(65536)
                    if not chunk:
                        selector.unregister(client)
                        client.close()
                        client = None
                        client_buffer.clear()
                        continue
                    client_buffer.extend(chunk)
                    if len(client_buffer) > MAX_MESSAGE_BYTES:
                        raise ValueError("control message exceeds 1 MiB")
                    while b"\n" in client_buffer:
                        raw, _, rest = client_buffer.partition(b"\n")
                        client_buffer[:] = rest
                        message = json.loads(raw)
                        if not isinstance(message, dict) or message.get("op") not in ALLOWED_OPS:
                            send_line(client, {"id": message.get("id") if isinstance(message, dict) else None, "ok": False, "error": "unsupported control message"})
                            continue
                        os.write(sys.stdout.fileno(), encode_native(message))
    finally:
        selector.close()
        server.close()
        if client is not None:
            client.close()
        remove_socket(path)


class ControlClient:
    def __init__(self, path: Path, timeout: float) -> None:
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(timeout)
        self.socket.connect(str(path))
        self.file = self.socket.makefile("rb")
        self.next_id = 1
        self.events: list[dict[str, Any]] = []

    def close(self) -> None:
        self.file.close()
        self.socket.close()

    def receive(self) -> dict[str, Any]:
        line = self.file.readline(MAX_MESSAGE_BYTES + 1)
        if not line or len(line) > MAX_MESSAGE_BYTES:
            raise RuntimeError("bridge disconnected or returned an oversized message")
        value = json.loads(line)
        if not isinstance(value, dict):
            raise RuntimeError("bridge response must be an object")
        return value

    def wait_for_event(self, name: str) -> dict[str, Any]:
        return self.wait_for_any_event({name})

    def wait_for_any_event(self, names: set[str]) -> dict[str, Any]:
        while True:
            message = self.receive()
            if message.get("event") in names:
                return message
            self.events.append(message)

    def request(self, op: str, **fields: Any) -> tuple[dict[str, Any], float]:
        request_id = self.next_id
        self.next_id += 1
        started = time.monotonic()
        send_line(self.socket, {"id": request_id, "op": op, **fields})
        while True:
            message = self.receive()
            if message.get("id") == request_id:
                if not message.get("ok"):
                    raise RuntimeError(str(message.get("error", "bridge request failed")))
                return message["result"], time.monotonic() - started
            self.events.append(message)


def parse_bounds(value: str) -> tuple[float, float, float, float]:
    try:
        bounds = tuple(float(part) for part in value.split(","))
    except ValueError as error:
        raise argparse.ArgumentTypeError("bounds must contain four numbers") from error
    if len(bounds) != 4 or bounds[2] <= 0 or bounds[3] <= 0:
        raise argparse.ArgumentTypeError("bounds must be left,top,width,height with positive size")
    return bounds[0], bounds[1], bounds[2], bounds[3]


def probe(
    pid: int,
    window_id: int,
    native_bounds: tuple[float, float, float, float],
    expected_origin: str,
    timeout: float,
    wait_for_detach: float,
) -> None:
    client = ControlClient(socket_path(), timeout)
    attached_tab_id: int | None = None
    detached = False
    evidence: dict[str, Any] = {
        "native": {"pid": pid, "window_id": window_id, "bounds": native_bounds},
        "expected_origin": expected_origin,
        "timings_ms": {},
    }
    try:
        hello = client.wait_for_event("hello")
        if hello.get("protocol_version") != 1:
            raise RuntimeError("extension protocol version mismatch")
        tab, elapsed = client.request("active_tab")
        evidence["timings_ms"]["active_tab"] = round(elapsed * 1000, 3)
        if tab.get("origin") != expected_origin:
            raise RuntimeError("active tab does not match the expected fixture origin")
        chrome_window = tab.get("window", {})
        chrome_bounds = tuple(
            float(chrome_window[name]) for name in ("left", "top", "width", "height")
        )
        if any(abs(native - chrome) > 8 for native, chrome in zip(native_bounds, chrome_bounds)):
            raise RuntimeError("Chrome window geometry does not match the approved native window")
        attached_tab, elapsed = client.request(
            "attach",
            tab_id=tab["tab_id"],
            chrome_window_id=tab["chrome_window_id"],
            expected_origin=expected_origin,
        )
        attached_tab_id = attached_tab["tab_id"]
        evidence["timings_ms"]["attach"] = round(elapsed * 1000, 3)
        evidence["tab"] = attached_tab
        generation = attached_tab["generation"]
        for method in ("DOMSnapshot.captureSnapshot", "Page.captureScreenshot"):
            result, elapsed = client.request(
                "cdp", tab_id=tab["tab_id"], generation=generation, method=method
            )
            evidence["timings_ms"][method] = round(elapsed * 1000, 3)
            evidence[method] = result
        if wait_for_detach:
            client.socket.settimeout(wait_for_detach)
            # Closing a tab can race debugger detachment; either event proves the
            # approved target is gone and must not be silently rebound.
            evidence["terminal_event"] = client.wait_for_any_event(
                {"detached", "tab_closed"}
            )
            detached = True
        else:
            result, elapsed = client.request("detach", tab_id=tab["tab_id"])
            detached = True
            evidence["timings_ms"]["detach"] = round(elapsed * 1000, 3)
            evidence["detach"] = result
        evidence["events"] = client.events
        print(json.dumps(evidence, indent=2, sort_keys=True))
    finally:
        # A failed conflict/close probe must not strand Chrome under debugger control.
        if attached_tab_id is not None and not detached:
            try:
                client.request("detach", tab_id=attached_tab_id)
            except Exception:
                pass
        client.close()


def self_check() -> None:
    messages = [{"id": 1, "op": "active_tab"}, {"event": "hello", "protocol_version": 1}]
    buffer = bytearray(b"".join(encode_native(message) for message in messages))
    assert decode_native(buffer) == messages
    assert not buffer
    try:
        encode_native({"value": "x" * MAX_MESSAGE_BYTES})
    except ValueError:
        pass
    else:
        raise AssertionError("oversized native message was accepted")
    print(json.dumps({"self_check": "passed", "sha256": hashlib.sha256(b"native-messaging").hexdigest()}))


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    commands = result.add_subparsers(dest="command", required=True)
    install_parser = commands.add_parser("install")
    install_parser.add_argument("--extension-id", required=True)
    commands.add_parser("uninstall")
    probe_parser = commands.add_parser("probe")
    probe_parser.add_argument("--pid", required=True, type=int)
    probe_parser.add_argument("--window-id", required=True, type=int)
    probe_parser.add_argument("--native-bounds", required=True, type=parse_bounds)
    probe_parser.add_argument("--expected-origin", required=True)
    probe_parser.add_argument("--timeout", type=float, default=10.0)
    probe_parser.add_argument("--wait-for-detach", type=float, default=0.0)
    commands.add_parser("self-check")
    return result


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1].startswith("chrome-extension://"):
        host(sys.argv[1])
        return
    arguments = parser().parse_args()
    if arguments.command == "install":
        install(arguments.extension_id)
    elif arguments.command == "uninstall":
        uninstall()
    elif arguments.command == "probe":
        probe(
            arguments.pid,
            arguments.window_id,
            arguments.native_bounds,
            arguments.expected_origin,
            arguments.timeout,
            arguments.wait_for_detach,
        )
    else:
        self_check()


if __name__ == "__main__":
    main()
