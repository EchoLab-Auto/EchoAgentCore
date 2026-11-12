/**
 * ProDoc 渲染器的**本地类型声明**（vendored dist 的伴生声明）。
 *
 * 上游 `dist/index.d.ts` 是给包消费者用的：它 re-export 了 `@echolab-auto/ui-frame/doc`
 * 的三个旧组件并 `import` 两份 CSS——在本仓（扩展被 Panel 的 vite 以源码形式编译）
 * 里这两件事都不需要：CSS 由 `index.ts` 显式 import，旧组件我们不使用。
 *
 * 保留最小声明即可：类型只需要覆盖 `DocGraphViewer` 的 props/emits，其余
 * （布局、搜索、编辑）都在组件内部。
 */
import type { Component } from 'vue';

/** 写盘结果：`ok = false` 时 `error` 会显示在面板的 toast 里。 */
export interface SaveResult {
  ok: boolean;
  error?: string;
  /** 合并成功时要写回的内容（ProDoc 的三路合并结果）。 */
  content?: string;
}

export interface ProDocPlugin {
  name?: string;
  [key: string]: unknown;
}

/**
 * 文档图画布。
 *
 * 注意：它自带的编辑能力需要 `saveHandler` / `deleteHandler`；不提供时编辑
 * 入口会以"未提供写处理器"失败——P1 只读阶段就是这样（见组件里的说明）。
 */
export const DocGraphViewer: Component;
