//! transport: D-003 的传输维度。三类执行模型, 上层 provider 按需选用。
//! - http_queue: 异步队列(submit -> poll status -> fetch result), fal/replicate
//! - http_sync:  同步(一次请求拿终态), OpenAI Images, 占位
//! - subprocess: 跑外部 CLI 解析 stdout, 占位
//! - async_task: 通用 async-task 骨架(submit/task_id/轮询/取产物 URL), D-014 视频地基,
//!   鉴权(Bearer/JWT/V4)与字段映射可注入, Seedance 首用。

pub mod async_task;
pub mod http_queue;
pub mod http_sync;
pub mod subprocess;
