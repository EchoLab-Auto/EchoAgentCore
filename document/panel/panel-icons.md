# 图标体系（2026-10 统一）

> **定位**：Panel 侧图标的唯一实现规范——图标载体、尺寸/线宽档位、语义色、防回归。读者：Panel 前端开发者、ui-frame 维护者。
> 上游规范：ui-frame 仓库 `document/develop/design-philosophy.md` §8「图标体系」（尺寸七档 / 线宽六档语义）。

---

## 一、载体分工

| 组件 | 承载 | 说明 |
| --- | --- | --- |
| `PanelIcon.vue` | **界面语义**图标 | 导航 / 状态 / 操作 / 方向；31 个语义名；无业务耦合 |
| `ToolIcon.vue` | **工具语义**图标 | 工具名 → 图标分派（`bash` / `files` / `code` / `subagent` …）+ 11 种专属 loading 动画 |

两个组件都遵循 ui-frame §8：**24 基准 viewBox / `stroke="currentColor"` / 尺寸取 `--nm-icon-size-*` / 线宽取六档语义**。

```vue
<PanelIcon name="home" />                        <!-- 默认 sm = 14px -->
<PanelIcon name="lock" size="xs" tone="warn" />  <!-- 12px，语义色 -->
<PanelIcon name="chevron-right" size="md" />
<PanelIcon name="gear" aria-label="设置" />       <!-- 语义图标必配 label -->
```

- **尺寸档**：`xs` 12 / `sm` 14 / `md` 16 / `lg` 18 / `xl` 20 / `2xl` 24（数字也可，但须落在这些值上）
- **线宽自动**：≤12px → 2.5（微图标补偿）；≥24px → 1.5（大图标轻描）；其余 → 2
- **语义色 `tone`**：`inherit`（默认）/ `accent` / `muted` / `ok` / `warn` / `err` / `reasoning` / `subagent`——全部映射到 `styles.css` 的 `--panel-*` 令牌，随亮暗主题切换

---

## 二、尺寸与线宽规则（强制）

| 项 | 允许值 | 依据 |
| --- | --- | --- |
| 图标尺寸 | 12 / 14 / 16 / 18 / 20 / 24 / 32 | `--nm-icon-size-*`（ui-frame §8） |
| 描边线宽 | 0.5 / 1 / 1.5 / 2 / 2.5 / 3 | ui-frame §8 六档语义（2 标准、2.5 ≤12px 补偿、3 勾选、1.5 ≥24px、1/0.5 图表） |

**禁用**：emoji 与符号字符充当控件图标（🔒 🏠 📁 ⛓ ☑ ☐ ⇥ ⇤ ▾ ▸ ⠿ ★ ⇧ ⚠ …）——内容表达（正文、说明文字）除外。

> 2026-10 修复前的实测偏差：尺寸 15px / 17px / 10px 三档自造（ChatView 发送/附件钮、文件树 chevron），线宽 1.3×5 / 1.8 自造（文件树自建 16 viewBox 图标体系），emoji 图标 15 处。

---

## 三、语义色令牌

| 令牌 | 亮色档 | 暗色档 | 亮底对比度 / 暗底对比度 |
| --- | --- | --- | --- |
| `--panel-reasoning` | `#7c3aed` | `#a78bfa` | 4.32:1 / 6.26:1 |
| `--panel-subagent` | `#0f766e` | `#14b8a6` | 4.15:1 / 6.85:1 |

背景取 `--nm-bg-color`（亮 `#e0e0e0` / 暗 `#1c1c1c`）。WCAG 1.4.11 对**非文本图形**要求 ≥3:1——修复前硬编码 `#a78bfa` / `#14b8a6` 是"只按暗色调"的值，亮色背景上仅 **2.06:1 / 1.89:1**（图标发虚难辨）。

新增语义色一律**成对**定义（`styles.css` 的 `:root` 与 `[data-theme='dark']`），不得在组件里写字面色。

---

## 四、防回归

`web/scripts/lint-styles.mjs` 规则 5/6（2026-10 新增）：

```bash
cd web && npm run lint:styles
```

- **规则 5**：SVG 的 `width`/`height` 属性与任意 `stroke-width` 须落在上表档位。
- **规则 6**：`class` 含 `icon|ico|mark|caret|grip|badge|dot` 的元素内不得出现 emoji/符号字符。

CI 里与既有令牌规则（`var(--nf-*)` 禁用命名空间、幻影令牌名单、`--nm-*` 全量对照）同级执行。

---

## 五、待办：ui-frame `NeumorphismIcon`

当前图标语义由 **Panel 自建 `PanelIcon`** 承载——这是过渡态。根因在库侧：

1. **ui-frame 无通用 Icon 组件**：31 个组件内联 75 个 `<svg>`，同一路径重复定义（`close` 9 处、`chevron-down` 5 处、`chevron-right` 3 处、`check` 2 处、spinner 弧 2 处）——规范（§8）只能被"检查"（`check:styles`），不能被"复用"。
2. **消费方因此无法共享**：Panel 过去用 emoji 顶替（`🔒`/`🏠`/`📁`/`⛓`/`☑`）、文件树自建 16 viewBox 体系，均源于库不提供 `lock`/`home`/`folder`/`link` 等语义。
3. **`NeumorphismThemeToggle` 两处缺陷**（Panel 实测可见）：
   - 三态图标风格不同族：`light` = 实心圆 + 线框射线、`auto` = 纯线框、`dark` = 纯实心块 → 切换主题时"实心↔线框"跳变
   - 激活态 `transform: rotate(15deg) scale(1.1)` → 14px 图标实测渲染 18.86px（变换包围盒，+35%），且脱离 §8 枚举档；同控件内激活项比相邻项明显更大更粗

→ 库侧改造方向见本节附录（提议，未实施）。

### 附录：ui-frame 图标组件改造提案

**A. 新增 `NeumorphismIcon` 组件 + `icons` 子路径**

```text
src/icons/
├── registry.ts        # name → path[]（去重自 31 个组件的内联 svg）
├── index.ts           # 导出 NeumorphismIcon 与 ICON_NAMES（供 tree-shaking 与校验）
src/components/NeumorphismIcon/
├── NeumorphismIcon.vue
├── index.ts
└── NeumorphismIcon.test.ts
```

```vue
<NeumorphismIcon name="close" />                         <!-- 默认 md 16px -->
<NeumorphismIcon name="chevron-down" size="sm" />
<NeumorphismIcon name="check" stroke-width="3" />        <!-- 勾选语义 -->
<NeumorphismIcon name="refresh" spin />                  <!-- 通用旋转 -->
<NeumorphismIcon name="cloud-sync" pulse />
```

- props：`name` / `size`（七档或数字，同 §8）/ `strokeWidth`（可选覆盖，缺省按档）/ `spin` / `pulse` / `label`（a11y）
- 尺寸走 `var(--nm-icon-size-*)`；线宽按档自动（≤12→2.5、≥24→1.5、其余 2）
- **`./icons` 子路径**独立导出，消费方只装图标表、不拖整包（与 `./doc` / `./chat` 同模式）
- 落地步骤：`vite.config.ts` entries 加 `'icons/index'` → `package.json` exports 加 `./icons` → barrel 注册（`check:registry` 会校验）→ changeset

**B. 语义覆盖（首批 40 个，按使用频次）**

| 组 | 图标 |
| --- | --- |
| 导航 | `chevron-down/up/left/right`、`arrow-left/right/up/down`、`home`、`external` |
| 状态 | `check`、`close`、`warning`、`info`、`dot`、`lock`、`unlock`、`spinner-arc` |
| 文件 | `folder`、`folder-open`、`file`、`link`、`cloud` |
| 操作 | `gear`、`refresh`、`plus`、`minus`、`search`、`trash`、`copy`、`pin`、`grip`、`more` |
| 数据 | `chart`、`table`、`list`、`calendar`、`clock`、`user`、`users`、`upload`、`download` |

**C. 内部组件迁用（分 commit，逐步）**

`close`（9 个组件）→ `chevron-down`（5）→ `chevron-right`（3）→ `check`（2）→ 其余。每个 commit 只换一个语义，`npm test` + 视觉快照对照。

**D. ThemeToggle 修复**

- 三态统一线框（`sun` 的圆改 `stroke`、`moon` 改轮廓线框），或统一"实心核心 + 线框外饰"；**关键是三态同族**
- 激活态去掉 `scale()`：改 `rotate(15deg)` 单变换，或尺寸档切换（`sm` 14 → `lg` 18），保证不出现非档位像素

**E. 动画边界**

库内只提供 `spin` / `pulse` 两个布尔 prop（覆盖绝大多数场景）。产品级专属动画（如 Panel 工具图标的 11 种路径动画）留在产品侧，不进库。
