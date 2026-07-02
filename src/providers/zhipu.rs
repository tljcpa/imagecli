//! 智谱 CogView provider —— D-010/D-012 的 A 类 OpenAI (近) drop-in。
//!
//! WebFetch 核实(2026-06):
//!   base_url = https://open.bigmodel.cn/api/paas/v4  endpoint = /images/generations
//!   鉴权     = Authorization: Bearer <ZHIPU_API_KEY>
//!   model    = cogview-4-250304(CogView-4; 文档代码示例确认; 亦可用别名 cogview-4)
//!   请求     = { model, prompt, size }  size 支持 1024x1024 / 1440x720 等
//!   返回     = { data: [ { url } ] }  近 drop-in(size/data[] 标准)
//!
//! 取舍: 智谱不保证认 response_format 字段, 故置 None 自动省略, 默认返回即 data[].url。
//! 凭证只从环境变量取(ZHIPU_API_KEY / GLM_API_KEY / IMAGECLI_ZHIPU_KEY), 绝不写死 key。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "zhipu";
/// text2image 默认 model(CogView-4)。
pub const DEFAULT_T2I_MODEL: &str = "cogview-4-250304";
/// API 根地址(到 /api/paas/v4 为止)。
const BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

/// 智谱 CogView 的模板配置常量。近 drop-in: size/data[] 默认方言, 省略 response_format。
const ZHIPU_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::ZHIPU_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::ZHIPU_KEY_MISSING_HINT,
    default_size: Some("1024x1024"),
    // 智谱不保证认 response_format, 省略以免 400; 默认返回 data[].url。
    default_response_format: None,
    size_field: "size",
    response_array_field: "data",
    catalog_alias: "cogview",
    supports_i2i: false,
};

/// 构造智谱 CogView provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(ZHIPU_CONFIG)
}
