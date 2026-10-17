# 图标体系（2026-10 统一）

> **定位**：Panel 侧图标的唯一实现规范——图标载体、尺寸/线宽档位、语义色、防回归。读者：Panel 前端开发者、ui-frame 维护者。
> 上游规范：ui-frame 仓库 `document/develop/design-philosophy.md` §8「图标体系」（尺寸七档 / 线宽六档语义）。

---

## 一、载体分工

| 组件 | 承载 | 说明 |
| --- | --- | --- |
| `PanelIcon.vue` | **界面语义**图标 | **薄封装**在 ui-frame `NeumorphismIcon` 之上（82 行）：只加 `tone` → `--panel-*` 语义色映射；几何全部来自 ui-frame 注册表（55 个语义） |
| `ToolIcon.vue` | **工具语义**图标 | 工具名 → 图标分派（`bash` / `files` / `code` / `subagent` …）+ 11 种专属 loading 动画；产品特有，留在 Panel |

底层是 ui-frame 的 `NeumorphismIcon` + `@echolab-auto/ui-frame/icons` 注册表（65 个语义）：**24 基准 viewBox / `stroke="currentColor"` / 尺寸七档 / 线宽按档自动**。

> 2026-10 演进：Panel 先自建了 305 行的 `PanelIcon`（含 31 个 path），随后把几何全部上收进 ui-frame 注册表（缺的 `message` / `network` / `sparkle` 也一并补进去），PanelIcon 降到 82 行、只留产品语义色。**Panel 不再持有任何 path 数据。**

```vue
<PanelIcon name="home" />                        <!-- 默认 sm = 14px -->
<PanelIcon name="lock" size="xs" tone="warn" />  <!-- 12px，语义色 -->
<PanelIcon name="chevron-right" size="md" />
<PanelIcon name="gear" aria-label="设置" />       <!-- 语义图标必配 label -->
```

- **尺寸档**：`xs` 12 / `sm` 14 / `md` 16 / `lg` 18 / `xl` 20 / `2xl` 24（数字也可，但须落在这些值上）
- **线宽自动**：≤12px → 2.5（微图标补偿）；≥24px → 1.5（大图标轻描）；其余 → 2（由 ui-frame 实现，Panel 不重复）
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
4. **勾选类图标用百分比定尺寸**（`_checkable.scss` 的 `checkable-sizes`）：
   `Checkbox` / `Radio` 的 box 为 18 / 24 / 30px（small/medium/large），内部图标 `width: 65%` → 实测 **11.7 / 15.6 / 19.5px**，三档全部落在枚举档之外。
   同理的还有 `Spinner` 的 `size` prop 已支持数字直传（`<NeumorphismSpinner :size="12" />`），但其它组件没有等价通道。

→ 库侧改造已全部落地（2026-10）：本节列为**已解决的历史问题**，保留供追溯。

### 附录：ui-frame 图标组件改造（**已实施** 2026-10）

实施提交：`4b0d14d`（Icon 组件 + 子路径 + ThemeToggle/Checkbox 修复）、`5bf1429`（补 3 个通用语义）。

**A. 新增 `NeumorphismIcon` 组件 + `icons` 子路径** — ✅ 已实施

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

**D. ThemeToggle 修复** — ✅ 已实施

- 三态统一线框（`sun` 的圆改 `stroke`、`moon` 改轮廓线框），或统一"实心核心 + 线框外饰"；**关键是三态同族**
- 激活态去掉 `scale()`：改 `rotate(15deg)` 单变换，或尺寸档切换（`sm` 14 → `lg` 18），保证不出现非档位像素

**D2. 勾选类图标尺寸改枚举档** — ✅ 已实施

`_checkable.scss` 的 `checkable-sizes` 把 `65%` 换成枚举值：small 18px 的 box 配 `--nm-icon-size-xs`(12)、medium 24px 配 `--nm-icon-size-sm`(14)、large 30px 配 `--nm-icon-size-md`(16)——视觉占比从 65% 降到 67%/58%/53%，观感与现状一致但尺寸可枚举、可被 `check:styles` 校验。

**F. 三条已复核的"疑似问题"（不必再查）**

- `NeumorphismThemeToggle` 组内存在 `stroke-width="1"` 的说法**不成立**——该组件全部 4 处线宽均为 `2`（实测 `grep -o 'stroke-width="[0-9.]*"'` 只有 `2`）。
- 12px 图标配 `stroke-width="2.5"` **符合** §8（2.5 = ≤12px 微图标补偿），不是越界；`ChatFold` 的 12px caret 即属此例。
- **旋转/缩放图标的 `getBoundingClientRect()` 不能当尺寸证据**：`transform: rotate(15deg)` 会把 14px 图标的包围盒撑到 17.15px（×cos15°+sin15° = ×1.2247），视觉尺寸完全没变。审查脚本请用 `offsetWidth` 或 `getComputedStyle().width`。上一轮报的「18.86px 越界」里，`scale(1.1)` 部分是真实缺陷（已修），旋转带来的 +22% 是测量假象。

---

## 六、内部迁用完成情况（2026-10）

ui-frame 内部 **75 个内联 `<svg>` → 6**（全是非语义图形），分 6 个提交按语义推进：

| 提交 | 语义 | 组件数 |
| --- | --- | --- |
| `d6c3324` | `close` | 9 |
| `c1eaf0d` | `chevron-down` | 5（6 处） |
| `0b91e9d` | `chevron-right/left` + `search` | 4（6 处） |
| `4bbfb46` | `check` / `close` / `plus` / `minus` / 状态图标 | 4（7 处） |
| `0aff9ad` | `spinner`（改走 `spin` prop） | 2（4 处） |
| `6c98bc0` | 其余全部（Canvas/DatePicker/Table/Select/Steps/Upload/Alert） | 8（26 处） |

**刻意保留的 6 处**（非语义图标，不属注册表职责）：

| 位置 | 性质 |
| --- | --- |
| `ChartBar` / `ChartLine` / `ChartPie` / `ChartCandlestick` | 数据可视化（动态 viewBox + 坐标轴/网格/数据点） |
| `Logo` | 品牌图形（512 viewBox + filter） |
| `Empty` | 160×160 空态插画 |
| `Progress` | 进度环（`stroke-dasharray` 动态计算） |
| `Spinner`（组件本体） | 双弧轨道设计，非通用图标 |
| `Button` / `Upload` 的 spinner | 带 `stroke-opacity` 轨道的进度指示器 |

**`src/composites/doc/**`（DocTreeIcon）**：仍自带 16 viewBox 图标体系（`ICON_PATHS`），
`check-icons.mjs` 对其豁免，列入后续迁移。

### 迁用中发现并修掉的既有缺陷

- `Steps` 的错误图标 `stroke-width="2.5"` 用在 14–20px 显示档（§8 里 2.5 只属 ≤12px 微图标补偿）→ 改按档自动 `2`
- `Alert` 内置类型图标是 **Material 实心字形**（`fill: currentColor`），与全库线框语言相反，且与 `Toast` 的状态图标（线框）不一致 → 统一为线框，`defaultIcons` 数据整体删除
- `AutoComplete` / `Select` 的 spinner 各自用 CSS `animation` 旋转（0.8s），与 `spin` prop 会双写 → 统一交给 `spin`（1s + `prefers-reduced-motion` 停用），删掉 3 处 animation 与 1 段无引用 keyframes
