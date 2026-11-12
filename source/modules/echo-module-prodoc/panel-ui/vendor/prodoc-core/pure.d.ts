/**
 * ProDoc core 的**最小本地声明**（vendored `pure.js` 的伴生）。
 *
 * 上游 `@prodoc/core` 的 `pure` 入口导出了十几个纯函数（图模型、布局、边路由、
 * 搜索、三路合并…）。本扩展只用一个：`merge3`——保存时磁盘内容、客户端基准、
 * 本次提交三者的 diff3 合并。
 *
 * 上游 `pure.d.ts` 会带出一串 `./frame.js` / `./graph.js` 的相对类型引用，
 * 而我们只 vendored 了运行时产物、没有逐个 `.d.ts`。声明一个精确的子集比伪造
 * 一整套类型安全：用错的导出会在编译期就报"模块没有导出成员"。
 */

/** 三路合并结果（字段名与 `prodoc-core/merge.ts` 的 `Merge3Result` 一致）。 */
export interface Merge3Result {
  /** 是否无冲突。`false` 时 `result` 含冲突标记，**不应写盘**。 */
  clean: boolean;
  /** 合并结果文本（EOL 跟随 `current`）。 */
  result: string;
  /** 冲突块数量。 */
  conflicts: number;
}

/**
 * 行级三路合并（diff3）。
 *
 * @param base 客户端据此修改的原始内容
 * @param current 磁盘当前内容
 * @param incoming 本次提交内容
 */
export declare function merge3(base: string, current: string, incoming: string): Merge3Result;
