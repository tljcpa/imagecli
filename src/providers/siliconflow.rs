//! SiliconFlow 硅基流动 provider —— D-012 的 B 类"同路径异 schema(方言)"首个实例。
//!
//! 端点同为 /v1/images/generations, 但请求/返回字段与 OpenAI 不同, 故走模板的方言分支:
//! 请求用 image_size(不是 size); 返回数组是 images[](不是 data[]), 每项仍 { url }。
//! 这正是 D-012 把"方言映射"抽成可配置的价值: 协议代码不动, 只换三个 config 值即接入。
//!
//! WebFetch 核实(2026-06):
//!   base_url = https://api.siliconflow.cn/v1  endpoint = /images/generations
//!   鉴权     = Authorization: Bearer <SILICONFLOW_API_KEY>
//!   model    = Kwai-Kolors/Kolors(可图; 平台另有 FLUX/Qwen-Image 等)
//!   请求     = { model, prompt, image_size, batch_size?, num_inference_steps?, ... }
//!   返回     = { images: [ { url } ], seed, timings }  (url 有效期约 1 小时)
//!
//! 凭证只从环境变量取(SILICONFLOW_API_KEY / IMAGECLI_SILICONFLOW_KEY), 绝不写死 key。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "siliconflow";
/// text2image 默认 model(可图 Kolors)。
pub const DEFAULT_T2I_MODEL: &str = "Kwai-Kolors/Kolors";
/// API 根地址(到 /v1 为止)。
const BASE_URL: &str = "https://api.siliconflow.cn/v1";

/// SiliconFlow 的模板配置常量。B 类方言: size_field=image_size, response_array_field=images。
const SILICONFLOW_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::SILICONFLOW_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::SILICONFLOW_KEY_MISSING_HINT,
    default_size: Some("1024x1024"),
    // SiliconFlow 不吃 OpenAI 的 response_format, 省略。
    default_response_format: None,
    // ---- 方言核心: 与 drop-in 的差异全在这两行 ----
    size_field: "image_size",
    response_array_field: "images",
    catalog_alias: "kolors",
    supports_i2i: false,
};

/// 构造 SiliconFlow provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(SILICONFLOW_CONFIG)
}
