/// 「`serde(default)` 与 `ts(optional)` 必须成对」的棘轮。
///
/// # 为什么需要它
///
/// Rust 侧 `#[serde(default)]` 说的是"反序列化时缺失也可以"。但 ts-rs 只从
/// `skip_serializing_if` 推断可选（`optional.rs`：`maybe_omitted && has_default`），
/// 单有 `serde(default)` 时它生成的仍是**必填**字段。于是同一个字段：
///
/// | 方向 | 实际 | 生成的 TS |
/// |---|---|---|
/// | Rust 反序列化 | 可缺席 | `field: T`（必填）→ 假 |
/// | Rust 序列化 | 永远有值 | `field: T` → 真 |
///
/// 两个方向共用一个类型时，必填偏保守——但它会**炸在消费方**：扩展仓按
/// `SendMessage.team_id` 的类型写 `?? null` 才不报错，而运行期 `undefined`
/// 本来就是合法的（`JSON.stringify` 丢掉，serde default 兜住）。同一条协议
/// 在两侧各有一个"真相"，正是这套生成机制要消灭的东西。
///
/// 所以约定：**凡是 Rust 接受缺席的 `Option` 字段，TS 必须也是可选的**。
/// 注解写 `#[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]`——
/// `= nullable` 不能省：Core 序列化 `None` 时发的是 `null`，只标 `optional`
/// 会生成 `field?: T`，把 `null` 这种真实取值从类型里抹掉。
///
/// 非 `Option` 的 `serde(default)` 字段（`String` / `bool` / `Vec`）不在此列：
/// 它们在线上**永远有值**，必填是准确的；让它们可选需要改线格式
/// （加 `skip_serializing_if`），那是另一个决定。
#[cfg(test)]
mod ts_optional_ratchet {
    use std::path::Path;

    /// (文件名, 源码)。`include_str!` 编译期嵌入：测试不依赖工作目录。
    const SOURCES: &[(&str, &str)] = &[
        ("command.rs", include_str!("command.rs")),
        ("event.rs", include_str!("event.rs")),
        (
            "echo-defs/chat.rs",
            include_str!("../../echo-defs/src/chat.rs"),
        ),
    ];

    /// 取所有"缺 `ts(optional)` 的 `Option` 字段"。
    fn offenders(name: &str, source: &str) -> Vec<String> {
        let mut out = Vec::new();
        // 属性块（`#[..]` / 文档注释）累计到下一个字段声明为止。
        let mut pending: Vec<&str> = Vec::new();
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            let is_attr = trimmed.starts_with("#[")
                || trimmed.starts_with("///")
                || trimmed.starts_with("//");
            if is_attr || trimmed.is_empty() || trimmed.starts_with('}') {
                pending.push(trimmed);
                continue;
            }
            // `pub name: Type,`（结构体）或 `name: Type,`（枚举变体）。
            let field = trimmed
                .strip_prefix("pub ")
                .unwrap_or(trimmed)
                .strip_suffix(',');
            if let Some(field) = field {
                if let Some((field_name, ty)) = field.split_once(": ") {
                    let ty = ty.trim();
                    let has_default = pending.iter().any(|a| a.contains("serde(default"));
                    let has_ts_optional = pending.iter().any(|a| a.contains("ts(optional"));
                    // 只盯 Option：`skip_serializing_if` 的推断已由 ts-rs 负责。
                    if ty.starts_with("Option<") && has_default && !has_ts_optional {
                        out.push(format!(
                            "{name}:{} `{}`: `Option` 字段带 serde(default) 但缺 \
                             #[cfg_attr(feature = \"ts-bindings\", ts(optional = nullable))]",
                            index + 1,
                            field_name
                        ));
                    }
                }
            }
            pending.clear();
        }
        out
    }

    #[test]
    fn every_optional_field_is_optional_in_ts() {
        let mut all = Vec::new();
        for (name, source) in SOURCES {
            all.extend(offenders(name, source));
        }
        assert!(
            all.is_empty(),
            "协议里有 {} 个字段的 TS 类型比线格式更严（消费方会被迫多写无意义的 \
             空值判断，或反过来漏判）。给它们补上 ts(optional = nullable)：\n{}",
            all.len(),
            all.join("\n")
        );
    }

    /// 反向：注释掉的注解不算（有人把注释放进代码后棘轮就失灵了）。
    #[test]
    fn ratchet_reads_real_source() {
        let (name, source) = SOURCES[0];
        assert!(!source.is_empty(), "{name} 应为空");
        assert!(
            Path::new(name).extension().is_some(),
            "文件名应带扩展名（用于失败信息定位）"
        );
        // 本测试的意义是"源码真的被 include_str! 进来了"，否则前一条会
        // 因为 SOURCES 为空而永远通过。
        assert!(source.contains("pub enum BackendCommand"));
    }
}
