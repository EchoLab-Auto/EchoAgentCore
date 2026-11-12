<script setup lang="ts">
//! ProDoc 文档群面板（overlay）。
//!
//! 两态：**选目录**（还没有文档根）与**看画布**（已打开）。
//!
//! ## P1 是只读的，而且是"明确拒绝"而不是"假装能编"
//!
//! `DocGraphViewer` 自带编辑入口。不给 `saveHandler` 时它会在点保存后报
//! "未提供写处理器"——那文案对用户是噪音。所以这里**显式**给一个总是拒绝
//! 的 handler，把话说清楚（"当前只读：写盘属 P2"），而不是让编辑按钮看起来能用。
//! 假成功更糟：用户以为存了，刷新后改动消失。
//!
//! ## 目录选择用两级
//!
//! 上面一行是配置好的根（一键进入），下面是逐层浏览（进子目录）。两级都要：
//! 只给根，遇到"文档在根下面三层"就得改 modules.toml；只给浏览器，每次都要
//! 从家目录一路点进去。

import { computed, onMounted } from 'vue';
import NeumorphismButton from '@echolab-auto/ui-frame/components/NeumorphismButton';
import { state } from '@echo-agent/panel';
import PanelIcon from '@echo-agent/panel/components/PanelIcon';
import { DocGraphViewer } from '../vendor/prodoc-renderer/index.js';
import '../vendor/prodoc-renderer/index.css';
import { activeEntry, browse, openRoot, prodoc, refresh, requestRoots } from '../index';
import { breadcrumbs, describeBundle, parentPath } from '../prodoc';

const props = defineProps<{ teamId: string | null; sessionId?: string }>();

const core = computed(() => state.activeRegion ?? null);
const entry = computed(() => activeEntry(props.teamId, core.value));
const roots = computed(() => prodoc.roots);
const browseRoots = computed(() => (prodoc.browseRoots.length > 0 ? prodoc.browseRoots : prodoc.roots));
const crumbs = computed(() => breadcrumbs(prodoc.browsePath, browseRoots.value));
const summary = computed(() => (entry.value ? describeBundle(entry.value) : ''));

/**
 * 写盘一律拒绝（P1）。
 *
 * 返回 `SaveResult` 而不是抛错：组件的 `normalizeSaveResult` 会把 `false`
 * 也归一成 `{ok:false}`，但不带原因的失败只会 toast 一句"保存失败"——用户
 * 不知道该去改配置还是等版本。
 */
function readOnlySave(): { ok: boolean; error: string } {
  return { ok: false, error: '文档当前为只读（写盘属 P2）：请用编辑器改文件，改完点右上角刷新' };
}

function pick(root: string): void {
  openRoot(props.teamId, core.value, root);
}

function up(): void {
  browse(core.value, parentPath(prodoc.browsePath, browseRoots.value));
}

function enter(path: string): void {
  browse(core.value, path);
}

function back(): void {
  prodoc.root = null;
  browse(core.value, null);
}

onMounted(() => {
  // 一进来就拉根列表：overlay 可能是被直接点开的（没经过入口按钮的 prefetch）。
  requestRoots(core.value);
  if (!prodoc.root) browse(core.value, null);
});
</script>

<template>
  <div class="prodoc-panel">
    <!-- ── 已打开：画布 ───────────────────────────────────────── -->
    <template v-if="prodoc.root && entry">
      <header class="prodoc-bar">
        <div class="prodoc-bar-main">
          <PanelIcon name="file-text" size="md" />
          <strong class="prodoc-root">{{ prodoc.root }}</strong>
          <span class="prodoc-summary">{{ summary }}</span>
        </div>
        <div class="prodoc-bar-actions">
          <NeumorphismButton variant="glass" shape="pill" size="small" @click="refresh(teamId, core)">
            刷新
          </NeumorphismButton>
          <NeumorphismButton variant="glass" shape="pill" size="small" @click="back">
            换目录
          </NeumorphismButton>
        </div>
      </header>

      <!-- 错误条：比空白画布有用得多 -->
      <div v-if="entry.error" class="prodoc-error">
        <PanelIcon name="circle-info" size="sm" />
        <span>{{ entry.error }}</span>
      </div>

      <!-- 被跳过的文件：不显示出来，用户会以为自己的 link 写错了 -->
      <details v-if="entry.skipped.length" class="prodoc-skipped">
        <summary>{{ entry.skipped.length }} 个文件未载入</summary>
        <ul>
          <li v-for="item in entry.skipped" :key="item.path">
            <code>{{ item.path }}</code> — {{ item.reason }}
          </li>
        </ul>
      </details>

      <div v-if="entry.loading" class="prodoc-loading">
        <PanelIcon name="circle-info" size="sm" />
        <span>正在扫描文档目录…</span>
      </div>

      <div v-else-if="!entry.error" class="prodoc-canvas">
        <DocGraphViewer
          :files="entry.files"
          :save-handler="readOnlySave"
        />
      </div>
    </template>

    <!-- ── 未打开：选目录 ─────────────────────────────────────── -->
    <template v-else>
      <section class="prodoc-picker">
        <h3>选择一个文档根</h3>
        <p class="prodoc-hint">
          文档根必须在 Core 的模块配置 <code>roots</code> 内（未配置时只允许家目录）。
          上限：{{ prodoc.caps.max_files || '—' }} 篇 /
          单文件 {{ Math.round((prodoc.caps.max_file_bytes || 0) / 1024) || '—' }} KB。
        </p>
        <div class="prodoc-roots">
          <NeumorphismButton
            v-for="root in roots"
            :key="root.path"
            variant="glass"
            shape="pill"
            size="small"
            :title="root.path"
            @click="pick(root.path)"
          >
            {{ root.label }}
          </NeumorphismButton>
          <span v-if="roots.length === 0" class="prodoc-empty">没有可用的文档根</span>
        </div>

        <div class="prodoc-crumbs">
          <button class="prodoc-crumb" @click="browse(core, null)">根列表</button>
          <template v-for="crumb in crumbs" :key="crumb.path">
            <span class="prodoc-sep">/</span>
            <button class="prodoc-crumb" @click="enter(crumb.path)">{{ crumb.label }}</button>
          </template>
          <NeumorphismButton
            v-if="prodoc.browsePath"
            variant="glass"
            shape="pill"
            size="small"
            @click="up"
          >
            上一级
          </NeumorphismButton>
          <NeumorphismButton
            v-if="prodoc.browsePath"
            variant="glass"
            shape="pill"
            size="small"
            @click="pick(prodoc.browsePath as string)"
          >
            用这个目录
          </NeumorphismButton>
        </div>

        <ul class="prodoc-dirs">
          <li v-for="dir in prodoc.browseEntries" :key="dir.path">
            <button class="prodoc-dir" @click="enter(dir.path)">
              <PanelIcon name="folder" size="sm" />
              <span>{{ dir.name }}</span>
            </button>
          </li>
          <li v-if="prodoc.browseEntries.length === 0" class="prodoc-empty">这一层没有子目录</li>
        </ul>
      </section>
    </template>
  </div>
</template>

<style scoped>
.prodoc-panel {
  display: flex;
  flex-direction: column;
  gap: 8px;
  height: 100%;
  min-height: 0;
  padding: 12px;
  box-sizing: border-box;
}

.prodoc-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}

.prodoc-bar-main {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.prodoc-root {
  font-family: var(--echo-font-mono, monospace);
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 46ch;
}

.prodoc-summary {
  font-size: 12px;
  opacity: 0.68;
  white-space: nowrap;
}

.prodoc-bar-actions {
  display: flex;
  gap: 8px;
}

.prodoc-error {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  border-radius: 10px;
  background: var(--echo-color-danger-soft, rgba(200, 60, 60, 0.12));
  color: var(--echo-color-danger, #b23);
  font-size: 13px;
}

.prodoc-skipped {
  font-size: 12px;
  opacity: 0.85;
}

.prodoc-skipped ul {
  margin: 4px 0 0;
  padding-left: 18px;
  max-height: 120px;
  overflow: auto;
}

.prodoc-loading {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  opacity: 0.8;
}

.prodoc-canvas {
  flex: 1;
  min-height: 0;
  /* ProDoc 的画布要明确的可用高度，否则 auto 布局会塌成 0 */
  position: relative;
}

.prodoc-picker {
  display: flex;
  flex-direction: column;
  gap: 10px;
  overflow: auto;
}

.prodoc-hint {
  margin: 0;
  font-size: 12px;
  opacity: 0.72;
  line-height: 1.6;
}

.prodoc-roots {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}

.prodoc-crumbs {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-wrap: wrap;
  font-size: 12px;
}

.prodoc-crumb {
  border: none;
  background: none;
  padding: 2px 4px;
  cursor: pointer;
  color: inherit;
  font: inherit;
  text-decoration: underline dotted;
}

.prodoc-sep {
  opacity: 0.5;
}

.prodoc-dirs {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.prodoc-dir {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  border: none;
  background: none;
  padding: 6px 8px;
  border-radius: 8px;
  cursor: pointer;
  color: inherit;
  font: inherit;
  text-align: left;
}

.prodoc-dir:hover {
  background: var(--echo-color-surface-raised, rgba(0, 0, 0, 0.05));
}

.prodoc-empty {
  font-size: 12px;
  opacity: 0.6;
}
</style>
