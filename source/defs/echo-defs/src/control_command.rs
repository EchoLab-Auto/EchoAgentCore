//! 平台无关的**控制命令词汇**：把一条用户输入识别成「停止任务」之类的意图。
//!
//! ## 为什么这属于定义层
//!
//! 此前这套词表住在宿主 `echo-agent/src/packages/instance_ops/bridge.rs`。
//! 但它是**跨边界的输入语义**，不是宿主的策略：
//!
//! - 每条入站消息都要过一遍它（判断"这是一条命令，还是交给模型"）；
//! - 判据是**用户打了什么字**，与平台、与 agent 的运行时状态都无关；
//! - 其它入口（Panel 的 `SendMessage`、未来新增的平台实例）若各写一份，
//!   "取消任务"在一个平台生效、在另一个不生效——而这种不一致很难被用户
//!   理解为"设计如此"。
//!
//! 归到定义层后，词表只有一份；宿主只负责**执行**（按令牌取消 turn），
//! 不负责**识别**。
//!
//! ## 匹配口径（刻意严格）
//!
//! 只认**整条输入等于某个词**，不认"包含"：
//!
//! - `"取消当前任务"` → `Some(false)`（停最新一个）
//! - `"取消任务应该如何实现？"` → `None`（这是个问题，交给模型）
//! - `"不要取消，继续执行"` → `None`
//!
//! 归一化只做三件事：去首尾空白、转小写、删掉全部空白与中英文标点。
//! `-` **不在**标点表里，所以 `"/cancel-all"` 不会被认成 `"/cancelall"`
//! ——斜杠命令的具体拼法由前端定义，这里只认两张表里逐字写下的形式。
//!
//! ## 为什么判定"整条等于"而不是"以…开头"
//!
//! 以"停止"开头的正常句子远多于真正的命令。伪装成命令的自由文本会让
//! agent 静默不回复——用户以为自己发的消息被吞了。宁可漏判（交给模型），
//! 不可误判。

/// 归一化：去首尾空白、转小写、删掉空白与常见中英文标点。
///
/// 只删**标点**不删其它字符：这样 `"取消任务！"` 与 `"取消任务"` 等价，
/// 而 `"取消任务blabla"` 仍然是 `None`。
fn normalize(content: &str) -> String {
    content
        .trim()
        .to_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace() && !"，。！？,.!?；;：:".contains(*ch))
        .collect()
}

/// 需要停掉**全部**进行中任务的输入。
///
/// 集合刻意短：这些都是不可能被误认成正常提问的短语。
const CANCEL_ALL: [&str; 8] = [
    "全部停止",
    "停止全部任务",
    "取消全部任务",
    "撤回全部任务",
    "终止全部任务",
    "cancelall",
    "/cancelall",
    "/stopall",
];

/// 只需停掉**最新一个**任务的输入。
///
/// 含斜杠形式（前端定义的快捷命令）与完整动词短语；**不含**裸的
/// `"停"` / `"取消"` / `"stop"` / `"cancel"` ——那些太容易是正常对话
/// （"停"可以是"停车"的前缀，"取消"可以是讨论对象）。
const CANCEL_ONE: [&str; 13] = [
    "停止任务",
    "停止当前任务",
    "停下当前任务",
    "取消任务",
    "取消当前任务",
    "取消刚才的任务",
    "撤回任务",
    "撤回刚才的任务",
    "终止任务",
    "终止当前任务",
    "/cancel",
    "/stop",
    "/abort",
];

/// 把一条用户输入识别成取消意图。
///
/// - `Some(true)` = 停止**全部**进行中的任务；
/// - `Some(false)` = 只停**最新一个**；
/// - `None` = 不是取消命令（照常交给模型）。
///
/// `"全部停止"` 这类泛词优先按"全部"处理；两张表若有重叠，`CANCEL_ALL`
/// 先匹配（调用方按此顺序判断即可，本函数已按此实现）。
pub fn cancel_scope(content: &str) -> Option<bool> {
    let normalized = normalize(content);
    if CANCEL_ALL.contains(&normalized.as_str()) {
        return Some(true);
    }
    CANCEL_ONE
        .contains(&normalized.as_str())
        .then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_all_phrases_are_recognised() {
        for input in [
            "全部停止",
            "停止全部任务",
            "取消全部任务",
            "/stopall",
            "/cancelall",
            "cancelall",
        ] {
            assert_eq!(cancel_scope(input), Some(true), "{input}");
        }
    }

    #[test]
    fn cancel_one_phrases_are_recognised() {
        for input in [
            "取消当前任务",
            "停止任务",
            " /cancel ",
            "/stop",
            "/abort",
        ] {
            assert_eq!(cancel_scope(input), Some(false), "{input}");
        }
    }

    /// **误判比漏判更糟**：正常提问/否定句必须交给模型，不能被吞掉。
    #[test]
    fn ordinary_text_is_never_a_command() {
        for input in [
            "取消任务应该如何实现？",
            "不要取消，继续执行",
            "停",
            "取消",
            "stop",
            "cancel",
            "算了",
            "不用了",
            " /cancel-all ",
        ] {
            assert_eq!(cancel_scope(input), None, "{input}");
        }
    }

    /// 归一化只吃空白与标点：多打了标点照样认。
    #[test]
    fn trailing_punctuation_is_normalised() {
        assert_eq!(cancel_scope("取消当前任务！"), Some(false));
        assert_eq!(cancel_scope("  取消当前任务。 "), Some(false));
        assert_eq!(cancel_scope("取消全部任务！"), Some(true));
    }

    /// 前缀/后缀扩展不匹配（`"取消任务blabla"` 不是命令）。
    #[test]
    fn prefix_and_suffix_extensions_do_not_match() {
        assert_eq!(cancel_scope("取消任务blabla"), None);
        assert_eq!(cancel_scope("请取消任务"), None);
    }
}
