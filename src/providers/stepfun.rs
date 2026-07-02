//! 阶跃 StepFun provider —— D-010/D-012 的 A 类 OpenAI drop-in。
//!
//! StepFun 官方提供 OpenAI 迁移文档, 模型可直接用 OpenAI SDK 调, 标准 size/data[]。
//!
//! WebFetch 核实(2026-06):
//!   base_url = https://api.stepfun.com/v1  endpoint = /images/generations
//!   鉴权     = Authorization: Bearer <STEPFUN_API_KEY>
//!   model    = step-1x-medium(通用文生图)
//!   请求     = { model, prompt, size }  支持 256x256/512x512/768x768/1024x1024/1280x800 等
//!   返回     = { data: [ { url } ] }  drop-in
//!
//! 凭证只从环境变量取(STEPFUN_API_KEY / IMAGECLI_STEPFUN_KEY), 绝不写死 key。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "stepfun";
/// text2image 默认 model。
pub const DEFAULT_T2I_MODEL: &str = "step-1x-medium";
/// API 根地址(到 /v1 为止)。
const BASE_URL: &str = "https://api.stepfun.com/v1";

/// StepFun 的模板配置常量。drop-in: size/data[] 全用默认方言。
const STEPFUN_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::STEPFUN_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::STEPFUN_KEY_MISSING_HINT,
    default_size: Some("1024x1024"),
    // StepFun 支持 response_format, 显式 url。
    default_response_format: Some("url"),
    size_field: "size",
    response_array_field: "data",
    catalog_alias: "stepfun",
    supports_i2i: false,
};

/// 构造 StepFun provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(STEPFUN_CONFIG)
}
