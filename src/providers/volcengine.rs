//! 火山引擎方舟(Volcengine Ark)Seedream provider —— D-010/D-012 的 A 类 OpenAI drop-in。
//!
//! 即梦同源(字节 Seedream 模型经火山引擎正规公开 API 获取, 见 D-010), 也是本批接入最值的一家。
//! 暴露标准 OpenAI images 接口, 故不写专属协议代码, 直接复用 openai_compat 模板填一个 config。
//!
//! WebFetch 核实(2026-06):
//!   base_url = https://ark.cn-beijing.volces.com/api/v3  endpoint = /images/generations
//!   鉴权     = Authorization: Bearer <ARK_API_KEY>
//!   model    = doubao-seedream-4-0-250828(Seedream 4.0; curl 示例确认)
//!   请求     = { model, prompt, size, response_format:"url", watermark? }  -> 标准 size/data[], drop-in
//!   返回     = { data: [ { url } ] }
//!
//! 凭证只从环境变量取(ARK_API_KEY / VOLC_API_KEY / IMAGECLI_VOLC_KEY), 绝不写死 key。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "volcengine";
/// text2image 默认 model(Seedream 4.0)。
pub const DEFAULT_T2I_MODEL: &str = "doubao-seedream-4-0-250828";
/// API 根地址(到 /api/v3 为止)。
const BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";

/// 火山方舟的模板配置常量。drop-in: size/data[] 全用默认方言。
const VOLCENGINE_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::VOLCENGINE_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::VOLCENGINE_KEY_MISSING_HINT,
    default_size: Some("1024x1024"),
    // 火山 Seedream 支持 response_format=url, 显式声明走 url 路(产物为远程链接)。
    default_response_format: Some("url"),
    size_field: "size",
    response_array_field: "data",
    catalog_alias: "seedream",
    // Seedream 4.0 的 generations 端点直接接受 image 字段(URL/data URI)做 i2i。
    supports_i2i: true,
};

/// 构造火山方舟 provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(VOLCENGINE_CONFIG)
}
