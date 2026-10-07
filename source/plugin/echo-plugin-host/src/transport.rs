//! 运输层抽象（骨架；实现由 P0 收尾提交填充）。
//!
//! 计划中的实现：
//! - `InprocTransport`：类型直连（零序列化）；
//! - `StdioTransport`：4 字节 LE 长度前缀 + JSON / msgpack（`LengthDelimitedCodec`）；
//! - `DylibTransport`（实验轨）；`WasmTransport`（可选沙箱轨）。
