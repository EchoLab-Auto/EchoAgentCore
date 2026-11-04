#!/usr/bin/env node
/**
 * ProDoc 文档图几何校验。
 *
 * 由 `tools/prodoc_layout.py --verify` 调用，也可单独跑：
 *   node --import tsx tools/prodoc_verify.mjs < graph.json
 *
 * 几何一律取自 DocRenderer 的 `prodoc-core` 源码（贝塞尔路由、分层布局的
 * 长边拐点、组区域 padding），本文件只负责采样与判定——不重写任何渲染算法。
 * 复写会与渲染器静默漂移：校验通过而画布上照样穿框，比没有校验更糟。
 *
 * 四项检查：
 *   框重叠         两框矩形相交（渲染器不会自动避让，重叠即视觉事故）
 *   组区域相交     分组区域两两相交（组名标签会互相压）
 *   连线穿框       按真实路由采样曲线，落入第三方框即失败
 *   边贴线         两条边的采样点长时间重合（视觉上分不清谁连谁）
 */
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const PRODOC_CORE = process.env.PRODOC_CORE
  ?? resolve(process.cwd(), 'packages/prodoc-core/src');
const load = (name) => import(pathToFileURL(resolve(PRODOC_CORE, name)).href);

const [
  { bezierRouter },
  { computeLayeredLayout },
  { computeGroupRegion },
] = await Promise.all([
  load('edge-router.ts'),
  load('layout.ts'),
  load('graph.ts'),
]);

const SIDES = { t: 'top', r: 'right', b: 'bottom', l: 'left' };
const side = (s) => (s ? SIDES[s] : undefined);

const input = JSON.parse(readFileSync(0, 'utf8'));
const { boxes, relations } = input;
const byId = new Map(boxes.map((b) => [b.id, b]));

const overlapOf = (a, b) => {
  const ox = Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x);
  const oy = Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y);
  return ox > 0 && oy > 0 ? `${ox}×${oy}` : null;
};

/* ---------------- 1. 框重叠 ---------------- */
const boxHits = [];
for (let i = 0; i < boxes.length; i++) {
  for (let j = i + 1; j < boxes.length; j++) {
    const hit = overlapOf(boxes[i], boxes[j]);
    if (hit) boxHits.push(`${boxes[i].id} × ${boxes[j].id} 重叠 ${hit}`);
  }
}

/* ---------------- 2. 组区域相交 ---------------- */
const groups = new Map();
for (const box of boxes) {
  if (!box.group) continue;
  if (!groups.has(box.group)) groups.set(box.group, []);
  groups.get(box.group).push(box);
}
const regions = [...groups.entries()].map(([name, members]) => ({
  name, ...computeGroupRegion(members),
}));
const regionHits = [];
for (let i = 0; i < regions.length; i++) {
  for (let j = i + 1; j < regions.length; j++) {
    const hit = overlapOf(regions[i], regions[j]);
    if (hit) regionHits.push(`${regions[i].name} × ${regions[j].name} 相交 ${hit}`);
  }
}

/* ---------------- 3. 连线穿框 + 4. 边贴线 ---------------- */
// 复刻 viewer：先用分层布局算长边拐点（虚拟节点位置），再交给路由器
const layered = computeLayeredLayout(
  boxes.map((b) => ({ id: b.id, w: b.w, h: b.h, fixedX: b.x, fixedY: b.y })),
  relations.map((r) => ({ from: r.from, to: r.to })),
  { iterations: 1 },
);

/** 把 SVG path 的 d 属性按 40 段/曲线采样成折线 */
function sample(d) {
  const nums = d.match(/-?\d+(?:\.\d+)?/g).map(Number);
  const points = [{ x: nums[0], y: nums[1] }];
  let cur = { x: nums[0], y: nums[1] };
  for (let i = 2; i + 5 < nums.length; i += 6) {
    const c1 = { x: nums[i], y: nums[i + 1] };
    const c2 = { x: nums[i + 2], y: nums[i + 3] };
    const end = { x: nums[i + 4], y: nums[i + 5] };
    for (let t = 1; t <= 40; t++) {
      const u = t / 40;
      const v = 1 - u;
      points.push({
        x: v ** 3 * cur.x + 3 * v ** 2 * u * c1.x + 3 * v * u ** 2 * c2.x + u ** 3 * end.x,
        y: v ** 3 * cur.y + 3 * v ** 2 * u * c1.y + 3 * v * u ** 2 * c2.y + u ** 3 * end.y,
      });
    }
    cur = end;
  }
  return points;
}

const paths = relations.map((relation) => {
  const from = byId.get(relation.from);
  const to = byId.get(relation.to);
  const route = bezierRouter({
    from: { x: from.x, y: from.y, w: from.w, h: from.h },
    to: { x: to.x, y: to.y, w: to.w, h: to.h },
    fromSide: side(relation.fromSide),
    toSide: side(relation.toSide),
    waypoints: layered.routes.get(`${relation.from}->${relation.to}`) ?? [],
  });
  return {
    key: `${relation.from} → ${relation.to}`,
    points: sample(route.d),
    length: Math.hypot(route.x2 - route.x1, route.y2 - route.y1),
  };
});

// 判定「落入框内」时向内缩 2px：端点贴边中点是正常情况，不算穿透
const inside = (p, box, inset = 2) =>
  p.x > box.x + inset && p.x < box.x + box.w - inset &&
  p.y > box.y + inset && p.y < box.y + box.h - inset;

const crossHits = [];
for (const [index, relation] of relations.entries()) {
  for (const box of boxes) {
    if (box.id === relation.from || box.id === relation.to) continue;
    if (paths[index].points.some((p) => inside(p, box))) {
      crossHits.push(`${paths[index].key} 穿过 ${box.id}`);
      break;
    }
  }
}

// 贴线：一条边 35% 以上的采样点落在另一条边 8px 内，视觉上就是同一根线
const braidHits = [];
for (let i = 0; i < paths.length; i++) {
  for (let j = i + 1; j < paths.length; j++) {
    const a = paths[i];
    const b = paths[j];
    let near = 0;
    for (const p of a.points) {
      let best = Infinity;
      for (const q of b.points) {
        const d2 = (p.x - q.x) ** 2 + (p.y - q.y) ** 2;
        if (d2 < best) best = d2;
      }
      if (best < 64) near++;
    }
    const share = near / a.points.length;
    if (share > 0.35) {
      braidHits.push(`${a.key} 与 ${b.key} 贴线 ${(share * 100).toFixed(0)}%`);
    }
  }
}

/* ---------------- 报告 ---------------- */
const minX = Math.min(...boxes.map((b) => b.x));
const minY = Math.min(...boxes.map((b) => b.y));
const width = Math.max(...boxes.map((b) => b.x + b.w)) - minX;
const height = Math.max(...boxes.map((b) => b.y + b.h)) - minY;
const meanLength = paths.reduce((sum, p) => sum + p.length, 0) / (paths.length || 1);
const waypointEdges = [...layered.routes.values()].filter((v) => v.length).length;

console.log(`框 ${boxes.length} · 连线 ${relations.length} · 画布 ${width}×${height}`);
console.log(`平均边长 ${meanLength.toFixed(0)}px · 长边拐点 ${waypointEdges} 条`);
console.log('\n组区域:');
for (const r of [...regions].sort((a, b) => a.y - b.y)) {
  console.log(
    `  ${r.name.padEnd(4)} x ${r.x.toFixed(0)}..${(r.x + r.w).toFixed(0)}` +
    `  y ${r.y.toFixed(0)}..${(r.y + r.h).toFixed(0)}  (${r.w.toFixed(0)}×${r.h.toFixed(0)})`,
  );
}

const report = (label, hits) => {
  console.log(`\n${label}: ${hits.length === 0 ? '✅ 无问题' : `❌ ${hits.length} 项`}`);
  for (const hit of hits) console.log(`  - ${hit}`);
};
report('框重叠', boxHits);
report('组区域相交', regionHits);
report('连线穿框', crossHits);
report('边贴线 (>35%)', braidHits);

const longest = [...paths].sort((a, b) => b.length - a.length).slice(0, 5);
if (longest.length) {
  console.log('\n最长 5 条边:');
  for (const p of longest) console.log(`  ${p.length.toFixed(0).padStart(5)}px  ${p.key}`);
}

const failed = boxHits.length + regionHits.length + crossHits.length + braidHits.length;
process.exit(failed === 0 ? 0 : 1);
