//! 契约产物生成入口：把 `echo_protocol::ts_bindings::declarations()` 写到 stdout
//! 或 `--out <path>`，**不带任何领域逻辑**、不读配置、不启动运行时。
//!
//! 为什么单独一个 example 而不是给 `echo-agent` 加子命令：契约产物（B3）要能
//! 在**只还原了 `echo-protocol` 一个 crate** 的环境里产出，不该为了导出 50KB
//! 文本去编译整个组合根（core 依赖 tokio/tungstenite/chrono…）。
//!
//! 用法（feature 必须显式打开；默认构建不引入 ts-rs）：
//!
//! ```bash
//! cargo run -q -p echo-protocol --features ts-bindings \
//!   --example protocol_contract_ts -- --out /tmp/protocol.generated.ts
//! ```
//!
//! stdout 只有 TypeScript 正文（可直接 `>` 重定向、也便于核对 sha256）；
//! 诊断信息一律走 stderr。

use std::io::Write;
use std::path::PathBuf;

const USAGE: &str = "用法: protocol_contract_ts [--out <path>]\n\
\n\
  --out <path>  原子写入该路径（父目录自动创建）；缺省写 stdout。\n\
  --help        显示本帮助。\n";

fn main() -> std::process::ExitCode {
    let mut out: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => match args.next() {
                Some(path) if !path.is_empty() => out = Some(PathBuf::from(path)),
                _ => {
                    eprintln!("error: --out 需要一个路径");
                    return std::process::ExitCode::from(2);
                }
            },
            "--help" | "-h" => {
                print!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: 未知参数 {other}\n\n{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    let body = echo_protocol::ts_bindings::declarations();
    // 空输出意味着绑定清单被清空/feature 走错路径——宁可失败也不要产出空契约。
    if body.trim().is_empty() {
        eprintln!("error: ts_bindings::declarations() 返回空字符串");
        return std::process::ExitCode::FAILURE;
    }

    match out {
        None => {
            let mut stdout = std::io::stdout().lock();
            if let Err(err) = stdout
                .write_all(body.as_bytes())
                .and_then(|()| stdout.flush())
            {
                eprintln!("error: 写 stdout 失败：{err}");
                return std::process::ExitCode::FAILURE;
            }
        }
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    eprintln!("error: 创建目录失败 {}：{err}", parent.display());
                    return std::process::ExitCode::FAILURE;
                }
            }
            // 原子替换：同目录 tmp + rename，消费方（tsc/vitest）永远不会读到半截文件。
            let tmp = path.with_extension("ts.tmp");
            if let Err(err) = std::fs::write(&tmp, body.as_bytes()) {
                eprintln!("error: 写入失败 {}：{err}", tmp.display());
                return std::process::ExitCode::FAILURE;
            }
            if let Err(err) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                eprintln!("error: 替换失败 {}：{err}", path.display());
                return std::process::ExitCode::FAILURE;
            }
            eprintln!("wrote {} ({} bytes)", path.display(), body.len());
        }
    }
    std::process::ExitCode::SUCCESS
}
