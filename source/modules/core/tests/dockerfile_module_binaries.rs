//! Guard: Dockerfile 里 COPY 的模块二进制，必须在同一个 builder 阶段被 `-p` 构建过。
//!
//! 背景（2026-11-20 实测）：清单模块作为独立产物进入发布链路时，`Dockerfile`
//! 加了从 builder 阶段复制 `target/release/agent-module-checklist` 的 COPY，
//! 但 builder 阶段仍然是 `cargo build --release --locked -p echo-agent-core`。
//! 清单模块在那里只是**依赖**，cargo 只编它的 lib
//! （`libagent_module_checklist-*.rlib`），二进制根本没生成，COPY 失败、整套
//! 镜像构建红掉。而这条链路只在 CI 的 `Docker` workflow 里跑，本地
//! `cargo test` 一律绿——从 v0.3.0 起连红两次都没人察觉。
//!
//! 判据刻意写成"COPY 的每个产物名都在 builder 阶段的 `-p` 列表里"，而不是
//! "含有某个字符串"：新增模块时漏改一边就会被拦下。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("modules/")
        .parent()
        .expect("source/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read_dockerfile() -> String {
    let path = repo_root().join("Dockerfile");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// 取出 Dockerfile 里 `builder` 阶段的文本（从它自己的 `FROM` 到下一个 `FROM`）。
fn builder_stage(dockerfile: &str) -> String {
    let mut stage = String::new();
    let mut inside = false;
    for line in dockerfile.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("FROM ") {
            if inside {
                break;
            }
            // `FROM ... AS builder`
            inside = rest.split_whitespace().last() == Some("builder");
            continue;
        }
        if inside {
            stage.push_str(line);
            stage.push('\n');
        }
    }
    assert!(
        !stage.is_empty(),
        "Dockerfile 里应有一个名为 builder 的阶段"
    );
    stage
}

/// builder 阶段每条 `cargo build`（含 `\` 续行）里的全部 `-p <包>`。
fn built_packages(stage: &str) -> Vec<String> {
    let lines: Vec<&str> = stage.lines().collect();
    let mut packages = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].contains("cargo build") {
            i += 1;
            continue;
        }
        // 拼接续行：统一去掉行尾 `\` 后用空格连起来，再按空白切词。
        let mut command = String::new();
        loop {
            let trimmed = lines[i].trim_end();
            let continues = trimmed.ends_with('\\');
            command.push_str(trimmed.trim_end_matches('\\'));
            command.push(' ');
            i += 1;
            if !continues || i >= lines.len() {
                break;
            }
        }
        let words: Vec<&str> = command.split_whitespace().collect();
        for (k, word) in words.iter().enumerate() {
            if *word == "-p" {
                if let Some(name) = words.get(k + 1) {
                    packages.push((*name).to_string());
                }
            }
        }
    }
    packages
}

/// `COPY --from=builder /app/target/release/<名>` 里的 `<名>`。
fn copied_from_builder(dockerfile: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in dockerfile.lines() {
        let line = line.trim();
        if !line.starts_with("COPY") || !line.contains("--from=builder") {
            continue;
        }
        for word in line.split_whitespace() {
            if let Some(name) = word.strip_prefix("/app/target/release/") {
                names.push(name.to_string());
            }
        }
    }
    names
}

#[test]
fn dockerfile_builds_every_module_binary_it_copies() {
    let dockerfile = read_dockerfile();
    let built = built_packages(&builder_stage(&dockerfile));
    let copied = copied_from_builder(&dockerfile);
    assert!(
        !copied.is_empty(),
        "Dockerfile 应从 builder 阶段复制产物（至少 Core 与模块）"
    );

    for name in &copied {
        assert!(
            built.contains(name),
            "Dockerfile 从 builder 阶段复制 `target/release/{name}`，但该阶段的 \
             `cargo build` 没有 `-p {name}`——它只会被编成依赖库（lib*.rlib），\
             二进制不存在，COPY 会在 CI 的 Docker workflow 里失败。\
             已构建：{built:?}"
        );
    }
}

/// COPY 进来的模块二进制必须落在一处被 **runtime 会去读** 的位置：
/// `libexec/modules/<包名>/<二进制>`。装到别处等于装了也没人用。
#[test]
fn dockerfile_module_binaries_land_where_the_runtime_looks() {
    let dockerfile = read_dockerfile();
    for name in copied_from_builder(&dockerfile) {
        if name == "echo-agent-core" {
            continue;
        }
        // 二进制名 == 包名 == 目录名（模块目录约定）。
        let expected = format!("/usr/local/libexec/echo-agent-core/modules/{name}/{name}");
        assert!(
            dockerfile.contains(&expected),
            "模块 `{name}` 的镜像落点应是 {expected}（与源码/二进制安装一致）；\
             modules.toml 的 command 按这个路径找二进制"
        );
    }
}
