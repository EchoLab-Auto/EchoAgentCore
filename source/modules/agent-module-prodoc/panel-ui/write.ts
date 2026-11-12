//! 单文件读写的**前端接线**：命令 + 回执关联 + 三路合并。
//!
//! ## 为什么回执要配 request_id
//!
//! 三条写命令载荷形状几乎一样，回执事件（`prodoc.file` / `prodoc.saved` /
//! `prodoc.deleted`）也没有天然关联键：同一个文件可以连续保存两次，靠
//! `(root, path)` 匹配会把第二次的补丁丢给第一次的 Promise——表现是"保存
//! 偶尔串版本"，而这种竞态在人工点按里几乎复现不出来。所以每次请求带一个
//! 面板生成的 `request_id`，模块原样带回。
//!
//! ## 为什么合并留在前端
//!
//! CLI 的冲突处理是 `merge3(base, current, incoming)`（`prodoc-core/merge.ts`，
//! 265 行 hunk 级 diff3）。在 Rust 里复刻必然与它语义分叉，结果是"面板说合并
//! 成功、命令行说冲突"。所以：Rust 只做**逐字比对 + 原子写**，冲突时把磁盘
//! 当前内容原样回传，这里跑 `merge3`，干净就以 `base = disk` 重发一次。
//!
//! 两次往返换来的是**两端行为一致**——这是刻意的取舍，不是疏忽。

import { sendCommand, state } from '@echo-agent/panel';
import { FILE_EVENT, SAVED_EVENT, DELETED_EVENT, type WriteAckEvent } from './prodoc';

/** 一次写操作的结果（与 ProDoc 组件的 `SaveResult` 形状兼容）。 */
export interface WriteResult {
  ok: boolean;
  error?: string;
  /** 合并成功时应写回的最终内容（组件据此同步草稿）。 */
  content?: string;
  merged?: boolean;
}

/** 等回执的超时（毫秒）。超时后 reject，让组件显示明确失败而不是永远转圈。 */
const ACK_TIMEOUT_MS = 15_000;

let seq = 0;

/** 生成回执关联 id（进程内唯一即可——回执只在本面板会话里匹配）。 */
function nextRequestId(): string {
  seq += 1;
  return `prodoc-${Date.now().toString(36)}-${seq}`;
}

/** 挂起中的写请求：`request_id` → resolve。 */
const pending = new Map<string, (ack: WriteAckEvent) => void>();

/** 已处理过的回执 id（事件流是滚动缓冲，同一批会被重复扫到）。 */
const handled = new Set<string>();

/**
 * 扫描事件流，把写回执派发给等待中的请求。
 *
 * 由 `index.ts` 的 watch 调用（它已经为快照事件扫了一遍；这里复用同一遍扫描
 * 的结果，不再单独 watch 一次——两次 watch 同长度变化会重复处理）。
 */
export function dispatchWriteAcks(acks: WriteAckEvent[]): void {
  for (const ack of acks) {
    const id = ack.request_id;
    if (!id || handled.has(id)) continue;
    handled.add(id);
    // 环形缓冲可能保留很久的旧回执：id 集合跟着裁剪，避免长期运行时无界增长。
    if (handled.size > 400) {
      const oldest = handled.values().next().value;
      if (oldest) handled.delete(oldest);
    }
    const resolve = pending.get(id);
    if (!resolve) continue;
    pending.delete(id);
    resolve(ack);
  }
}

/** 发一条写命令并等它的回执（超时 / 断线都落到 reject 的可读错误）。 */
function requestWrite(
  command:
    | { ReadProDocFile: { team_id?: string; root: string; path: string; request_id: string } }
    | {
        SaveProDocFile: {
          team_id?: string;
          root: string;
          path: string;
          content: string;
          base?: string;
          request_id: string;
        };
      }
    | {
        DeleteProDocFile: {
          team_id?: string;
          root: string;
          path: string;
          base?: string;
          request_id: string;
        };
      },
  core: string | null,
): Promise<WriteAckEvent> {
  const request_id = nextRequestId();
  const payload = Object.values(command)[0] as Record<string, unknown>;
  payload.request_id = request_id;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(request_id);
      reject(new Error('Core 未在 15 秒内确认（连接可能已断开）'));
    }, ACK_TIMEOUT_MS);
    pending.set(request_id, (ack) => {
      clearTimeout(timer);
      resolve(ack);
    });
    const sent = sendCommand(command as never, core ?? undefined);
    if (!sent) {
      clearTimeout(timer);
      pending.delete(request_id);
      reject(new Error('命令未发送（未连接或无法确定目标 Core）'));
    }
  });
}

/** 把绝对根 + 相对路径拼成界面/事件里用的完整标识。 */
export interface WriteTarget {
  teamId: string | null;
  core: string | null;
  root: string;
}

/**
 * 保存单个文档（`DocGraphViewer` 的 `saveHandler`）。
 *
 * 冲突时的处理顺序与 CLI 完全一致：
 *
 * 1. 带 `base` 提交；
 * 2. 成功 → done（内容与磁盘已一致时后端也会直接成功，见模块侧的幂等分支）；
 * 3. 冲突且磁盘文件还在 → `merge3(base, disk, incoming)`；
 *    - 干净 → 以 `base = disk` 重发，成功后把合并结果回给组件；
 *    - 有重叠 → 明确报冲突块数，交给用户决定（组件会打开冲突面板）；
 * 4. 冲突且磁盘文件已删 → 报错，**不重建**（重建等于把别人的删除撤销掉）。
 */
export async function saveFile(
  target: WriteTarget,
  path: string,
  content: string,
  base?: string,
): Promise<WriteResult> {
  try {
    const first = await requestWrite(
      {
        SaveProDocFile: {
          team_id: target.teamId ?? undefined,
          root: target.root,
          path,
          content,
          base,
          request_id: '',
        },
      },
      target.core,
    );
    if (first.ok) return { ok: true };
    if (!first.conflict) return { ok: false, error: first.error ?? '保存失败' };
    // 到这里必然是 base 存在且磁盘变了（base 缺省时后端不会报冲突）。
    if (base === undefined) {
      return { ok: false, error: first.error ?? '磁盘内容已变更' };
    }
    if (first.disk === null || first.disk === undefined) {
      return { ok: false, error: first.error ?? '文件已从磁盘删除，无法保存' };
    }

    // diff3 **动态加载**：只有真出现"磁盘被外部改过"时才需要它，占掉首屏
    // 6 KB 是不必要的。`./vendor/prodoc-core/pure` 不带 `.js` 后缀——bundler
    // 解析会命中同目录的 `pure.d.ts`（带后缀时 tsc 会去找 JS 本身，而我们
    // 没有开 allowJs），vite 侧仍解析到 `pure.js`。
    const { merge3 } = await import('./vendor/prodoc-core/pure');
    const merged = merge3(base, first.disk, content);
    if (!merged.clean) {
      return {
        ok: false,
        error: `磁盘上有 ${merged.conflicts} 处与本次修改重叠，无法自动合并（请对照后手动处理）`,
      };
    }
    const second = await requestWrite(
      {
        SaveProDocFile: {
          team_id: target.teamId ?? undefined,
          root: target.root,
          path,
          content: merged.result,
          // 以刚读到的磁盘内容为基准重发：若此刻又变了，会再次冲突并重新合并。
          base: first.disk,
          request_id: '',
        },
      },
      target.core,
    );
    if (second.ok) {
      return { ok: true, merged: true, content: merged.result };
    }
    return { ok: false, error: second.error ?? '合并后写入失败' };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error) };
  }
}

/**
 * 删除单个文档（`DocGraphViewer` 的 `deleteHandler`）。
 *
 * 删除**不做合并**（与 CLI 一致）：文件都删了，没有可合并的对象。`base`
 * 不匹配就取消——"别人改过的文件被删掉"是不可恢复的损失。
 */
export async function deleteFile(
  target: WriteTarget,
  path: string,
  base?: string,
): Promise<WriteResult> {
  try {
    const ack = await requestWrite(
      {
        DeleteProDocFile: {
          team_id: target.teamId ?? undefined,
          root: target.root,
          path,
          base,
          request_id: '',
        },
      },
      target.core,
    );
    return ack.ok ? { ok: true } : { ok: false, error: ack.error ?? '删除失败' };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error) };
  }
}

/** 当前活动 Core（写操作统一走它，与快照命令保持一致）。 */
export function activeCore(): string | null {
  return state.activeRegion ?? null;
}

export { DELETED_EVENT, FILE_EVENT, SAVED_EVENT };
