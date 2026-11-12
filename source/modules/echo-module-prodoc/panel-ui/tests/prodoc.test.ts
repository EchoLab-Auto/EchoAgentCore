//! ProDoc 扩展纯函数层的测试。
//!
//! 覆盖三件容易写错、错了又难看出来���事：
//!
//! 1. **按键去重取最新**——`state.moduleEvents` 是 200 条环形缓冲，同一目录
//!    刷新两次就有两份快照，取错那份会让界面停在旧内容；
//! 2. **形状不对的事件必须丢掉**——模块与扩展版本漂移时宁可空态，也不能让
//!    面板抛异常（扩展崩了会连带整个 overlay 白屏）；
//! 3. **面包屑与"上一级"**——文档在根下三四层时这两个是唯一的导航手段，
//!    算错会让人卡在某一层出不来。

import { describe, expect, it } from 'vitest';
import {
  BUNDLE_EVENT,
  DIRS_EVENT,
  ERROR_EVENT,
  ROOTS_EVENT,
  breadcrumbs,
  bundleKeyOf,
  collectProDocEvents,
  describeBundle,
  formatBytes,
  parentPath,
} from '../prodoc';

const MODULE = 'echo-agent.prodoc';

function bundle(overrides: Record<string, unknown> = {}) {
  return {
    module: MODULE,
    name: BUNDLE_EVENT,
    core: 'core-a',
    payload: {
      team_id: 't1',
      root: '/srv/docs',
      files: { 'a.md': '# a' },
      stats: { documents: 1, bytes: 2048, skipped: 0, truncated: false },
      ...overrides,
    },
  };
}

describe('bundleKeyOf', () => {
  it('三段都参与键（core / team / root）', () => {
    const a = bundleKeyOf({ core: 'core-a', team_id: 't1', root: '/srv/docs' });
    const b = bundleKeyOf({ core: 'core-b', team_id: 't1', root: '/srv/docs' });
    const c = bundleKeyOf({ core: 'core-a', team_id: 't2', root: '/srv/docs' });
    expect(new Set([a, b, c]).size).toBe(3);
  });

  it('null / undefined 归一成同一段（否则会生成两条键）', () => {
    expect(bundleKeyOf({ core: null, team_id: null, root: '/x' })).toBe(
      bundleKeyOf({ core: undefined, team_id: undefined, root: '/x' }),
    );
  });
});

describe('collectProDocEvents', () => {
  it('同一 root 的多份快照只取**最新**那份', () => {
    // 新的在前（state.ts 用 unshift）。
    const events = [
      bundle({ files: { 'new.md': '# new' } }),
      { module: MODULE, name: BUNDLE_EVENT, core: 'core-a', payload: { team_id: 't1', root: '/srv/docs', files: { 'old.md': '# old' } } },
    ];
    const { bundles } = collectProDocEvents(events);
    expect(bundles).toHaveLength(1);
    expect(Object.keys(bundles[0].files)).toEqual(['new.md']);
  });

  it('不同 core 的同名目录各自保留（多上游不能互相覆盖）', () => {
    const events = [
      { ...bundle(), core: 'core-a' },
      { ...bundle(), core: 'core-b' },
    ];
    const { bundles } = collectProDocEvents(events);
    expect(bundles.map((item) => item.core).sort()).toEqual(['core-a', 'core-b']);
  });

  it('忽略别的模块的事件（ModuleEvent 通道是共用的）', () => {
    const events = [
      { module: 'echo-agent.workspace', name: BUNDLE_EVENT, payload: { root: '/x' } },
      bundle(),
    ];
    const { bundles } = collectProDocEvents(events);
    expect(bundles).toHaveLength(1);
  });

  it('形状不对的快照被丢掉（缺 root / files 不是对象）', () => {
    const events = [
      { module: MODULE, name: BUNDLE_EVENT, payload: { files: {} } },
      { module: MODULE, name: BUNDLE_EVENT, payload: { root: '/x', files: 'nope' } },
    ];
    const { bundles } = collectProDocEvents(events);
    // 第二个能过（root 有），但 files 会被归一成空对象而不是字符串。
    expect(bundles).toHaveLength(1);
    expect(bundles[0].files).toEqual({});
  });

  it('files 里非字符串的值被过滤（防止把 null 塞给画布）', () => {
    const events = [bundle({ files: { 'a.md': '# a', bad: 42, worse: null } })];
    const { bundles } = collectProDocEvents(events);
    expect(Object.keys(bundles[0].files)).toEqual(['a.md']);
  });

  it('根列表只取第一条（最新）', () => {
    const events = [
      { module: MODULE, name: ROOTS_EVENT, payload: { roots: [{ label: 'new', path: '/new' }] } },
      { module: MODULE, name: ROOTS_EVENT, payload: { roots: [{ label: 'old', path: '/old' }] } },
    ];
    const { roots } = collectProDocEvents(events);
    expect(roots?.roots[0].label).toBe('new');
  });

  it('错误事件与快照分开收集（都按 root 去重）', () => {
    const events = [
      { module: MODULE, name: ERROR_EVENT, payload: { team_id: 't1', root: '/srv/docs', error: '不在允许范围' } },
      { module: MODULE, name: ERROR_EVENT, payload: { team_id: 't1', root: '/srv/docs', error: '旧的' } },
    ];
    const { errors, bundles } = collectProDocEvents(events);
    expect(bundles).toHaveLength(0);
    expect(errors).toHaveLength(1);
    expect(errors[0].error).toBe('不在允许范围');
  });

  it('目录层事件带上 path 与 entries', () => {
    const events = [
      {
        module: MODULE,
        name: DIRS_EVENT,
        payload: { path: '/srv/docs', entries: [{ name: 'guide', path: '/srv/docs/guide', is_dir: true }] },
      },
    ];
    const { dirs } = collectProDocEvents(events);
    expect(dirs?.path).toBe('/srv/docs');
    expect(dirs?.entries[0].name).toBe('guide');
  });

  it('空事件流不炸', () => {
    const result = collectProDocEvents([]);
    expect(result.bundles).toEqual([]);
    expect(result.roots).toBeNull();
    expect(result.dirs).toBeNull();
  });
});

describe('describeBundle', () => {
  it('正常：篇数 + 体积', () => {
    expect(describeBundle({ docs: 12, bytes: 184 * 1024, skipped: [], truncated: false })).toBe('12 篇 · 184.0 KB');
  });

  it('有跳过就显式说出来（否则用户以为文档少了是自己写错）', () => {
    const text = describeBundle({
      docs: 3,
      bytes: 1024,
      skipped: [{ path: 'big.md', reason: '超限' }],
      truncated: false,
    });
    expect(text).toContain('跳过 1');
  });

  it('截断要说"已达上限"——这是最容易被误解成"文档本来就这么点"的状态', () => {
    const text = describeBundle({ docs: 2000, bytes: 1024, skipped: [], truncated: true });
    expect(text).toContain('已达上限');
  });
});

describe('formatBytes', () => {
  it('按量级切换单位', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(2048)).toBe('2.0 KB');
    expect(formatBytes(3 * 1024 * 1024)).toBe('3.0 MB');
  });

  it('负数 / NaN 不产生奇怪的输出', () => {
    expect(formatBytes(-1)).toBe('0 B');
    expect(formatBytes(Number.NaN)).toBe('0 B');
  });
});

describe('breadcrumbs / parentPath', () => {
  const roots = [{ label: 'docs', path: '/srv/docs' }];

  it('根 + 逐层', () => {
    expect(breadcrumbs('/srv/docs/a/b', roots)).toEqual([
      { label: 'docs', path: '/srv/docs' },
      { label: 'a', path: '/srv/docs/a' },
      { label: 'b', path: '/srv/docs/a/b' },
    ]);
  });

  it('未选路径时为空（界面显示"根列表"）', () => {
    expect(breadcrumbs(null, roots)).toEqual([]);
  });

  it('上一级：根的下层 → 根；根 → null（回根列表）', () => {
    expect(parentPath('/srv/docs/a', roots)).toBe('/srv/docs');
    expect(parentPath('/srv/docs', roots)).toBeNull();
    expect(parentPath(null, roots)).toBeNull();
  });

  it('深路径逐级回退（不跳到根的上一级）', () => {
    expect(parentPath('/srv/docs/a/b/c', roots)).toBe('/srv/docs/a/b');
  });
});
