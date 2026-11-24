//! 命令侧的**业务策略**：谁可以发、哪些能广播、哪些只读身份可以执行。
//!
//! 线格式本体（`BackendCommand` / `PendingFile`）已上移到 `echo-protocol`；
//! 这里只留下策略，并原样再导出线类型，保持 `protocol::command::*` 旧路径可用。

pub use echo_protocol::command::*;

/// 多上游聚合下**适合广播**的幂等只读命令名集合（参考清单）。
///
/// 历史（准确性校准 2026-10 巡检）：中继曾按命令名白名单判别"能否无壳
/// 广播"，后已改为**纯意图透传**——投递范围由客户端显式声明
/// （`{"core": <name>}` 定向 / `{"broadcast": true}` 广播 / 无壳仅单上游
/// 直发），中继不做命令名分类，原镜像列表与其一致性测试均已移除。
/// 本清单保留价值：定义"哪些命令是幂等只读、可安全广播"的权威参考
/// （前端引导命令即从此类命令中选取），无跨仓强制同步义务；
/// 契约快照测试见本文件 `broadcast_readonly_contract_snapshot`。
pub const BROADCAST_READONLY_COMMANDS: &[&str] = &[
    // 前端引导命令组（顺序同 Panel 的 onopen）
    "RequestState",
    "RequestTeamsList",
    "RequestInstanceStatus",
    "RequestShellSessions",
    "RequestTrunkTimeline",
    // 清单 / 发现类
    "RequestModulesList",
    "RequestSkillsList",
    "RequestToolsList",
    "RequestSystemPrompt",
    "RequestRemoteApiProfiles",
    "RequestChatGptStatus",
    "RequestWorkspaceSessions",
    // 联邦与自更新
    "RequestFederationStatus",
    "RequestFederationDirectory",
    "RequestFederationInvite",
    "RequestSelfUpdateStatus",
];

/// **只读 Panel 身份**可执行的命令名集合（P1，2026-11）。
///
/// 背景：Panel 可以用**联邦级 Panel 身份密钥**接入任一节点（见
/// `[federation] panel_keys` / `panel_access`）。`readonly` 级别只允许
/// 观察，不允许驱动 agent 或改配置——这条边界必须在**服务端**强制
/// （客户端自律不算边界）。
///
/// 与 [`BROADCAST_READONLY_COMMANDS`] 的区别（两者**刻意**不同）：
/// - 那份的判据是"能否无壳广播"（含 `RequestFederationInvite`——它泄露
///   一份可用于配对的密钥，只读面板不该拿）；
/// - 这份的判据是"只观察、无副作用"：**不含**任何 SendMessage / Shell* /
///   Save* / Delete* / Toggle* / Migrate* / RequestSelfUpdate（会重启服务）
///   / RequestFederationInvite（泄露 token）。
///
/// fail-closed：未列出的命令一律拒绝。新增只读命令时若忘了登记，代价是
/// 「只读面板看不到它」——而不是「只读面板能写」。
pub const PANEL_READONLY_COMMANDS: &[&str] = &[
    // 状态与清单
    "RequestState",
    "RequestTeamsList",
    "RequestContext",
    "RequestModulesList",
    "RequestSkillsList",
    "RequestToolsList",
    "RequestSystemPrompt",
    "RequestRemoteApiProfiles",
    "RequestInstanceStatus",
    "RequestSelfUpdateStatus",
    // 会话与时间线（只读）
    "RequestShellSessions",
    "RequestTrunkTimeline",
    "RequestTimelineEarlier",
    "RequestSessionHistory",
    // 工作区（只读）
    "RequestWorkspaceSessions",
    "RequestWorkspaceGitStatus",
    "RequestWorkspaceFiles",
    "RequestBrowseDirectories",
    // ProDoc（只读：扫描 / 浏览 / 读单文件；Save/Delete 不在其列）
    "RequestProDocBundle",
    "BrowseProDocDirs",
    "ProDocRoots",
    "ReadProDocFile",
    "ProDocCheck",
    // 联邦（只读：状态与目录；邀请串与增删 peer 不在其列）
    "RequestFederationStatus",
    "RequestFederationDirectory",
];

/// 只读 Panel 身份能否执行该命令（名字口径，见 [`PANEL_READONLY_COMMANDS`]）。
pub fn is_panel_readonly_allowed(command_name: &str) -> bool {
    PANEL_READONLY_COMMANDS.contains(&command_name)
}

/// 命令名是否属于可广播只读集合（中继白名单的权威定义）。
pub fn is_broadcast_readonly(command_name: &str) -> bool {
    BROADCAST_READONLY_COMMANDS.contains(&command_name)
}

/// Who is allowed to issue a given command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClearance {
    /// Commands any frontend (Panel/TUI/API) may send.
    Frontend,
    /// Commands the agent/LLM may also trigger through its own tools.
    ///
    /// Reserved for future agent-originated command paths; today the agent
    /// never constructs [`BackendCommand`] values directly.
    Agent,
}

/// Return the command clearance level for `cmd`.
///
/// **类别判定（W2.3，2026-10）**：模块实例管理命令按**类别**判定——
/// 一切 [`BackendCommand::InvokeInstanceOp`] 都是 Frontend-only：人在环
/// 配置面，模型绝不可控。类别判定取代了「逐平台变体枚举」——新增管理
/// 方法不再需要记得加枚举臂（消除「忘加枚举臂」回归窗口，fail-closed）。
pub fn command_clearance(cmd: &BackendCommand) -> CommandClearance {
    match cmd {
        // 模块实例管理命令：类别判定（任意平台、任意方法）。
        BackendCommand::InvokeInstanceOp { .. }
        // 通用模块调用：同上——任意模块、任意方法都是人在环的管理/读取面。
        | BackendCommand::InvokeModuleOp { .. }
        // 面板服务调用（2026-11）：按服务名寻址的同一条通道，同样是人在环的
        // 读取面。模块只把**只读**能力登记成服务（写操作走各自模块的私有
        // 方法），所以作用面比 InvokeModuleOp 更窄。
        | BackendCommand::InvokeService { .. }
        | BackendCommand::SaveWorkspaceSession { .. }
        | BackendCommand::DeleteWorkspaceSession { .. }
        // 工作区 git fetch：**写**操作（连远端、改 .git/FETCH_HEAD）。
        // 与上面那条只读的 RequestWorkspaceGitStatus 分开正是为了这里能把它
        // 排除出 PANEL_READONLY_COMMANDS、并且不让模型触发（见该命令的文档）。
        | BackendCommand::FetchWorkspaceGit { .. }
        | BackendCommand::ActivateWorkspaceSession { .. }
        | BackendCommand::SaveFederationPeer { .. }
        | BackendCommand::DeleteFederationPeer { .. }
        | BackendCommand::RequestFederationStatus
        | BackendCommand::RequestFederationDirectory
        | BackendCommand::RequestFederationInvite
        | BackendCommand::MigrateSession { .. }
        | BackendCommand::RequestSelfUpdate
        | BackendCommand::RequestSelfUpdateStatus
        | BackendCommand::ClearHistory { .. }
        // ChatGPT 订阅登录：**人在环**的操作，模型绝不可触发——登录态就是
        // 账户凭证，让 agent 自主登录等于把凭证的获取权交给提示注入。
        | BackendCommand::StartChatGptLogin
        | BackendCommand::CancelChatGptLogin
        | BackendCommand::SubmitChatGptLoginCode { .. }
        | BackendCommand::RequestChatGptStatus => CommandClearance::Frontend,
        _ => CommandClearance::Agent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 集合里的每个名字都必须真的是一个 `BackendCommand` 变体。
    ///
    /// 回归背景（2026-10 审计发现）：清单里曾长期躺着 `"Ping"`——`BackendCommand` 没有这个
    /// 变体，也没有任何发送方（Panel 的探活走 WS 层 Ping 控制帧，不经命令通道）。它留在
    /// 「权威参考」里会误导读者以为协议有探活命令。快照测试只比对字面量列表，抓不到这种
    /// 「名字根本不是命令」的情况——补充这条不变量。
    #[test]
    fn every_broadcast_readonly_name_is_a_real_command_variant() {
        // serde 的 unknown-variant 错误里带着全部合法变体名（"expected one of ..."）。
        let error = serde_json::from_str::<BackendCommand>(r#""__not_a_command__""#)
            .expect_err("unknown command must fail to deserialize")
            .to_string();
        // 末尾会带上 "` at line 1 column 7"，先切掉，否则最后一个变体名会被污染。
        let list = error
            .split("expected one of ")
            .nth(1)
            .unwrap_or_default()
            .split("` at line")
            .next()
            .unwrap_or_default();
        let variants: Vec<String> = list
            .split(',')
            .map(|s| s.trim().trim_matches('`').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(
            variants.len() > 50,
            "变体名单解析失败（解析到 {} 个）：{error}",
            variants.len()
        );
        for name in super::BROADCAST_READONLY_COMMANDS {
            assert!(
                variants.iter().any(|v| v == name),
                "BROADCAST_READONLY_COMMANDS 里的 `{name}` 不是 BackendCommand 的变体"
            );
        }
    }

    /// 可广播只读集合的契约快照：改动必须与中继 `is_readonly_command` 同步。
    #[test]
    fn broadcast_readonly_contract_snapshot() {
        let mut names = super::BROADCAST_READONLY_COMMANDS.to_vec();
        names.sort();
        assert_eq!(
            names,
            vec![
                "RequestChatGptStatus",
                "RequestFederationDirectory",
                "RequestFederationInvite",
                "RequestFederationStatus",
                "RequestInstanceStatus",
                "RequestModulesList",
                "RequestRemoteApiProfiles",
                "RequestSelfUpdateStatus",
                "RequestShellSessions",
                "RequestSkillsList",
                "RequestState",
                "RequestSystemPrompt",
                "RequestTeamsList",
                "RequestToolsList",
                "RequestTrunkTimeline",
                "RequestWorkspaceSessions",
            ]
        );
        assert!(super::is_broadcast_readonly("RequestState"));
        assert!(!super::is_broadcast_readonly("SendMessage"));
    }

    /// 只读 Panel 身份的边界（P1）：能看、不能写、不能拿密钥。
    ///
    /// 这条测试是**安全契约**——放宽它等于给只读身份开写权限：
    /// ① 每个登记项必须是真的命令变体（防止像历史上 `"Ping"` 那样
    ///    留一个不存在的名字）；② 明确的写命令/密钥泄露命令必须**不在**
    ///    名单里。
    #[test]
    fn panel_readonly_gate_excludes_writes_and_secrets() {
        // ① 登记项都是真变体（借用 serde 的 unknown-variant 报错名单）。
        let error = serde_json::from_str::<BackendCommand>(r#""__not_a_command__""#)
            .expect_err("unknown command must fail to deserialize")
            .to_string();
        let list = error
            .split("expected one of ")
            .nth(1)
            .unwrap_or_default()
            .split("` at line")
            .next()
            .unwrap_or_default();
        let variants: Vec<String> = list
            .split(',')
            .map(|s| s.trim().trim_matches('`').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(variants.len() > 50, "变体名单解析失败：{error}");
        for name in super::PANEL_READONLY_COMMANDS {
            assert!(
                variants.iter().any(|v| v == name),
                "PANEL_READONLY_COMMANDS 里的 `{name}` 不是 BackendCommand 的变体"
            );
        }
        // ② 写/密钥类必须被拒（fail-closed 反例清单）。
        for forbidden in [
            "SendMessage",
            "ShellStart",
            "ShellExec",
            "ShellStop",
            "SaveFederationPeer",
            "DeleteFederationPeer",
            "RequestFederationInvite",
            "MigrateSession",
            "RequestSelfUpdate",
            "ClearHistory",
            "SaveTeam",
            "StartInstance",
            "StopInstance",
            "InstallSkillFromGit",
            "SaveWorkspaceSession",
            "ActivateWorkspaceSession",
        ] {
            assert!(
                !super::is_panel_readonly_allowed(forbidden),
                "{forbidden} 必须被只读 Panel 身份拒绝"
            );
        }
        // ③ 观察类放行。
        assert!(super::is_panel_readonly_allowed("RequestState"));
        assert!(super::is_panel_readonly_allowed(
            "RequestFederationDirectory"
        ));
    }

    /// 模块实例管理命令的**类别判定**（W2.3）：`InvokeInstanceOp` 一律
    /// Frontend-only——不依赖逐变体枚举（新增方法零维护）。
    #[test]
    fn instance_management_commands_are_frontend_only_by_category() {
        for (method, payload) in [
            ("set_gate_mode", serde_json::json!({"mode": "allowlist"})),
            ("update_allowlist", serde_json::json!({"user_ids": [1]})),
            ("set_owner", serde_json::json!({"owner_qq": 123})),
            ("login_qrcode", serde_json::json!({})),
            ("future_unknown_method", serde_json::json!({})),
        ] {
            assert_eq!(
                command_clearance(&BackendCommand::InvokeInstanceOp {
                    instance: "qq".into(),
                    method: method.into(),
                    payload,
                }),
                CommandClearance::Frontend,
                "adapter op {method} must be frontend-only"
            );
        }
    }

    /// 通用模块调用的线形状：`module` / `method` 必填，`payload` 可缺席
    /// （缺省空对象）。**`payload` 是自由 JSON**——宿主不认识里面的字段，
    /// 这正是这条通道存在的理由（模块的业务词汇不进 Core 协议）。
    #[test]
    fn invoke_module_op_wire_shape_is_generic() {
        let with_payload: BackendCommand = serde_json::from_str(
            r#"{"InvokeModuleOp":{"module":"echo-agent.checklist","method":"checklist.snapshot","payload":{"team_id":"alix"}}}"#,
        )
        .unwrap();
        assert_eq!(
            with_payload,
            BackendCommand::InvokeModuleOp {
                module: "echo-agent.checklist".into(),
                method: "checklist.snapshot".into(),
                payload: serde_json::json!({"team_id": "alix"}),
            }
        );
        // payload 缺席合法（模块自己决定"没有参数"是什么意思）。
        let no_payload: BackendCommand =
            serde_json::from_str(r#"{"InvokeModuleOp":{"module":"m","method":"x"}}"#).unwrap();
        assert_eq!(
            no_payload,
            BackendCommand::InvokeModuleOp {
                module: "m".into(),
                method: "x".into(),
                payload: serde_json::Value::Null,
            }
        );
        // module / method 必填：缺了就是接线错误，不能默默调空。
        assert!(
            serde_json::from_str::<BackendCommand>(r#"{"InvokeModuleOp":{"method":"x"}}"#).is_err()
        );
        assert!(
            serde_json::from_str::<BackendCommand>(r#"{"InvokeModuleOp":{"module":"m"}}"#).is_err()
        );
        // 通用调用是**人在环**的面（任意模块、任意方法）：不是只读广播命令。
        assert!(!is_broadcast_readonly("InvokeModuleOp"));
        assert_eq!(
            command_clearance(&BackendCommand::InvokeModuleOp {
                module: "m".into(),
                method: "x".into(),
                payload: serde_json::Value::Null,
            }),
            CommandClearance::Frontend
        );
    }
}
