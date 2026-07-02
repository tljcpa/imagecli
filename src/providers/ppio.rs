//! PPIO 派欧云 provider —— D-010/D-012 的 A 类 OpenAI drop-in。
//!
//! WebFetch 核实(2026-06, 部分项 #uncertain, 真实联调需主控确认):
//!   OpenAI 兼容根 = https://api.ppinfra.com/v3/openai(chat 在 /v3/openai/chat/completions,
//!                   故 images 取同根下 /images/generations)。亦见 api.ppio.com/openai 别名域。
//!   鉴权     = Authorization: Bearer <PPIO_API_KEY>
//!   model    = qwen-image(PPIO 文生图; 平台另有 seedream-4.0 等可切)
//!   请求/返回 = 标准 size / data[].url(drop-in)
//!
//! 不确定点: PPIO 文档难以离线核实 images 路径前缀(/v3/openai 下是否再带 /v1), 故 base_url 暂取
//! 与 chat 同根; 若真实联调 404, 改 base_url 为 .../v3/openai/v1 即可, 协议代码无需动。
//! 凭证只从环境变量取(PPIO_API_KEY / IMAGECLI_PPIO_KEY), 绝不写死 key。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "ppio";
/// text2image 默认 model。
pub const DEFAULT_T2I_MODEL: &str = "qwen-image";
/// API 根地址(OpenAI 兼容根, 与 chat 同根)。
const BASE_URL: &str = "https://api.ppinfra.com/v3/openai";

/// PPIO 的模板配置常量。drop-in: size/data[] 全用默认方言。
const PPIO_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::PPIO_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::PPIO_KEY_MISSING_HINT,
    default_size: Some("1024x1024"),
    // response_format 不保证, 省略以免 400; 默认返回 data[].url。
    default_response_format: None,
    size_field: "size",
    response_array_field: "data",
    catalog_alias: "ppio",
    supports_i2i: false,
};

/// 构造 PPIO provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(PPIO_CONFIG)
}
