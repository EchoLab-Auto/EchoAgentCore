#!/bin/bash
# ============================================================
# 配置 NapCat 反向 WebSocket 连接到 EchoAgentPanel
#
# 用法: bash napcat/setup.sh [ws地址]
# 默认: ws://host.docker.internal:3131
# ============================================================
set -e

WS_URL="${1:-ws://host.docker.internal:3131}"  # host.docker.internal = host from Docker
NAPCAT_API="http://localhost:3000"

echo "=== NapCat 反向 WebSocket 配置 ==="
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
USER_ID=$(echo "$LOGIN_INFO" | grep -o '"user_id":"[^"]*"' | cut -d'"' -f4)
NICKNAME=$(echo "$LOGIN_INFO" | grep -o '"nickname":"[^"]*"' | cut -d'"' -f4)

if [ -z "$USER_ID" ] || [ "$USER_ID" = "0" ]; then
    echo ""
    echo "⚠️  NapCat 尚未登录 QQ"
    echo "   请打开 http://localhost:6099 扫码登录后重新运行此脚本"
    exit 1
fi
echo "✓ QQ 已登录: $NICKNAME ($USER_ID)"

# 3. 配置反向 WebSocket (通过 NapCat WebUI API)
echo ""
echo "正在配置反向 WebSocket 连接..."

# 尝试通过 internal API 配置
CONFIG_RESULT=$(curl -s -X POST "http://localhost:6099/api/network/wsReverse" \
    -H 'Content-Type: application/json' \
    -d "{
        \"enabled\": true,
        \"url\": \"$WS_URL\",
        \"type\": \"array\",
        \"token\": \"\",
        \"reconnectInterval\": 5000,
        \"heartInterval\": 30000
    }" 2>/dev/null || echo '{"error":"api not available"}')

if echo "$CONFIG_RESULT" | grep -q '"ok"\|"success"\|"status":"ok"'; then
    echo "✓ 反向 WebSocket 已自动配置"
else
    # API 可能不可用，提示手动配置
    echo ""
    echo "⚠️  自动配置不可用，请手动在 WebUI 中配置:"
    echo ""
    echo "   1. 打开 http://localhost:6099"
    echo "   2. 网络配置 → 新建 → WebSocket 客户端"
    echo "   3. 地址:    $WS_URL"
    echo "   4. 消息格式: Array"
    echo "   5. Token:   (留空)"
    echo "   6. 保存"
fi

echo ""
echo "=== 配置完成 ==="
echo "EchoAgentPanel 启动后，NapCat 将自动连接到 $WS_URL"
echo "查看连接状态: docker compose logs -f napcat"
