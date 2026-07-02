//! agnes provider(D-009 首个 OpenAI 兼容实例, 也是项目首个真实端到端出图 provider)。
//!
//! agnes(新加坡 Agnes AI, 免费层)暴露标准 OpenAI images 接口, 故不写专属协议代码:
//! 直接用 openai_compat 模板, 只填一个 config 常量。这正是 D-009 "一份适配器接一片
//! 兼容服务" 的验证——接 SiliconFlow / DeepSeek 等同理, 复制本文件改三个值即可。
//!
//! 三个参数化点(对应模板):
//!   base_url = https://apihub.agnes-ai.com/v1
//!   model    = agnes-image-2.1-flash(text2image 默认)
//!   key 来源 = 环境变量 AGNES_API_KEY(优先) / IMAGECLI_AGNES_KEY, 见 config::keys。
//!
//! 凭证只从环境变量取, 绝不写死 key, 也不读任何本地 pool 文件(真实联调由主控用 env 注入)。

use crate::config::keys;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "agnes";
/// text2image 默认 model。
pub const DEFAULT_T2I_MODEL: &str = "agnes-image-2.1-flash";
/// API 根地址。
const BASE_URL: &str = "https://apihub.agnes-ai.com/v1";

/// agnes 的模板配置常量。接新兼容服务时, 复制本常量改 name/base_url/model/key 候选即可。
const AGNES_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    name: PROVIDER_NAME,
    base_url: BASE_URL,
    default_t2i_model: DEFAULT_T2I_MODEL,
    key_env_candidates: &keys::AGNES_KEY_ENV_CANDIDATES,
    keyring_service: PROVIDER_NAME,
    key_missing_hint: keys::AGNES_KEY_MISSING_HINT,
    // agnes 实测不吃 response_format(litellm UnsupportedParamsError, HTTP 400), 置 None 自动省略该字段;
    // size 暂保留, 若再报 UnsupportedParams 同样改 None。
    default_size: Some("1024x1024"),
    default_response_format: None,
    // agnes 是标准 drop-in: size 字段名 "size", 产物数组 "data"; 别名取 provider 名。
    size_field: "size",
    response_array_field: "data",
    catalog_alias: PROVIDER_NAME,
    supports_i2i: false,
};

/// 构造 agnes provider(供 registry 注册)。
pub fn provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(AGNES_CONFIG)
}
