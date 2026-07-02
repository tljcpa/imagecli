//! OpenAI 兼容 provider 模板(D-009 核心): 一份适配器代码接一片 OpenAI images 兼容服务。
//!
//! 协议形态(OpenAI Images `/v1/images/generations`):
//! 1. 提交: POST {base_url}/images/generations, header `Authorization: Bearer <key>`,
//!    body 为 OpenAI 风格 `{ "model", "prompt", "n", "size"?, "response_format"? }`。
//! 2. 响应: 一次返回终态结果 `{ "data": [ {"url": ...} 或 {"b64_json": ...} ] }`。
//!
//! 走 D-003 的 http-sync 传输: submit 同步 POST 直接拿终态结果、产物收敛成 Asset、
//! 返回 Succeeded 的 Job; poll 为 no-op(传入已终态 Job 原样返回), cancel 无意义。
//!
//! 可复用的关键: 把三样东西参数化进 `OpenAiCompatConfig`——
//!   base_url + 默认 model + key 来源(候选环境变量 + keyring service + 缺 key 文案)。
//! 任何 OpenAI images 兼容服务(中转站、SiliconFlow、DeepSeek、agnes 等)未来只需
//! 填一个 config 常量 + 在 registry 注册一行即可复用本适配器, 无需再写协议代码。
//!
//! 产物两路解析:
//! - url 路: `data[].url` -> Asset::from_url, 由 download 阶段 GET 落盘。
//! - b64_json 路: `data[].b64_json` -> 解码后走 Asset 的 inline 字节路径
//!   (复用 D-008 已建好的 InlineBytes + download 内联落盘基础设施)。
//!   取舍: 内联字节会随 outputs 进 store 的 result_json(DB 略增大); 因兼容服务默认
//!   response_format=url(inline 仅 b64_json 回退路径), 影响有限, 故不像 google 那样
//!   在 submit 内预落盘。若某服务强制 b64_json 且产物巨大, 再按 google 模式优化。

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::config::keys;
use crate::core::provider::{Asset, AssetKind, Capability, GenRequest, Job, JobStatus, Provider};
use crate::transport::http_sync::HttpSyncClient;

/// OpenAI 兼容服务的参数化配置(模板的全部可变项集中于此)。
///
/// 全部用 `'static` 借用: 这些值都是编译期常量(各服务的 config 写死成常量),
/// 不需要运行时拥有所有权, 借用即可、零分配。
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// provider 名(注册表 key / Job.provider), 如 "agnes"。
    pub name: &'static str,
    /// API 根地址(到 /v1 为止), 如 "https://apihub.agnes-ai.com/v1"。
    /// endpoint 由其拼 "/images/generations"。
    pub base_url: &'static str,
    /// text2image 默认 model。
    pub default_t2i_model: &'static str,
    /// key 候选环境变量(按优先级从高到低), 模板的 "key 来源" 参数化点。
    pub key_env_candidates: &'static [&'static str],
    /// keyring 回退用的 service 名。
    pub keyring_service: &'static str,
    /// 缺 key 时的中文指引(抽成常量便于单测断言)。
    pub key_missing_hint: &'static str,
    /// 默认出图尺寸(如 "1024x1024"); 服务不支持时填 None 省略该字段。
    pub default_size: Option<&'static str>,
    /// 默认 response_format("url" 或 "b64_json"); 服务不支持时填 None 省略该字段。
    pub default_response_format: Option<&'static str>,

    // ---- 以下三项是 D-012 "方言(同路径异 schema)" 的可配置分支 ----
    // drop-in(A 类)全用默认值; SiliconFlow(B 类)等同路径异 schema 的填方言值。
    /// 请求体里"尺寸"字段名。drop-in 用 "size"; SiliconFlow 方言用 "image_size"。
    /// 抽成可配置后, default_size 的值会写进这个字段名下, 避免硬编码 "size" 在 B 类炸 400。
    pub size_field: &'static str,
    /// 响应体里"产物数组"的字段名。drop-in 用 "data"; SiliconFlow 方言用 "images"。
    /// parse_images_response 按它取数组, 数组内每项仍按 url / b64_json 两路解析。
    pub response_array_field: &'static str,
    /// catalog 里给该 model 的短别名(便于直接键入选中, 如 "seedream"/"kolors")。
    /// 与全限定名 "provider/model" 并存; drop-in 各家填各自语义别名而非一律 provider 名。
    pub catalog_alias: &'static str,

    /// 该服务的 generations 端点是否支持 image2image(参考图驱动)。
    /// 多数 OpenAI 兼容服务只做纯文生图; 仅个别(如火山 Seedream 4.0)在 generations
    /// 端点直接接受 `image` 字段(URL 或 data URI base64)做 i2i。
    /// 为 true 时: (a) capabilities 增加 Image2Image; (b) capability=Image2Image 时
    /// 把输入图塞进请求体 `image` 字段。为 false(默认)时行为完全不变, 不虚标 i2i。
    pub supports_i2i: bool,
}

/// OpenAI 兼容 provider 实现。无状态: 只持有 HTTP 客户端、能力声明与 config。
pub struct OpenAiCompatProvider {
    http: reqwest::Client,
    caps: Vec<Capability>,
    config: OpenAiCompatConfig,
}

impl OpenAiCompatProvider {
    /// 用给定 config 构造一个 OpenAI 兼容 provider。
    ///
    /// caps 基础为 Text2Image(`/images/generations` 默认是纯文生图接口); 仅当
    /// config.supports_i2i 显式声明时才追加 Image2Image(如 Seedream 4.0 在同端点接受
    /// image 字段)。不支持的服务保持只声明 text2image, 不虚标 i2i。
    pub fn new(config: OpenAiCompatConfig) -> OpenAiCompatProvider {
        // 基础能力恒为 text2image; 仅当 config.supports_i2i 时才追加 image2image(诚实声明)。
        let mut caps = vec![Capability::Text2Image];
        if config.supports_i2i {
            caps.push(Capability::Image2Image);
        }
        OpenAiCompatProvider {
            http: reqwest::Client::new(),
            caps,
            config,
        }
    }
}

/// 由 GenRequest 构造 OpenAI images 请求体(纯函数, 便于离线单测)。
///
/// 基础字段: model + prompt + n。size / response_format 仅在 config 提供时才加
/// (某些兼容服务不认这两个字段, 留 None 即省略, 避免 400)。
/// 最后把用户 `--param key=value` 合并进 body: 用户可覆盖默认值或追加服务特有字段
/// (如 quality / style), 体现 "透传自由参数" 的设计。
pub fn build_images_request_body(config: &OpenAiCompatConfig, req: &GenRequest) -> Value {
    let mut body = Map::new();
    // model: 用请求里已确定的 model(CLI 显式或按能力取的默认)
    body.insert("model".to_string(), json!(req.model));
    // prompt: 文生图必有; 缺失时给空串(由服务端决定如何报错), 不在此 panic
    let prompt = req.prompt.clone().unwrap_or_default();
    body.insert("prompt".to_string(), json!(prompt));
    // n: 当前一次出一张(批量由编排层用多请求实现, 不靠单请求 n)
    body.insert("n".to_string(), json!(1));

    // size: 仅当 config 声明时才加; 字段名按方言取(drop-in "size" / SiliconFlow "image_size")
    if let Some(size) = config.default_size {
        body.insert(config.size_field.to_string(), json!(size));
    }
    // response_format: 仅当 config 声明时才加
    if let Some(rf) = config.default_response_format {
        body.insert("response_format".to_string(), json!(rf));
    }

    // i2i 喂图: 仅当服务声明支持且本次能力是 Image2Image 时, 把输入图塞进 `image` 字段。
    // 形态: 单图填字符串, 多图填字符串数组(Seedream 4.0 的 image 两种都接受)。
    // 每个串: 远程 URL 原样, 本地字节拼成 data URI(`data:<mime>;base64,...`)。
    if config.supports_i2i && matches!(req.capability, Capability::Image2Image) {
        let mut images: Vec<String> = Vec::new();
        for asset in req.inputs.iter() {
            if !matches!(asset.kind, AssetKind::Image) {
                continue;
            }
            if let Some(img) = asset.as_input_image() {
                images.push(img.to_image_field_string());
            }
        }
        if images.len() == 1 {
            body.insert("image".to_string(), json!(images[0]));
        } else if images.len() > 1 {
            body.insert("image".to_string(), json!(images));
        }
    }

    // 合并用户自由参数(覆盖同名默认; 也可追加服务特有字段)
    for (k, v) in req.params.iter() {
        body.insert(k.clone(), v.clone());
    }

    Value::Object(body)
}

/// 解析 OpenAI images 响应为 Asset 列表(纯函数, 便于离线单测两路解析与两种方言)。
///
/// `array_field` 是产物数组的字段名(方言参数化点): drop-in 用 "data", SiliconFlow 用 "images"。
/// 遍历该数组, 每项两种产物形态:
/// - `url`: 远程链接 -> Asset::from_url(交给 download GET 落盘);
/// - `b64_json`: base64 编码的图片字节 -> 解码 -> Asset::from_inline_bytes
///   (走 download 的内联字节落盘路径, mime 默认 image/png——OpenAI images 出图为 png)。
///
/// 两者皆无的项跳过。返回空向量则由调用方据响应文本报错。
pub fn parse_images_response(resp: &Value, array_field: &str) -> anyhow::Result<Vec<Asset>> {
    let mut out: Vec<Asset> = Vec::new();

    let data = match resp.get(array_field).and_then(|v| v.as_array()) {
        Some(d) => d,
        None => return Ok(out),
    };

    for item in data {
        // 优先 url 路(默认 response_format=url)
        if let Some(url) = item.get("url").and_then(|v| v.as_str()) {
            if !url.is_empty() {
                out.push(Asset::from_url(AssetKind::Image, url));
                continue;
            }
        }
        // 回退 b64_json 路: 解码成原始字节, 走 inline 落盘
        if let Some(b64) = item.get("b64_json").and_then(|v| v.as_str()) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| anyhow::anyhow!("OpenAI 兼容产物 b64_json 解码失败: {}", e))?;
            out.push(Asset::from_inline_bytes(AssetKind::Image, "image/png", bytes));
            continue;
        }
        // 两者皆无: 跳过该项(不报错, 由整体空判兜底)
    }

    Ok(out)
}

/// 抽取响应里可能的报错文本(无产物时给排查上下文)。
/// OpenAI 风格错误体: `{ "error": { "message": ... } }`; 也兜底直接 stringify。
fn extract_error_text(resp: &Value) -> String {
    if let Some(msg) = resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
    {
        return msg.to_string();
    }
    resp.to_string()
}

/// 生成本进程唯一的 job id(同步接口无队列请求 id, 自造稳定标识)。
fn generate_job_id(prefix: &str) -> String {
    use rand::Rng;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix: u32 = rand::thread_rng().gen();
    format!("{}-{}-{:08x}", prefix, nanos, suffix)
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        self.config.name
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // OpenAI 兼容服务默认暴露其 text2image 默认 model; alias 取 config.catalog_alias
        // (各家语义短名, 如 "seedream"/"kolors"), 与全限定名 "provider/model" 并存。
        let mut entry = crate::core::catalog::ModelEntry::single(
            self.config.name,
            self.config.default_t2i_model,
            Some(self.config.catalog_alias),
            Capability::Text2Image,
        );
        // 支持 i2i 的服务在同一 model 上一并声明 image2image, catalog 体现其能力。
        if self.config.supports_i2i {
            entry.capabilities.push(Capability::Image2Image);
        }
        vec![entry]
    }

    fn has_key(&self) -> bool {
        // 走 config 声明的候选环境变量 + keyring service
        keys::resolve_candidates_key(self.config.key_env_candidates, self.config.keyring_service)
            .is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        // 占位 schema: 参数随服务/model 变, MVP 给静态描述, 不打网络。
        Ok(json!({
            "provider": self.config.name,
            "model": model,
            "note": "占位 schema; OpenAI images 兼容接口参数见各服务文档",
            "common_params": {
                "prompt": "string, 文本提示词",
                "n": "int, 出图数量(本模板固定 1)",
                "size": "string, 如 1024x1024(服务支持时)",
                "response_format": "url 或 b64_json(服务支持时)"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        // 1. 取 key(无 key 在此返回中文指引, 绝不 panic; 绝不写死 key)
        let key = keys::require_candidates_key(
            self.config.key_env_candidates,
            self.config.keyring_service,
            self.config.key_missing_hint,
        )?;

        // 2. 构造 endpoint 与请求体
        let url = format!("{}/images/generations", self.config.base_url);
        let body = build_images_request_body(&self.config, &req);

        // 3. 同步 POST 一次拿终态结果(Authorization: Bearer <key>)
        let client = HttpSyncClient::new(self.http.clone(), "Authorization", format!("Bearer {}", key));
        let resp = client.post_json(&url, &body).await?;

        // 4. 解析产物(url 路 / b64_json 路); 数组字段名按方言取(data / images)
        let outputs = parse_images_response(&resp, self.config.response_array_field)?;
        if outputs.is_empty() {
            // 无产物: 给出服务端报错文本便于排查(鉴权失败/参数非法/配额耗尽等)
            anyhow::bail!(
                "{} 未返回图片产物。服务端响应: {}",
                self.config.name,
                extract_error_text(&resp)
            );
        }

        // 5. 组装终态 Job。raw_meta 只存元信息(不含图片字节)
        let job_id = generate_job_id(self.config.name);
        let raw_meta = json!({
            "model": req.model,
            "image_count": outputs.len(),
        });

        Ok(Job {
            id: job_id,
            provider: self.config.name.to_string(),
            status: JobStatus::Succeeded,
            outputs,
            error: None,
            raw_meta,
        })
    }

    async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
        // 同步 provider: submit 已返回终态, poll 为 no-op, 原样返回。
        Ok(job.clone())
    }

    async fn cancel(&self, _job: &Job) -> anyhow::Result<()> {
        // 同步无在途任务, 取消无意义, 尽力而为返回 Ok。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AssetSource;

    /// 测试用 config(不指向真实服务; 仅驱动纯函数)。
    fn test_config() -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            name: "testcompat",
            base_url: "https://example.invalid/v1",
            default_t2i_model: "test-model",
            key_env_candidates: &["TESTCOMPAT_API_KEY"],
            keyring_service: "testcompat",
            key_missing_hint: "缺 key",
            default_size: Some("1024x1024"),
            default_response_format: Some("url"),
            // drop-in 默认方言: size 字段名 "size", 产物数组 "data"。
            size_field: "size",
            response_array_field: "data",
            catalog_alias: "testcompat",
            supports_i2i: false,
        }
    }

    /// SiliconFlow 方言的测试 config: size 字段名改 "image_size", 产物数组改 "images"。
    fn siliconflow_dialect_config() -> OpenAiCompatConfig {
        let mut cfg = test_config();
        cfg.name = "siliconflow";
        cfg.size_field = "image_size";
        cfg.response_array_field = "images";
        cfg
    }

    fn t2i_req(prompt: &str) -> GenRequest {
        GenRequest {
            capability: Capability::Text2Image,
            model: "test-model".to_string(),
            prompt: Some(prompt.to_string()),
            inputs: Vec::new(),
            params: serde_json::Map::new(),
        }
    }

    #[test]
    fn build_body_has_model_prompt_n() {
        let cfg = test_config();
        let body = build_images_request_body(&cfg, &t2i_req("a red fox in snow"));
        assert_eq!(body["model"].as_str(), Some("test-model"));
        assert_eq!(body["prompt"].as_str(), Some("a red fox in snow"));
        assert_eq!(body["n"].as_i64(), Some(1));
        // config 声明了 size / response_format, 应出现
        assert_eq!(body["size"].as_str(), Some("1024x1024"));
        assert_eq!(body["response_format"].as_str(), Some("url"));
        // 不支持 i2i 的默认 config, 即便 t2i 也不应出现 image 字段
        assert!(body.get("image").is_none());
    }

    /// 支持 i2i 的测试 config(模拟 Seedream): supports_i2i=true。
    fn i2i_config() -> OpenAiCompatConfig {
        let mut cfg = test_config();
        cfg.supports_i2i = true;
        cfg
    }

    #[test]
    fn i2i_config_declares_image2image_capability() {
        // supports_i2i=true 的 provider 应声明 Image2Image; 默认 config 不应声明。
        let p = OpenAiCompatProvider::new(i2i_config());
        assert!(p.capabilities().contains(&Capability::Image2Image));
        let p0 = OpenAiCompatProvider::new(test_config());
        assert!(!p0.capabilities().contains(&Capability::Image2Image));
    }

    #[test]
    fn build_body_i2i_local_bytes_to_data_uri_image() {
        // 本地图(内联字节) + capability=Image2Image -> image 字段为单个 data URI 字符串。
        let cfg = i2i_config();
        let mut req = t2i_req("turn into watercolor");
        req.capability = Capability::Image2Image;
        req.inputs = vec![Asset::from_inline_bytes(
            AssetKind::Image,
            "image/png",
            vec![1u8, 2, 3, 4],
        )];
        let body = build_images_request_body(&cfg, &req);
        // data:image/png;base64,AQIDBA==
        assert_eq!(
            body.get("image").and_then(|v| v.as_str()),
            Some("data:image/png;base64,AQIDBA==")
        );
    }

    #[test]
    fn build_body_i2i_disabled_config_ignores_inputs() {
        // supports_i2i=false 时, 即便能力是 Image2Image 且带输入图, 也不塞 image 字段(不虚标)。
        let cfg = test_config();
        let mut req = t2i_req("x");
        req.capability = Capability::Image2Image;
        req.inputs = vec![Asset::from_url(AssetKind::Image, "https://x/in.png")];
        let body = build_images_request_body(&cfg, &req);
        assert!(body.get("image").is_none());
    }

    #[test]
    fn build_body_omits_size_and_rf_when_config_none() {
        // 服务不支持 size/response_format 时, config 填 None -> body 不含这两字段
        let mut cfg = test_config();
        cfg.default_size = None;
        cfg.default_response_format = None;
        let body = build_images_request_body(&cfg, &t2i_req("x"));
        assert!(body.get("size").is_none());
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn build_body_user_param_overrides_default() {
        // 用户 --param size=512x512 应覆盖 config 默认
        let cfg = test_config();
        let mut req = t2i_req("x");
        req.params.insert("size".to_string(), json!("512x512"));
        let body = build_images_request_body(&cfg, &req);
        assert_eq!(body["size"].as_str(), Some("512x512"));
    }

    #[test]
    fn parse_url_path_to_url_asset() {
        let resp = json!({ "data": [ { "url": "https://cdn.example/out.png" } ] });
        let assets = parse_images_response(&resp, "data").expect("应能解析 url 路");
        assert_eq!(assets.len(), 1);
        match assets[0].source() {
            AssetSource::Url(u) => assert_eq!(u, "https://cdn.example/out.png"),
            _ => panic!("应为 Url 来源"),
        }
    }

    #[test]
    fn dropin_request_uses_size_field_and_data_array() {
        // drop-in(火山/StepFun/CogView/PPIO 同款): 请求用 "size", 解析 "data[]"。
        let cfg = test_config();
        let body = build_images_request_body(&cfg, &t2i_req("a cat"));
        // 请求体应含 "size" 而非 "image_size"
        assert_eq!(body["size"].as_str(), Some("1024x1024"));
        assert!(body.get("image_size").is_none());
        // 解析 "data[]" 形态
        let resp = json!({ "data": [ { "url": "https://cdn.example/d.png" } ] });
        let assets = parse_images_response(&resp, cfg.response_array_field).expect("解析 data[]");
        assert_eq!(assets.len(), 1);
        // 同样的响应若把数组字段名当成 "images" 取, 则取不到(证明字段名确实参数化)
        let empty = parse_images_response(&resp, "images").expect("images 字段不存在应 Ok 空");
        assert!(empty.is_empty());
    }

    #[test]
    fn siliconflow_dialect_uses_image_size_and_images_array() {
        // SiliconFlow 方言(B 类): 请求用 "image_size", 解析 "images[]"。
        let cfg = siliconflow_dialect_config();
        let body = build_images_request_body(&cfg, &t2i_req("a dog"));
        // 请求体应含 "image_size" 而非 "size"
        assert_eq!(body["image_size"].as_str(), Some("1024x1024"));
        assert!(body.get("size").is_none());
        // 返回是 "images[]" 而非 "data[]"
        let resp = json!({ "images": [ { "url": "https://cdn.siliconflow/out.png" } ] });
        let assets =
            parse_images_response(&resp, cfg.response_array_field).expect("应能解析 images[]");
        assert_eq!(assets.len(), 1);
        match assets[0].source() {
            AssetSource::Url(u) => assert_eq!(u, "https://cdn.siliconflow/out.png"),
            _ => panic!("应为 Url 来源"),
        }
        // 用 drop-in 的 "data" 字段名去取 SiliconFlow 响应, 取不到(方言不可混用)
        let empty = parse_images_response(&resp, "data").expect("data 字段不存在应 Ok 空");
        assert!(empty.is_empty());
    }

    #[test]
    fn parse_b64_json_path_to_inline_asset() {
        // 1x1 png 的 base64 fixture
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQAY3Y2wAAAAAElFTkSuQmCC";
        let resp = json!({ "data": [ { "b64_json": b64 } ] });
        let assets = parse_images_response(&resp, "data").expect("应能解析 b64_json 路");
        assert_eq!(assets.len(), 1);
        // 走 inline 字节路径, 解码后以 PNG 魔数开头
        match assets[0].source() {
            AssetSource::Inline(inline) => {
                assert_eq!(inline.mime, "image/png");
                assert!(!inline.data.is_empty());
                assert_eq!(&inline.data[0..4], &[0x89, 0x50, 0x4E, 0x47]);
            }
            _ => panic!("b64_json 应走 Inline 来源"),
        }
    }

    #[test]
    fn parse_empty_data_is_empty() {
        let resp = json!({ "data": [] });
        let assets = parse_images_response(&resp, "data").expect("空 data 应 Ok");
        assert!(assets.is_empty());
        // 错误文本可从 error.message 抽出
        let err_resp = json!({ "error": { "message": "invalid api key" } });
        assert_eq!(extract_error_text(&err_resp), "invalid api key");
    }
}
