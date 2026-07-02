//! imagecli 库根。
//!
//! 把内核模块以库形式导出, 既供 `main.rs` 二进制使用, 也供 `tests/` 集成测试
//! 直接调用(如跨进程持久化冒烟需要在测试进程里用 JobStore 写库)。
//!
//! 本仓库是地基骨架: provider 契约(schema/cancel/Uploader)、http_sync 与 subprocess
//! 两类 transport、以及部分前瞻性辅助函数(如 Asset::from_path / store_in_keyring)是
//! 按 D-003/D-005 预先铺好的扩展面, MVP 的 CLI 尚未全部接线。这里在 crate 级别允许
//! dead_code, 避免把"故意预留的 API 表面"误报成死代码而污染 clippy -D warnings。
#![allow(dead_code)]

pub mod cli;
pub mod config;
pub mod core;
pub mod mcp;
pub mod providers;
pub mod transport;
