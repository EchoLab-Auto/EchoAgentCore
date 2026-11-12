//! 写接线（保存 / 删除 / 三路合并）的测试。
//!
//! 这里 mock 掉 `@echo-agent/panel`：真实模块一加载就会牵出 socket 与全局
//! state，测试只需要"命令发出去了"和"回执按 request_id 派发回来"两件事。
//!
//! 重点是**合并重试**这条路径：它是与 ProDoc CLI 行为一致的唯一保证，也是最
//! 容易写错的地方（基准取错一次，就会把磁盘上的外部改动悄悄覆盖掉）。

import { beforeEach, describe, expect, it, vi } from 'vitest';

/** 记录发出的命令（`sendCommand` 的替身）。 */
const sent: Array<Record<string, unknown>> = [];

vi.mock('@echo-agent/panel', () => ({
  sendCommand: (command: Record<string, unknown>) => {
    sent.push(command);
    return true;
  },
  state: { activeRegion: 'core-a' },
}));

// 必须在 mock 之后 import（vi.mock 会被提升，但显式放后面更清楚）。
const { saveFile, deleteFile, dispatchWriteAcks } = await import('../write');
const { merge3 } = await import('../vendor/prodoc-core/pure');

const target = { teamId: 't1', core: 'core-a', root: '/srv/docs' };

/**
 * 让挂起的 Promise 有机会注册 pending 表。
 *
 * 冲突分支里 `merge3` 是**动态 import**（只有真冲突才加载那 6 KB），模块解析
 * 会跨几个宏任务，所以这里等到宏任务队列排空而不是只让一轮微任务。
 */
async function tick() {
  await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
}

/** 取出最近一条命令的 `request_id`（模拟 Core 回执时要原样带回）。 */
function lastRequestId(): string {
  const command = sent[sent.length - 1];
  const payload = Object.values(command)[0] as Record<string, unknown>;
  return String(payload.request_id);
}

beforeEach(() => {
  sent.length = 0;
});

describe('merge3 可用性', () => {
  it('vendored 的 diff3 能合并不重叠的改动', () => {
    const base = '# 标题\nA\nB\nC\n';
    const disk = '# 标题\nA\nB\nC\n磁盘新增一行\n';
    const mine = '# 标题\nA 改过\nB\nC\n';
    const merged = merge3(base, disk, mine);
    expect(merged.clean).toBe(true);
    expect(merged.result).toContain('A 改过');
    expect(merged.result).toContain('磁盘新增一行');
  });

  it('重叠改动判为冲突且给出块数', () => {
    const base = '# 标题\nA\n';
    const disk = '# 标题\n磁盘版\n';
    const mine = '# 标题\n我的版\n';
    const merged = merge3(base, disk, mine);
    expect(merged.clean).toBe(false);
    expect(merged.conflicts).toBeGreaterThan(0);
  });
});

describe('saveFile', () => {
  it('一次成功：只发一条命令，不带重试', async () => {
    const promise = saveFile(target, 'a.md', 'v2', 'v1');
    await tick();
    expect(sent).toHaveLength(1);
    dispatchWriteAcks([
      { request_id: lastRequestId(), root: '/srv/docs', path: 'a.md', ok: true },
    ]);
    await expect(promise).resolves.toEqual({ ok: true });
  });

  it('冲突且能干净合并：以磁盘内容为基准重发，并回合并结果', async () => {
    const base = 'l1\nl2\nl3\n';
    const disk = 'l1\nl2\nl3\ndisk-extra\n';
    const mine = 'l1\nmine\nl3\n';

    const promise = saveFile(target, 'a.md', mine, base);
    await tick();
    const firstId = lastRequestId();
    dispatchWriteAcks([
      { request_id: firstId, root: '/srv/docs', path: 'a.md', ok: false, conflict: true, disk, error: '需要合并' },
    ]);
    // 等第二次提交发出（动态 import + 重新计算合并结果）。
    await vi.waitFor(() => expect(sent).toHaveLength(2));

    // 第二次提交：内容 = 合并结果，base = 刚读到的磁盘内容。
    const second = Object.values(sent[1])[0] as Record<string, unknown>;
    expect(second.base).toBe(disk);
    expect(String(second.content)).toContain('disk-extra');
    expect(String(second.content)).toContain('mine');

    dispatchWriteAcks([
      { request_id: lastRequestId(), root: '/srv/docs', path: 'a.md', ok: true },
    ]);
    const result = await promise;
    expect(result.ok).toBe(true);
    expect(result.merged).toBe(true);
    expect(result.content).toContain('disk-extra');
  });

  it('冲突且重叠：不重发，报冲突块数', async () => {
    const promise = saveFile(target, 'a.md', 'mine', 'base');
    await tick();
    dispatchWriteAcks([
      {
        request_id: lastRequestId(),
        root: '/srv/docs',
        path: 'a.md',
        ok: false,
        conflict: true,
        disk: 'disk',
        error: '需要合并',
      },
    ]);
    const result = await promise;
    expect(result.ok).toBe(false);
    expect(result.error).toContain('处');
    expect(sent).toHaveLength(1);
  });

  it('冲突且磁盘文件已删：报错且不重发（不重建别人的删除）', async () => {
    const promise = saveFile(target, 'a.md', 'mine', 'base');
    await tick();
    dispatchWriteAcks([
      {
        request_id: lastRequestId(),
        root: '/srv/docs',
        path: 'a.md',
        ok: false,
        conflict: true,
        disk: null,
        error: '文件已从磁盘删除，无法保存（请决定是否重新创建）',
      },
    ]);
    const result = await promise;
    expect(result.ok).toBe(false);
    expect(result.error).toContain('删除');
    expect(sent).toHaveLength(1);
  });

  it('平台错误（非冲突）：原样透出原因', async () => {
    const promise = saveFile(target, 'a.md', 'v2', 'v1');
    await tick();
    dispatchWriteAcks([
      {
        request_id: lastRequestId(),
        root: '/srv/docs',
        path: 'a.md',
        ok: false,
        error: '只允许 .md 文件',
      },
    ]);
    const result = await promise;
    expect(result.ok).toBe(false);
    expect(result.error).toBe('只允许 .md 文件');
  });
});

describe('deleteFile', () => {
  it('成功', async () => {
    const promise = deleteFile(target, 'a.md', 'v1');
    await tick();
    dispatchWriteAcks([
      { request_id: lastRequestId(), root: '/srv/docs', path: 'a.md', ok: true },
    ]);
    await expect(promise).resolves.toEqual({ ok: true });
  });

  it('base 不匹配 → 不删，报原因', async () => {
    const promise = deleteFile(target, 'a.md', 'stale');
    await tick();
    dispatchWriteAcks([
      {
        request_id: lastRequestId(),
        root: '/srv/docs',
        path: 'a.md',
        ok: false,
        conflict: true,
        disk: 'changed',
        error: '文件在磁盘上已变更，删除已取消',
      },
    ]);
    const result = await promise;
    expect(result.ok).toBe(false);
    expect(result.error).toContain('取消');
  });
});

describe('回执关联', () => {
  it('两次并发保存的回执乱序到达也各自落到正确的 Promise', async () => {
    const first = saveFile(target, 'a.md', 'A', 'base-a');
    await tick();
    const firstId = lastRequestId();
    const second = saveFile(target, 'b.md', 'B', 'base-b');
    await tick();
    const secondId = lastRequestId();
    expect(firstId).not.toBe(secondId);

    // 后发的先回：靠 request_id 而不是 (root, path) 匹配。
    dispatchWriteAcks([
      { request_id: secondId, root: '/srv/docs', path: 'b.md', ok: true },
      { request_id: firstId, root: '/srv/docs', path: 'a.md', ok: false, error: 'A 失败' },
    ]);

    await expect(second).resolves.toEqual({ ok: true });
    const firstResult = await first;
    expect(firstResult.ok).toBe(false);
    expect(firstResult.error).toBe('A 失败');
  });

  it('同一份回执重复投递只生效一次（事件流是滚动缓冲，会重复扫到）', async () => {
    const promise = saveFile(target, 'a.md', 'v2', 'v1');
    await tick();
    const ack = { request_id: lastRequestId(), root: '/srv/docs', path: 'a.md', ok: true };
    dispatchWriteAcks([ack]);
    dispatchWriteAcks([ack]);
    await expect(promise).resolves.toEqual({ ok: true });
  });

  it('没有 request_id 的回执被忽略（无法关联，不该误配）', async () => {
    // 不 dispatch 任何带 id 的回执 → Promise 仍挂起。用一个已知 id 的回执收尾
    // 验证"忽略"没有把别的请求也吞掉。
    const promise = saveFile(target, 'a.md', 'v2', 'v1');
    await tick();
    dispatchWriteAcks([{ root: '/srv/docs', path: 'a.md', ok: false, error: '孤儿回执' }]);
    dispatchWriteAcks([
      { request_id: lastRequestId(), root: '/srv/docs', path: 'a.md', ok: true },
    ]);
    await expect(promise).resolves.toEqual({ ok: true });
  });
});
