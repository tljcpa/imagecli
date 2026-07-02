//! OpenAI 官方 provider(D-011 海外首接, 走 D-009 OpenAI 兼容模板的 drop-in)。
//!
//! WebFetch 核实(2026-06-26, 经 Azure OpenAI 官方文档交叉验证, openai.com 文档 403 拦截):
//!   - 端点: POST https://api.openai.com/v1/images/generations
//!   - 鉴权: header `Authorization: Bearer <OPENAI_API_KEY>`
//!   - 模型: gpt-image-1(文生图)
//!   - 请求字段: model + prompt + n + size + quality 等; size 取值为
//!     `1024x1024` / `1024x1536` / `1536x1024`(gpt-image-1 系列)。
//!   - 关键差异: **gpt-image-1 不支持 response_format 字段, 且永远返回 base64**
//!     (data[].b64_json, 无 url)。故 default_response_format 必须置 None,
//!     否则传该字段会被服务端拒绝(与 agnes 同坑)。
//!
//! 因此 OpenAI 官方就是模板的标准 drop-in(A 类): 协议路径、鉴权头、请求/返回 schema
//! 全部对齐模板已实现的 OpenAI images 形态。模板的 b64_json -> inline 字节落盘路径
//! (D-008 建好的基础设施)正好接住 gpt-image-1 的 base64 产物, 无需写任何协议代码,
//! 只填一个 config 常量即可。
//!
//! 凭证只从环境变量取(OPENAI_API_KEY 优先, IMAGECLI_OPENAI_KEY 回退), 绝不写死 key。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "openai";
/// text2image 默认 model(OpenAI 官方图像模型)。
pub const DEFAULT_T2I_MODEL: &str = "gpt-image-1";
/// API 根地址(到 /v1 为止), endpoint 由模板拼 "/images/generations"。
const BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI 官方的模板配置常量。
const OPENAI_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::OPENAI_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::OPENAI_KEY_MISSING_HINT,
    // gpt-image-1 支持的 size: 1024x1024 / 1024x1536 / 1536x1024, 取方形为默认(最快)。
    default_size: Some("1024x1024"),
    // 关键: gpt-image-1 不支持 response_format 且永远返回 b64_json, 置 None 省略该字段,
    // 避免传它被服务端拒绝; 产物走模板已实现的 b64_json -> inline 字节落盘路径。
    default_response_format: None,
    // 标准 drop-in: size 字段名 "size", 产物数组 "data"。
    size_field: "size",
    response_array_field: "data",
    // 语义别名: 直接键入 "gpt-image" 即可选中(与全限定名 "openai/gpt-image-1" 并存)。
    catalog_alias: "gpt-image",
    supports_i2i: false,
};

/// 构造 OpenAI 官方 provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OPENAI_CONFIG)
}
