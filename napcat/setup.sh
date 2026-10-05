#!/bin/bash
# ============================================================
# 配置 NapCat 反向 WebSocket 连接到 EchoAgentCore
#
# 用法: bash napcat/setup.sh [ws地址] [WebUI token]
# 默认: ws://host.docker.internal:3131
#
# 说明（2026-10 更新到 OB11Config API，与 Core 侧 napcat/mod.rs 同路径）：
#   此前用的 /api/network/wsReverse 是 NapCat v3 旧 API，v4+ 已漂移。
#   本脚本走 /api/OB11Config/GetConfig + SetConfig（与 Core 自动配置同
#   一个接口），需要 WebUI token（napcat.json 的 token 或 WebUI 登录凭证）。
# ============================================================
set -e

WS_URL="${1:-ws://host.docker.internal:3131}"  # host.docker.internal = host from Docker
WEBUI_TOKEN="${2:-}"
NAPCAT_API="http://localhost:3000"             # OneBot HTTP（登录状态查询）
NAPCAT_WEBUI="http://localhost:6099"           # WebUI API（配置接口）

echo "=== NapCat 反向 WebSocket 配置（OB11Config API）==="
echo "目标地址: $WS_URL"
echo ""

# 1. 检查 NapCat 是否在运行
if ! docker ps --format '{{.Names}}' | grep -q napcat; then
    echo "❌ NapCat 容器未运行，请先执行: docker compose up -d"
    exit 1
fi
echo "✓ NapCat 容器运行中"

# 2. 检查是否已扫码登录
LOGIN_INFO=$(curl -s "$NAPCAT_API/get_login_info" -d '{}' -H 'Content-Type: application/json')
USER_ID=$(echo "$LOGIN_INFO" | grep -o '"user_id":\?[0-9"]*' | grep -o '[0-9]*' | head -1)
NICKNAME=$(echo "$LOGIN_INFO" | grep -o '"nickname":"[^"]*"' | cut -d'"' -f4)

if [ -z "$USER_ID" ] || [ "$USER_ID" = "0" ]; then
    echo ""
    echo "⚠️  NapCat 尚未登录 QQ"
    echo "   请打开 $NAPCAT_WEBUI 扫码登录后重新运行此脚本"
    exit 1
fi
echo "✓ QQ 已登录: $NICKNAME ($USER_ID)"

if [ -z "$WEBUI_TOKEN" ]; then
    echo ""
    echo "⚠️  需要 WebUI token 才能写配置：bash napcat/setup.sh <ws地址> <token>"
    echo "   token 见 NapCat 容器内 webui.json / WebUI 登录凭证。"
    echo "   或手动配置：$NAPCAT_WEBUI → 网络配置 → WebSocket 客户端 →"
    echo "     名称 EchoAgentCore / 地址 $WS_URL / 消息格式 array / 重连 30s"
    exit 1
fi

# 3. 读当前 OB11 配置
echo ""
echo "正在读取 OB11 配置…"
GET_BODY=$(curl -s -X POST "$NAPCAT_WEBUI/api/OB11Config/GetConfig" \
    -H "Authorization: Bearer $WEBUI_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{}')
if ! echo "$GET_BODY" | grep -q '"code":0'; then
    echo "❌ GetConfig 失败：$GET_BODY"
    exit 1
fi

# 4. 合并写入 EchoAgentCore 客户端条目（幂等：同名先删后加）
CONFIG=$(echo "$GET_BODY" | python3 - "$WS_URL" <<'PYEOF'
import json, sys
ws_url = sys.argv[1]
body = json.load(sys.stdin)
config = body.get("data") or {}
if not isinstance(config, dict):
    config = {}
network = config.setdefault("network", {})
clients = network.setdefault("websocketClients", [])
if not isinstance(clients, list):
    clients = []
    network["websocketClients"] = clients
clients[:] = [c for c in clients if c.get("name") != "EchoAgentCore"]
clients.append({
    "enable": True,
    "name": "EchoAgentCore",
    "url": ws_url,
    "reportSelfMessage": False,
    "messagePostFormat": "array",
    "token": "",
    "debug": False,
    "heartInterval": 30000,
    "reconnectInterval": 30000,
    "verifyCertificate": True,
})
print(json.dumps({"config": json.dumps(config, ensure_ascii=False)}))
PYEOF
)

SET_BODY=$(curl -s -X POST "$NAPCAT_WEBUI/api/OB11Config/SetConfig" \
    -H "Authorization: Bearer $WEBUI_TOKEN" \
    -H 'Content-Type: application/json' \
    -d "$CONFIG")
if echo "$SET_BODY" | grep -q '"code":0'; then
    echo "✓ 反向 WebSocket 已配置（EchoAgentCore → $WS_URL）"
else
    echo "❌ SetConfig 失败：$SET_BODY"
    exit 1
fi

echo ""
echo "=== 配置完成 ==="
echo "EchoAgentCore 启动后，NapCat 将自动重连到 $WS_URL"
