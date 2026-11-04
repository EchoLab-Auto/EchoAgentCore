#!/usr/bin/env python3
"""ProDoc 文档图重排与几何自检。

按「一组一带 + 左侧扇出通道」的规则重算 `document/` 下所有 `.md` 参数区的
`x` / `y` / `link`，并用**真实渲染器几何**校验结果。规则的理由写在
`document/documentation-guide.md`「坐标：一组一带，组内网格」一节；本脚本是
那套规则的唯一实现。

用法:
    python3 tools/prodoc_layout.py                   # 只打印排布表（dry-run）
    python3 tools/prodoc_layout.py --write            # 落盘（只改 x / y / link 三行）
    python3 tools/prodoc_layout.py --verify           # 校验「按本次规则落盘」是否干净
    python3 tools/prodoc_layout.py --write --verify   # 落盘并校验
    python3 tools/prodoc_layout.py --verify --from-disk   # 校验磁盘上的现有坐标

    --docrenderer PATH   DocRenderer 仓库路径（校验用；默认自动探测）
    --json PATH          把图数据另存一份 JSON（调试用）

校验需要 [DocRenderer] 检出：贝塞尔路由、分层布局的长边拐点、组区域 padding
一律调用它的 `prodoc-core` 源码，绝不在这里复写一份——复写会与渲染器静默
漂移，届时校验通过而画布上照样穿框，比没有校验更糟。

退出码: 0 干净 / 1 校验发现问题或断链 / 2 环境或输入错误
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

# ---------------------------------------------------------------- 布局规则
# 与 documentation-guide.md「坐标：一组一带，组内网格」一一对应。

#: 组带自上而下。总览在最上做入口，工程收在末尾。
BAND_ORDER = ["总览", "Core", "前端", "工程"]

#: 组内 x 步距 = 框宽 220 + 88 间隙。横向相邻不打架的最小值。
DX = 320

#: 组内 y 步距：同层并列 160（给标签留位），父子分层 280（给连接边留弧顶）。
DY_SAME = 160
DY_CROSS = 280

#: 组带之间的空隙。组区域 = 成员包围盒 + padding（左右/下 24、顶部 34），
#: 所以带间留 184 时，相邻两个**组区域**之间剩 184 − 24 − 34 = 126px，
#: 满足文档里「相邻带留 ≥ 120px」的约束。
BAND_GAP = 184

#: 画布左上留白。
BASE_Y = 80

#: 组带左边界 = index 扇出通道的右边界。组根一律贴这一列，跨组长边才能走
#: 空通道而不穿框。
CORRIDOR_X = 960

#: index 独占左上角，它的六条出边都从右边界（960 之前）离开。
INDEX_X = 160

#: 组带的列原点：总览组留给画布入口，其余贴扇出通道右边界。
BAND_ORIGIN = {"总览": INDEX_X}

#: 组根各分支之间空出几列：Core 的内核列（`960..2560`）与插件列（`3200..4800`）
#: 必须分开，否则 12 个框连成一排，「两支同属 Core」看不出来。
BRANCH_GAP = {"Core": 1}

#: 父节点对齐方式。`left` = 贴子群最左（双主干结构连成竖线），
#: `center` = 落子群正中（多叉树，避免长斜边）。
ALIGN = {"总览": "left", "Core": "left", "前端": "center", "工程": "left"}

#: 子节点超过这个数就强制居中（不看 ALIGN）：风扇太宽时贴左会让最远的边
#: 横穿整排兄弟框。14 个插件是当前最宽的扇。
WIDE_FANOUT = 6

#: 显式连接边。缺省交给渲染器自动选边（行距 280 ≫ 框高 96，父上子下必得
#: 「底出顶进」）；这里只列自动选边会出错的那些：
#:
#:   index → 各组根    r>l   从右边界出发，沿左侧空通道下行，进组根左边界
#:   core → plugins    b>t   与 core → frame 对称成扇形；自动选边会判成左右
#:                          相接，把 frame 绕过
SIDES = {
    ("index", "core"): "r>l",
    ("index", "panel"): "r>l",
    ("index", "protocol"): "r>l",
    ("index", "deploy"): "r>l",
    ("index", "guide"): "r>l",
    ("core", "plugins"): "b>t",
}

#: 框尺寸取渲染器缺省值（不在参数区声明 w/h 时）。
BOX_W, BOX_H = 220, 96

DEFAULT_DOC_ROOT = Path(__file__).resolve().parent.parent / "document"
VERIFY_SCRIPT = Path(__file__).resolve().parent / "prodoc_verify.mjs"
DOCRENDERER_CANDIDATES = [
    Path.home() / "code_file/gitlab/echo-tech/DocRenderer",
    Path.home() / "DocRenderer",
]


def fail(message: str):
    """输入或环境错误：打印原因并以退出码 2 结束。"""
    print(message, file=sys.stderr)
    raise SystemExit(2)


# ---------------------------------------------------------------- 参数区
def split_front(text: str):
    """拆出参数区行与正文；无参数区返回 (None, text)。"""
    lines = text.split("\n")
    if not lines or lines[0].strip() != "---":
        return None, text
    for i in range(1, len(lines)):
        if lines[i].strip() == "---":
            return lines[1:i], "\n".join(lines[i + 1:])
    return None, text


def unquote(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def split_list(inner: str):
    """按逗号切内联列表，尊重引号。"""
    parts, buf, quote = [], "", None
    for ch in inner:
        if quote:
            buf += ch
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            buf += ch
        elif ch == ",":
            parts.append(buf.strip())
            buf = ""
        else:
            buf += ch
    if buf.strip():
        parts.append(buf.strip())
    return [p for p in parts if p]


def parse_value(key: str, raw: str):
    raw = raw.strip()
    if raw.startswith("[") and raw.endswith("]"):
        return [unquote(p) for p in split_list(raw[1:-1].strip())]
    if key in ("x", "y", "w", "h") or re.fullmatch(r"-?\d+", raw):
        try:
            return int(raw)
        except ValueError:
            pass
    return unquote(raw)


def read_docs(root: Path) -> dict:
    docs = {}
    for path in sorted(root.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        front, body = split_front(text)
        rel = str(path.relative_to(root))
        if front is None:
            docs[rel] = {"path": path, "rel": rel, "fm": None, "body": text}
            continue

        keys, raw = [], {}
        for line in front:
            match = re.match(r"^([A-Za-z_][\w-]*):\s*(.*)$", line)
            if match:
                keys.append(match.group(1))
                raw[match.group(1)] = match.group(2)

        links = []
        for entry in parse_value("link", raw["link"]) if "link" in raw else []:
            bits = [b.strip() for b in entry.split("|")]
            links.append({
                "to": unquote(bits[0]),
                "label": unquote(bits[1]) if len(bits) > 1 and bits[1] else None,
                "side": bits[2] if len(bits) > 2 and bits[2] else None,
            })

        docs[rel] = {
            "path": path,
            "rel": rel,
            "fm": front,
            "keys": keys,
            "raw": raw,
            "body": body,
            "id": unquote(raw.get("id", rel[:-3])),
            "group": parse_value("group", raw["group"]) if "group" in raw else None,
            "x": parse_value("x", raw["x"]) if "x" in raw else None,
            "y": parse_value("y", raw["y"]) if "y" in raw else None,
            "links": links,
        }
    return docs


def compute_layout(by_id: dict) -> dict:
    """返回 {id: (x, y)}。组带自上而下，组内按链接结构做 tidy 布局。

    三条规则，都不依赖具体文档 id（换一批文档照样成立）：

    1. **每个组带有一列原点**：总览组是 `INDEX_X`（画布左上角留给入口），
       其余是 `CORRIDOR_X`（扇出通道的右边界）。所有组根都贴这一列，
       跨组长边才能走空通道而不穿框。
    2. **多个根竖排**：一个组带里有几个无组内父节点的框时，它们各自带一棵
       子树，按 fork 顺序自上而下分块；块内父子间距 `DY_CROSS`，块与块之间
       只隔 `DY_SAME`（同层兄弟的间距）。
    3. **父节点 x**：`ALIGN=left` 取子群最左（双主干结构因此连成竖线），
       `ALIGN=center` 取子群中点（多叉树因此没有长斜边）。
    """
    parent, children = {}, {i: [] for i in by_id}
    for doc in by_id.values():
        for link in doc["links"]:
            if link["to"] not in by_id:
                continue
            if link["to"] in parent:
                fail(
                    f"doc {link['to']} 有多个父节点（{parent[link['to']]} / {doc['id']}）；"
                    "文档规范要求一个框只被一条 link 指向"
                )
            parent[link["to"]] = doc["id"]
            children[doc["id"]].append(link["to"])

    # 环检测：`link` 是**导航**不是引用。子文档指回父文档（或任意 A→…→A）
    # 会让渲染器把环上成员塞进同一个强连通分量、排在同一层，"父子"关系在
    # 画布上就消失了。这里在排序之前先查，报出整条环路径。
    state: dict = {i: 0 for i in by_id}     # 0=未访问 1=在栈上 2=已完成

    def assert_acyclic(node: str, stack: list):
        state[node] = 1
        stack.append(node)
        for kid in children[node]:
            if state.get(kid) == 1:
                loop = stack[stack.index(kid):] + [kid]
                fail("检测到循环连线：" + " → ".join(loop) +
                     "\n  画布 `link` 只表达主干导航（父 → 直接子文档）；"
                     "交叉引用请写正文相对链接")
            if state.get(kid) == 0:
                assert_acyclic(kid, stack)
        stack.pop()
        state[node] = 2

    for node in by_id:
        if state[node] == 0:
            assert_acyclic(node, [])

    unknown = {d["group"] for d in by_id.values()} - set(BAND_ORDER)
    if unknown:
        fail(f"未登记的 group: {sorted(unknown)}；先加进 BAND_ORDER（并写进文档规范）")

    # 全局访问顺序：从无父节点出发，按各文档 `link` 的书写顺序深度优先。
    # 组带内的多个根按这个顺序竖排（= 父文档 fork 的顺序），而不是按文件
    # 路径——否则改个文件名就会让画布上的竖排次序变掉。
    order: dict = {}

    def walk(node: str, seen: set):
        if node in seen:
            return
        seen.add(node)
        order[node] = len(order)
        for kid in children[node]:
            walk(kid, seen)

    seen: set = set()
    for doc in by_id.values():
        if doc["id"] not in parent:
            walk(doc["id"], seen)
    for doc in by_id.values():          # 兜底：环或被断链隔开的子图
        walk(doc["id"], seen)

    pos, band_top = {}, BASE_Y
    for band in BAND_ORDER:
        members = [d for d in by_id.values() if d["group"] == band]
        if not members:
            continue
        ids = {d["id"] for d in members}
        roots = sorted(
            (d["id"] for d in members if parent.get(d["id"]) not in ids),
            key=lambda node: order.get(node, 0),
        )
        origin = BAND_ORIGIN.get(band, CORRIDOR_X)

        # slot 是列游标。多根竖排时根列被那几个根占满，子节点从下一列起，
        # 否则整组会压成一列（protocol / deploy / guide / testing … 全在 x=960）。
        xs, slot = {}, [1 if len(roots) > 1 else 0]

        def visit(node: str, depth: int, acc: dict) -> int:
            """排布 node 的子树；返回本子树的最大深度。"""
            kids = [c for c in children[node] if c in ids]
            acc[node] = depth
            if not kids:
                if depth == 0:
                    # 无子的组根：x 稍后统一强制成组带原点，不占用列
                    xs[node] = origin
                    return depth
                xs[node] = origin + slot[0] * DX
                slot[0] += 1
                return depth
            for index, kid in enumerate(kids):
                if depth == 0 and index > 0:
                    slot[0] += BRANCH_GAP.get(band, 0)
                visit(kid, depth + 1, acc)
            lo, hi = min(xs[k] for k in kids), max(xs[k] for k in kids)
            # 分支数超过 WIDE_FANOUT 时强制居中：一个父带十来个子，风扇最大展开
            # 有 4000px 宽，若父贴最左，最右那条边会以极小的斜率横穿整排兄弟框。
            # 居中让最远边的水平距离减半，曲线沿父的底/顶法线下去就落到自己那段。
            left = ALIGN.get(band) == "left" and len(kids) <= WIDE_FANOUT
            xs[node] = lo if left else (lo + hi) // 2
            return max(acc.values())

        block_top = 0
        for root in roots:
            acc: dict = {}
            deepest = visit(root, 0, acc)
            for node, depth in acc.items():
                pos[node] = (xs[node], band_top + block_top + depth * DY_CROSS)
            block_top += deepest * DY_CROSS + DY_SAME

        for root in roots:      # 组根贴组带原点（跨组边的落点必须可预期）
            x, y = pos[root]
            pos[root] = (origin, y)

        band_top = max(y for _, y in (pos[n] for n in xs)) + BOX_H + BAND_GAP

    return pos

    return pos


# ---------------------------------------------------------------- 落盘
def write_layout(by_id: dict, pos: dict) -> None:
    """只改 `x` / `y` / `link` 三行，其余参数与正文原样保留。"""
    for doc in by_id.values():
        x, y = pos[doc["id"]]
        out, seen_pos = [], False
        for key in doc["keys"]:
            if key == "x":
                out.append(f"x: {x}")
                seen_pos = True
            elif key == "y":
                out.append(f"y: {y}")
            elif key == "link":
                entries = []
                for link in doc["links"]:
                    side = SIDES.get((doc["id"], link["to"]))
                    bits = [link["to"]]
                    if link["label"]:
                        bits.append(link["label"])
                    if side:
                        bits.append(side)
                    entries.append(" | ".join(bits))
                if entries:
                    body = ", ".join(f'"{e}"' if " " in e else e for e in entries)
                    out.append(f"link: [{body}]")
                else:
                    out.append("link: []")
            else:
                out.append(f"{key}: {doc['raw'][key]}")
        if not seen_pos:
            out.extend([f"x: {x}", f"y: {y}"])
        text = "---\n" + "\n".join(out) + "\n---\n" + doc["body"]
        doc["path"].write_text(text, encoding="utf-8")


# ---------------------------------------------------------------- 图数据
def build_graph(by_id: dict, pos: dict) -> dict:
    boxes = [
        {
            "id": d["id"],
            "x": pos[d["id"]][0],
            "y": pos[d["id"]][1],
            "w": BOX_W,
            "h": BOX_H,
            "group": d["group"],
        }
        for d in by_id.values()
    ]
    relations, broken = [], []
    for doc in by_id.values():
        for link in doc["links"]:
            if link["to"] not in by_id:
                broken.append(f"{doc['id']} -> {link['to']}")
                continue
            side = SIDES.get((doc["id"], link["to"]))
            from_side, to_side = side.split(">") if side else (None, None)
            relations.append({
                "from": doc["id"],
                "to": link["to"],
                "fromSide": from_side,
                "toSide": to_side,
            })
    return {"boxes": boxes, "relations": relations, "broken": broken}


def current_pos(by_id: dict) -> dict:
    missing = [d["id"] for d in by_id.values() if d["x"] is None or d["y"] is None]
    if missing:
        fail(f"这些文档没有 x/y，无法校验磁盘现状: {missing}")
    return {d["id"]: (d["x"], d["y"]) for d in by_id.values()}


# ---------------------------------------------------------------- 校验
def find_docrenderer(explicit: str | None) -> Path:
    """定位 DocRenderer 检出。

    显式指定（`--docrenderer` 或 `PRODOC_RENDERER`）时**不回落**到默认候选：
    用户指了路径就只用它，静默改用别处的检出会让校验结论无法解释。
    """
    pinned = explicit or os.environ.get("PRODOC_RENDERER")
    candidates = [pinned] if pinned else [str(p) for p in DOCRENDERER_CANDIDATES]
    tried = []
    for candidate in candidates:
        path = Path(candidate).expanduser().resolve()
        tried.append(str(path))
        if (path / "packages/prodoc-core/src/edge-router.ts").is_file():
            return path
    fail(
        "找不到 DocRenderer 检出（需要 packages/prodoc-core/src/edge-router.ts）。\n"
        "  已尝试:\n    " + "\n    ".join(tried) +
        "\n  用 --docrenderer PATH 或环境变量 PRODOC_RENDERER 指定。\n"
        "  校验故意不降级：复写一份路由算法必然与渲染器漂移，"
        "那样的「通过」比不校验更有害。"
    )


def verify(graph: dict, docrenderer: Path) -> int:
    payload = json.dumps(graph, ensure_ascii=False)
    proc = subprocess.run(
        [os.environ.get("NODE", "node"), "--import", "tsx", str(VERIFY_SCRIPT)],
        input=payload, text=True, capture_output=True, cwd=str(docrenderer),
    )
    if proc.stdout:
        sys.stdout.write(proc.stdout)
    if proc.returncode != 0 and proc.stderr:
        sys.stderr.write(proc.stderr)
    return proc.returncode


# ---------------------------------------------------------------- main
def main() -> int:
    parser = argparse.ArgumentParser(description="ProDoc 文档图重排与几何自检")
    parser.add_argument("--document", default=str(DEFAULT_DOC_ROOT), help="文档根目录")
    parser.add_argument("--write", action="store_true", help="落盘（默认只打印）")
    parser.add_argument("--verify", action="store_true", help="调用渲染器几何校验")
    parser.add_argument("--from-disk", action="store_true", help="校验磁盘现有坐标，而非本次重排结果")
    parser.add_argument("--docrenderer", default=None, help="DocRenderer 仓库路径")
    parser.add_argument("--json", default=None, help="另存图数据 JSON")
    parser.add_argument("--quiet", action="store_true", help="不打印排布表")
    args = parser.parse_args()

    if args.write and args.from_disk:
        fail("--write 与 --from-disk 互斥：前者写重排结果，后者读磁盘现状")

    root = Path(args.document).expanduser().resolve()
    if not root.is_dir():
        print(f"文档目录不存在: {root}", file=sys.stderr)
        return 2

    docs = read_docs(root)
    by_id = {d["id"]: d for d in docs.values() if d.get("fm") is not None}
    if not by_id:
        print(f"{root} 下没有带参数区的 .md", file=sys.stderr)
        return 2

    plan = compute_layout(by_id)
    target = current_pos(by_id) if args.from_disk else plan

    if not args.quiet:
        for band in BAND_ORDER:
            members = sorted(
                [d for d in by_id.values() if d["group"] == band],
                key=lambda d: (target[d["id"]][1], target[d["id"]][0]),
            )
            if not members:
                continue
            print(f"== {band} ==")
            for doc in members:
                x, y = target[doc["id"]]
                print(f"   {doc['id']:<24} ({x:>5}, {y:>5})")
        xs = [p[0] for p in target.values()]
        ys = [p[1] for p in target.values()]
        print(f"\n框 {len(target)} · 画布 {max(xs) + BOX_W - min(xs)}×{max(ys) + BOX_H - min(ys)}")

    if args.write:
        write_layout(by_id, plan)
        print(f"\n已写入 {len(by_id)} 篇（只改 x / y / link 三行）")

    graph = build_graph(by_id, target)
    if graph["broken"]:
        print("\n!! 断链（link 指向不存在的文档）:", file=sys.stderr)
        for item in graph["broken"]:
            print(f"   {item}", file=sys.stderr)

    if args.json:
        Path(args.json).write_text(json.dumps(graph, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"图数据已存 {args.json}")

    if args.verify:
        print()
        code = verify(graph, find_docrenderer(args.docrenderer))
        return code or (1 if graph["broken"] else 0)

    return 1 if graph["broken"] else 0


if __name__ == "__main__":
    sys.exit(main())
