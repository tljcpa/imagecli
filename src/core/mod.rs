//! core: 项目语义内核。
//! - provider: 统一契约与归一化类型
//! - registry: provider 注册表
//! - runner:   并发提交 + 退避轮询编排
//! - download: 产物下载

pub mod catalog;
pub mod download;
pub mod pricing;
pub mod provider;
pub mod registry;
pub mod retry;
pub mod route;
pub mod runner;
pub mod store;
