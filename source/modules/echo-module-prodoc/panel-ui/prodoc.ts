//! ProDoc 扩展的**纯函数层**：把面板的模块事件流解析成可渲染的状态。
//!
//! 单独成文件是为了可测：这些函数不碰 Vue、不碰 socket，输入是
//! `state.moduleEvents` 那样的数组，输出是幂等的状态补丁。副作用全留在
//! `index.ts`（写 reactive）与组件（渲染）里。
//!
//! ## 为什么不给线协议加事件变体
//!
//! ProDoc 的三类载荷（快照 / 根列表 / 目录层）都走 `ModuleEvent` 这条
//! **通用**通道：模块 `emit` 一个名字 + 载荷，宿主不认识就原样包成
//! `BackendEvent::ModuleEvent` 推给面板。加专用变体意味着改两端的类型定义、
//! 更新协议同步测试，而收益只是"类型更窄"——对三个事件不值得。

/** 面板 `state.moduleEvents` 里的一条（结构见 `ModuleEventEntry`）。 */
export interface ModuleEventLike {
  module: string;
  name: string;
  payload?: unknown;
  at_ms?: number;
  /** 多上游聚合下来源 Core（空串 = 未知）。 */
  core?: string;
}

/** 一份文档群快照（`prodoc.bundle`）。 */
export interface ProDocBundleEvent {
  team_id?: string | null;
  core?: string;
  root: string;
  /** 相对路径 → 完整原文。 */
  files: Record<string, string>;
  /** 被跳过的条目（超限 / 非 UTF-8 / 读失败）。 */
  skipped?: Array<{ path: string; reason: string }>;
  stats?: {
    documents?: number;
    bytes?: number;
    skipped?: number;
    truncated?: boolean;
  };
}

/** 一次失败（`prodoc.error`）。 */
export interface ProDocErrorEvent {
  team_id?: string | null;
  core?: string;
  root: string;
  error: string;
}

/** 可打开的根（`prodoc.roots`）。 */
export interface ProDocRootsEvent {
  roots: Array<{ label: string; path: string }>;
  caps?: {
    max_files?: number;
    max_file_bytes?: number;
    max_total_bytes?: number;
  };
}

/** 目录选择器的一层（`prodoc.dirs`）。 */
export interface ProDocDirsEvent {
  path?: string | null;
  roots?: Array<{ label: string; path: string }>;
  entries: Array<{ name: string; path: string; is_dir: boolean }>;
}

export const BUNDLE_EVENT = 'prodoc.bundle';
export const ERROR_EVENT = 'prodoc.error';
export const ROOTS_EVENT = 'prodoc.roots';
export const DIRS_EVENT = 'prodoc.dirs';
/** 单文件读取回执。 */
export const FILE_EVENT = 'prodoc.file';
/** 保存回执（成功 / 冲突）。 */
export const SAVED_EVENT = 'prodoc.saved';
/** 删除回执。 */
export const DELETED_EVENT = 'prodoc.deleted';

/** 写操作回执（三条写命令共用的形状，见模块的 `WriteAck`）。 */
export interface WriteAckEvent {
  request_id?: string | null;
  team_id?: string | null;
  root: string;
  path: string;
  ok: boolean;
  error?: string | null;
  /** 需要前端合并（磁盘内容既不是 base 也不是本次提交内容）。 */
  conflict?: boolean;
  /** 磁盘当前内容；`null` = 文件不存在。 */
  disk?: string | null;
  /** 读取结果（仅 `prodoc.file`）。 */
  content?: string | null;
}

/**
 * 状态键：`core|team|root`。
 *
 * 三段都必要：面板能同时连多个 Core（多上游聚合），同一个 Core 下多个 team
 * 也可能配了同名目录。少任何一段都会让"切了上游/人格、画布还是旧内容"。
 * 空值统一写成空串，避免 `null` / `undefined` 两种写法产生两条键。
 */
export function bundleKeyOf(entry: {
  team_id?: string | null;
  core?: string | null;
  root: string;
}): string {
  return `${entry.core ?? ''}|${entry.team_id ?? ''}|${entry.root}`;
}

/** 从事件流里抽出各类 ProDoc 事件（**新的在前**）。 */
export function collectProDocEvents(events: readonly ModuleEventLike[]): {
  bundles: ProDocBundleEvent[];
  errors: ProDocErrorEvent[];
  roots: ProDocRootsEvent | null;
  dirs: ProDocDirsEvent | null;
  /** 写回执（**不去重**：每个 request_id 各是一份，越界去重会吞掉连续保存）。 */
  acks: WriteAckEvent[];
} {
  const bundles: ProDocBundleEvent[] = [];
  const seenBundleKeys = new Set<string>();
  const errors: ProDocErrorEvent[] = [];
  const seenErrorKeys = new Set<string>();
  let roots: ProDocRootsEvent | null = null;
  let dirs: ProDocDirsEvent | null = null;
  const acks: WriteAckEvent[] = [];

  // 事件数组是「新的在前」（state.ts 用 unshift + slice）。所以：
  // - 同一个 root 的多份快照，**第一条**（最新）才是要用的；
  // - 根列表 / 目录层同理取第一条。
  for (const event of events) {
    if (event.module !== 'echo-agent.prodoc') continue;
    const payload = (event.payload ?? {}) as Record<string, unknown>;
    switch (event.name) {
      case BUNDLE_EVENT: {
        const bundle = normalizeBundle(payload, event.core ?? '');
        if (!bundle) break;
        const key = bundleKeyOf(bundle);
        if (seenBundleKeys.has(key)) break;
        seenBundleKeys.add(key);
        bundles.push(bundle);
        break;
      }
      case ERROR_EVENT: {
        const error = normalizeError(payload, event.core ?? '');
        if (!error) break;
        const key = bundleKeyOf(error);
        if (seenErrorKeys.has(key)) break;
        seenErrorKeys.add(key);
        errors.push(error);
        break;
      }
      case ROOTS_EVENT:
        roots = roots ?? normalizeRoots(payload);
        break;
      case DIRS_EVENT:
        dirs = dirs ?? normalizeDirs(payload);
        break;
      case FILE_EVENT:
      case SAVED_EVENT:
      case DELETED_EVENT: {
        const ack = normalizeAck(payload);
        if (ack) acks.push(ack);
        break;
      }
      default:
        break;
    }
  }
  return { bundles, errors, roots, dirs, acks };
}

/** 写回执归一：`root`/`path` 缺失的一律丢弃（无法定位到具体文件）。 */
function normalizeAck(payload: Record<string, unknown>): WriteAckEvent | null {
  const root = typeof payload.root === 'string' ? payload.root : '';
  const path = typeof payload.path === 'string' ? payload.path : '';
  if (!root || !path) return null;
  return {
    request_id: typeof payload.request_id === 'string' ? payload.request_id : null,
    team_id: typeof payload.team_id === 'string' ? payload.team_id : null,
    root,
    path,
    ok: payload.ok === true,
    error: typeof payload.error === 'string' ? payload.error : null,
    conflict: payload.conflict === true,
    disk: typeof payload.disk === 'string' ? payload.disk : null,
    content: typeof payload.content === 'string' ? payload.content : null,
  };
}

/** 载荷校验：形状不对的事件直接丢掉（模块与扩展版本漂移时不该崩面板）。 */
function normalizeBundle(payload: Record<string, unknown>, core: string): ProDocBundleEvent | null {
  const root = typeof payload.root === 'string' ? payload.root : '';
  if (!root) return null;
  const files: Record<string, string> = {};
  const rawFiles = payload.files;
  if (rawFiles && typeof rawFiles === 'object') {
    for (const [path, content] of Object.entries(rawFiles as Record<string, unknown>)) {
      if (typeof content === 'string') files[path] = content;
    }
  }
  return {
    team_id: typeof payload.team_id === 'string' ? payload.team_id : null,
    core,
    root,
    files,
    skipped: Array.isArray(payload.skipped)
      ? (payload.skipped as Array<{ path: string; reason: string }>)
      : [],
    stats: (payload.stats as ProDocBundleEvent['stats']) ?? undefined,
  };
}

function normalizeError(payload: Record<string, unknown>, core: string): ProDocErrorEvent | null {
  const root = typeof payload.root === 'string' ? payload.root : '';
  const error = typeof payload.error === 'string' ? payload.error : '';
  if (!root || !error) return null;
  return {
    team_id: typeof payload.team_id === 'string' ? payload.team_id : null,
    core,
    root,
    error,
  };
}

function normalizeRoots(payload: Record<string, unknown>): ProDocRootsEvent {
  return {
    roots: Array.isArray(payload.roots) ? (payload.roots as ProDocRootsEvent['roots']) : [],
    caps: (payload.caps as ProDocRootsEvent['caps']) ?? undefined,
  };
}

function normalizeDirs(payload: Record<string, unknown>): ProDocDirsEvent {
  return {
    path: typeof payload.path === 'string' ? payload.path : null,
    roots: Array.isArray(payload.roots) ? (payload.roots as ProDocDirsEvent['roots']) : [],
    entries: Array.isArray(payload.entries) ? (payload.entries as ProDocDirsEvent['entries']) : [],
  };
}

/**
 * 一次扫描的摘要文案（界面上那一行"12 篇 · 184 KB"）。
 *
 * 单独抽出来是因为它有三个容易写错的点：字节要人读的单位、超限必须显式
 * 说出来（否则用户以为文档少了是自己写错）、跳过数要能在标题里看见。
 */
export function describeBundle(entry: {
  docs: number;
  bytes: number;
  skipped: Array<{ path: string; reason: string }>;
  truncated: boolean;
}): string {
  const parts = [`${entry.docs} 篇`, formatBytes(entry.bytes)];
  if (entry.skipped.length > 0) {
    parts.push(`跳过 ${entry.skipped.length}`);
  }
  if (entry.truncated) {
    parts.push('已达上限');
  }
  return parts.join(' · ');
}

/** 字节 → 人读（KB/MB，一位小数；小于 1 KB 用字节）。 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * 目录层级：把一条绝对路径拆成面包屑（含根列表的"起点"）。
 *
 * 只用于显示与"上一级"跳转——真正的越权判定在 Core（`check_allowed`），
 * 前端不做安全判断，只做导航。
 */
export function breadcrumbs(path: string | null, roots: Array<{ label: string; path: string }>): Array<{ label: string; path: string }> {
  if (!path) return [];
  const root = roots.find((item) => path === item.path || path.startsWith(`${item.path}/`));
  const base = root ? root.path : '';
  const label = root ? root.label : '';
  const rest = path.slice(base.length).split('/').filter(Boolean);
  const crumbs: Array<{ label: string; path: string }> = [];
  if (root) crumbs.push({ label, path: base });
  let accumulated = base;
  for (const part of rest) {
    accumulated = `${accumulated}/${part}`;
    crumbs.push({ label: part, path: accumulated });
  }
  return crumbs;
}

/** 上一级路径（已在根时返回 null = 回到根列表）。 */
export function parentPath(path: string | null, roots: Array<{ label: string; path: string }>): string | null {
  if (!path) return null;
  const root = roots.find((item) => path === item.path || path.startsWith(`${item.path}/`));
  if (root && path === root.path) return null;
  const index = path.lastIndexOf('/');
  if (index <= 0) return null;
  const parent = path.slice(0, index);
  if (root && parent.length < root.path.length) return root.path;
  return parent;
}
