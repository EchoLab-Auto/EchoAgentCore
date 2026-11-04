/* ═══════════════════════════════════════════════════════════════════════════
 * view3d.js — 建模软件式三维视口（WebGL2 光栅化管线，零依赖）
 *
 * 这一版不再是"光线步进出来的近似渲染"，而是**真正的三角网格**：
 *
 *   几何 —— 从 gpu/rt_neu.cu 的解析形状（圆角矩形轮廓 × 圆角剖面）直接
 *           三角化成 BufferGeometry：顶面扇形、竖直侧壁、1/4 圆环倒角、
 *           带洞的地面。三角形约 2k 个，顶点法线解析给出。
 *   渲染 —— 标准光栅化：MVP 变换 → 深度缓冲 → 背面剔除 → 片元着色。
 *           没有累积、没有噪点，一帧就是终帧。
 *   着色 —— 相机空间的 studio 三光（主光 / 补光 / 边缘压暗）+ 高度提示。
 *           光跟着相机走，所以转视角时同一个面的明暗不变 —— 这是建模软件
 *           的约定，不是场景光照：**没有光源、没有阴影、没有遮挡**。
 *   线框 —— 深度 + 法线两个缓冲做屏幕空间边缘检测，得到一像素宽的硬边；
 *           这就是三维软件视口里那条轮廓线。
 *
 * 于是：几何与内核完全同源（同一台正交相机、同一个 ortho_h），但颜色不是
 * 最终材质 —— 光怎么打、影子落多远，是「渲染」页那一帧由内核算的事。
 *
 * 坐标与内核一致：像素单位 = CSS px，z 向上，平面在 z = 0。
 * ═══════════════════════════════════════════════════════════════════════════ */
(function (global) {
  "use strict";

  /* ═════════════════════════ 着色器 ═════════════════════════ */

  const VS_BODY = `#version 300 es
  in vec3 aPos;
  in vec3 aNrm;
  uniform mat4 uMVP;
  out vec3 vN;
  out vec3 vP;
  void main() {
    vN = aNrm;
    vP = aPos;
    gl_Position = uMVP * vec4(aPos, 1.0);
  }`;

  const FS_BODY = `#version 300 es
  precision highp float;
  in vec3 vN;
  in vec3 vP;

  uniform vec3  uEyeDir, uCamRight, uCamUp;
  uniform vec3  uObjCol, uPlaneCol, uBgCol, uGridCol;
  uniform float uGround;      // 1 = 地面，0 = 元件
  uniform float uWire;        // 1 = 线框：元件内部掏空
  uniform float uGridAmt;
  uniform float uDepth;       // 浮雕深度（高度提示用）

  layout(location = 0) out vec4 oCol;
  layout(location = 1) out vec4 oNrm;

  void main() {
    vec3 N = normalize(vN);
    vec3 col;

    if (uGround > 0.5) {
      /* 地面：平色 + 50/200px 两层网格。fwidth 在光栅化管线里是可靠的
         （光线步进时它对 march 出来的坐标经常给 0），所以网格线始终清楚。 */
      col = uPlaneCol;
      if (uGridAmt > 0.5) {
        vec2 w = max(fwidth(vP.xy), vec2(1e-4));
        vec2 a1 = abs(fract(vP.xy / 50.0 + 0.5) - 0.5) * 50.0 / w;
        vec2 a2 = abs(fract(vP.xy / 200.0 + 0.5) - 0.5) * 200.0 / w;
        float l1 = 1.0 - smoothstep(0.0, 1.4, min(a1.x, a1.y));
        float l2 = 1.0 - smoothstep(0.0, 1.8, min(a2.x, a2.y));
        col = mix(col, uGridCol, clamp(max(l1 * 0.55, l2 * 0.90), 0.0, 1.0));
      }
    } else if (uWire > 0.5) {
      /* 线框：面片涂成背景色但仍写深度 —— 剩下的棱靠后处理的边缘检测画 */
      col = uBgCol;
    } else {
      /* 相机空间的三点布光。光跟着相机，所以"转视角"只改变看的方向，
         不改变同一个面亮几度 —— 这正是建模软件视口的手感。 */
      vec3 Nc = vec3(dot(N, uCamRight), dot(N, uCamUp), dot(N, uEyeDir));
      float key  = max(dot(Nc, normalize(vec3(-0.42, 0.55, 0.72))), 0.0);
      float fill = max(dot(Nc, normalize(vec3( 0.68, -0.30, 0.67))), 0.0);
      float rim  = pow(1.0 - clamp(Nc.z, 0.0, 1.0), 3.0);
      // 高度提示：抬得越高越亮、挖得越深越暗（凹陷才看得出是个坑）
      float hz = clamp(vP.z / max(uDepth, 1e-3), -1.0, 1.0);
      float s = 0.50 + 0.38 * key + 0.10 * fill + 0.05 * hz - 0.05 * rim;
      col = uObjCol * s;
    }

    // 远处融进背景，免得看见地面的边界
    float fog = 1.0 - smoothstep(1500.0, 3400.0, length(vP.xy));
    col = mix(uBgCol, col, fog);

    oCol = vec4(col, 1.0);
    oNrm = vec4(N * 0.5 + 0.5, 1.0);
  }`;

  const VS_POST = `#version 300 es
  in vec2 aPos;
  void main() { gl_Position = vec4(aPos, 0.0, 1.0); }`;

  const FS_POST = `#version 300 es
  precision highp float;
  precision highp int;

  uniform sampler2D uColor, uNormal, uDepth;
  uniform ivec2 uRes;
  uniform float uDepthRange;    // NDC 深度差 → 世界距离
  uniform vec3  uLine;
  uniform float uEdge;
  out vec4 outColor;

  vec3 lin2srgb(vec3 c) {
    c = max(c, vec3(0.0));
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(vec3(0.0031308), c));
  }

  void main() {
    ivec2 q = ivec2(gl_FragCoord.xy);
    vec3 base = texelFetch(uColor, q, 0).rgb;
    vec4 n0 = texelFetch(uNormal, q, 0);

    // 背景像素不参与边缘检测（否则地平线会被描一条线）
    if (n0.a < 0.5) {
      outColor = vec4(lin2srgb(base), 1.0);
      return;
    }

    float d0 = texelFetch(uDepth, q, 0).r;
    vec3 nn0 = normalize(n0.xyz * 2.0 - 1.0);
    float e = 0.0;

    // 4 邻域（含两条对角）：深度断层 → 轮廓；法线突变 → 棱
    ivec2 offs[4] = ivec2[4](ivec2(1, 0), ivec2(0, 1), ivec2(1, 1), ivec2(1, -1));
    for (int i = 0; i < 4; i++) {
      ivec2 q1 = clamp(q + offs[i], ivec2(0), uRes - 1);
      vec4 n1 = texelFetch(uNormal, q1, 0);
      float d1 = texelFetch(uDepth, q1, 0).r;
      float dd = abs(d1 - d0) * uDepthRange;
      e = max(e, smoothstep(0.7, 2.4, dd));
      if (n1.a > 0.5) {
        vec3 nn1 = normalize(n1.xyz * 2.0 - 1.0);
        e = max(e, smoothstep(0.22, 0.55, 1.0 - dot(nn0, nn1)));
      }
    }

    vec3 c = mix(base, uLine, clamp(e * uEdge, 0.0, 1.0));
    outColor = vec4(lin2srgb(c), 1.0);
  }`;

  /* ═════════════════════════ 数学 ═════════════════════════ */

  /** 4×4 列主序矩阵相乘：C = A · B */
  function mul4(a, b) {
    const o = new Float32Array(16);
    for (let c = 0; c < 4; c++) {
      for (let r = 0; r < 4; r++) {
        o[c * 4 + r] =
          a[r] * b[c * 4] + a[4 + r] * b[c * 4 + 1] +
          a[8 + r] * b[c * 4 + 2] + a[12 + r] * b[c * 4 + 3];
      }
    }
    return o;
  }

  function ortho(l, r, b, t, n, f) {
    return new Float32Array([
      2 / (r - l), 0, 0, 0,
      0, 2 / (t - b), 0, 0,
      0, 0, -2 / (f - n), 0,
      -(r + l) / (r - l), -(t + b) / (t - b), -(f + n) / (f - n), 1,
    ]);
  }

  function lookAt(eye, center, up) {
    let zx = eye[0] - center[0], zy = eye[1] - center[1], zz = eye[2] - center[2];
    let l = Math.hypot(zx, zy, zz) || 1;
    zx /= l; zy /= l; zz /= l;
    let xx = up[1] * zz - up[2] * zy, xy = up[2] * zx - up[0] * zz, xz = up[0] * zy - up[1] * zx;
    l = Math.hypot(xx, xy, xz) || 1;
    xx /= l; xy /= l; xz /= l;
    const yx = zy * xz - zz * xy, yy = zz * xx - zx * xz, yz = zx * xy - zy * xx;
    return new Float32Array([
      xx, yx, zx, 0,
      xy, yy, zy, 0,
      xz, yz, zz, 0,
      -(xx * eye[0] + xy * eye[1] + xz * eye[2]),
      -(yx * eye[0] + yy * eye[1] + yz * eye[2]),
      -(zx * eye[0] + zy * eye[1] + zz * eye[2]), 1,
    ]);
  }

  /* ═════════════════════════ 几何 ═════════════════════════ */

  const LOOP_STRAIGHT = 6;   // 每条直边的分段
  const LOOP_ARC = 8;        // 每个圆角的分段
  const GROUND_HALF = 4000;  // 地面半宽（世界 px）

  /**
   * 圆角矩形的边界：逆时针一圈，返回扁平数组 [x, y, nx, ny, ...]。
   * 法向朝外，与顶点一一对应 —— 侧壁直接用它当法线。
   */
  function loopPoints(hx, hy, R) {
    hx = Math.max(hx, 0.01);
    hy = Math.max(hy, 0.01);
    const r = Math.min(Math.max(R, 0), Math.min(hx, hy));
    const ex = hx - r, ey = hy - r;
    const pts = [];
    const straight = (x0, y0, x1, y1, nx, ny) => {
      for (let i = 0; i < LOOP_STRAIGHT; i++) {
        const t = i / LOOP_STRAIGHT;
        pts.push(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, nx, ny);
      }
    };
    const arc = (cx, cy, a0, a1) => {
      const n = r > 0.05 ? LOOP_ARC : 0;
      for (let i = 0; i < n; i++) {
        const a = a0 + ((a1 - a0) * i) / n;
        pts.push(cx + r * Math.cos(a), cy + r * Math.sin(a), Math.cos(a), Math.sin(a));
      }
    };
    straight(hx, -ey, hx, ey, 1, 0);
    arc(ex, ey, 0, Math.PI / 2);
    straight(ex, hy, -ex, hy, 0, 1);
    arc(-ex, ey, Math.PI / 2, Math.PI);
    straight(-hx, ey, -hx, -ey, -1, 0);
    arc(-ex, -ey, Math.PI, Math.PI * 1.5);
    straight(-ex, -hy, ex, -hy, 0, -1);
    arc(ex, -ey, Math.PI * 1.5, Math.PI * 2);
    return pts;
  }

  /**
   * 把"圆角矩形轮廓 × 圆角剖面"的解析形状三角化成网格。
   *
   *   mode 0（凸起）剖面：z 0 → depth−b 竖直侧壁，再 1/4 圆环收到顶面
   *   mode 1（凹陷）剖面：地面外扩 b → 坑口圆角 → 竖壁 → 底圆角 → 坑底
   *
   * 所有三角形都按"给定参考法线"校正绕序，所以朝内朝外的面都不会写反。
   */
  function buildMesh(S) {
    const pos = [];
    const nrm = [];
    const idx = [];
    const addV = (x, y, z, nx, ny, nz) => {
      pos.push(x, y, z);
      nrm.push(nx, ny, nz);
      return pos.length / 3 - 1;
    };
    const tri = (a, b, c, nx, ny, nz) => {
      const ux = pos[b * 3] - pos[a * 3], uy = pos[b * 3 + 1] - pos[a * 3 + 1];
      const uz = pos[b * 3 + 2] - pos[a * 3 + 2];
      const vx = pos[c * 3] - pos[a * 3], vy = pos[c * 3 + 1] - pos[a * 3 + 1];
      const vz = pos[c * 3 + 2] - pos[a * 3 + 2];
      const cx = uy * vz - uz * vy, cy = uz * vx - ux * vz, cz = ux * vy - uy * vx;
      if (cx * nx + cy * ny + cz * nz < 0) idx.push(a, c, b);
      else idx.push(a, b, c);
    };

    const d = Math.max(S.depth, 0.05);
    const hx = Math.max(S.hx, 0.05), hy = Math.max(S.hy, 0.05);
    const r = Math.min(Math.max(S.radius, 0), Math.min(hx, hy));
    const b = Math.max(0.01, Math.min(S.bevel, 0.999 * Math.min(hx, hy, d / 2)));
    const W = GROUND_HALF;
    const base = loopPoints(hx, hy, r);
    const nB = base.length / 4;

    /* ── 地面 ───────────────────────────────────────────────────────────── */
    if (S.mode === 0) {
      const a = addV(-W, -W, 0, 0, 0, 1), c = addV(W, -W, 0, 0, 0, 1);
      const e = addV(W, W, 0, 0, 0, 1), f = addV(-W, W, 0, 0, 0, 1);
      tri(a, c, e, 0, 0, 1);
      tri(a, e, f, 0, 0, 1);
    } else {
      // 凹陷：地面要在坑口挖个洞，环带从"坑口外扩 b"投到外框
      const inner = loopPoints(hx + b, hy + b, r + b);
      const nI = inner.length / 4;
      const inIds = [], outIds = [];
      for (let i = 0; i < nI; i++) {
        const x = inner[i * 4], y = inner[i * 4 + 1];
        const nx = inner[i * 4 + 2], ny = inner[i * 4 + 3];
        inIds.push(addV(x, y, 0, 0, 0, 1));
        let t = Infinity;
        if (nx > 1e-4) t = Math.min(t, (W - x) / nx);
        if (nx < -1e-4) t = Math.min(t, (-W - x) / nx);
        if (ny > 1e-4) t = Math.min(t, (W - y) / ny);
        if (ny < -1e-4) t = Math.min(t, (-W - y) / ny);
        if (!isFinite(t)) t = 1;
        outIds.push(addV(x + nx * t * 1.002, y + ny * t * 1.002, 0, 0, 0, 1));
      }
      for (let i = 0; i < nI; i++) {
        const j = (i + 1) % nI;
        tri(inIds[i], inIds[j], outIds[j], 0, 0, 1);
        tri(inIds[i], outIds[j], outIds[i], 0, 0, 1);
      }
    }
    const groundIdx = idx.length;

    /* ── 元件 ───────────────────────────────────────────────────────────── */
    const RING = 6;   // 圆角剖面的分段

    /// 沿剖面扫一圈：f(k) → [内缩量 o, 高度 z, 法线倾斜角 φ]
    const sweep = (rings, flip) => {
      for (let k = 0; k < rings.length - 1; k++) {
        const A = rings[k], B = rings[k + 1];
        const phi = rings[k].phi;
        for (let i = 0; i < nB; i++) {
          const j = (i + 1) % nB;
          const nx = base[i * 4 + 2] * Math.cos(phi) * flip;
          const ny = base[i * 4 + 3] * Math.cos(phi) * flip;
          const nz = Math.sin(phi);
          tri(A.ids[i], A.ids[j], B.ids[j], nx, ny, nz);
          tri(A.ids[i], B.ids[j], B.ids[i], nx, ny, nz);
        }
      }
    };

    if (S.mode === 0) {
      const zWall = Math.max(0, d - b);

      // 竖直侧壁
      if (zWall > 1e-3) {
        const lo = [], hi = [];
        for (let i = 0; i < nB; i++) {
          const x = base[i * 4], y = base[i * 4 + 1];
          const nx = base[i * 4 + 2], ny = base[i * 4 + 3];
          lo.push(addV(x, y, 0, nx, ny, 0));
          hi.push(addV(x, y, zWall, nx, ny, 0));
        }
        for (let i = 0; i < nB; i++) {
          const j = (i + 1) % nB;
          const nx = base[i * 4 + 2], ny = base[i * 4 + 3];
          tri(lo[i], lo[j], hi[j], nx, ny, 0);
          tri(lo[i], hi[j], hi[i], nx, ny, 0);
        }
      }

      // 顶倒角：从 (o=0, z=zWall) 卷到 (o=b, z=d)
      const rings = [];
      for (let k = 0; k <= RING; k++) {
        const phi = (k / RING) * Math.PI * 0.5;
        const o = b * (1 - Math.cos(phi));
        const z = zWall + b * Math.sin(phi);
        const ids = [];
        for (let i = 0; i < nB; i++) {
          ids.push(addV(
            base[i * 4] - base[i * 4 + 2] * o,
            base[i * 4 + 1] - base[i * 4 + 3] * o, z,
            base[i * 4 + 2] * Math.cos(phi), base[i * 4 + 3] * Math.cos(phi), Math.sin(phi),
          ));
        }
        ids.phi = phi;
        rings.push({ ids, phi });
      }
      sweep(rings, 1);

      // 顶面（扇形三角化）
      const tHx = hx - b, tHy = hy - b;
      if (tHx > 0.02 && tHy > 0.02) {
        const top = loopPoints(tHx, tHy, r - b);
        const nT = top.length / 4;
        const cid = addV(0, 0, d, 0, 0, 1);
        const tids = [];
        for (let i = 0; i < nT; i++) tids.push(addV(top[i * 4], top[i * 4 + 1], d, 0, 0, 1));
        for (let i = 0; i < nT; i++) tri(cid, tids[i], tids[(i + 1) % nT], 0, 0, 1);
      }
    } else {
      /* 凹陷：法线朝坑内，所以 flip = −1（绕序由 tri() 自动校正） */
      const zWall = -(d - b);

      // 坑口圆角：地面 (o=−b, z=0) → 坑壁 (o=0, z=−b)
      const rim = [];
      for (let k = 0; k <= RING; k++) {
        const phi = Math.PI * 0.5 * (1 - k / RING);   // π/2 → 0
        const o = -b + b * Math.cos(phi);
        const z = -b + b * Math.sin(phi);
        const ids = [];
        for (let i = 0; i < nB; i++) {
          ids.push(addV(
            base[i * 4] - base[i * 4 + 2] * o,
            base[i * 4 + 1] - base[i * 4 + 3] * o, z,
            -base[i * 4 + 2] * Math.cos(phi), -base[i * 4 + 3] * Math.cos(phi), Math.sin(phi),
          ));
        }
        rim.push({ ids, phi });
      }
      for (let k = 0; k < RING; k++) {
        const A = rim[k].ids, B = rim[k + 1].ids, phi = rim[k].phi;
        for (let i = 0; i < nB; i++) {
          const j = (i + 1) % nB;
          tri(A[i], A[j], B[j],
            -base[i * 4 + 2] * Math.cos(phi), -base[i * 4 + 3] * Math.cos(phi), Math.sin(phi));
          tri(A[i], B[j], B[i],
            -base[i * 4 + 2] * Math.cos(phi), -base[i * 4 + 3] * Math.cos(phi), Math.sin(phi));
        }
      }

      // 坑壁
      if (Math.abs(zWall) > b + 1e-3) {
        const lo = [], hi = [];
        for (let i = 0; i < nB; i++) {
          const x = base[i * 4], y = base[i * 4 + 1];
          lo.push(addV(x, y, -b, -base[i * 4 + 2], -base[i * 4 + 3], 0));
          hi.push(addV(x, y, zWall, -base[i * 4 + 2], -base[i * 4 + 3], 0));
        }
        for (let i = 0; i < nB; i++) {
          const j = (i + 1) % nB;
          tri(lo[i], lo[j], hi[j], -base[i * 4 + 2], -base[i * 4 + 3], 0);
          tri(lo[i], hi[j], hi[i], -base[i * 4 + 2], -base[i * 4 + 3], 0);
        }
      }

      // 坑底圆角：坑壁底 → 坑底
      const foot = [];
      for (let k = 0; k <= RING; k++) {
        const phi = (k / RING) * Math.PI * 0.5;
        const o = b * (1 - Math.cos(phi));
        const z = zWall - b * Math.sin(phi);
        const ids = [];
        for (let i = 0; i < nB; i++) {
          ids.push(addV(
            base[i * 4] - base[i * 4 + 2] * o,
            base[i * 4 + 1] - base[i * 4 + 3] * o, z,
            -base[i * 4 + 2] * Math.cos(phi), -base[i * 4 + 3] * Math.cos(phi), Math.sin(phi),
          ));
        }
        foot.push({ ids, phi });
      }
      for (let k = 0; k < RING; k++) {
        const A = foot[k].ids, B = foot[k + 1].ids, phi = foot[k].phi;
        for (let i = 0; i < nB; i++) {
          const j = (i + 1) % nB;
          tri(A[i], A[j], B[j],
            -base[i * 4 + 2] * Math.cos(phi), -base[i * 4 + 3] * Math.cos(phi), Math.sin(phi));
          tri(A[i], B[j], B[i],
            -base[i * 4 + 2] * Math.cos(phi), -base[i * 4 + 3] * Math.cos(phi), Math.sin(phi));
        }
      }

      // 坑底（扇形，法线朝上）
      const bHx = hx - b, bHy = hy - b;
      if (bHx > 0.02 && bHy > 0.02) {
        const bot = loopPoints(bHx, bHy, r - b);
        const nT = bot.length / 4;
        const cid = addV(0, 0, -d, 0, 0, 1);
        const tids = [];
        for (let i = 0; i < nT; i++) tids.push(addV(bot[i * 4], bot[i * 4 + 1], -d, 0, 0, 1));
        for (let i = 0; i < nT; i++) tri(cid, tids[i], tids[(i + 1) % nT], 0, 0, 1);
      }
    }

    return {
      pos: new Float32Array(pos),
      nrm: new Float32Array(nrm),
      idx: new Uint32Array(idx),
      groundIdx,
      tris: idx.length / 3,
    };
  }

  /* ═════════════════════════ 渲染器 ═════════════════════════ */

  function compile(gl, type, src) {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
      console.error("着色器编译失败:\n" + gl.getShaderInfoLog(s) + "\n" + src);
      gl.deleteShader(s);
      return null;
    }
    return s;
  }

  function program(gl, vsSrc, fsSrc) {
    const vs = compile(gl, gl.VERTEX_SHADER, vsSrc);
    const fs = compile(gl, gl.FRAGMENT_SHADER, fsSrc);
    if (!vs || !fs) return null;
    const p = gl.createProgram();
    gl.attachShader(p, vs);
    gl.attachShader(p, fs);
    gl.bindAttribLocation(p, 0, "aPos");
    gl.bindAttribLocation(p, 1, "aNrm");
    gl.linkProgram(p);
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
      console.error("着色器链接失败: " + gl.getProgramInfoLog(p));
      return null;
    }
    return p;
  }

  const MAX_PIXELS = 3.6e6;   // 内部渲染像素上限（超采样后的）

  function create(canvas, gizmo, opts) {
    opts = opts || {};
    const gl = canvas.getContext("webgl2", {
      alpha: false, antialias: false, depth: false, stencil: false,
      premultipliedAlpha: false, preserveDrawingBuffer: false,
      powerPreference: "high-performance",
    });
    if (!gl) return null;

    const renderer = (() => {
      const ext = gl.getExtension("WEBGL_debug_renderer_info");
      return ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
    })();
    const software = /swiftshader|llvmpipe|software|basic render/i.test(String(renderer));

    const progBody = program(gl, VS_BODY, FS_BODY);
    const progPost = program(gl, VS_POST, FS_POST);
    if (!progBody || !progPost) return null;

    /* 顶点缓冲 / 索引缓冲 / VAO（交错：pos3 + nrm3） */
    const vbo = gl.createBuffer();
    const ibo = gl.createBuffer();
    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, 24, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 3, gl.FLOAT, false, 24, 12);
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, ibo);

    /* 后处理用的全屏三角形 */
    const quadVbo = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, quadVbo);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const quadVao = gl.createVertexArray();
    gl.bindVertexArray(quadVao);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    const U = (p, names) => {
      const o = {};
      names.forEach((n) => { o[n] = gl.getUniformLocation(p, n); });
      return o;
    };
    const uB = U(progBody, ["uMVP", "uEyeDir", "uCamRight", "uCamUp", "uObjCol", "uPlaneCol",
      "uBgCol", "uGridCol", "uGround", "uWire", "uGridAmt", "uDepth"]);
    const uP = U(progPost, ["uColor", "uNormal", "uDepth", "uRes", "uDepthRange", "uLine", "uEdge"]);

    /* 场景 / 相机状态 */
    const scene = {
      mode: 0, hx: 130, hy: 75, radius: 28, bevel: 12, depth: 20,
      albedo: [0.745, 0.745, 0.745],
      plane: [0.56, 0.56, 0.56], bg: [0.48, 0.48, 0.48], line: [0.22, 0.22, 0.22],
      gridCol: [0.45, 0.45, 0.45], grid: 1, edge: 1, wire: 0,
      orthoH: 520,
    };
    const cam = { az: 0, el: 0, zoom: 1 };

    let aspect = 900 / 520;
    let geoKey = "";
    let geo = null;

    let iw = 0, ih = 0;
    let quality = software ? 0.75 : 1;
    let frameMs = 16;
    let frames = 0;
    let dirty = true;
    let running = false;
    let visible = true;
    let autoRotate = false;
    let raf = 0, statAt = 0;

    /* ── FBO：颜色 + 法线 + 深度 ── */
    const floatOk = !!gl.getExtension("EXT_color_buffer_float");
    const colFmt = floatOk ? gl.RGBA16F : gl.RGBA8;
    const colType = floatOk ? gl.HALF_FLOAT : gl.UNSIGNED_BYTE;
    let fbo = null, texCol = null, texNrm = null, texDep = null;

    function dropTargets() {
      if (texCol) gl.deleteTexture(texCol);
      if (texNrm) gl.deleteTexture(texNrm);
      if (texDep) gl.deleteTexture(texDep);
      if (fbo) gl.deleteFramebuffer(fbo);
      texCol = texNrm = texDep = fbo = null;
    }

    function makeTargets() {
      dropTargets();
      if (iw <= 0 || ih <= 0) return;
      const mk = (fmt, type, internal, attach) => {
        const t = gl.createTexture();
        gl.bindTexture(gl.TEXTURE_2D, t);
        gl.texImage2D(gl.TEXTURE_2D, 0, internal, iw, ih, 0, fmt, type, null);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
        return t;
      };
      texCol = mk(gl.RGBA, colType, colFmt);
      texNrm = mk(gl.RGBA, gl.UNSIGNED_BYTE, gl.RGBA8);
      texDep = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, texDep);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.DEPTH_COMPONENT24, iw, ih, 0,
        gl.DEPTH_COMPONENT, gl.UNSIGNED_INT, null);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

      fbo = gl.createFramebuffer();
      gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, texCol, 0);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT1, gl.TEXTURE_2D, texNrm, 0);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.DEPTH_ATTACHMENT, gl.TEXTURE_2D, texDep, 0);
      gl.drawBuffers([gl.COLOR_ATTACHMENT0, gl.COLOR_ATTACHMENT1]);
      const st = gl.checkFramebufferStatus(gl.FRAMEBUFFER);
      if (st !== gl.FRAMEBUFFER_COMPLETE) console.warn("FBO 不完整: 0x" + st.toString(16));
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    }

    /* ── 布局：保持与渲染画布一致的纵横比 ── */
    function layout() {
      const host = canvas.parentElement;
      if (!host) return false;
      const cs = getComputedStyle(host);
      const availW = Math.max(120, host.clientWidth -
        parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight));
      const availH = Math.max(90, host.clientHeight -
        parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom));
      let cw = availW, ch = cw / aspect;
      if (ch > availH) { ch = availH; cw = ch * aspect; }
      canvas.style.width = Math.round(cw) + "px";
      canvas.style.height = Math.round(ch) + "px";

      // 超采样：2× 交给真显卡，软件渲染只跑 1×（但要留 dpr 上限）
      const dpr = Math.min(global.devicePixelRatio || 1, 2);
      const ss = software ? 1 : 2;
      let scale = dpr * ss * quality;
      let w = Math.max(2, Math.round(cw * scale));
      let h = Math.max(2, Math.round(ch * scale));
      if (w * h > MAX_PIXELS) {
        const k = Math.sqrt(MAX_PIXELS / (w * h));
        w = Math.max(2, Math.round(w * k));
    function ensureGeo() {
      const key = [scene.mode, scene.hx, scene.hy, scene.radius, scene.bevel, scene.depth]
        .map((v) => Number(v).toFixed(3)).join("|");
      if (key === geoKey && geo) return;
      geo = buildMesh(scene);
      geoKey = key;
      gl.bindVertexArray(vao);
      gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
      gl.bufferData(gl.ARRAY_BUFFER, interleave(geo.pos, geo.nrm), gl.STATIC_DRAW);
      // 注意：ELEMENT_ARRAY_BUFFER 的绑定是 VAO 状态的一部分，必须在 VAO 绑定期间设置
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, ibo);
      gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, geo.idx, gl.STATIC_DRAW);
      gl.bindVertexArray(null);
    }
      const oh = Math.max(scene.orthoH, 8);
      const ow = oh * aspect;
      const dist = 6 * oh + 2000;
      const eye = [eyeDir[0] * dist, eyeDir[1] * dist, eyeDir[2] * dist];
      const near = dist - 9000, far = dist + 9000;
      const proj = ortho(-ow / 2, ow / 2, -oh / 2, oh / 2, near, far);
      const view = lookAt(eye, [0, 0, 0], up);
      return { eyeDir, right, up, mvp: mul4(proj, view), near, far };
    }

    function ensureGeo() {
      const key = [scene.mode, scene.hx, scene.hy, scene.radius, scene.bevel, scene.depth]
        .map((v) => Number(v).toFixed(3)).join("|");
      if (key === geoKey && geo) return;
      geo = buildMesh(scene);
      geoKey = key;
      gl.bindVertexArray(vao);
      gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
      gl.bufferData(gl.ARRAY_BUFFER, geo.pos.length + geo.nrm.length > 0
        ? interleave(geo.pos, geo.nrm) : new Float32Array(0), gl.STATIC_DRAW);
      gl.bufferData(gl.ARRAY_BUFFER, 0, gl.STATIC_DRAW);   // 占位，下面立刻覆盖
      gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
      gl.bufferData(gl.ARRAY_BUFFER, interleave(geo.pos, geo.nrm), gl.STATIC_DRAW);
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, ibo);
      gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, geo.idx, gl.STATIC_DRAW);
      gl.bindVertexArray(null);
    }

    function interleave(p, n) {
      const out = new Float32Array(p.length + n.length);
      for (let i = 0, k = 0; i < p.length; i += 3, k += 6) {
        out[k] = p[i]; out[k + 1] = p[i + 1]; out[k + 2] = p[i + 2];
        out[k + 3] = n[i]; out[k + 4] = n[i + 1]; out[k + 5] = n[i + 2];
      }
      return out;
    }

    function render() {
      if (!iw || !ih || !fbo) return;
      ensureGeo();
      const C = camera();

      gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
      gl.viewport(0, 0, iw, ih);
      gl.clearColor(scene.bg[0], scene.bg[1], scene.bg[2], 1);
      gl.clearDepth(1);
      gl.enable(gl.DEPTH_TEST);
      gl.depthFunc(gl.LEQUAL);
      gl.enable(gl.CULL_FACE);
      gl.cullFace(gl.BACK);
      gl.disable(gl.BLEND);
      gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);

      gl.useProgram(progBody);
      gl.uniformMatrix4fv(uB.uMVP, false, C.mvp);
      gl.uniform3fv(uB.uEyeDir, C.eyeDir);
      gl.uniform3fv(uB.uCamRight, C.right);
      gl.uniform3fv(uB.uCamUp, C.up);
      gl.uniform3fv(uB.uObjCol, scene.albedo);
      gl.uniform3fv(uB.uPlaneCol, scene.plane);
      gl.uniform3fv(uB.uBgCol, scene.bg);
      gl.uniform3fv(uB.uGridCol, scene.gridCol);
      gl.uniform1f(uB.uGridAmt, scene.grid);
      gl.uniform1f(uB.uDepth, Math.max(scene.depth, 0.05));
      gl.bindVertexArray(vao);

      // 地面
      gl.uniform1f(uB.uGround, 1);
      gl.uniform1f(uB.uWire, 0);
      if (geo.groundIdx > 0) {
        gl.drawElements(gl.TRIANGLES, geo.groundIdx, gl.UNSIGNED_INT, 0);
      }

      // 元件（略微前移，避免与地面共面打架）
      gl.uniform1f(uB.uGround, 0);
      gl.uniform1f(uB.uWire, scene.wire);
      gl.enable(gl.POLYGON_OFFSET_FILL);
      gl.polygonOffset(1.0, 1.0);
      gl.drawElements(gl.TRIANGLES, geo.idx.length - geo.groundIdx, gl.UNSIGNED_INT, geo.groundIdx * 4);
      gl.disable(gl.POLYGON_OFFSET_FILL);
      gl.bindVertexArray(null);

      // 后处理：边缘检测 + sRGB
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
      gl.viewport(0, 0, iw, ih);
      gl.disable(gl.DEPTH_TEST);
      gl.disable(gl.CULL_FACE);
      gl.useProgram(progPost);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, texCol);
      gl.uniform1i(uP.uColor, 0);
      gl.activeTexture(gl.TEXTURE1);
      gl.bindTexture(gl.TEXTURE_2D, texNrm);
      gl.uniform1i(uP.uNormal, 1);
      gl.activeTexture(gl.TEXTURE2);
      gl.bindTexture(gl.TEXTURE_2D, texDep);
      gl.uniform1i(uP.uDepth, 2);
      gl.uniform2i(uP.uRes, iw, ih);
      gl.uniform1f(uP.uDepthRange, (C.far - C.near) * 0.5);
      gl.uniform3fv(uP.uLine, scene.line);
      gl.uniform1f(uP.uEdge, scene.edge);
      gl.bindVertexArray(quadVao);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      gl.bindVertexArray(null);
      frames++;
    }

    /* ── 坐标轴 gizmo（2D canvas，跟主视口共用角度） ── */
    function drawGizmo() {
      if (!gizmo) return;
      const g = gizmo.getContext("2d");
      const W = gizmo.width, H = gizmo.height;
      const cx = W / 2, cy = H / 2, R = Math.min(W, H) * 0.33;
      g.clearRect(0, 0, W, H);

      const el = cam.el, az = cam.az;
      const eye = [Math.sin(el) * Math.sin(az), Math.sin(el) * Math.cos(az), Math.cos(el)];
      const ref = Math.abs(eye[2]) > 0.999 ? [0, 1, 0] : [0, 0, 1];
      const cross = (a, b) => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
      const norm = (a) => { const l = Math.hypot(a[0], a[1], a[2]) || 1; return [a[0] / l, a[1] / l, a[2] / l]; };
      const u = norm(cross(ref, eye));
      const v = cross(eye, u);

      const axes = [
        { d: [1, 0, 0], col: "#c26a5f", name: "X" },
        { d: [0, 1, 0], col: "#5f9b63", name: "Y" },
        { d: [0, 0, 1], col: "#5b7fc7", name: "Z" },
      ].map((a) => {
        const sx = a.d[0] * u[0] + a.d[1] * u[1] + a.d[2] * u[2];
        const sy = a.d[0] * v[0] + a.d[1] * v[1] + a.d[2] * v[2];
        const depth = a.d[0] * eye[0] + a.d[1] * eye[1] + a.d[2] * eye[2];
        return { ...a, sx, sy, depth };
      }).sort((a, b) => a.depth - b.depth);

      g.font = "9px ui-monospace, monospace";
      g.textAlign = "center";
      g.textBaseline = "middle";
      axes.forEach((a) => {
        const k = 1 - 0.18 * (-a.depth);
        const x = cx + a.sx * R * k;
        const y = cy - a.sy * R * k;
        const alpha = 0.35 + 0.65 * (a.depth * 0.5 + 0.5);
        g.globalAlpha = alpha;
        g.strokeStyle = a.col;
        g.lineWidth = 1.4;
        g.beginPath();
        g.moveTo(cx, cy);
        g.lineTo(x, y);
        g.stroke();
        g.fillStyle = a.col;
        g.beginPath();
        g.arc(x, y, 2.2, 0, 6.2832);
        g.fill();
        g.fillText(a.name, x + (x - cx) * 0.22, y + (y - cy) * 0.22);
        g.globalAlpha = 1;
      });
      g.globalAlpha = 1;
    }

    function loop() {
      raf = requestAnimationFrame(loop);
      if (!visible || !iw) return;
      if (autoRotate && !dragging) {
        cam.az += (0.35 * Math.PI) / 180;
        if (cam.az > Math.PI) cam.az -= 2 * Math.PI;
        dirty = true;
        pushCam();
        drawGizmo();
      }
      if (!dirty) return;
      const t0 = performance.now();
      render();
      const dt = performance.now() - t0;
      frameMs = frameMs * 0.7 + dt * 0.3;
      dirty = false;

      const now = performance.now();
      if (opts.onStats && now - statAt > 240) {
        statAt = now;
        opts.onStats({
          fps: frameMs > 0 ? 1000 / frameMs : 0,
          samples: frames, maxSpp: frames,
          tris: geo ? geo.tris : 0,
          quality, iw, ih, software, floatOk, renderer,
        });
      }
    }

    /* ── 交互：拖动环绕 / 滚轮缩放 / 双击回正视 ── */
    let dragging = false;
    let lastX = 0, lastY = 0;

    function pushCam() {
      if (opts.onCam) opts.onCam({ az: cam.az, el: cam.el, zoom: cam.zoom });
    }

    function setCam(az, el) {
      cam.az = az;
      cam.el = el;
      dirty = true;
      drawGizmo();
    }

    function setZoom(z) {
      cam.zoom = z;
      dirty = true;
    }

    canvas.addEventListener("pointerdown", (e) => {
      if (e.button !== 0 && e.pointerType === "mouse") return;
      dragging = true;
      lastX = e.clientX;
      lastY = e.clientY;
      canvas.setPointerCapture(e.pointerId);
      canvas.classList.add("grabbing");
      e.preventDefault();
    });

    canvas.addEventListener("pointermove", (e) => {
      if (!dragging) return;
      const dx = e.clientX - lastX;
      const dy = e.clientY - lastY;
      lastX = e.clientX;
      lastY = e.clientY;
      // 抓取式：内容跟着指针走（拖右 → 相机往左绕）
      cam.az -= (dx * 0.45 * Math.PI) / 180;
      if (cam.az > Math.PI) cam.az -= 2 * Math.PI;
      if (cam.az < -Math.PI) cam.az += 2 * Math.PI;
      const maxEl = (86 * Math.PI) / 180;
      cam.el = Math.min(maxEl, Math.max(0, cam.el + (dy * 0.45 * Math.PI) / 180));
      dirty = true;
      pushCam();
      drawGizmo();
    });

    const endDrag = (e) => {
      if (!dragging) return;
      dragging = false;
      canvas.classList.remove("grabbing");
      if (e && e.pointerId !== undefined && canvas.hasPointerCapture(e.pointerId)) {
        canvas.releasePointerCapture(e.pointerId);
      }
    };
    canvas.addEventListener("pointerup", endDrag);
    canvas.addEventListener("pointercancel", endDrag);

    canvas.addEventListener("wheel", (e) => {
      e.preventDefault();
      const z = Math.exp(-e.deltaY * 0.0012);
      cam.zoom = Math.min(3, Math.max(0.45, cam.zoom * z));
      dirty = true;
      pushCam();
    }, { passive: false });

    canvas.addEventListener("dblclick", () => {
      cam.az = 0;
      cam.el = 0;
      cam.zoom = 1;
      dirty = true;
      pushCam();
      drawGizmo();
    });

    canvas.addEventListener("contextmenu", (e) => e.preventDefault());

    return {
      setScene(p) {
        let changed = false;
        for (const k in p) {
          if (!(k in scene)) continue;
          const a = scene[k], b = p[k];
          if (Array.isArray(a)) {
            if (a[0] !== b[0] || a[1] !== b[1] || a[2] !== b[2]) { scene[k] = b.slice(); changed = true; }
          } else if (a !== b) { scene[k] = b; changed = true; }
        }
        if (changed) dirty = true;
        if (p.aspect && Math.abs(p.aspect - aspect) > 1e-6) { aspect = p.aspect; layout(); }
      },
      setCam,
      setZoom(z) { setZoom(z); },
      getCam() { return { az: cam.az, el: cam.el, zoom: cam.zoom }; },
      setAutoRotate(on) { autoRotate = !!on; },
      setVisible(v) { visible = !!v; if (v) { layout(); dirty = true; } },
      resize() { layout(); drawGizmo(); },
      invalidate() { dirty = true; },
      start() {
        if (running) return;
        running = true;
        layout();
        drawGizmo();
        raf = requestAnimationFrame(loop);
      },
      stop() {
        running = false;
        cancelAnimationFrame(raf);
        raf = 0;
      },
      setQuality(q) { quality = Math.min(1.5, Math.max(0.25, q)); layout(); },
      stats() {
        return {
          fps: frameMs > 0 ? 1000 / frameMs : 0,
          samples: frames, maxSpp: frames,
          tris: geo ? geo.tris : 0, verts: geo ? geo.pos.length / 3 : 0,
          quality, iw, ih, software, floatOk, renderer,
        };
      },
      dispose() {
        this.stop();
        dropTargets();
        gl.deleteProgram(progBody);
        gl.deleteProgram(progPost);
        gl.deleteBuffer(vbo);
        gl.deleteBuffer(ibo);
        gl.deleteBuffer(quadVbo);
      },
    };
  }

  global.View3D = { create };
})(window);
