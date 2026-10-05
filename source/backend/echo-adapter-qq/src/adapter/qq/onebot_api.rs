//! OneBot HTTP API 客户端方法群（出站调用封装）。
//!
//! 从 `adapter/qq/mod.rs` 拆出（框架优化议题 3）：群/好友列表、成员
//! 信息、文件上传等纯 OneBot HTTP 调用——本层**不懂门控**（门控在
//! 调用方：`impl Adapter` 的 get_groups/get_friend_list 与 send 路径）。

use super::*;

impl QqAdapter {
    /// Get ALL groups without gate-mode filtering (privileged — used by TUI).
    pub async fn get_all_groups(&self) -> Result<Vec<(i64, String)>, String> {
        // Add diagnostic logging
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let running = self.inner.running.load(Ordering::SeqCst);
        let has_context = self
            .inner
            .active_context
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false);

        tracing::info!(
            "get_all_groups called - connected: {}, running: {}, has_context: {}, self_id: {:?}",
            connected,
            running,
            has_context,
            self.inner.self_id.lock().ok()
        );

        // First check if QQ adapter is running
        if !running {
            return Err("QQ适配器未启动，请检查配置".to_string());
        }

        let napcat = NapCatClient::with_onebot_url(
            &self.inner.config.napcat_webui_url,
            &self.inner.config.napcat_onebot_url,
        );
        match napcat.get_group_list().await {
            Ok(groups) => return Ok(groups),
            Err(error) => {
                tracing::debug!(%error, "OneBot HTTP group list unavailable; trying WebSocket");
            }
        }

        // Fall back to reverse WebSocket when the HTTP API is unavailable.
        if !connected {
            let self_id = self.inner.self_id.lock().ok().and_then(|g| g.clone());
            return Err(format!(
                "QQ WebSocket未连接{}，请先使用 /qq login 登录",
                self_id
                    .map(|id| format!(" (账号: {id})"))
                    .unwrap_or_default()
            ));
        }

        // Check for active context and provide guidance if missing
        let ctx_opt = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone();

        let ctx = match ctx_opt {
            Some(ctx) => ctx,
            None => {
                // QQ is connected but API context not yet activated
                return Err(
                    "QQ已连接但API上下文尚未激活，请发送任意消息到QQ群组来激活API功能".to_string(),
                );
            }
        };

        let request = echo_core::action::actions::get_group_list();
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        let groups: Vec<(i64, String)> = resp
            .data
            .as_array()
            .ok_or("unexpected response format")?
            .iter()
            .filter_map(|v| {
                let gid = v.get("group_id")?.as_i64()?;
                let name = v
                    .get("group_name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((gid, name))
            })
            .collect();
        Ok(groups)
    }

    /// Get the QQ friend list via the active QQ connection.
    pub async fn get_friend_list(&self) -> Result<Vec<(i64, String)>, String> {
        // Add diagnostic logging
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let running = self.inner.running.load(Ordering::SeqCst);
        let has_context = self
            .inner
            .active_context
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false);

        tracing::info!(
            "get_friend_list called - connected: {}, running: {}, has_context: {}, self_id: {:?}",
            connected,
            running,
            has_context,
            self.inner.self_id.lock().ok()
        );

        // First check if QQ adapter is running
        if !running {
            return Err("QQ适配器未启动，请检查配置".to_string());
        }

        let napcat = NapCatClient::with_onebot_url(
            &self.inner.config.napcat_webui_url,
            &self.inner.config.napcat_onebot_url,
        );
        match napcat.get_friend_list().await {
            Ok(friends) => return Ok(friends),
            Err(error) => {
                tracing::debug!(%error, "OneBot HTTP friend list unavailable; trying WebSocket");
            }
        }

        // Fall back to reverse WebSocket when the HTTP API is unavailable.
        if !connected {
            let self_id_opt = self.inner.self_id.lock().ok().and_then(|g| g.clone());
            return Err(format!(
                "QQ WebSocket未连接{}，请先使用 /qq login 登录",
                self_id_opt
                    .map(|id| format!(" (账号: {id})"))
                    .unwrap_or_default()
            ));
        }
        let ctx_opt = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone();

        let ctx = match ctx_opt {
            Some(ctx) => ctx,
            None => {
                // QQ is connected but API context not yet activated
                return Err(
                    "QQ已连接但API上下文尚未激活，请发送任意消息到QQ群组来激活API功能".to_string(),
                );
            }
        };

        let request = echo_core::action::actions::get_friend_list();
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        let friends: Vec<(i64, String)> = resp
            .data
            .as_array()
            .ok_or("unexpected response format")?
            .iter()
            .filter_map(|v| {
                let uid = v.get("user_id")?.as_i64()?;
                let name = v
                    .get("nickname")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((uid, name))
            })
            .collect();
        Ok(friends)
    }

    /// Get the friend list visible under the current gate configuration.
    /// The TUI deliberately uses [`get_friend_list`](Self::get_friend_list)
    /// instead so administrators can still edit the complete list.
    pub async fn get_gated_friend_list(&self) -> Result<Vec<(i64, String)>, String> {
        let friends = self.get_friend_list().await?;
        Ok(self.apply_friend_gate(friends))
    }

    /// Get group list via the active QQ connection.
    pub async fn get_group_list(&self) -> Result<Vec<(i64, String)>, String> {
        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone()
            .ok_or("no QQ connection active")?;
        let request = echo_core::action::actions::get_group_list();
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        // The response data is an array of { group_id, group_name, ... }.
        let groups: Vec<(i64, String)> = resp
            .data
            .as_array()
            .ok_or("unexpected response format")?
            .iter()
            .filter_map(|v| {
                let gid = v.get("group_id")?.as_i64()?;
                let name = v
                    .get("group_name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((gid, name))
            })
            .collect();

        // Apply gate-mode filtering to the group list.
        let mode = self.get_gate_mode();
        if mode != QqGateMode::None {
            let cfg = self.get_filter_config();
            let mut filtered = groups;
            match mode {
                QqGateMode::Allowlist => {
                    let allow_groups: std::collections::HashSet<i64> =
                        cfg.allowlist.group_ids.iter().cloned().collect();
                    filtered.retain(|(gid, _)| allow_groups.contains(gid));
                }
                QqGateMode::Denylist => {
                    let deny_groups: std::collections::HashSet<i64> =
                        cfg.denylist.group_ids.iter().cloned().collect();
                    if !deny_groups.is_empty() {
                        filtered.retain(|(gid, _)| !deny_groups.contains(gid));
                    }
                }
                QqGateMode::None => {}
            }
            return Ok(filtered);
        }
        Ok(groups)
    }

    /// Get group member info via the active QQ connection.
    pub async fn get_group_member_info(
        &self,
        group_id: i64,
        user_id: i64,
    ) -> Result<String, String> {
        // ── Gate mode check ──
        {
            let mode = self.get_gate_mode();
            if mode != QqGateMode::None {
                let cfg = self.get_filter_config();
                match mode {
                    QqGateMode::Allowlist => {
                        let allow_groups: std::collections::HashSet<i64> =
                            cfg.allowlist.group_ids.iter().cloned().collect();
                        if !allow_groups.is_empty() && !allow_groups.contains(&group_id) {
                            return Err(format!("group {group_id} is not allowlisted"));
                        }
                    }
                    QqGateMode::Denylist => {
                        let deny_groups: std::collections::HashSet<i64> =
                            cfg.denylist.group_ids.iter().cloned().collect();
                        if !deny_groups.is_empty() && deny_groups.contains(&group_id) {
                            return Err(format!("group {group_id} is denylisted"));
                        }
                    }
                    QqGateMode::None => {}
                }
            }
        }
        let _ = user_id; // reserved for future per-user gating

        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone();
        let ctx = ctx.ok_or("no QQ connection active")?;
        let request = echo_core::action::actions::get_group_member_info(group_id, user_id);
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        serde_json::to_string(&resp.data).map_err(|e| e.to_string())
    }

    /// Upload a file to a group via the active QQ connection.
    ///
    /// `file_path` is a path on the machine running the agent (the host). It
    /// is transparently bridged to a location NapCat can read: container paths
    /// pass through, `docker cp` is attempted when available, and otherwise a
    /// local HTTP file server hands the file to NapCat via URL.
    /// `file_name` is the display name shown in QQ.
    pub async fn upload_group_file(
        &self,
        group_id: i64,
        file_path: &str,
        file_name: &str,
    ) -> Result<String, String> {
        // 出站门控（2026-10 巡检 P1）：文件上传此前绕开
        // check_outbound_gate——send_message/send_json_card 都过门控，
        // upload 不应是后门。构造等效 MessageTarget 复用同一门控。
        let target = echo_defs::chat::MessageTarget {
            adapter_name: self.name.clone(),
            channel: echo_defs::chat::ChannelType::Group {
                group_id: group_id.to_string(),
            },
            user_id: String::new(),
        };
        self.check_outbound_gate(&target)?;
        let candidates = self
            .inner
            .file_bridge
            .resolve_all(file_path, file_name)
            .await?;
        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone()
            .ok_or("no QQ connection active")?;

        let mut last_error = None;
        for remote in &candidates {
            let request =
                echo_core::action::actions::upload_group_file(group_id, remote, file_name);
            let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
            if resp.is_ok() {
                tracing::info!(%group_id, %remote, "group file uploaded");
                return Ok(format!("file uploaded to group {group_id}"));
            }
            last_error = Some(
                resp.error_message()
                    .unwrap_or_else(|| "upload failed".into()),
            );
            tracing::debug!(%remote, error = last_error.as_deref().unwrap_or(""), "upload candidate failed; trying next");
        }
        Err(last_error.unwrap_or_else(|| "upload failed".into()))
    }

    /// Upload a file to a private chat via the active QQ connection.
    ///
    /// `file_path` is a path on the machine running the agent (the host). It
    /// is transparently bridged to a location NapCat can read (see
    /// [`upload_group_file`](Self::upload_group_file)).
    /// `file_name` is the display name shown in QQ.
    pub async fn upload_private_file(
        &self,
        user_id: i64,
        file_path: &str,
        file_name: &str,
    ) -> Result<String, String> {
        // 出站门控（同 upload_group_file 注释）。
        let target = echo_defs::chat::MessageTarget {
            adapter_name: self.name.clone(),
            channel: echo_defs::chat::ChannelType::Direct,
            user_id: user_id.to_string(),
        };
        self.check_outbound_gate(&target)?;
        let candidates = self
            .inner
            .file_bridge
            .resolve_all(file_path, file_name)
            .await?;
        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone()
            .ok_or("no QQ connection active")?;

        let mut last_error = None;
        for remote in &candidates {
            let request =
                echo_core::action::actions::upload_private_file(user_id, remote, file_name);
            let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
            if resp.is_ok() {
                tracing::info!(%user_id, %remote, "private file uploaded");
                return Ok(format!("file uploaded to user {user_id}"));
            }
            last_error = Some(
                resp.error_message()
                    .unwrap_or_else(|| "upload failed".into()),
            );
            tracing::debug!(%remote, error = last_error.as_deref().unwrap_or(""), "upload candidate failed; trying next");
        }
        Err(last_error.unwrap_or_else(|| "upload failed".into()))
    }
}
