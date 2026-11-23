#!/usr/bin/env python3
"""装配管理面协议契约产物（B3）：TS 绑定 + 可搬运夹具 + 版本/来源/哈希。

设计目标（与 `document/protocol/contract-artifact.md` 对齐）：

1. **单一真值**：TypeScript 只能由 Core 的 Rust 类型生成
   （`echo-protocol` 的 `protocol_contract_ts` example），本脚本不解析、不拼接、
   不改写 TS 正文，只做「取字节 → 校验 sha256 → 装进目录」。
2. **自包含**：产物目录解压后不需要 Core 源码、不需要 Panel 的 node_modules，
   仅凭 `manifest.json` + `SHA256SUMS` 即可离线核验（见 verify-protocol-contract.py）。
3. **可追溯**：记录固定 Core 提交（40 位）、crate 版本、生成入口、rustc/cargo
   版本，以及每个文件的 sha256；工作树模式必须显式 `--allow-dirty` 且标记 dirty。
4. **可失败**：TS 摘要与预期（Panel `protocolCore.generatedSha256`）不符时
   **不产出产物**——错误的契约比没有契约更糟。

本脚本不发布、不提交、不接触 GitHub；它只写 `--out` 指定的目录。

用法见 `--help`。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = "echo-agent.protocol-contract/1"
FIXTURE_SCHEMA = "echo-agent.protocol-contract.fixtures/1"
TS_NAME = "protocol.generated.ts"
MANIFEST_NAME = "manifest.json"
SUMS_NAME = "SHA256SUMS"
INDEX_NAME = "fixtures.index.json"
EXAMPLE_REL = "source/modules/echo-protocol/examples/protocol_contract_ts.rs"
CRATE_DIR_REL = "source/modules/echo-protocol"
FIXTURES_REL = "source/modules/agent-module-management/tests/fixtures"
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")


def die(message: str, code: int = 1) -> "NoReturn":  # type: ignore[name-defined]
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(code)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git(args: list[str], cwd: Path) -> str:
    try:
        return subprocess.run(
            ["git", *args], cwd=str(cwd), check=True, capture_output=True, text=True
        ).stdout.strip()
    except FileNotFoundError:
        die("找不到 git；契约产物的来源必须是 git 提交（或显式 --allow-dirty 工作树）")
    except subprocess.CalledProcessError as err:
        die(f"git {' '.join(args)} 失败：{err.stderr.strip()}")


def run_cargo(args: list[str], cwd: Path, env: dict[str, str]) -> str:
    try:
        result = subprocess.run(
            args, cwd=str(cwd), check=True, capture_output=True, text=True, env=env
        )
    except FileNotFoundError:
        die("找不到 cargo；请装 Rust 工具链，或改用 --ts-from 预生成文件")
    except subprocess.CalledProcessError as err:
        tail = (err.stderr or "").strip().splitlines()[-12:]
        die("生成 TypeScript 失败：\n    " + "\n    ".join(tail))
    return result.stderr.strip()


def workspace_version(core: Path) -> str:
    text = (core / "Cargo.toml").read_text(encoding="utf-8")
    section = text.split("[workspace.package]", 1)
    if len(section) != 2:
        die("Cargo.toml 里没有 [workspace.package]，无法确定契约版本")
    match = re.search(r'^version\s*=\s*"([^"]+)"', section[1], re.MULTILINE)
    if not match:
        die("Cargo.toml 的 [workspace.package] 没有 version，无法确定契约版本")
    return match.group(1)


def materialize_source(core: Path, commit: str, worktree: bool, tmp: Path) -> tuple[Path, bool]:
    """把待生成源码落到临时目录：提交模式用 git archive（只含跟踪文件）。"""
    root = tmp / "src"
    root.mkdir()
    if worktree:
        # 工作树模式：B2 未合并前的本地验证路径。排除 target/.git（可能几十 GB）。
        tar_path = tmp / "src.tar"
        with tar_path.open("wb") as handle:
            subprocess.run(
                [
                    "tar",
                    "-C",
                    str(core),
                    "--exclude=./target",
                    "--exclude=./.git",
                    "-cf",
                    "-",
                    ".",
                ],
                check=True,
                stdout=handle,
            )
        with tarfile.open(tar_path) as archive:
            archive.extractall(root, filter="data")
        tar_path.unlink()
        return root, True

    tar_path = tmp / "src.tar"
    with tar_path.open("wb") as handle:
        subprocess.run(
            ["git", "-C", str(core), "archive", "--format=tar", commit],
            check=True,
            stdout=handle,
        )
    with tarfile.open(tar_path) as archive:
        archive.extractall(root, filter="data")
    tar_path.unlink()
    if not (root / EXAMPLE_REL).is_file():
        die(
            f"提交 {commit} 里没有 {EXAMPLE_REL}；契约生成入口必须与 echo-protocol 同提交。\n"
            "  若要验证尚未提交的工作树，用 --allow-dirty（产物会标记 dirty=true，不可用于发版）。"
        )
    return root, False


def derive_fixture(path: Path) -> dict:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as err:
        die(f"夹具不是合法 UTF-8 JSON：{path.name}（{err}）")
    if not isinstance(data, dict):
        die(f"夹具顶层必须是对象：{path.name}")
    frame = data.get("type")
    if frame not in ("event", "command"):
        die(f"夹具顶层 type 必须是 event/command：{path.name}（实际 {frame!r}）")
    payload = data.get("payload")
    if not isinstance(payload, dict) or len(payload) != 1:
        die(f"夹具 payload 必须恰好一个变体键：{path.name}")
    variant, body = next(iter(payload.items()))
    if not isinstance(body, dict):
        die(f"夹具 payload.{variant} 必须是对象：{path.name}")
    seq = data.get("seq")
    if seq is not None and not isinstance(seq, int):
        die(f"夹具 seq 必须是整数或缺省：{path.name}")
    if frame == "command" and seq is not None:
        die(f"命令帧不应带顶层 seq：{path.name}")
    return {
        "path": f"fixtures/{path.name}",
        "sha256": sha256_file(path),
        "bytes": path.stat().st_size,
        "frame": frame,
        "variant": variant,
        "seq": seq,
        "payload_keys": sorted(body.keys()),
    }


def aggregate(files: list[dict]) -> str:
    """对所有载荷文件（不含 manifest / SHA256SUMS）排序后做聚合摘要。"""
    digest = hashlib.sha256()
    for entry in sorted(files, key=lambda item: item["path"]):
        digest.update(entry["path"].encode("utf-8"))
        digest.update(b"\0")
        digest.update(entry["sha256"].encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def write_sums(artifact: Path, entries: list[dict]) -> None:
    lines = [f"{entry['sha256']}  {entry['path']}" for entry in sorted(entries, key=lambda e: e["path"])]
    (artifact / SUMS_NAME).write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="装配自包含的管理面协议契约产物（TS + 夹具 + 来源/哈希）",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例：\n"
            "  # 发版路径：固定提交，摘要必须等于 Panel 的 protocolCore.generatedSha256\n"
            "  scripts/build-protocol-contract.py --out dist --commit \"$(git rev-parse HEAD)\" \\\n"
            "      --expect-ts-sha256 bbab13db...\n\n"
            "  # B2 未合并前的本地验证（工作树 + 预生成 TS，不跑 cargo）\n"
            "  scripts/build-protocol-contract.py --out /tmp/contract --allow-dirty \\\n"
            "      --ts-from /tmp/protocol.generated.ts --expect-ts-sha256 bbab13db...\n"
        ),
    )
    parser.add_argument("--out", required=True, help="产物输出父目录（会写入 protocol-contract-<version>/）")
    parser.add_argument("--source", default=str(Path(__file__).resolve().parent.parent), help="Core 仓库根目录")
    parser.add_argument("--commit", help="固定 Core 提交（40 位）；缺省取 --source 的 HEAD")
    parser.add_argument("--allow-dirty", action="store_true", help="允许未提交的工作树（产物标记 dirty=true）")
    parser.add_argument("--contract-version", help="契约版本；缺省取 Core Cargo.toml 的 workspace.package.version")
    parser.add_argument("--expect-ts-sha256", help="预期 TS 摘要（hex64）；建议取 Panel 的 protocolCore.generatedSha256")
    parser.add_argument("--panel-package", help="Panel web/package.json 路径；存在时读取 protocolCore 作为预期值/来源记录")
    parser.add_argument("--ts-from", help="预生成 TS 文件（跳过 cargo；仅本地验证/测试用，manifest 会标记 generator_mode）")
    parser.add_argument("--fixtures", help=f"夹具目录；缺省 {FIXTURES_REL}")
    parser.add_argument("--generated-at", help="覆盖 generated_at（ISO8601），用于可复现重放；也认 SOURCE_DATE_EPOCH")
    parser.add_argument("--target-dir", help="cargo target 目录（默认隔离到 ${XDG_CACHE_HOME:-~/.cache}/echo-agent-core/protocol-contract-target）")
    parser.add_argument("--offline", action="store_true", help="cargo --offline（无网 CI 用；需本地已有依赖缓存）")
    parser.add_argument("--force", action="store_true", help="覆盖已存在的同名产物目录")
    args = parser.parse_args()

    core = Path(args.source).resolve()
    if not (core / "Cargo.toml").is_file():
        die(f"--source 不是 Core 仓库根（没有 Cargo.toml）：{core}")

    head = git(["rev-parse", "HEAD"], core)
    commit = args.commit or head
    if args.commit and not HEX40.match(commit.lower()):
        die(f"--commit 必须是完整 40 位提交，拒绝分支/短 SHA：{commit}")
    dirty = git(["status", "--porcelain", "--untracked-files=normal"], core) != ""
    if dirty and not args.allow_dirty:
        die("Core 工作树有未提交改动；发版必须用固定提交（git archive 只含跟踪文件）。\n"
            "  本地验证请显式加 --allow-dirty（产物 dirty=true）。")
    if args.ts_from and not args.allow_dirty:
        die("预生成绑定仅供本地测试，必须显式传 --allow-dirty")
    if dirty and args.commit and args.commit.lower() != head.lower():
        die("工作树有未提交改动时不能指定另一个 --commit（来源会被伪造）")
    # 只有「确实脏 + 显式允许」才走工作树快照；干净树即使给了 --allow-dirty 也走归档。
    worktree = dirty and args.allow_dirty

    expected = args.expect_ts_sha256
    panel_pin = None
    if args.panel_package:
        package_path = Path(args.panel_package).resolve()
        try:
            package = json.loads(package_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as err:
            die(f"读不了 Panel package.json：{package_path}（{err}）")
        pin = package.get("protocolCore") or {}
        panel_pin = {
            # 记调用方给的路径（通常是相对路径），避免把本机绝对路径写进产物。
            "package": args.panel_package,
            "repository": pin.get("repository"),
            "commit": pin.get("commit"),
            "generatedSha256": pin.get("generatedSha256"),
        }
        if not expected:
            expected = pin.get("generatedSha256")
    if expected and not HEX64.match(expected.lower()):
        die(f"--expect-ts-sha256 必须是 64 位小写十六进制：{expected}")
    if not expected:
        die("缺少预期 TS 摘要：传 --expect-ts-sha256 或 --panel-package。\n"
            "  没有预期值就无法判断生成物是否漂移，本脚本不会盲产。")

    target_dir = args.target_dir or str(
        Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "echo-agent-core" / "protocol-contract-target"
    )

    with tempfile.TemporaryDirectory(prefix="echo-protocol-contract-") as tmp_name:
        tmp = Path(tmp_name)
        source_root, worktree = materialize_source(core, commit, worktree, tmp)

        # Version, binding generator and default fixtures must come from the same snapshot.
        crate_version = workspace_version(source_root)
        version = args.contract_version or crate_version
        if not re.fullmatch(r"[0-9A-Za-z.+-]+", version):
            die(f"契约版本含非法字符：{version}")
        out_root = Path(args.out).resolve()
        artifact = out_root / f"protocol-contract-{version}"
        if artifact.exists() and not args.force:
            die(f"产物目录已存在（要覆盖请加 --force）：{artifact}")

        if args.fixtures and not args.allow_dirty:
            die("自定义夹具仅供本地测试，必须显式传 --allow-dirty")
        fixtures_dir = Path(args.fixtures).resolve() if args.fixtures else source_root / FIXTURES_REL
        fixtures_label = "external-test-fixtures" if args.fixtures else FIXTURES_REL
        fixture_paths = sorted(fixtures_dir.glob("*.json"))
        if not fixture_paths:
            die(f"夹具目录不存在或没有 *.json：{fixtures_dir}")


        cargo_version = run_cargo(["cargo", "--version"], core, os.environ.copy())
        rustc_version = run_cargo(["rustc", "--version"], core, os.environ.copy())

        staged_ts = tmp / "protocol.generated.ts"
        generator_mode = "prebuilt"
        if args.ts_from:
            shutil.copyfile(Path(args.ts_from).resolve(), staged_ts)
        else:
            generator_mode = "cargo"
            env = os.environ.copy()
            env["CARGO_TARGET_DIR"] = target_dir
            command = [
                "cargo", "run", "-q",
                "-p", "echo-protocol",
                "--features", "ts-bindings",
                "--example", "protocol_contract_ts",
                "--", "--out", str(staged_ts),
            ]
            if args.offline:
                command.insert(3, "--offline")
            if not worktree:
                command.insert(3, "--locked")
            run_cargo(command, source_root, env)

        ts_bytes = staged_ts.read_bytes()
        if not ts_bytes.strip():
            die("生成器输出为空——拒绝产出空契约")
        ts_sha = sha256_bytes(ts_bytes)
        if ts_sha != expected.lower():
            die(
                "TypeScript 绑定与预期摘要不符：\n"
                f"  预期 {expected}\n  实际 {ts_sha}\n"
                "  Core 的协议类型与 Panel 固定的 protocol.generated.ts 已经漂移；\n"
                "  先确认是「该更新 Panel 的 protocolCore」还是「生成入口走错了 crate」，不要直接发版。"
            )
        if b"\r\n" in ts_bytes:
            die("TS 正文含 CRLF——跨平台生成会让 Panel 的 sha256 校验误报")

        # ── 组装 ──
        staging = out_root / f".staging-{os.getpid()}"
        if staging.exists():
            shutil.rmtree(staging)
        (staging / "fixtures").mkdir(parents=True)
        shutil.copyfile(staged_ts, staging / TS_NAME)

        fixture_entries = []
        for path in fixture_paths:
            target = staging / "fixtures" / path.name
            shutil.copyfile(path, target)
            fixture_entries.append(derive_fixture(target))
        index = {
            "schema": FIXTURE_SCHEMA,
            "source": fixtures_label,
            "count": len(fixture_entries),
            "fixtures": fixture_entries,
        }
        (staging / INDEX_NAME).write_text(
            json.dumps(index, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

        payload_files = [
            {"path": TS_NAME, "sha256": ts_sha, "bytes": len(ts_bytes)},
            *[
                {"path": entry["path"], "sha256": entry["sha256"], "bytes": entry["bytes"]}
                for entry in fixture_entries
            ],
            {
                "path": INDEX_NAME,
                "sha256": sha256_file(staging / INDEX_NAME),
                "bytes": (staging / INDEX_NAME).stat().st_size,
            },
        ]

        if args.generated_at:
            generated_at = args.generated_at
        elif os.environ.get("SOURCE_DATE_EPOCH"):
            generated_at = datetime.fromtimestamp(
                int(os.environ["SOURCE_DATE_EPOCH"]), tz=timezone.utc
            ).strftime("%Y-%m-%dT%H:%M:%SZ")
        else:
            generated_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

        manifest = {
            "schema": SCHEMA,
            "generated_at": generated_at,
            "contract": {
                "version": version,
                "ts_file": TS_NAME,
                "ts_sha256": ts_sha,
                "ts_bytes": len(ts_bytes),
                "aggregate_sha256": aggregate(payload_files),
                "fixtures": {
                    "file": INDEX_NAME,
                    "count": len(fixture_entries),
                    "source": fixtures_label,
                },
            },
            "source": {
                "repository": "https://github.com/EchoLab-Auto/EchoAgentCore",
                "commit": commit.lower(),
                "dirty": dirty or bool(args.fixtures or args.ts_from),
                "crate": "echo-protocol",
                "crate_version": crate_version,
                "generator": EXAMPLE_REL,
                "generator_mode": generator_mode,
                "feature": "ts-bindings",
                "cargo": cargo_version,
                "rustc": rustc_version,
                "panel_pin": panel_pin,
            },
            "files": sorted(payload_files, key=lambda entry: entry["path"]),
        }
        (staging / MANIFEST_NAME).write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

        sums_entries = [
            *payload_files,
            {"path": MANIFEST_NAME, "sha256": sha256_file(staging / MANIFEST_NAME)},
        ]
        write_sums(staging, sums_entries)

        if artifact.exists():
            shutil.rmtree(artifact)
        out_root.mkdir(parents=True, exist_ok=True)
        staging.rename(artifact)

    print(f"✓ 契约产物：{artifact}")
    print(f"  version   {version}")
    print(f"  ts sha256 {ts_sha} ({len(ts_bytes)} B)")
    print(f"  fixtures  {len(fixture_entries)}")
    print(f"  aggregate {manifest['contract']['aggregate_sha256']}")
    print(f"  source    {commit.lower()} dirty={str(manifest['source']['dirty']).lower()} mode={generator_mode}")
    print(f"  下一步：python3 {Path(__file__).resolve().parent / 'verify-protocol-contract.py'} {artifact}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
