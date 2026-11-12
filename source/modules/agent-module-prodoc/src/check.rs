//! 文档图检查（`ProDocCheck`）：断链 / 自身连线 / 重复 id / 环 / 孤立 /
//! 空正文 / 超长 / 缺参数区 / 只写单轴坐标。
//!
//! ## 与 `echo-prodoc check` 的关系：**刻意是子集**
//!
//! ProDoc CLI 的检查在 TypeScript 里（`prodoc-core/graph.ts` 的 `buildDocGraph`
//! 与 `prodoc-cli/check.ts`）。本模块在 Rust 里重新实现了一遍**图结构**那部分，
//! 理由是 agent 需要在 Core 进程内检查文档图——它不能起一个 Node 进程去跑
//! CLI。
//!
//! 注意上面这行以 `+` 开头会被 Markdown 当成列表项（clippy 会要求后续行缩进），
//! 所以这里写「与」而不是加号。
//!
//! 复刻必然带来分叉风险，所以边界划得很清楚：
//!
//! | 检查项 | 本模块 | CLI |
//! |---|---|---|
//! | 断链 / 自身连线 / 重复 id | 有 | 有 |
//! | 环（SCC） | 有 | 有 |
//! | 孤立 / 空正文 / 超长 / 缺参数区 / 单轴坐标 | 有 | 有 |
//! | group 相关（多 group 声明 / 显式几何冲突） | **无** | 有 |
//!
//! group 那两项是**纯布局外观**的问题（框画在哪里、围成什么形状），不影响图的
//! 结构与连通性；复刻要连 `parseGroupEntry` 的几何语法一起搬，收益不抵风险。
//! 需要那两项请跑 CLI。
//!
//! 解析规则与 `frame.ts` / `graph.ts` **逐条对齐**：参数区必须从第 0 字节开始、
//! 值按「整数 → 浮点 → 布尔 → 内联列表 → 引号串 → 裸串」判定、引用归一化会剥
//! `<>`、去 `#锚点`/`?查询`、URL 解码、反斜杠转斜杠、折叠 `.`/`..`、外链跳过。
//! 这些不是可选项——少一条就会把合法文档报成断链。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// 问题级别（与 CLI 的 `CheckIssue.level` 同词）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
    Info,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// 一条检查结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Issue {
    pub level: Level,
    /// 规则名（`broken-link` / `cycle` / …，与 CLI 的 `rule` 同名）。
    pub rule: &'static str,
    pub message: String,
    /// 相关文档（相对路径；能定位时给）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// 检查统计（面板/工具回执里的摘要）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Stats {
    pub documents: usize,
    pub relations: usize,
    pub groups: usize,
    pub errors: usize,
    pub warnings: usize,
}

/// 一份检查报告。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Report {
    pub issues: Vec<Issue>,
    pub stats: Stats,
}

impl Report {
    /// 是否有错误级问题（CI / 工具里据此给非零结论）。
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|issue| issue.level == Level::Error)
    }

    /// 一行摘要（工具回执的第一行）。
    pub fn summary(&self) -> String {
        format!(
            "{} 篇 · {} 条连线 · {} 个错误 / {} 条警告",
            self.stats.documents, self.stats.relations, self.stats.errors, self.stats.warnings
        )
    }
}

// ── 框架参数区解析（与 frame.ts 逐条对齐）──────────────────────────────

/// 参数值（只保留检查需要的判别：字符串 / 数字 / 列表 / 其它）。
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Str(String),
    Num(f64),
    List(Vec<Param>),
    Bool(bool),
    /// `''`（空值）。
    Empty,
}

impl Param {
    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Self::Num(value) => Some(*value),
            _ => None,
        }
    }
}

/// 参数区解析结果。
pub struct FrameBlock<'a> {
    pub params: BTreeMap<String, Param>,
    pub body: &'a str,
    pub has_frame: bool,
}

/// 解析框架参数区。
///
/// 参数区必须**从第 0 字节开始**：首行（去空白）是 `---`，下一条 `---` 结束。
/// 找不到闭合就按"没有参数区"处理，原文即正文——与 CLI 一致，这样
/// 「写了一行 `---` 就忘了闭合」不会把整个文件判成参数区。
pub fn parse_frame(content: &str) -> FrameBlock<'_> {
    let first_break = content.find('\n');
    let first_line = match first_break {
        Some(index) => &content[..index],
        None => content,
    };
    if first_line.trim() != "---" {
        return FrameBlock {
            params: BTreeMap::new(),
            body: content,
            has_frame: false,
        };
    }
    let rest = match first_break {
        Some(index) => &content[index + 1..],
        None => "",
    };
    let lines: Vec<&str> = rest.split('\n').collect();
    let close = lines.iter().position(|line| line.trim() == "---");
    let Some(close) = close else {
        return FrameBlock {
            params: BTreeMap::new(),
            body: content,
            has_frame: false,
        };
    };

    let frame_lines = &lines[..close];
    // 正文起点 = 参数区各行 + **闭合行**（含）的总长；与 CLI 的
    // `lines.slice(closeIdx + 1)` 等价。只算到 `close` 之前会把闭合的 `---`
    // 留在正文里——那会让「只有参数区」的文档被误判成有正文。
    let body_start = lines[..=close]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>();
    let body = &rest[body_start.min(rest.len())..];

    let mut params = BTreeMap::new();
    let mut index = 0;
    while index < frame_lines.len() {
        let line = frame_lines[index];
        index += 1;
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, raw)) = split_param_line(line) else {
            continue; // 无法识别的行跳过，不中断解析
        };
        let mut raw = raw.to_string();
        // 值换行书写（格式化工具会把 `key: [...]` 排成 `key:` + 下行 `[`）。
        if raw.trim().is_empty()
            && index < frame_lines.len()
            && frame_lines[index].trim_start().starts_with('[')
        {
            raw = frame_lines[index].to_string();
            index += 1;
        }
        // 多行数组：值以 `[` 开头但本行未闭合 → 跨行收集到闭括号或参数区结束。
        if raw.trim_start().starts_with('[') && !raw.trim_end().ends_with(']') {
            let mut collected = vec![raw.clone()];
            while index < frame_lines.len() {
                let next = frame_lines[index];
                index += 1;
                collected.push(next.to_string());
                if next.trim_end().ends_with(']') {
                    break;
                }
            }
            raw = collected.join("\n");
        }
        params.insert(key, parse_value(&raw));
    }

    FrameBlock {
        params,
        body,
        has_frame: true,
    }
}

/// `key: value` 行拆分（键必须 `[A-Za-z_][A-Za-z0-9_-]*`）。
fn split_param_line(line: &str) -> Option<(String, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    if key.is_empty() {
        return None;
    }
    let mut chars = key.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some((key.to_string(), &line[colon + 1..]))
}

/// 值解析：整数 → 浮点 → 布尔 → 内联列表 → 引号串 → 裸串（顺序与 CLI 一致）。
fn parse_value(raw: &str) -> Param {
    let value = raw.trim();
    if value.is_empty() {
        return Param::Empty;
    }
    if value.starts_with('[') && value.ends_with(']') {
        let inner = &value[1..value.len() - 1];
        let items = split_inline_list(inner)
            .into_iter()
            .map(|item| parse_value(&item))
            .filter(|item| *item != Param::Empty)
            .collect();
        return Param::List(items);
    }
    if value == "true" {
        return Param::Bool(true);
    }
    if value == "false" {
        return Param::Bool(false);
    }
    if is_integer(value) {
        if let Ok(parsed) = value.parse::<f64>() {
            return Param::Num(parsed);
        }
    }
    if is_decimal(value) {
        if let Ok(parsed) = value.parse::<f64>() {
            return Param::Num(parsed);
        }
    }
    // 引号串：首尾同一种引号，中间原样（允许空串）。
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        if (first == b'"' || first == b'\'') && bytes[bytes.len() - 1] == first {
            return Param::Str(value[1..value.len() - 1].to_string());
        }
    }
    Param::Str(value.to_string())
}

/// `^-?\d+$`
fn is_integer(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `^-?\d*\.\d+$`
fn is_decimal(value: &str) -> bool {
    let body = value.strip_prefix('-').unwrap_or(value);
    let Some(dot) = body.find('.') else {
        return false;
    };
    let (int_part, frac_part) = (&body[..dot], &body[dot + 1..]);
    !frac_part.is_empty()
        && frac_part.bytes().all(|b| b.is_ascii_digit())
        && int_part.bytes().all(|b| b.is_ascii_digit())
}

/// 内联列表切分：逗号分隔，引号内的逗号不切。
fn split_inline_list(raw: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(open) => {
                current.push(ch);
                if ch == open {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    current.push(ch);
                } else if ch == ',' {
                    items.push(current.clone());
                    current.clear();
                } else {
                    current.push(ch);
                }
            }
        }
    }
    items.push(current);
    items
}

/// `asRefs`：数组原样，字符串按「逗号 + 空白」拆（纯逗号不拆——文件名允许含逗号）。
pub fn as_refs(param: Option<&Param>) -> Vec<String> {
    let raw = match param {
        Some(Param::List(items)) => {
            return items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .collect()
        }
        Some(Param::Str(text)) => text.clone(),
        _ => return Vec::new(),
    };
    split_on_comma_space(&raw)
}

/// 按「逗号 + 空白」拆（正则 `,\s+` 的等价物）。
fn split_on_comma_space(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ',' {
            // 后面必须紧跟空白才算分隔符。
            match chars.peek() {
                Some(next) if next.is_whitespace() => {
                    parts.push(current.clone());
                    current.clear();
                    continue;
                }
                _ => {
                    current.push(ch);
                    continue;
                }
            }
        }
        current.push(ch);
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

// ── 引用解析（与 graph.ts 对齐）─────────────────────────────────────────

/// 外链判定：`scheme:` 或 `//` 开头。外链不是图上的边，跳过。
fn is_external_ref(raw: &str) -> bool {
    let text = raw.trim();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && (bytes[index].is_ascii_alphabetic()) {
        index += 1;
    }
    if index > 0 && index < bytes.len() && bytes[index] == b':' {
        return true;
    }
    text.starts_with("//")
}

/// 归一化文档引用：去 `<>`、剥 `#`/`?`、URL 解码、反斜杠转斜杠、折叠 `.`/`..`。
pub fn normalize_doc_ref(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    if text.starts_with('<') && text.ends_with('>') && text.len() >= 2 {
        text = text[1..text.len() - 1].trim().to_string();
    }
    if let Some(cut) = text.find(['#', '?']) {
        text.truncate(cut);
    }
    let text = percent_decode(&text);
    let mut segments: Vec<String> = Vec::new();
    for segment in text.replace('\\', "/").split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            segments.pop();
            continue;
        }
        segments.push(segment.to_string());
    }
    segments.join("/")
}

/// `decodeURI` 的近似实现：解 `%XX`，但保留 URI 保留字符的编码。
///
/// 保留集与 JS `decodeURI` 一致（`; / ? : @ & = + $ , #`）。差异会在
/// 「文件名里带 `%2F`」这种极端情况下体现，此时我们不会把它解码成路径分隔符
/// ——比起误判成断链，宁可保持原样再查表失败。
fn percent_decode(text: &str) -> String {
    const KEEP: &[u8] = b";/?:@&=+$,#";
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &text[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                if KEEP.contains(&byte) {
                    out.extend_from_slice(&bytes[index..index + 3]);
                } else {
                    out.push(byte);
                }
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// link 条目的解析结果。
#[derive(Debug, Default, PartialEq)]
pub struct LinkEntry {
    pub reference: String,
    pub label: Option<String>,
}

/// 解析 link 条目：`目标 | 标签 | t>b`（`|` 分隔，第二段可能是边覆盖）。
pub fn parse_link_entry(raw: &str) -> LinkEntry {
    let parts: Vec<&str> = raw.split('|').map(str::trim).collect();
    let mut entry = LinkEntry {
        reference: parts.first().copied().unwrap_or("").to_string(),
        label: None,
    };
    for segment in parts.iter().skip(1) {
        if is_side_segment(segment) {
            continue; // 边覆盖与检查无关
        }
        if !segment.is_empty() {
            entry.label = Some(segment.to_string());
        }
    }
    entry
}

/// 边覆盖段：`t>b` / `_>l` 之类（两侧各一个边代号或 `_`）。
fn is_side_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(from) = chars.next() else {
        return false;
    };
    if !matches!(from, 't' | 'b' | 'l' | 'r' | '_') {
        return false;
    }
    if chars.next() != Some('>') {
        return false;
    }
    let rest: String = chars.collect();
    matches!(rest.as_str(), "t" | "b" | "l" | "r" | "_")
}

// ── 图与检查 ────────────────────────────────────────────────────────────

/// 一篇文档的检查视图。
struct Doc {
    id: String,
    title: String,
    doc_path: String,
    links: Vec<String>,
    has_frame: bool,
    body_empty: bool,
    lines: usize,
    has_x: bool,
    has_y: bool,
    group_decls: usize,
}

/// 跑一遍检查。
///
/// `files` 就是 `bundle::scan` 的产物：扫描规则（跳过目录 / 扩展名 / 上限）
/// 与打开文档群时完全一致——否则会出现"面板里看得到、检查说没有"的分裂。
pub fn check(files: &BTreeMap<String, String>) -> Report {
    let mut report = Report::default();
    if files.is_empty() {
        report.issues.push(Issue {
            level: Level::Error,
            rule: "no-documents",
            message: "目录下没有任何 .md 文件".into(),
            file: None,
        });
        report.stats.errors = 1;
        return report;
    }

    let mut docs: Vec<Doc> = Vec::new();
    for (path, content) in files {
        let frame = parse_frame(content);
        let id = match frame.params.get("id").and_then(Param::as_str) {
            Some(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => path
                .strip_suffix(".md")
                .or_else(|| path.strip_suffix(".MD"))
                .unwrap_or(path)
                .to_string(),
        };
        let title = match frame.params.get("title").and_then(Param::as_str) {
            Some(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => first_h1(frame.body).unwrap_or_else(|| id.clone()),
        };
        let links = as_refs(frame.params.get("link"))
            .into_iter()
            .map(|raw| parse_link_entry(&raw).reference)
            .filter(|reference| !reference.is_empty())
            .collect();
        let group_decls = match frame.params.get("group") {
            Some(Param::List(items)) => items.len(),
            Some(_) => 1,
            None => 0,
        };
        docs.push(Doc {
            id,
            title,
            doc_path: path.clone(),
            links,
            has_frame: frame.has_frame,
            body_empty: frame.body.trim().is_empty(),
            lines: content.split('\n').count(),
            // 与 CLI 的 `typeof params.x === 'number'` 一致：写了 `x: abc`
            // 不算声明了坐标（布局会当它没写），报"只写单轴"才是对的。
            has_x: frame.params.get("x").and_then(Param::as_number).is_some(),
            has_y: frame.params.get("y").and_then(Param::as_number).is_some(),
            group_decls,
        });
    }

    // 重复 id：后者覆盖前者（与 CLI 一致：遍历顺序 = 路径字典序）。
    let mut by_id: HashMap<String, &Doc> = HashMap::new();
    let mut by_path: HashMap<String, &Doc> = HashMap::new();
    for doc in &docs {
        if let Some(previous) = by_id.get(&doc.id) {
            report.issues.push(Issue {
                level: Level::Error,
                rule: "duplicate-id",
                message: format!(
                    "重复 id \"{}\"：{} 被 {} 覆盖",
                    doc.id, previous.doc_path, doc.doc_path
                ),
                file: Some(doc.doc_path.clone()),
            });
        }
        by_id.insert(doc.id.clone(), doc);
        by_path.insert(doc.doc_path.clone(), doc);
    }

    // 解析引用（先按全局查表，失败再按声明方目录做相对解析）。
    let mut relations: BTreeSet<(String, String)> = BTreeSet::new();
    for doc in &docs {
        for reference in &doc.links {
            if is_external_ref(reference) {
                continue;
            }
            let normalized = normalize_doc_ref(reference);
            let from = by_id.get(&doc.id).copied();
            let Some(from) = from else { continue };
            let target = lookup(&normalized, &by_id, &by_path)
                .or_else(|| relative_lookup(&normalized, &doc.doc_path, &by_id, &by_path));
            let Some(target) = target else {
                report.issues.push(Issue {
                    level: Level::Error,
                    rule: "broken-link",
                    message: format!("连线 {}.link 引用了不存在的文档 \"{reference}\"", doc.id),
                    file: Some(doc.doc_path.clone()),
                });
                continue;
            };
            if from.id == target.id {
                report.issues.push(Issue {
                    level: Level::Error,
                    rule: "self-link",
                    message: format!("文档 \"{}\" 不能连线自身", from.id),
                    file: Some(doc.doc_path.clone()),
                });
                continue;
            }
            relations.insert((from.id.clone(), target.id.clone()));
        }
    }

    // 环：强连通分量里成员 > 1 即环（与 layout.ts 的判据一致）。
    for component in strongly_connected(&docs, &relations) {
        if component.len() > 1 {
            report.issues.push(Issue {
                level: Level::Warning,
                rule: "cycle",
                message: format!("检测到循环连线（{}）：环上文档排在同一层", component.join("、")),
                file: None,
            });
        }
    }

    // 连带：孤立判定要用到「作为端点出现过」。
    let mut linked: HashSet<&str> = HashSet::new();
    for (from, to) in &relations {
        linked.insert(from.as_str());
        linked.insert(to.as_str());
    }

    // 逐**框**检查，不是逐文件：重复 id 时后者覆盖前者（CLI 的
    // `graph.boxes = [...byId.values()]`），所以被覆盖的那份不该再出现一次
    // ——否则同一个问题会报两遍，而且报在一个"图上不存在"的框上。
    let mut boxes: Vec<&Doc> = by_id.values().copied().collect();
    boxes.sort_by(|a, b| a.doc_path.cmp(&b.doc_path));

    for doc in boxes {
        if doc.body_empty {
            report.issues.push(Issue {
                level: Level::Warning,
                rule: "empty-document",
                message: format!("「{}」正文为空（只有参数区）", doc.title),
                file: Some(doc.doc_path.clone()),
            });
        }
        if doc.lines > LONG_DOC_LINES {
            report.issues.push(Issue {
                level: Level::Warning,
                rule: "long-document",
                message: format!(
                    "「{}」有 {} 行，超过 {} 行建议拆分",
                    doc.title, doc.lines, LONG_DOC_LINES
                ),
                file: Some(doc.doc_path.clone()),
            });
        }
        if !doc.has_frame {
            report.issues.push(Issue {
                level: Level::Info,
                rule: "no-frame",
                message: format!(
                    "「{}」没有参数区，id/标题/坐标将由文件名与首个一级标题推断",
                    doc.title
                ),
                file: Some(doc.doc_path.clone()),
            });
        }
        if !linked.contains(doc.id.as_str()) {
            report.issues.push(Issue {
                level: Level::Warning,
                rule: "orphan",
                message: format!("「{}」没有任何连线（既非起点也非终点），在大图上会孤立漂浮", doc.title),
                file: Some(doc.doc_path.clone()),
            });
        }
        if doc.has_x != doc.has_y {
            report.issues.push(Issue {
                level: Level::Info,
                rule: "partial-position",
                message: format!(
                    "「{}」只声明了 {} 坐标，另一个轴会自动布局",
                    doc.title,
                    if doc.has_x { "x" } else { "y" }
                ),
                file: Some(doc.doc_path.clone()),
            });
        }
        if doc.group_decls > 1 {
            report.issues.push(Issue {
                level: Level::Warning,
                rule: "multi-group",
                message: format!("文档 \"{}\" 声明了多个 group，仅取第一个", doc.id),
                file: Some(doc.doc_path.clone()),
            });
        }
    }

    // 排序：error → warning → info；同级按文件路径，便于稳定对比。
    report
        .issues
        .sort_by(|a, b| a.level.cmp(&b.level).then_with(|| a.file.cmp(&b.file)));

    let errors = report
        .issues
        .iter()
        .filter(|issue| issue.level == Level::Error)
        .count();
    let warnings = report
        .issues
        .iter()
        .filter(|issue| issue.level == Level::Warning)
        .count();
    report.stats = Stats {
        // 与 CLI 的 `graph.boxes.length` 同义：**唯一 id 数**，不是文件数。
        documents: by_id.len(),
        relations: relations.len(),
        groups: by_id.values().filter(|doc| doc.group_decls > 0).count(),
        errors,
        warnings,
    };
    report
}

/// 超长阈值（与 CLI 的 `maxLines` 缺省一致）。
const LONG_DOC_LINES: usize = 800;

/// 查表：id → path → path.md → id（去掉 .md）。
fn lookup<'a>(
    reference: &str,
    by_id: &HashMap<String, &'a Doc>,
    by_path: &HashMap<String, &'a Doc>,
) -> Option<&'a Doc> {
    if reference.is_empty() {
        return None;
    }
    let with_ext = if reference.to_ascii_lowercase().ends_with(".md") {
        reference.to_string()
    } else {
        format!("{reference}.md")
    };
    by_id
        .get(reference)
        .copied()
        .or_else(|| by_path.get(reference).copied())
        .or_else(|| by_path.get(&with_ext).copied())
        .or_else(|| {
            let trimmed = with_ext
                .strip_suffix(".md")
                .or_else(|| with_ext.strip_suffix(".MD"))
                .unwrap_or(&with_ext);
            by_id.get(trimmed).copied()
        })
}

/// 相对解析：以声明方所在目录为基准再查一次（`../b.md`、`sub/c.md` 的自然写法）。
fn relative_lookup<'a>(
    reference: &str,
    from_doc_path: &str,
    by_id: &HashMap<String, &'a Doc>,
    by_path: &HashMap<String, &'a Doc>,
) -> Option<&'a Doc> {
    let index = from_doc_path.rfind('/')?;
    let dir = &from_doc_path[..=index];
    lookup(&format!("{dir}{reference}"), by_id, by_path)
}

/// 首个一级标题（跳过代码围栏内的 `#`）。
fn first_h1(body: &str) -> Option<String> {
    let mut fence: Option<char> = None;
    for line in body.split('\n') {
        let trimmed = line.trim_start();
        let fence_match = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if fence_match {
            let ch = trimmed.chars().next().unwrap_or('`');
            match fence {
                None => fence = Some(ch),
                Some(open) if open == ch => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim().trim_end_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("#\t") {
            let title = rest.trim().trim_end_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// 强连通分量（Tarjan），返回每个分量内的 id 列表。
fn strongly_connected(docs: &[Doc], relations: &BTreeSet<(String, String)>) -> Vec<Vec<String>> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for doc in docs {
        adjacency.entry(doc.id.as_str()).or_default();
    }
    for (from, to) in relations {
        adjacency.entry(from.as_str()).or_default().push(to.as_str());
    }

    struct State<'a> {
        index: usize,
        indices: HashMap<&'a str, usize>,
        low: HashMap<&'a str, usize>,
        on_stack: HashSet<&'a str>,
        stack: Vec<&'a str>,
        out: Vec<Vec<String>>,
    }

    fn visit<'a>(node: &'a str, adjacency: &HashMap<&'a str, Vec<&'a str>>, state: &mut State<'a>) {
        state.indices.insert(node, state.index);
        state.low.insert(node, state.index);
        state.index += 1;
        state.stack.push(node);
        state.on_stack.insert(node);

        if let Some(neighbours) = adjacency.get(node) {
            for &next in neighbours {
                if !state.indices.contains_key(next) {
                    visit(next, adjacency, state);
                    let low_next = state.low[next];
                    let low_node = state.low[node];
                    state.low.insert(node, low_node.min(low_next));
                } else if state.on_stack.contains(next) {
                    let index_next = state.indices[next];
                    let low_node = state.low[node];
                    state.low.insert(node, low_node.min(index_next));
                }
            }
        }

        if state.low[node] == state.indices[node] {
            let mut component = Vec::new();
            while let Some(top) = state.stack.pop() {
                state.on_stack.remove(top);
                component.push(top.to_string());
                if top == node {
                    break;
                }
            }
            component.sort();
            state.out.push(component);
        }
    }

    let mut state = State {
        index: 0,
        indices: HashMap::new(),
        low: HashMap::new(),
        on_stack: HashSet::new(),
        stack: Vec::new(),
        out: Vec::new(),
    };
    let mut nodes: Vec<&str> = adjacency.keys().copied().collect();
    nodes.sort_unstable();
    for node in nodes {
        if !state.indices.contains_key(node) {
            visit(node, &adjacency, &mut state);
        }
    }
    state.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, content)| (path.to_string(), content.to_string()))
            .collect()
    }

    fn rules(report: &Report) -> Vec<&'static str> {
        report.issues.iter().map(|issue| issue.rule).collect()
    }

    #[test]
    fn frame_block_must_start_at_byte_zero() {
        // 前有空行 → 不是参数区（CLI 同款）。
        let frame = parse_frame("\n---\nid: a\n---\n正文");
        assert!(!frame.has_frame);
        assert_eq!(frame.body, "\n---\nid: a\n---\n正文");

        let frame = parse_frame("---\nid: a\n---\n正文");
        assert!(frame.has_frame);
        assert_eq!(frame.params.get("id").and_then(Param::as_str), Some("a"));
        assert_eq!(frame.body, "正文");
    }

    /// 未闭合的 `---` 按"没有参数区"处理，不能把整个文件吃掉。
    #[test]
    fn unclosed_frame_falls_back_to_body() {
        let frame = parse_frame("---\nid: a\n正文");
        assert!(!frame.has_frame);
        assert!(frame.params.is_empty());
    }

    #[test]
    fn values_follow_cli_type_ladder() {
        let frame = parse_frame("---\nx: -12\ny: 3.5.ok\nw: 2.5\nflag: true\nq: \"a, b\"\n---\n");
        assert_eq!(frame.params["x"].as_number(), Some(-12.0));
        assert_eq!(frame.params["w"].as_number(), Some(2.5));
        assert_eq!(frame.params["flag"], Param::Bool(true));
        assert_eq!(frame.params["q"].as_str(), Some("a, b"));
        // `3.5.ok` 不是合法浮点 → 裸串
        assert_eq!(frame.params["y"].as_str(), Some("3.5.ok"));
    }

    #[test]
    fn multiline_list_is_collected() {
        let frame = parse_frame("---\nlink: [\n  \"a\",\n  \"b\",\n]\nid: x\n---\n");
        let links = as_refs(frame.params.get("link"));
        assert_eq!(links, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(frame.params.get("id").and_then(Param::as_str), Some("x"));
    }

    /// `asRefs` 只按「逗号 + 空白」拆：`a,b` 是一个（文件名可含逗号）。
    #[test]
    fn refs_split_on_comma_space_only() {
        let param = Param::Str("a.md, b.md".into());
        assert_eq!(as_refs(Some(&param)), vec!["a.md".to_string(), "b.md".to_string()]);
        let param = Param::Str("a,b.md".into());
        assert_eq!(as_refs(Some(&param)), vec!["a,b.md".to_string()]);
    }

    #[test]
    fn link_entry_splits_label_and_sides() {
        let entry = parse_link_entry("b.md | 下一步 | t>b");
        assert_eq!(entry.reference, "b.md");
        assert_eq!(entry.label.as_deref(), Some("下一步"));
        // 边覆盖段不算标签
        let entry = parse_link_entry("b.md | _>l");
        assert_eq!(entry.label, None);
    }

    #[test]
    fn normalize_strips_anchor_angle_and_backslashes() {
        assert_eq!(normalize_doc_ref("./a/b.md#sec"), "a/b.md");
        assert_eq!(normalize_doc_ref("<a.md>"), "a.md");
        assert_eq!(normalize_doc_ref("sub\\b.md"), "sub/b.md");
        assert_eq!(normalize_doc_ref("a/../b.md"), "b.md");
        assert_eq!(normalize_doc_ref("a.md?x=1"), "a.md");
    }

    /// 保留字符不解码：`%2F` 不该变成路径分隔符（decodeURI 语义）。
    #[test]
    fn percent_decode_keeps_reserved() {
        assert_eq!(percent_decode("a%2Fb"), "a%2Fb");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
    }

    #[test]
    fn external_refs_are_skipped() {
        assert!(is_external_ref("https://example.com"));
        assert!(is_external_ref("//cdn/x"));
        assert!(!is_external_ref("b.md"));
        assert!(!is_external_ref("sub/b.md"));
    }

    /// 正常的树形文档：无 error，只有可能的信息级提示。
    #[test]
    fn clean_tree_has_no_errors() {
        let report = check(&files(&[
            ("project.md", "---\nid: project\nlink: [\"a\"]\n---\n# 根\n"),
            ("a.md", "---\nid: a\n---\n# A\n"),
        ]));
        assert!(!report.has_errors(), "issues: {:?}", report.issues);
        assert_eq!(report.stats.documents, 2);
        assert_eq!(report.stats.relations, 1);
    }

    #[test]
    fn broken_link_is_an_error() {
        let report = check(&files(&[
            ("a.md", "---\nid: a\nlink: [\"ghost\"]\n---\n# A\n"),
        ]));
        assert!(report.has_errors());
        assert!(rules(&report).contains(&"broken-link"));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.message.contains("ghost")));
    }

    #[test]
    fn self_link_is_an_error() {
        let report = check(&files(&[("a.md", "---\nid: a\nlink: [\"a\"]\n---\n# A\n")]));
        assert!(rules(&report).contains(&"self-link"));
    }

    /// 重复 id：后者覆盖前者并报错（顺序按路径字典序，与 CLI 一致）。
    #[test]
    fn duplicate_id_is_an_error() {
        let report = check(&files(&[
            ("a.md", "---\nid: same\n---\n# A\n"),
            ("b.md", "---\nid: same\n---\n# B\n"),
        ]));
        let issue = report
            .issues
            .iter()
            .find(|issue| issue.rule == "duplicate-id")
            .expect("应报重复 id");
        assert!(issue.message.contains("a.md"), "{}", issue.message);
        assert!(issue.message.contains("b.md"), "{}", issue.message);
    }

    #[test]
    fn cycle_is_a_warning() {
        let report = check(&files(&[
            ("a.md", "---\nid: a\nlink: [\"b\"]\n---\n# A\n"),
            ("b.md", "---\nid: b\nlink: [\"a\"]\n---\n# B\n"),
        ]));
        let cycle = report
            .issues
            .iter()
            .find(|issue| issue.rule == "cycle")
            .expect("应报环");
        assert_eq!(cycle.level, Level::Warning);
        assert!(cycle.message.contains('a') && cycle.message.contains('b'));
        assert!(!report.has_errors(), "环是警告不是错误");
    }

    /// 三节点环只报**一个**分量（不是三条边各报一次）。
    #[test]
    fn cycle_reports_one_component_per_scc() {
        let report = check(&files(&[
            ("a.md", "---\nid: a\nlink: [\"b\"]\n---\n# A\n"),
            ("b.md", "---\nid: b\nlink: [\"c\"]\n---\n# B\n"),
            ("c.md", "---\nid: c\nlink: [\"a\"]\n---\n# C\n"),
        ]));
        let cycles: Vec<_> = report
            .issues
            .iter()
            .filter(|issue| issue.rule == "cycle")
            .collect();
        assert_eq!(cycles.len(), 1, "{:?}", cycles);
        assert!(cycles[0].message.contains("a、b、c") || cycles[0].message.contains("c"));
    }

    #[test]
    fn orphan_long_empty_and_partial_position_are_reported() {
        let long = "---\nid: long\nx: 1\n---\n".to_string() + &"正文\n".repeat(900);
        let report = check(&files(&[
            ("root.md", "---\nid: root\nlink: [\"long\"]\n---\n# 根\n"),
            ("long.md", &long),
            ("empty.md", "---\nid: empty\ny: 2\n---\n"),
            ("noframe.md", "# 只有正文\n"),
        ]));
        let found = rules(&report);
        assert!(found.contains(&"long-document"));
        assert!(found.contains(&"partial-position"));
        assert!(found.contains(&"orphan"));
        // 空正文：只有参数区
        assert!(found.contains(&"empty-document"));
        // 没有参数区
        assert!(found.contains(&"no-frame"));
    }

    /// 相对路径引用（`../b.md` / `sub/c.md`）必须解析成功，否则会误判断链。
    #[test]
    fn relative_links_resolve() {
        let report = check(&files(&[
            (
                "guide/a.md",
                "---\nid: a\nlink: [\"../top.md\", \"inner.md\"]\n---\n# A\n",
            ),
            ("guide/inner.md", "---\nid: inner\n---\n# I\n"),
            ("top.md", "---\nid: top\n---\n# Top\n"),
        ]));
        assert!(!report.has_errors(), "issues: {:?}", report.issues);
        assert_eq!(report.stats.relations, 2);
    }

    /// 外链不参与图，也不该报断链。
    #[test]
    fn external_links_are_ignored() {
        let report = check(&files(&[(
            "a.md",
            "---\nid: a\nlink: [\"https://example.com/x\"]\n---\n# A\n",
        )]));
        assert!(!report.has_errors(), "issues: {:?}", report.issues);
        assert_eq!(report.stats.relations, 0);
    }

    #[test]
    fn empty_directory_reports_no_documents() {
        let report = check(&BTreeMap::new());
        assert!(report.has_errors());
        assert_eq!(rules(&report), vec!["no-documents"]);
    }

    /// 排序稳定：error 先于 warning 先于 info。
    #[test]
    fn issues_are_sorted_by_level() {
        let report = check(&files(&[
            ("ghost.md", "---\nid: ghost\nlink: [\"nope\"]\n---\n"),
        ]));
        let levels: Vec<Level> = report.issues.iter().map(|issue| issue.level).collect();
        let mut sorted = levels.clone();
        sorted.sort();
        assert_eq!(levels, sorted, "{:?}", report.issues);
    }
}
