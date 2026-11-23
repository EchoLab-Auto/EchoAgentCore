#!/usr/bin/env python3
"""tests/smoke-workspace-restart.py — 隔离验收用的最小 WS / 假 LLM 辅助。

本文件只被 `tests/smoke-workspace-restart.sh` 调用，不导入项目代码、不写项目
目录。它做三件事：

1. `serve-llm`：在 127.0.0.1 的随机端口上起一个**完全离线**的 OpenAI
   兼容 `/v1/chat/completions` 假服务；只回固定文本，不访问外部网络。
2. `send`：以管理面 WS 客户端身份发一条 `SendMessage`，等到该会话的
   `AgentCompleted`。
3. `probe`：以管理面 WS 客户端身份只发 `RequestState` +
   `RequestSessionHistory`，校验激活工作区通道会话与历史。

关键约束（写在代码里，不靠调用方自律）：

* `RequestWorkspaceSessions` **是禁止命令**——`command_frame()` 遇到它直接
  退出。这样验收不会用客户端主动拉取来掩盖 Core 的启动恢复。
* WS 客户端用 Python 标准库手写（socket + 帧编解码），不依赖 `websockets`
  包，避免“测试机少装一个包就 SKIP”。
* 所有网络目标都是 127.0.0.1；没有账号、没有真实 API key、没有联邦 peer。
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# 验收脚本允许出现的命令全集。新增命令必须显式改这里——防止“顺手加一条
# RequestWorkspaceSessions”把启动恢复的真实性冲掉。
ALLOWED_COMMANDS = {
    "SendMessage",
    "RequestState",
    "RequestSessionHistory",
}
FORBIDDEN_COMMANDS = {"RequestWorkspaceSessions"}

_WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# 区分“没有 payload”（单变体命令，线格式是字符串）与显式 null。
_MISSING = object()


class WSError(RuntimeError):
    """WS 握手/帧错误。"""


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    chunks: list[bytes] = []
    remaining = n
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise WSError("connection closed while reading")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


class WSClient:
    """最小 WebSocket 客户端：文本帧 + ping/pong + close。

    服务端是 tokio-tungstenite，帧不会分片得过分刁钻；这里仍按 RFC 6455
    处理 continuation / ping / pong，避免“某次事件刚好分片”造成假失败。
    """

    def __init__(self, host: str, port: int, token: str, timeout: float = 10.0):
        self.host = host
        self.port = port
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self._buf = b""
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            f"GET /ws HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
        )
        if token:
            request += f"Authorization: Bearer {token}\r\n"
        request += "\r\n"
        self.sock.sendall(request.encode("ascii"))

        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise WSError("handshake closed before headers")
            head += chunk
            if len(head) > 64 * 1024:
                raise WSError("handshake headers too large")
        header_blob, _, rest = head.partition(b"\r\n\r\n")
        self._buf = rest
        lines = header_blob.decode("latin-1").split("\r\n")
        status = lines[0] if lines else ""
        if " 101 " not in status:
            raise WSError(f"handshake rejected: {status}")
        headers: dict[str, str] = {}
        for line in lines[1:]:
            if ":" in line:
                name, value = line.split(":", 1)
                headers[name.strip().lower()] = value.strip()
        expected = base64.b64encode(
            hashlib.sha1((key + _WS_GUID).encode("ascii")).digest()
        ).decode("ascii")
        if headers.get("sec-websocket-accept") != expected:
            raise WSError("bad Sec-WebSocket-Accept")

    def _read_n(self, n: int) -> bytes:
        if self._buf:
            take = min(n, len(self._buf))
            out, self._buf = self._buf[:take], self._buf[take:]
            if take == n:
                return out
            return out + _recv_exact(self.sock, n - take)
        return _recv_exact(self.sock, n)

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        header = bytearray([0x80 | opcode])
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < 65536:
            header.append(0x80 | 126)
            header.extend(struct.pack("!H", length))
        else:
            header.append(0x80 | 127)
            header.extend(struct.pack("!Q", length))
        mask = os.urandom(4)
        header.extend(mask)
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.sock.sendall(bytes(header) + masked)

    def send_text(self, text: str) -> None:
        self._send_frame(0x1, text.encode("utf-8"))

    def _read_frame(self) -> tuple[bool, int, bytes]:
        first, second = self._read_n(2)
        fin = bool(first & 0x80)
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_n(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_n(8))[0]
        mask = self._read_n(4) if masked else b""
        payload = self._read_n(length)
        if masked:
            payload = bytes(
                byte ^ mask[index % 4] for index, byte in enumerate(payload)
            )
        return fin, opcode, payload

    def recv_text(self, timeout: float) -> str:
        self.sock.settimeout(timeout)
        fragments: list[bytes] = []
        while True:
            fin, opcode, payload = self._read_frame()
            if opcode == 0x8:
                raise WSError("closed by peer")
            if opcode == 0x9:  # ping
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode in (0x1, 0x2):
                fragments = [payload]
            elif opcode == 0x0:
                fragments.append(payload)
            else:
                continue
            if fin:
                return b"".join(fragments).decode("utf-8", "replace")

    def close(self) -> None:
        try:
            self._send_frame(0x8, b"\x03\xe8")
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass


def command_frame(name: str, payload=_MISSING) -> str:
    if name in FORBIDDEN_COMMANDS:
        raise SystemExit(
            f"forbidden command `{name}`: this smoke must not mask startup recovery"
        )
    if name not in ALLOWED_COMMANDS:
        raise SystemExit(f"command `{name}` is not in the smoke allowlist")
    value = name if payload is _MISSING else {name: payload}
    return json.dumps(
        {"type": "command", "payload": value}, ensure_ascii=False, separators=(",", ":")
    )


def wait_port(port: int, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        sock = socket.socket()
        sock.settimeout(0.5)
        try:
            sock.connect(("127.0.0.1", port))
            return
        except OSError as error:
            last_error = error
            time.sleep(0.1)
        finally:
            sock.close()
    raise SystemExit(f"port 127.0.0.1:{port} not ready in {timeout}s ({last_error})")


def _iter_payloads(ws: WSClient, timeout: float):
    """Yield `payload` objects until timeout / close."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        remaining = max(0.05, min(1.0, deadline - time.monotonic()))
        try:
            raw = ws.recv_text(remaining)
        except socket.timeout:
            continue
        except WSError as error:
            print(f"WS_CLOSED {error}", file=sys.stderr, flush=True)
            return
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        payload = message.get("payload")
        if isinstance(payload, dict):
            yield payload


# ── fake OpenAI-compatible LLM ──────────────────────────────────────────────


class _FakeLlmHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    reply_text = "SMOKE-FAKE-LLM-REPLY"
    served = 0
    lock = threading.Lock()

    def log_message(self, fmt, *args):  # noqa: N802 (stdlib naming)
        print("fake-llm: " + (fmt % args), file=sys.stderr, flush=True)

    def do_POST(self):  # noqa: N802
        try:
            length = int(self.headers.get("Content-Length", "0") or "0")
        except ValueError:
            length = 0
        body = self.rfile.read(length) if length else b""
        model = ""
        try:
            request = json.loads(body.decode("utf-8", "replace"))
            model = request.get("model", "")
        except Exception:
            pass
        with _FakeLlmHandler.lock:
            _FakeLlmHandler.served += 1
            served = _FakeLlmHandler.served
        print(
            f"fake-llm: served POST {self.path} model={model!r} count={served}",
            file=sys.stderr,
            flush=True,
        )
        payload = json.dumps(
            {
                "id": "chatcmpl-smoke",
                "object": "chat.completion",
                "model": model or "smoke-model",
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": _FakeLlmHandler.reply_text,
                        },
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 5,
                    "total_tokens": 17,
                },
            }
        ).encode("utf-8")
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):  # noqa: N802
        payload = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def cmd_serve_llm(args: argparse.Namespace) -> int:
    _FakeLlmHandler.reply_text = args.reply
    server = ThreadingHTTPServer(("127.0.0.1", args.port), _FakeLlmHandler)
    server.daemon_threads = True
    print(f"READY 127.0.0.1:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


# ── management WS commands ─────────────────────────────────────────────────


def cmd_send(args: argparse.Namespace) -> int:
    ws = WSClient("127.0.0.1", args.port, args.token, timeout=args.timeout)
    commands: list[str] = []
    try:
        frame = command_frame(
            "SendMessage",
            {
                "session_id": args.session,
                "content": args.content,
                "images": [],
                "files": [],
                "team_id": args.team,
            },
        )
        ws.send_text(frame)
        commands.append("SendMessage")
        deadline = time.monotonic() + args.timeout
        errors: list[str] = []
        while time.monotonic() < deadline:
            try:
                raw = ws.recv_text(max(0.1, min(1.0, deadline - time.monotonic())))
            except socket.timeout:
                continue
            except WSError as error:
                errors.append(f"WS closed: {error}")
                break
            try:
                message = json.loads(raw)
            except json.JSONDecodeError:
                continue
            payload = message.get("payload")
            if not isinstance(payload, dict):
                continue
            if "AgentCompleted" in payload:
                completed = payload["AgentCompleted"]
                if completed.get("session_id") == args.session:
                    print(
                        "SEND_OK " + json.dumps({"commands": commands}, ensure_ascii=False),
                        flush=True,
                    )
                    return 0
            if "Error" in payload:
                error = payload["Error"]
                sid = error.get("session_id")
                if sid in (None, args.session):
                    errors.append(str(error.get("message", "")))
        print(
            "SEND_FAIL " + json.dumps(
                {"commands": commands, "errors": errors[-5:]}, ensure_ascii=False
            ),
            flush=True,
        )
        return 3
    finally:
        ws.close()


def _collect_until(
    ws: WSClient,
    timeout: float,
    predicate,
):
    """Read payloads until predicate(payload) returns a non-None value."""
    for payload in _iter_payloads(ws, timeout):
        found = predicate(payload)
        if found is not None:
            return found
    return None


def cmd_probe(args: argparse.Namespace) -> int:
    ws = WSClient("127.0.0.1", args.port, args.token, timeout=args.timeout)
    commands: list[str] = []
    try:
        # ① 只读状态快照：不碰工作区命令。RequestState 会把 trunk 里已注册的
        #    会话以 SessionUpdated 逐条发回。
        ws.send_text(command_frame("RequestState"))
        commands.append("RequestState")
        session = _collect_until(
            ws,
            min(8.0, args.timeout),
            lambda payload: (
                payload.get("SessionUpdated", {}).get("session")
                if payload.get("SessionUpdated", {}).get("session", {}).get("id")
                == args.session
                else None
            ),
        )

        # ② 只读历史：同样不碰工作区命令。
        ws.send_text(
            command_frame(
                "RequestSessionHistory",
                {
                    "team_id": args.team,
                    "session_id": args.session,
                    "limit": 100,
                    "expand_depth": None,
                },
            )
        )
        commands.append("RequestSessionHistory")
        history = _collect_until(
            ws,
            args.timeout,
            lambda payload: (
                payload.get("SessionHistory")
                if payload.get("SessionHistory", {}).get("session_id") == args.session
                else None
            ),
        )

        errors: list[str] = []
        if session is None:
            errors.append("RequestState 没有返回目标会话的 SessionUpdated")
        else:
            if session.get("team_id") != args.team:
                errors.append(
                    f"会话 team_id={session.get('team_id')!r} != {args.team!r}"
                )
            if session.get("platform") != "local":
                errors.append(f"会话 platform={session.get('platform')!r} != 'local'")
            if session.get("scope") != "workspace":
                errors.append(f"会话 scope={session.get('scope')!r} != 'workspace'")
            if args.expect_nickname and args.expect_nickname not in (
                session.get("nickname") or ""
            ):
                errors.append(
                    f"会话昵称 {session.get('nickname')!r} 不含期望 {args.expect_nickname!r}"
                )

        if history is None:
            errors.append("RequestSessionHistory 没有返回目标会话的历史")
        else:
            messages = history.get("messages") or []
            user_texts = [
                str(message.get("content", ""))
                for message in messages
                if message.get("kind") == "user"
            ]
            backend_texts = [
                str(message.get("content", ""))
                for message in messages
                if message.get("kind") == "backend"
            ]
            for marker in args.expect_user_marker:
                if not any(marker in text for text in user_texts):
                    errors.append(f"历史 user 条目缺标记 {marker!r}")
            for marker in args.expect_assistant_marker:
                if not any(marker in text for text in backend_texts):
                    errors.append(f"历史 backend 条目缺标记 {marker!r}")
            if history.get("total", 0) < len(args.expect_user_marker) + len(
                args.expect_assistant_marker
            ):
                errors.append(
                    f"历史 total={history.get('total')} 少于期望标记数"
                )

        if "RequestWorkspaceSessions" in commands:
            errors.append("客户端误发了 RequestWorkspaceSessions")

        summary = {
            "ok": not errors,
            "commands": commands,
            "session": session,
            "history_total": (history or {}).get("total"),
            "history_messages": [
                {
                    "kind": message.get("kind"),
                    "content": str(message.get("content", ""))[:180],
                }
                for message in (history or {}).get("messages", [])
            ],
            "errors": errors,
        }
        print(("PROBE_OK " if not errors else "PROBE_FAIL ") + json.dumps(
            summary, ensure_ascii=False
        ), flush=True)
        return 0 if not errors else 3
    finally:
        ws.close()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    wait = subparsers.add_parser("wait-port", help="等待回环 TCP 端口就绪")
    wait.add_argument("--port", type=int, required=True)
    wait.add_argument("--timeout", type=float, default=30.0)
    wait.set_defaults(func=lambda args: (wait_port(args.port, args.timeout), 0)[1])

    llm = subparsers.add_parser("serve-llm", help="起离线 OpenAI 兼容假服务")
    llm.add_argument("--port", type=int, required=True)
    llm.add_argument("--reply", required=True)
    llm.set_defaults(func=cmd_serve_llm)

    send = subparsers.add_parser("send", help="发一条 SendMessage 并等 AgentCompleted")
    send.add_argument("--port", type=int, required=True)
    send.add_argument("--token", default="")
    send.add_argument("--team", required=True)
    send.add_argument("--session", required=True)
    send.add_argument("--content", required=True)
    send.add_argument("--timeout", type=float, default=60.0)
    send.set_defaults(func=cmd_send)

    probe = subparsers.add_parser(
        "probe", help="只发 RequestState + RequestSessionHistory 做只读校验"
    )
    probe.add_argument("--port", type=int, required=True)
    probe.add_argument("--token", default="")
    probe.add_argument("--team", required=True)
    probe.add_argument("--session", required=True)
    probe.add_argument("--expect-nickname", default="")
    probe.add_argument("--expect-user-marker", action="append", default=[])
    probe.add_argument("--expect-assistant-marker", action="append", default=[])
    probe.add_argument("--timeout", type=float, default=30.0)
    probe.set_defaults(func=cmd_probe)

    return parser


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except WSError as error:
        print(f"WS_ERROR {error}", flush=True)
        return 4
    except socket.timeout:
        print("WS_TIMEOUT", flush=True)
        return 4


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
