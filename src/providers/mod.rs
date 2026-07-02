//! providers: 各家具体 provider 实现。
//! 每个 provider 把自家协议翻译进 core::provider::Provider 统一契约。
//!
//! - fal: D-004 首个落地 provider, 走 http-queue, 完整实现 text2image。
//! - google: D-008 首个真实端到端 provider, 走 http-sync(Gemini generateContent)。
//! - openai_compat: D-009 OpenAI images 兼容 provider 模板(可复用适配器)。
//! - agnes: D-009 模板的首个实例(填一个 config 常量即接入)。
//!
//! 下一棒加 provider(如 Replicate)只需在此 `pub mod` 一行, 并在
//! core::registry::Registry::build_default 里多注册一次。
//! 接 OpenAI 兼容服务更省: 仿 agnes.rs 填一个 config 常量, 无需写协议代码。

pub mod agnes;
pub mod fal;
pub mod google;
pub mod openai_compat;
// D-011 海外: OpenAI 官方(drop-in 模板) + Replicate(C 类异步 prediction)。
pub mod openai;
pub mod replicate;
// D-010/D-012 大陆 5 家 OpenAI 兼容 provider(仿 agnes 各填一个 config 常量):
// 火山 Seedream / StepFun / 智谱 CogView / PPIO 走 A 类 drop-in; SiliconFlow 走 B 类方言。
pub mod ppio;
pub mod siliconflow;
pub mod stepfun;
pub mod volcengine;
pub mod zhipu;
// D-014 视频: 火山方舟 Ark Seedance(C 类异步任务, 走通用 async-task 骨架, 首个视频 provider)。
pub mod seedance;
// D-014 视频: 可灵 Kling(C 类异步任务, 复用骨架; 鉴权换成本地 HS256 JWT)。
pub mod kling;
// D-014 图像: 即梦 visual(C 类异步任务, 复用骨架; 鉴权为火山 AK/SK V4 签名)。
pub mod jimeng;
