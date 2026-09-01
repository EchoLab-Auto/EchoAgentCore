---
id: adr-0011
title: "ADR-0011 QQ 管理员设置"
group: 架构决策
x: 1614
y: 1097
---
# ADR-0011: QQ 管理员运行时设置(SetQqOwner)

状态: accepted

## 问题

`owner_qq`(QQ 管理员)只能通过 `install.sh --owner-qq` 在安装时经 `sed`
写入配置文件。部署后要更换管理员必须编辑配置文件并重启 Core——没有运行时
更新路径,与门控/名单(已有 `SetQqGateMode`/`UpdateQqAllowlist` 运行时命令)
不对称。

## 决策

打通一条端到端链路,使 owner 可在 `/qq setting` 中运行时设置并持久化:

1. **协议**(echo-protocol):新增 `BackendCommand::SetQqOwner { owner_qq: i64 }`
   (向后兼容,新字段/变体)。
2. **适配器**(echo-adapter-qq):
   - `QqInner` 加运行时 `owner_qq: StdMutex<i64>`(初始化自 config);
   - `set_owner_qq` 更新运行时值并持久化 `[adapters.qq] owner_qq` 到共享
     ConfigStore;`get_owner_qq` 读运行时值;
   - 两处门控豁免读取(`get_gated_friend_list`、出站门控)改用运行时值;
   - `echo_adapter::Adapter` trait 加 `set_owner_qq`/`get_owner_qq` 默认
     方法(no-op),QQ 实现覆盖。
3. **命令分发**(echo-agent):`apply_qq_command` 处理 `SetQqOwner`(成功/未找到
   适配器提示),主分发器委托。
4. **Panel**:`/qq setting` 菜单加"👑 设置管理员 (owner)"项,输入 QQ 号后
   发送 `SetQqOwner`;`handle_set_owner` 校验数字并提示。

owner 语义保持:门控豁免(始终可用)+ 自更新授权(install.sh 时并入
`[agent.self_update]` 的机制不变;运行时设置的 owner 仅影响门控豁免,自更新
授权仍由 `[agent.self_update]` 控制——见已知限制)。

## 备选方案

- **只改配置文件+重启**:无运行时路径,与门控/名单不对称。
- **复用现有命令变体**:没有合适的现成命令;独立 `SetQqOwner` 语义清晰。

## 后果

- owner 可运行时设置/清除(`0` 清除)并持久化,无需重启。
- 行为零变化:Core 473 / Panel 178 测试全绿(新增 3+3 个 owner 测试);
  门控豁免语义原样。
- 已知限制:运行时设置的 owner 自动进入自更新授权仍需后续接入
  (`[agent.self_update].allowed_qq_users` 与 owner 的联动),当前仅门控豁免。

## 后续变更

- `scripts/install.sh` 的 `--owner-qq` 参数已移除:运行时路径成为设置 owner
  的唯一入口(`/qq setting` → 👑 设置管理员,或直接编辑 `[adapters.qq]`),
  安装脚本不再改写配置。
