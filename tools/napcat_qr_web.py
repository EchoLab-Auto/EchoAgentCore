#!/usr/bin/env python3
"""NapCat 登录二维码实时展示页。

从 Docker 容器实时读取 /app/napcat/cache/qrcode.png 并转发给浏览器，
二维码刷新/过期时页面自动更新，无需手动导出文件。

用法:
    python3 tools/napcat_qr_web.py                     # 默认容器 elastic_merkle, 端口 8088
    python3 tools/napcat_qr_web.py --container <name> --port 8088
"""
import argparse
import json
import subprocess
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

DEFAULT_CONTAINER = "elastic_merkle"
QR_PATH = "/app/napcat/cache/qrcode.png"

PAGE = """<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>NapCat 登录二维码</title>
<style>
  :root { --bg: #14161a; --card: #1e2228; --fg: #e8eaed; --dim: #9aa0a6; --accent: #4f8cff; }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: var(--bg); color: var(--fg); font-family: -apple-system, "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif;
         min-height: 100vh; display: flex; align-items: center; justify-content: center; }
  .card { background: var(--card); border: 1px solid #2a2f36; border-radius: 16px; padding: 32px 40px; text-align: center; max-width: 420px; }
  h1 { font-size: 18px; margin-bottom: 4px; }
  .sub { color: var(--dim); font-size: 12px; margin-bottom: 20px; }
  .qr-box { position: relative; width: 300px; height: 300px; margin: 0 auto; border-radius: 12px;
            background: #fff; display: flex; align-items: center; justify-content: center; overflow: hidden; }
  #qr { width: 100%; height: 100%; object-fit: contain; display: none; }
  .placeholder { color: #666; font-size: 13px; padding: 0 20px; }
  .pulse { position: absolute; inset: 0; background: rgba(79,140,255,.12); animation: pulse 1.2s ease-in-out infinite; }
  .ok-overlay { position: absolute; inset: 0; background: rgba(20,22,26,.88); display: none;
                flex-direction: column; align-items: center; justify-content: center; gap: 10px; font-size: 15px; }
  .ok-badge { width: 64px; height: 64px; border-radius: 50%; background: #34c759; color: #fff;
              font-size: 30px; display: flex; align-items: center; justify-content: center; }
  @keyframes pulse { 0%,100% { opacity: .2; } 50% { opacity: .8; } }
  .status { margin-top: 16px; display: flex; align-items: center; justify-content: center; gap: 8px; font-size: 13px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; background: #f5a623; flex-shrink: 0; }
  .dot.online { background: #34c759; }
  .dot.error { background: #ff453a; }
  .dot.waiting { animation: blink 1s steps(2, start) infinite; }
  @keyframes blink { to { visibility: hidden; } }
  .meta { color: var(--dim); font-size: 12px; margin-top: 14px; }
  .tips { color: var(--dim); font-size: 12px; margin-top: 16px; line-height: 1.6; }
  .tips b { color: var(--accent); font-weight: 600; }
</style>
</head>
<body>
<div class="card">
  <h1>NapCat 登录二维码</h1>
  <div class="sub" id="container-name">容器: <span id="cname">-</span></div>
  <div class="qr-box">
    <img id="qr" alt="二维码">
    <div class="placeholder" id="placeholder">等待二维码生成…</div>
    <div class="pulse" id="pulse" style="display:none"></div>
    <div class="ok-overlay" id="okOverlay"><div class="ok-badge">✓</div>已登录，无需扫码</div>
  </div>
  <div class="status"><span class="dot" id="dot"></span><span id="status">连接中…</span></div>
  <div class="meta">每 2 秒自动检测刷新 · <span id="mtime">-</span></div>
  <div class="tips">用<b>手机 QQ</b> 扫码授权登录。二维码刷新/过期时本页会自动更新。</div>
</div>
<script>
const $ = id => document.getElementById(id);
let lastMtime = null;
const STATUS_TEXT = {
  waiting:  ["等待扫码", "waiting"],
  online:   ["已登录 ✓", "online"],
  offline:  ["会话已离线", "waiting"],
  unknown:  ["登录状态未知", "waiting"],
  noqr:     ["二维码尚未生成", "waiting"],
  error:    ["无法连接容器", "error"],
};

function apply(state) {
  $("cname").textContent = state.container || "-";
  const [text, cls] = STATUS_TEXT[state.login] || STATUS_TEXT.unknown;
  $("status").textContent = text;
  $("dot").className = "dot " + cls;

  const qr = $("qr"), ph = $("placeholder"), pulse = $("pulse"), ok = $("okOverlay");
  if (state.login === "online") {
    qr.style.display = "none"; ph.style.display = "none"; pulse.style.display = "none";
    ok.style.display = "flex";
    return;
  }
  ok.style.display = "none";
  if (state.hasQr && state.mtime && state.mtime !== lastMtime) {
    lastMtime = state.mtime;
    qr.src = "/qrcode?t=" + state.mtime;
    qr.style.display = "block";
    ph.style.display = "none";
    pulse.style.display = "block";
    setTimeout(() => pulse.style.display = "none", 600);
  }
  if (!state.hasQr) { qr.style.display = "none"; ph.style.display = "block"; }
  $("mtime").textContent = state.mtime ? new Date(state.mtime * 1000).toLocaleTimeString() : "-";
}

async function poll() {
  try {
    const r = await fetch("/status", { cache: "no-store" });
    apply(await r.json());
  } catch (e) { apply({ container: null, hasQr: false, mtime: null, login: "error" }); }
}
poll();
setInterval(poll, 2000);
</script>
</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    server_version = "NapCatQRWeb/1.0"

    def log_message(self, fmt, *args):
        pass  # 静默访问日志

    # ---- helpers -------------------------------------------------------
    def _exec(self, *cmd):
        try:
            return subprocess.run(cmd, capture_output=True, timeout=10)
        except (subprocess.TimeoutExpired, FileNotFoundError):
            return None

    def _container_exec(self, shell_cmd):
        r = self._exec("docker", "exec", self.server.container, "sh", "-c", shell_cmd)
        if r is None or r.returncode != 0:
            return None
        return r

    def _qr_info(self):
        r = self._container_exec(f"cat {QR_PATH} 2>/dev/null; echo; stat -c '%Y %s' {QR_PATH} 2>/dev/null")
        if r is None:
            return None, None, None
        lines = r.stdout.decode("latin-1").rsplit("\n", 2)
        meta = lines[-2] if len(lines) >= 2 else ""
        try:
            mtime_s, size = meta.split()
            mtime, size = int(mtime_s), int(size)
        except ValueError:
            return None, None, None
        data = r.stdout[:size]  # 截断 echo/stat 前缀, 只保留 PNG 本体
        if not data.startswith(b"\x89PNG"):
            return None, None, None
        return data, mtime, size

    def _login_state(self):
        # 权威检测: 直接调 NapCat OneBot API (v4 路径式路由)
        try:
            req = urllib.request.Request(
                "http://127.0.0.1:3000/get_login_info",
                data=b"{}", headers={"Content-Type": "application/json"}, method="POST")
            with urllib.request.urlopen(req, timeout=5) as resp:
                body = json.loads(resp.read().decode("utf-8", "replace"))
            if body.get("status") == "ok" and body.get("data", {}).get("user_id"):
                return "online"
            return "waiting"
        except Exception:
            pass  # 回退到日志检测
        r = self._exec("docker", "logs", "--since", "180s", self.server.container)
        if r is None or r.returncode != 0:
            return "error"
        text = r.stdout.decode("utf-8", "replace") + r.stderr.decode("utf-8", "replace")
        if "登录成功" in text or "账号状态变更为在线" in text or "接收 <-" in text:
            return "online"
        if "请扫描下面的二维码" in text or "二维码" in text:
            return "waiting"
        if "账号状态变更为离线" in text:
            return "offline"
        return "unknown"

    # ---- routes --------------------------------------------------------
    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/":
            self._send_text(200, "text/html; charset=utf-8", PAGE)
        elif path == "/status":
            data, mtime, size = self._qr_info()
            body = json.dumps({
                "container": self.server.container,
                "hasQr": data is not None,
                "mtime": mtime,
                "size": size,
                "login": self._login_state(),
            })
            self._send_text(200, "application/json; charset=utf-8", body)
        elif path == "/qrcode":
            data, mtime, _ = self._qr_info()
            if data is None:
                self._send_text(404, "text/plain; charset=utf-8", "no qrcode")
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Qr-Mtime", str(mtime or ""))
            self.end_headers()
            self.wfile.write(data)
        else:
            self._send_text(404, "text/plain; charset=utf-8", "not found")

    def do_HEAD(self):
        self.do_GET()

    def _send_text(self, code, ctype, text):
        body = text.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)


class Server(ThreadingHTTPServer):
    def __init__(self, addr, container):
        super().__init__(addr, Handler)
        self.container = container
        self.daemon_threads = True


def main():
    ap = argparse.ArgumentParser(description="NapCat 登录二维码实时展示页")
    ap.add_argument("--container", default=DEFAULT_CONTAINER, help=f"NapCat 容器名 (默认 {DEFAULT_CONTAINER})")
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--port", type=int, default=8088)
    args = ap.parse_args()

    server = Server((args.host, args.port), args.container)
    print(f"NapCat 二维码页面: http://localhost:{args.port}  (容器: {args.container})")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
