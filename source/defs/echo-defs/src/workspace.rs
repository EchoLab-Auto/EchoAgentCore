//! 跨机工作区查询的**接缝词汇**（2026-11 S12）。
//!
//! ## 为什么在定义层
//!
//! 工作区目录可以是 `node://<peer>/<path>`——**对端机器**上的目录。查看它的
//! git 状态、列文件、浏览子目录都要问到对端，而那份传输属联邦子系统。
//!
//! 三方关系：
//!
//! | 角色 | 谁 | 需要什么 |
//! |---|---|---|
//! | 声明（消费方） | `echo-plugin-workspace` | "我要问这三个问题" |
//! | 实现方 | 组合根（把联邦的 `remote_querier` 适配过来） | 联邦 `QueryKind` |
//! | 旁证 | `echo-plugin-federation` | 只是**转手**这个 trait |
//!
//! 此前 trait 住在 workspace 插件里，于是联邦插件要
//! `use echo_plugin_workspace::{QueryFuture, RemoteWorkspaceQuery}` 才能实现它
//! ——两个插件因为"三个查询语义"结了 Cargo 依赖。语义本身（问 git / 问文件 /
//! 问目录）不属于任何一方，所以它的家在定义层。
//!
//! **未注入 = 降级**：远程目录显示「远程不可采集」占位，本机功能不受影响
//! （单机部署不需要这个接缝）。

use std::future::Future;
use std::pin::Pin;

/// 一次远程查询的 future（结果 = 对端回推的 JSON 载荷）。
pub type QueryFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>;

/// 跨机工作区查询接缝（宿主注入）。
///
/// 为什么不直接暴露联邦的 `QueryKind`：消费方不该为了三个查询而依赖整个联邦
/// 协议 crate；语义方法也让宿主侧的实现变成一行映射，而不是字符串拼接。
pub trait RemoteWorkspaceQuery: Send + Sync {
    /// 对端某目录的 git 状态（`{git: WorkspaceGitInfo}`）。
    fn git_status(&self, peer: String, path: String) -> QueryFuture;

    /// 对端某目录的文件清单（`Vec<WorkspaceFileEntry>`）。
    fn list_files(&self, peer: String, path: String) -> QueryFuture;

    /// 对端目录浏览（`{roots, entries}`；空路径 = 根列表）。
    fn browse(&self, peer: String, path: String) -> QueryFuture;
}
