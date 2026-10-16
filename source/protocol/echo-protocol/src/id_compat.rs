//! W2.4 迁移期兼容层：线协议 id 字段（`i64` → `String` 改型）的「双读」。
//!
//! 决策与范围（01b §A）：**命令侧双读**——旧客户端照常可发 JSON number，
//! 新客户端发 string；两者统一归一为 `String`（number 走 `to_string()`）。
//! **事件侧纯 breaking**——事件由 Core 序列化、Panel 消费，只输出 string
//! （无法「双写」同一字段），靠同批发布兜底；故本模块只服务
//! [`crate::command::BackendCommand`]。
//!
//! 不引入 `#[serde(untagged)]` 枚举（会把类型噪声扩散到消费点）。

use serde::{Deserialize, Deserializer};

/// 单个 id：接受 JSON number 或 string，归一为 `String`。
pub fn deserialize_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match id_from_value(&value) {
        Some(id) => Ok(id),
        None => Err(<D::Error as serde::de::Error>::custom(
            "id must be a JSON number or string",
        )),
    }
}

/// id 列表：逐元素接受 JSON number 或 string，归一为 `Vec<String>`。
pub fn deserialize_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    values
        .iter()
        .map(|value| match id_from_value(value) {
            Some(id) => Ok(id),
            None => Err(<D::Error as serde::de::Error>::custom(
                "id must be a JSON number or string",
            )),
        })
        .collect()
}

/// number → `to_string()`；string → 原样；其它形态 → `None`（拒绝）。
fn id_from_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// 直接经 serde 派生验证双读助手（不经命令枚举）。
    #[derive(Deserialize)]
    struct Holder {
        #[serde(deserialize_with = "deserialize_id")]
        id: String,
        #[serde(deserialize_with = "deserialize_ids")]
        ids: Vec<String>,
    }

    #[test]
    fn accepts_numbers_and_strings_and_mixed_lists() {
        let holder: Holder = serde_json::from_str(r#"{"id": 123, "ids": [1, "2", 3]}"#).unwrap();
        assert_eq!(holder.id, "123");
        assert_eq!(holder.ids, vec!["1", "2", "3"]);

        let holder: Holder = serde_json::from_str(r#"{"id": "123", "ids": ["1", "2"]}"#).unwrap();
        assert_eq!(holder.id, "123");
        assert_eq!(holder.ids, vec!["1", "2"]);
    }

    #[test]
    fn rejects_null_and_objects() {
        let err = serde_json::from_str::<Holder>(r#"{"id": null, "ids": []}"#);
        assert!(err.is_err());
        let err = serde_json::from_str::<Holder>(r#"{"id": 1, "ids": [{"x": 1}]}"#);
        assert!(err.is_err());
    }
}
