//! ProDoc 文档群模块的 Panel UI 扩展入口。
//!
//! 一个入口动作（「文档」）+ 一个 overlay：选文档根 → 拉快照 → 交给 ProDoc 的
//! `DocGraphViewer` 渲染。
//!
//! ## 数据怎么流动
//!
//! ```text
//! 扩展 sendCommand(RequestProDocBundle)
//!        └─► Core 命令通道 ─► prodoc 模块扫描目录
//!                                   │
//! 扩展读 state.moduleEvents ◄───────┘  ModuleEvent{prodoc.bundle}
//! ```
//!
//! **内容不走命令回执**：宿主 `apply_command` 只认 `Err`（发错误事件），
//! `Ok(value)` 的返回值被直接丢弃。所以三条命令的输出全部走模块事件——这条
//! 事实如果搞错，表现是"命令发了、界面永远转圈"，而且日志里一片安静。
//!
//! ## 为什么按 core+team+root 分键
//!
//! 面板能同时连多个 Core（多上游聚合），同一 Core 下多个 team 也可能配了同名
//! 目录。只按 root 分键会让它们互相覆盖——表现是"切了上游、画布还是上一个
//! Core 的文档"，极难从界面上看出来。

import { defineAsyncComponent, reactive, watch } from 'vue';
import {
  currentTeam,
  definePanelExtension,
  registerPanelExtension,
  sendCommand,
  state,
  teamModuleEnabled,
  type PanelAppContext,
} from '@echo-agent/panel';
// 画布组件**懒加载**：它把 ProDoc 的渲染器（布局 / 边路由 / 搜索 / 编辑器）
// 一起带进来，约 240 KB。面板首屏只需要一个「文档」按钮，没必要为没打开过
// 文档群的人付这份体积——所以走 `defineAsyncComponent`，rollup 会切成独立 chunk。
const ProDocPanel = defineAsyncComponent(() => import('./components/ProDocPanel.vue'));
import {
  type ProDocBundleEvent,
  type ProDocDirsEvent,
  bundleKeyOf,
  collectProDocEvents,
} from './prodoc';

/** 一个文档根的当前状态。 */
export interface ProDocEntry {
  /** 相对路径 → 全文（`DocGraphViewer` 要的形状）。 */
  files: Record<string, string>;
  /** 被跳过的条目（超限 / 非 UTF-8 / 读失败）。 */
  skipped: Array<{ path: string; reason: string }>;
  /** 错误文案（非空时不渲染画布，显示错误条）。 */
  error: string | null;
  docs: number;
  bytes: number;
  truncated: boolean;
  /** 已发命令、还没收到快照。 */
  loading: boolean;
  core: string;
}

/**
 * 扩展的全局状态。
 *
 * 放模块顶层而不是组件里：overlay 关闭再打开应该回到同一个文档根，而不是
 * 空白。但**不**持久化到 localStorage——文档以磁盘为准，缓存会在磁盘改动后
 * 显示旧内容，那比"重开要再点一次目录"糟得多。
 */
export const prodoc = reactive({
  /** 当前打开的文档根（null = 还没选）。 */
  root: null as string | null,
  /** `core|team|root` → 状态。 */
  entries: {} as Record<string, ProDocEntry>,
  /** 可打开的根（来自 `prodoc.roots`）。 */
  roots: [] as Array<{ label: string; path: string }>,
  /** 生效上限（界面上说明"最多 2000 篇"这类）。 */
  caps: { max_files: 0, max_file_bytes: 0, max_total_bytes: 0 },
  /** 目录选择器的当前层（null = 根列表）。 */
  browsePath: null as string | null,
  /** 目录选择器的条目。 */
  browseEntries: [] as Array<{ name: string; path: string; is_dir: boolean }>,
  /** 目录选择器里的根列表（浏览事件也带一份）。 */
  browseRoots: [] as Array<{ label: string; path: string }>,
});

/** 门控：模块启用（team 没关它）。 */
function available(teamId: string | null): boolean {
  const team = teamId === null
    ? currentTeam(state)
    : state.teams.find((item) => item.id === teamId) ?? null;
  return !!team && teamModuleEnabled(team, 'echo-agent.prodoc');
}

function emptyEntry(core: string): ProDocEntry {
  return {
    files: {},
    skipped: [],
    error: null,
    docs: 0,
    bytes: 0,
    truncated: false,
    loading: false,
    core,
  };
}

/** 当前 root 的键（没选 root 时 null）。 */
export function activeKey(teamId: string | null, core: string | null): string | null {
  if (!prodoc.root) return null;
  return bundleKeyOf({ team_id: teamId, core, root: prodoc.root });
}

/** 当前 entry（不存在时 null；调用方自己决定要不要空态）。 */
export function activeEntry(teamId: string | null, core: string | null): ProDocEntry | null {
  const key = activeKey(teamId, core);
  return key ? prodoc.entries[key] ?? null : null;
}

/** 打开某个文档根。 */
export function openRoot(teamId: string | null, core: string | null, root: string): void {
  const key = bundleKeyOf({ team_id: teamId, core, root });
  prodoc.entries[key] = { ...(prodoc.entries[key] ?? emptyEntry(core ?? '')), loading: true, error: null };
  prodoc.root = root;
  sendCommand({ RequestProDocBundle: { team_id: teamId, root } }, core ?? undefined);
}

/** 重新拉一次当前根（磁盘改过之后手动刷新用）。 */
export function refresh(teamId: string | null, core: string | null): void {
  if (prodoc.root) openRoot(teamId, core, prodoc.root);
}

/** 请求根列表。 */
export function requestRoots(core: string | null): void {
  sendCommand({ ProDocRoots: {} }, core ?? undefined);
}

/** 请求目录选择器的一层（`path = null` = 根列表）。 */
export function browse(core: string | null, path: string | null): void {
  sendCommand({ BrowseProDocDirs: { path } }, core ?? undefined);
}

function setup(context: PanelAppContext): void {
  // 模块事件是**追加**到 `state.moduleEvents` 头部的：watch 长度变化即可。
  // 每次都从头扫一遍（上限 200 条，代价可忽略），并按键去重取最新一份。
  watch(
    () => state.moduleEvents.length,
    () => {
      const { bundles, errors, roots, dirs } = collectProDocEvents(state.moduleEvents);
      for (const bundle of bundles) applyBundle(bundle);
      for (const error of errors) applyError(error);
      if (roots) {
        prodoc.roots = roots.roots;
        if (roots.caps) {
          prodoc.caps = {
            max_files: roots.caps.max_files ?? 0,
            max_file_bytes: roots.caps.max_file_bytes ?? 0,
            max_total_bytes: roots.caps.max_total_bytes ?? 0,
          };
        }
      }
      if (dirs) applyDirs(dirs);
    },
  );

  // 连上之后拉一次根列表：扩展加载早于连接完成是常态，切 Core 要重拉。
  watch(
    [() => state.connected, () => context.core.value],
    ([connected, core]) => {
      if (!connected) return;
      requestRoots(core ?? null);
      // 已选过 root 时顺手刷新一次：断线重连后画布不该停在旧内容。
      if (prodoc.root) openRoot(context.teamId.value ?? null, core ?? null, prodoc.root);
    },
    { immediate: true },
  );
}

function applyBundle(bundle: ProDocBundleEvent): void {
  const key = bundleKeyOf(bundle);
  const previous = prodoc.entries[key];
  prodoc.entries[key] = {
    files: bundle.files ?? {},
    skipped: bundle.skipped ?? [],
    error: null,
    docs: bundle.stats?.documents ?? Object.keys(bundle.files ?? {}).length,
    bytes: bundle.stats?.bytes ?? 0,
    truncated: !!bundle.stats?.truncated,
    loading: false,
    core: bundle.core ?? previous?.core ?? '',
  };
}

function applyError(error: { team_id?: string | null; core?: string; root: string; error: string }): void {
  const key = bundleKeyOf(error);
  const previous = prodoc.entries[key] ?? emptyEntry(error.core ?? '');
  // 失败时**清空 files**：留着上一次的内容会让人以为"目录打开了"，而实际
  // 上是旧快照——文档图的节点与磁盘对不上比空白更难排查。
  prodoc.entries[key] = { ...previous, files: {}, error: error.error, loading: false };
}

function applyDirs(dirs: ProDocDirsEvent): void {
  prodoc.browsePath = dirs.path ?? null;
  prodoc.browseEntries = dirs.entries;
  if (dirs.roots && dirs.roots.length > 0) prodoc.browseRoots = dirs.roots;
}

registerPanelExtension(definePanelExtension({
  id: 'echo-agent.prodoc',
  hooks: { setup },
  entries: [{
    id: 'docs',
    label: '文档',
    title: 'ProDoc 文档群',
    // `file-text` 是同义里最贴的（`folder` 已被工作区用掉）。
    icon: 'file-text',
    when: ({ teamId }) => available(teamId),
    prefetch: ({ core }) => requestRoots(core ?? null),
  }],
  overlays: [{ id: 'docs', label: 'ProDoc 文档群', component: ProDocPanel }],
}));
