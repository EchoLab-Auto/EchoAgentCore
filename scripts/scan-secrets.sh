#!/usr/bin/env bash
# 扫描（可选清理）本机 EchoAgentCore 数据目录中的明文密钥残留。
#
# 背景：脱敏服务（document/security-redaction-design.md）保证**新**数据不再
# 以明文落盘（会话事件 / 工具结果 / 日志）；本脚本用于回扫**历史**文件，
# 输出修复清单；`--fix` 可离线替换残留（需先停 Core）。
#
# 用法：
#   scripts/scan-secrets.sh [配置目录]          # 只扫描（默认）
#   scripts/scan-secrets.sh --fix [配置目录]    # 扫描并替换残留
#   ECHO_CONFIG_DIR=/path scripts/scan-secrets.sh
#
# 默认配置目录：~/.config/echo-agent-core（core.toml 所在目录）。
# 退出码：0 = 未发现残留；1 = 发现残留（或已修复）；2 = 用法/环境错误。

set -euo pipefail

FIX=0
POSITIONAL=()
for arg in "$@"; do
    case "$arg" in
        --fix) FIX=1 ;;
        -h|--help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) POSITIONAL+=("$arg") ;;
    esac
done

CONFIG_DIR="${POSITIONAL[0]:-${ECHO_CONFIG_DIR:-$HOME/.config/echo-agent-core}}"
if [[ ! -d "$CONFIG_DIR" ]]; then
    echo "配置目录不存在: $CONFIG_DIR" >&2
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "需要 python3（读取 core.toml 并扫描文件）。" >&2
    exit 2
fi
if [[ "$FIX" == "1" ]]; then
    echo "注意：--fix 是离线操作——请先停止 Core（避免运行中进程用内存中的旧数据覆盖修复）。"
fi

python3 - "$CONFIG_DIR" "$FIX" <<'PY'
import os
import re
import sys

config_dir = sys.argv[1]
fix = sys.argv[2] == "1"
config_file = os.path.join(config_dir, "core.toml")

secrets: list[tuple[str, str]] = []  # (label, value)


def placeholder(label: str) -> str:
    return f"【已隐藏:{label}】"


def add(label: str, value) -> None:
    if not isinstance(value, str):
        return
    value = value.strip()
    # 与脱敏器同口径：过短值误报噪声大；含占位符括号的值跳过。
    if len(value) >= 8 and "【" not in value and "】" not in value:
        secrets.append((label, value))


# ---- 1) 从 core.toml 提取已知密钥（tomllib；Python 3.11+） ----
try:
    import tomllib  # noqa: F401
except ImportError:  # pragma: no cover - 老 Python 走正则回退
    tomllib = None


def walk(node, path=""):
    """递归扫描任意层级的 access_token（QQ 适配器新旧结构都覆盖）。"""
    if not isinstance(node, dict):
        return
    for key, value in node.items():
        if key == "access_token":
            add(f"{path}access_token", value)
        elif isinstance(value, dict):
            walk(value, f"{path}{key}.")


def fallback_regex_scan():
    if os.path.exists(config_file):
        text = open(config_file, encoding="utf-8", errors="replace").read()
        for match in re.finditer(r'(api_key|access_token)\s*=\s*"([^"]{8,})"', text):
            add(match.group(1), match.group(2))


if tomllib is not None and os.path.exists(config_file):
    with open(config_file, "rb") as fh:
        try:
            data = tomllib.load(fh)
        except Exception as exc:  # noqa: BLE001
            print(f"warn: {config_file} 解析失败（{exc}），改用正则回退", file=sys.stderr)
            data = None
    if data is not None:
        agent = data.get("agent") or {}
        add("agent.api_key", agent.get("api_key"))
        for profile in agent.get("api_profiles") or []:
            add(f"api_key:{profile.get('name', '?')}", profile.get("api_key"))
        walk(data)
        sanitize = (data.get("security") or {}).get("sanitize") or {}
        for index, value in enumerate(sanitize.get("extra_secrets") or []):
            add(f"extra:{index}", value)
    else:
        fallback_regex_scan()
else:
    fallback_regex_scan()

print(f"扫描目录: {config_dir}")
print(f"提取密钥: {len(secrets)} 条（值不显示）")
print("-" * 60)

# ---- 2) 全目录扫描（跳过 core.toml 本体：密钥的合法归宿，不算异常） ----
findings = 0
fixed_files = 0
skip_names = {"core.toml"}
for root, _dirs, files in os.walk(config_dir):
    for name in files:
        if name in skip_names:
            continue
        path = os.path.join(root, name)
        try:
            with open(path, "rb") as fh:
                raw = fh.read()
        except OSError:
            continue
        rel = os.path.relpath(path, config_dir)
        hits = []
        for label, value in secrets:
            count = raw.count(value.encode("utf-8"))
            if count:
                hits.append((label, value, count))
        if not hits:
            continue

        if fix:
            # 修复只在严格 UTF-8 且可逆替换时进行（JSON 由 Rust 侧写出，均为 UTF-8）。
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                print(f"SKIP {rel} 非 UTF-8，未修复（请人工处理）")
                findings += len(hits)
                continue
            replaced = 0
            for label, value, count in hits:
                text = text.replace(value, placeholder(label))
                replaced += count
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(text)
            fixed_files += 1
            print(f"FIXED {rel}  replaced={replaced}")
        else:
            for label, _value, count in hits:
                # 只报位置与次数——本工具本身也绝不放明文。
                print(f"HIT  {rel}  label={label}  occurrences={count}")
                findings += 1

print("-" * 60)
if fix:
    if fixed_files:
        print(f"已修复 {fixed_files} 个文件（替换为【已隐藏:<label>】占位符）。")
        print("提醒：明文曾落盘的时间窗内视同泄漏，仍建议在供应商侧轮换这些密钥。")
    else:
        print("无可修复的残留。")
    sys.exit(0)

if findings:
    print(f"发现 {findings} 处明文残留 —— 处理建议：")
    print("  1) 立即在对应 API 供应商处轮换密钥（明文已落盘的时间窗内视为泄漏）；")
    print("  2) 保持 [security.sanitize].enabled = true（默认）并重启 Core，新数据不再落明文；")
    print("  3) 停止 Core 后运行本脚本 --fix 清理残留（或人工复核后删除旧 archives/*.json）；")
    print("  4) 若该机器上存在其他明文副本（备份 / 导出），一并清理。")
    sys.exit(1)
print("未发现明文密钥残留。")
PY
