//! fal.ai provider(D-004 首个落地 provider, 走 D-003 的 http-queue 传输)。
//!
//! 协议形态(fal Queue API):
//! 1. 提交: POST https://queue.fal.run/{model} 带 `Authorization: Key {FAL_KEY}`,
//!    返回体含 request_id / status_url / response_url。
//! 2. 轮询: GET status_url, 返回 `status` 为 IN_QUEUE / IN_PROGRESS / COMPLETED。
//! 3. 取结果: GET response_url, 返回含 `images` 等产物字段的 JSON。
//!
//! 本文件把上面三步翻译进统一的 Provider 契约(submit/poll/cancel)。
//! 真正的 HTTP 收发复用 transport::http_queue::HttpQueueClient, 这里只做
//! "GenRequest -> fal 请求体" 与 "fal 响应 -> Job" 的双向翻译。

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::keys;
use crate::core::provider::{
    Asset, AssetKind, Capability, GenRequest, Job, JobStatus, Provider,
};
use crate::transport::http_queue::HttpQueueClient;

/// fal provider 名(注册表 key / Job.provider)。
const PROVIDER_NAME: &str = "fal";
/// fal Queue API 根地址。
const FAL_QUEUE_BASE: &str = "https://queue.fal.run";
/// text2image 的默认 model(Flux dev 文生图 endpoint)。
pub const DEFAULT_T2I_MODEL: &str = "fal-ai/flux/dev";

/// 文生视频默认 model(fal 视频仍走同一套 Queue API, 只是 endpoint 与产物不同)。
///
/// 注意: fal 视频 endpoint 会随平台上下架/改版而变(如 kling 的版本号 v2/master),
/// 这里只给一个 WebFetch 核实(2026-06-26)当时可用的合理默认, 用户可用 `--model`
/// 覆盖任意 fal 视频 endpoint, 绝不硬依赖某个确切版本号。产物在响应 `video.url`。
pub const DEFAULT_T2V_MODEL: &str = "fal-ai/kling-video/v2/master/text-to-video";
/// 图生视频默认 model(同上, 以 fal 平台为准, 可被 --model 覆盖)。
pub const DEFAULT_I2V_MODEL: &str = "fal-ai/kling-video/v2/master/image-to-video";

/// 超分(upscale)默认 model。WebFetch 核实(fal.ai/models/fal-ai/clarity-upscaler/api):
/// 输入必填 `image_url`(待放大图 URL), 缩放参数 `upscale_factor`(float, 默认 2),
/// 产物在响应 `image.url`(更高清图)。仍走同一套 Queue API, 只是 endpoint 与产物字段不同,
/// 可被 --model 覆盖(如 fal-ai/esrgan / fal-ai/aura-sr)。产物是 Image(不是 video)。
pub const DEFAULT_UPSCALE_MODEL: &str = "fal-ai/clarity-upscaler";

/// 一次提交后需要记住的"句柄": 后续 poll/cancel 要用到的 URL。
///
/// 为什么不再用内存表存它: trait 的 poll/cancel 现在接收完整 Job(D-007),
/// 句柄随 Job.raw_meta 落进 store 跨进程流转。这里只在 submit 时把它写进 raw_meta,
/// 在 poll/cancel 时从 raw_meta 还原, FalProvider 本身彻底无状态。
#[derive(Debug, Clone)]
struct FalHandle {
    status_url: String,
    response_url: String,
    cancel_url: Option<String>,
}

impl FalHandle {
    /// 序列化进 Job.raw_meta 用的 JSON 结构。submit 时写, poll/cancel 时读。
    fn to_raw_meta(&self, request_id: &str) -> Value {
        json!({
            "request_id": request_id,
            "status_url": self.status_url,
            "response_url": self.response_url,
            "cancel_url": self.cancel_url,
        })
    }

    /// 从 Job.raw_meta 还原句柄。缺 status_url / response_url 时给清晰中文错误。
    fn from_raw_meta(raw_meta: &Value) -> anyhow::Result<FalHandle> {
        // status_url / response_url 是 poll 必需; 缺任一都无法继续, 直接报错
        let status_url = raw_meta
            .get("status_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Job.raw_meta 缺少 status_url, 无法轮询 fal 任务(句柄已丢失)")
            })?
            .to_string();
        let response_url = raw_meta
            .get("response_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Job.raw_meta 缺少 response_url, 无法取 fal 结果(句柄已丢失)")
            })?
            .to_string();
        // cancel_url 可缺(部分 endpoint 不支持取消)
        let cancel_url = raw_meta
            .get("cancel_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(FalHandle {
            status_url,
            response_url,
            cancel_url,
        })
    }
}

/// fal provider 实现。D-007 后无状态: 只持有 HTTP 客户端与能力声明, 不再存句柄表。
pub struct FalProvider {
    /// 复用连接池的 HTTP 客户端
    http: reqwest::Client,
    /// 本 provider 支持的能力(MVP 完整实现 text2image)
    caps: Vec<Capability>,
}

impl FalProvider {
    /// 构造一个默认 fal provider。
    pub fn new() -> FalProvider {
        FalProvider {
            http: reqwest::Client::new(),
            // 声明 text2image + 文生视频/图生视频。fal 托管大量视频模型, 仍走同一 Queue API,
            // 只是 model endpoint 不同、产物是 video(响应 video.url), 复用现有 http-queue 骨架。
            caps: vec![
                Capability::Text2Image,
                Capability::Text2Video,
                Capability::Image2Video,
                // 超分: 复用同一 Queue API, endpoint(如 clarity-upscaler)输入 image_url、产物 image.url。
                Capability::Upscale,
            ],
        }
    }

    /// 取 fal 鉴权头值。fal 要求 `Authorization: Key {api_key}`。
    /// 没 key 时返回带中文指引的错误, 绝不 panic。
    fn auth_header_value() -> anyhow::Result<String> {
        let key = keys::require_key(PROVIDER_NAME)?;
        Ok(format!("Key {}", key))
    }

    /// 构造一个带鉴权的 http-queue 客户端。
    fn queue_client(&self) -> anyhow::Result<HttpQueueClient> {
        let auth = Self::auth_header_value()?;
        Ok(HttpQueueClient::new(self.http.clone(), auth))
    }
}

impl Default for FalProvider {
    fn default() -> FalProvider {
        FalProvider::new()
    }
}

/// 把 fal 的状态字符串映射到归一化 JobStatus。
///
/// fal 三态: IN_QUEUE / IN_PROGRESS / COMPLETED。
/// 其它(含 ERROR 或未知)一律归 Failed, 保证 match 穷尽且不漏判终态。
/// 抽成纯函数便于单测, 不依赖网络。
pub fn map_fal_status(raw: &str) -> JobStatus {
    // fal 大小写稳定为大写, 这里仍做大写归一以容错
    let upper = raw.to_ascii_uppercase();
    match upper.as_str() {
        "IN_QUEUE" => JobStatus::Queued,
        "IN_PROGRESS" => JobStatus::Running,
        "COMPLETED" => JobStatus::Succeeded,
        // ERROR 以及任何未知状态都视为失败终态
        _ => JobStatus::Failed,
    }
}

/// 由 GenRequest 构造 fal 提交请求体(纯函数, 便于单测)。
///
/// 规则:
/// - prompt 存在则写入 "prompt" 字段。
/// - params 里的自由参数(image_size / num_images / seed / ...)整体并入顶层透传。
/// - 输入素材里第一张 image 写入 "image_url"(供图生图/超分类 endpoint 用):
///   远程 URL 原样透传; 本地图(内联字节, CLI 已读成 base64+mime)拼成 data URI 塞入。
///   image_url 既服务图生图, 也是超分(clarity-upscaler 等)的待放大图字段, 复用同一路径。
///   纯文生图通常没有输入, 该字段就不出现。
pub fn build_fal_request_body(req: &GenRequest) -> Value {
    // 以一个空 JSON 对象起步
    let mut body = serde_json::Map::new();

    // prompt
    if let Some(prompt) = &req.prompt {
        body.insert("prompt".to_string(), json!(prompt));
    }

    // 透传自由参数。用户给的 params 优先级最高, 直接覆盖同名默认。
    // 超分的缩放参数(clarity-upscaler 的 upscale_factor 等)走 --param 透传, 在此并入。
    for (k, v) in req.params.iter() {
        body.insert(k.clone(), v.clone());
    }

    // 第一张图片输入 -> image_url。经 Asset::as_input_image 归一: URL 直接透传;
    // 本地字节(CLI 读入的 inline)拼成 data URI(fal 的 image_url 接受 data URI)。
    // 纯 local_path(未读字节)返回 None, 此处跳过(CLI 加载阶段已把本地图读成 inline)。
    for asset in req.inputs.iter() {
        let is_image = matches!(asset.kind, AssetKind::Image);
        if is_image {
            if let Some(img) = asset.as_input_image() {
                body.insert("image_url".to_string(), json!(img.to_image_field_string()));
                break;
            }
        }
    }

    Value::Object(body)
}

/// 从 fal 结果 JSON 中抽取产物素材。
///
/// fal 文生图典型结构: `{ "images": [ {"url": "...", "content_type": "image/png"}, ... ] }`。
/// 视频类则常见 `{ "video": {"url": "..."} }`。这里两者都尝试解析, 解析不到就返回空。
pub fn extract_outputs(result: &Value) -> Vec<Asset> {
    let mut outputs = Vec::new();

    // images 数组
    if let Some(images) = result.get("images").and_then(|v| v.as_array()) {
        for img in images {
            if let Some(url) = img.get("url").and_then(|u| u.as_str()) {
                outputs.push(Asset::from_url(AssetKind::Image, url));
            }
        }
    }

    // 单个 image 对象(部分 endpoint)
    if let Some(url) = result
        .get("image")
        .and_then(|v| v.get("url"))
        .and_then(|u| u.as_str())
    {
        outputs.push(Asset::from_url(AssetKind::Image, url));
    }

    // 单个 video 对象
    if let Some(url) = result
        .get("video")
        .and_then(|v| v.get("url"))
        .and_then(|u| u.as_str())
    {
        outputs.push(Asset::from_url(AssetKind::Video, url));
    }

    outputs
}

#[async_trait]
impl Provider for FalProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // fal 暴露 text2image 的 Flux dev(alias "flux"), 以及若干视频 endpoint。
        // 视频 endpoint 版本号以 fal 平台为准, 可 --model 覆盖; 产物为 video.url。
        vec![
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_T2I_MODEL,
                Some("flux"),
                Capability::Text2Image,
            ),
            // 文生视频: Kling v2 master(alias "fal-kling")。
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_T2V_MODEL,
                Some("fal-kling"),
                Capability::Text2Video,
            ),
            // 图生视频: Kling v2 master i2v(alias "fal-kling-i2v")。
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_I2V_MODEL,
                Some("fal-kling-i2v"),
                Capability::Image2Video,
            ),
            // 文生视频: MiniMax Hailuo(alias "fal-minimax")。fal 平台上另一常用视频家族。
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                "fal-ai/minimax/hailuo-02/standard/text-to-video",
                Some("fal-minimax"),
                Capability::Text2Video,
            ),
            // 超分: Clarity Upscaler(alias "fal-upscale")。输入 image_url、参数 upscale_factor,
            // 产物 image.url(更高清图)。
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_UPSCALE_MODEL,
                Some("fal-upscale"),
                Capability::Upscale,
            ),
        ]
    }

    fn has_key(&self) -> bool {
        // fal 走通用命名空间(IMAGECLI_FAL_KEY / FAL_KEY / keyring)
        keys::resolve_key(PROVIDER_NAME).is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        // 占位: fal 有 OpenAPI schema 端点, MVP 先返回静态描述, 不打网络。
        // 后续可改为拉取 https://fal.run/{model}/schema 之类的真实 schema。
        Ok(json!({
            "provider": PROVIDER_NAME,
            "model": model,
            "note": "占位 schema; fal 真实参数随 model 而异, 后续接入 fal schema 端点",
            "common_params": {
                "prompt": "string, 文本提示词",
                "image_size": "string|object, 如 landscape_4_3 或 {width,height}",
                "num_images": "int, 生成张数",
                "seed": "int, 随机种子"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        // 1. 构造提交 URL 与请求体
        let submit_url = format!("{}/{}", FAL_QUEUE_BASE, req.model);
        let body = build_fal_request_body(&req);

        // 2. 鉴权 + 提交(无 key 在此处返回中文错误, 不 panic)
        let client = self.queue_client()?;
        let submitted = client.submit(&submit_url, &body).await?;

        // 3. 把句柄塞进 Job.raw_meta(结构化 JSON), 供后续跨进程 poll/cancel 还原
        let handle = FalHandle {
            status_url: submitted.status_url.clone(),
            response_url: submitted.response_url.clone(),
            cancel_url: submitted.cancel_url.clone(),
        };

        // 4. 提交成功后 fal 通常处于排队态; 归一化为 Queued
        let job = Job {
            id: submitted.request_id.clone(),
            provider: PROVIDER_NAME.to_string(),
            status: JobStatus::Queued,
            outputs: Vec::new(),
            error: None,
            raw_meta: handle.to_raw_meta(&submitted.request_id),
        };
        Ok(job)
    }

    async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
        // 从传入 Job 的 raw_meta 还原句柄(D-007: provider 无状态, 句柄随 Job 流转)。
        // 还原不到 status_url/response_url 时由 from_raw_meta 给清晰中文错误。
        let handle = FalHandle::from_raw_meta(&job.raw_meta)?;

        let client = self.queue_client()?;

        // 查询状态
        let status_resp = client.poll_status(&handle.status_url).await?;
        let status = map_fal_status(&status_resp.status);

        // 组装结果 Job。保留原 raw_meta(句柄)不丢, 让后续轮询仍能拿到句柄;
        // 把本次查询的状态信息合并进句柄 JSON 的 last_status 字段。
        let mut next_meta = job.raw_meta.clone();
        if let Some(obj) = next_meta.as_object_mut() {
            obj.insert("last_status".to_string(), json!(status_resp.status));
            obj.insert("queue_position".to_string(), json!(status_resp.queue_position));
        }
        let mut next = Job {
            id: job.id.clone(),
            provider: PROVIDER_NAME.to_string(),
            status,
            outputs: Vec::new(),
            error: None,
            raw_meta: next_meta,
        };

        // 穷尽处理四态
        match status {
            JobStatus::Succeeded => {
                // 终态成功: 拉结果, 解析产物。result 并入 raw_meta 的 result 字段,
                // 但不覆盖句柄字段(status_url 等仍需保留以便重复查询)。
                let result = client.fetch_result(&handle.response_url).await?;
                next.outputs = extract_outputs(&result);
                if let Some(obj) = next.raw_meta.as_object_mut() {
                    obj.insert("result".to_string(), result);
                }
            }
            JobStatus::Failed => {
                // fal 状态非三态正常值, 记错误信息
                next.error = Some(format!("fal 返回非成功状态: {}", status_resp.status));
            }
            JobStatus::Queued => {}
            JobStatus::Running => {}
        }

        Ok(next)
    }

    async fn cancel(&self, job: &Job) -> anyhow::Result<()> {
        // 从 raw_meta 还原句柄。句柄缺失或无 cancel_url 都不算硬错误(尽力而为)。
        let handle = match FalHandle::from_raw_meta(&job.raw_meta) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };

        let cancel_url = match handle.cancel_url {
            Some(u) => u,
            None => {
                // 该 endpoint 不支持取消
                return Ok(());
            }
        };

        let client = self.queue_client()?;
        client.cancel(&cancel_url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::Capability;

    #[test]
    fn status_mapping_covers_three_states() {
        // fal 三态 -> 归一化四态
        assert_eq!(map_fal_status("IN_QUEUE"), JobStatus::Queued);
        assert_eq!(map_fal_status("IN_PROGRESS"), JobStatus::Running);
        assert_eq!(map_fal_status("COMPLETED"), JobStatus::Succeeded);
    }

    #[test]
    fn status_mapping_is_case_insensitive() {
        assert_eq!(map_fal_status("in_progress"), JobStatus::Running);
        assert_eq!(map_fal_status("completed"), JobStatus::Succeeded);
    }

    #[test]
    fn status_unknown_and_error_are_failed() {
        // ERROR 与任何未知状态都归 Failed
        assert_eq!(map_fal_status("ERROR"), JobStatus::Failed);
        assert_eq!(map_fal_status("WHATEVER"), JobStatus::Failed);
        assert_eq!(map_fal_status(""), JobStatus::Failed);
    }

    #[test]
    fn build_body_includes_prompt_and_params() {
        // 构造一个文生图请求: prompt + num_images + image_size
        let mut params = serde_json::Map::new();
        params.insert("num_images".to_string(), json!(2));
        params.insert("image_size".to_string(), json!("landscape_4_3"));

        let req = GenRequest {
            capability: Capability::Text2Image,
            model: DEFAULT_T2I_MODEL.to_string(),
            prompt: Some("a red fox".to_string()),
            inputs: Vec::new(),
            params,
        };

        let body = build_fal_request_body(&req);
        assert_eq!(body.get("prompt").unwrap(), &json!("a red fox"));
        assert_eq!(body.get("num_images").unwrap(), &json!(2));
        assert_eq!(body.get("image_size").unwrap(), &json!("landscape_4_3"));
        // 纯文生图无输入, 不应出现 image_url
        assert!(body.get("image_url").is_none());
    }

    #[test]
    fn build_body_picks_first_image_input_url() {
        // 带一个 URL 形态的图片输入(图生图场景)
        let req = GenRequest {
            capability: Capability::Image2Image,
            model: "fal-ai/flux/dev/image-to-image".to_string(),
            prompt: Some("make it night".to_string()),
            inputs: vec![Asset::from_url(AssetKind::Image, "https://x/in.png")],
            params: serde_json::Map::new(),
        };
        let body = build_fal_request_body(&req);
        assert_eq!(body.get("image_url").unwrap(), &json!("https://x/in.png"));
    }

    #[test]
    fn extract_outputs_reads_images_array() {
        // fal 文生图典型返回
        let result = json!({
            "images": [
                {"url": "https://cdn.fal.ai/a.png", "content_type": "image/png"},
                {"url": "https://cdn.fal.ai/b.png", "content_type": "image/png"}
            ],
            "seed": 12345
        });
        let outputs = extract_outputs(&result);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].url.as_deref(), Some("https://cdn.fal.ai/a.png"));
        assert_eq!(outputs[1].url.as_deref(), Some("https://cdn.fal.ai/b.png"));
        assert_eq!(outputs[0].kind, AssetKind::Image);
    }

    #[test]
    fn capabilities_declare_image_and_video() {
        // fal 现在同时声明文生图与文生/图生视频(复用同一 Queue API)。
        let p = FalProvider::new();
        assert!(p.capabilities().contains(&Capability::Text2Image));
        assert!(p.capabilities().contains(&Capability::Text2Video));
        assert!(p.capabilities().contains(&Capability::Image2Video));
    }

    #[test]
    fn catalog_contains_video_models() {
        // catalog 应含视频 model 条目, 且能力标 video。
        let p = FalProvider::new();
        let cat = p.catalog();
        let t2v = cat
            .iter()
            .find(|e| e.capabilities.contains(&Capability::Text2Video))
            .expect("fal catalog 应含文生视频条目");
        assert_eq!(t2v.model_id, DEFAULT_T2V_MODEL);
        let i2v = cat
            .iter()
            .find(|e| e.capabilities.contains(&Capability::Image2Video))
            .expect("fal catalog 应含图生视频条目");
        assert_eq!(i2v.model_id, DEFAULT_I2V_MODEL);
    }

    #[test]
    fn capabilities_declare_upscale() {
        // fal 声明支持超分。
        let p = FalProvider::new();
        assert!(p.capabilities().contains(&Capability::Upscale));
    }

    #[test]
    fn catalog_contains_upscale_model() {
        // catalog 应含超分条目, 能力标 upscale, model 为 clarity-upscaler。
        let p = FalProvider::new();
        let cat = p.catalog();
        let up = cat
            .iter()
            .find(|e| e.capabilities.contains(&Capability::Upscale))
            .expect("fal catalog 应含超分条目");
        assert_eq!(up.model_id, DEFAULT_UPSCALE_MODEL);
        assert_eq!(up.alias.as_deref(), Some("fal-upscale"));
    }

    #[test]
    fn build_upscale_body_puts_url_in_image_url_with_factor() {
        // 超分: 待放大图(URL)写入 image_url; scale 参数(upscale_factor)经 params 透传。
        let mut params = serde_json::Map::new();
        params.insert("upscale_factor".to_string(), json!(2));
        let req = GenRequest {
            capability: Capability::Upscale,
            model: DEFAULT_UPSCALE_MODEL.to_string(),
            prompt: None,
            inputs: vec![Asset::from_url(AssetKind::Image, "https://x/low.png")],
            params,
        };
        let body = build_fal_request_body(&req);
        assert_eq!(body.get("image_url").unwrap(), &json!("https://x/low.png"));
        assert_eq!(body.get("upscale_factor").unwrap(), &json!(2));
    }

    #[test]
    fn build_upscale_body_local_image_becomes_data_uri() {
        // 本地图(内联字节)超分: image_url 应是 data URI(fal 接受), 复用 i2i 喂图路径。
        let req = GenRequest {
            capability: Capability::Upscale,
            model: DEFAULT_UPSCALE_MODEL.to_string(),
            prompt: None,
            // 0x01020304 -> base64 "AQIDBA=="
            inputs: vec![Asset::from_inline_bytes(AssetKind::Image, "image/png", vec![1, 2, 3, 4])],
            params: serde_json::Map::new(),
        };
        let body = build_fal_request_body(&req);
        assert_eq!(
            body.get("image_url").unwrap(),
            &json!("data:image/png;base64,AQIDBA==")
        );
    }

    #[test]
    fn extract_upscale_output_reads_single_image_object() {
        // clarity-upscaler 产物在单个 image 对象的 url; 应解析成一张 Image。
        let result = json!({ "image": { "url": "https://cdn.fal.ai/up.png", "width": 2048 } });
        let outputs = extract_outputs(&result);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, AssetKind::Image);
        assert_eq!(outputs[0].url.as_deref(), Some("https://cdn.fal.ai/up.png"));
    }

    #[test]
    fn extract_outputs_reads_video_object() {
        let result = json!({ "video": { "url": "https://cdn.fal.ai/v.mp4" } });
        let outputs = extract_outputs(&result);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, AssetKind::Video);
        assert_eq!(outputs[0].url.as_deref(), Some("https://cdn.fal.ai/v.mp4"));
    }

    #[test]
    fn handle_roundtrips_through_raw_meta() {
        // submit 写进 raw_meta 的句柄, poll 必须能原样还原出来(跨进程 store 还原的核心)。
        let handle = FalHandle {
            status_url: "https://queue.fal.run/x/requests/req-1/status".to_string(),
            response_url: "https://queue.fal.run/x/requests/req-1".to_string(),
            cancel_url: Some("https://queue.fal.run/x/requests/req-1/cancel".to_string()),
        };
        let raw_meta = handle.to_raw_meta("req-1");
        // 模拟从 store 读出的 Job(只带 raw_meta 句柄, 无内存表)
        let restored = FalHandle::from_raw_meta(&raw_meta).expect("应能从 raw_meta 还原句柄");
        assert_eq!(restored.status_url, handle.status_url);
        assert_eq!(restored.response_url, handle.response_url);
        assert_eq!(restored.cancel_url, handle.cancel_url);
    }

    #[test]
    fn handle_from_raw_meta_errors_when_handle_missing() {
        // 句柄丢失(raw_meta 里没有 status_url)时, 必须给清晰错误而非 panic。
        let bad = json!({ "request_id": "req-1" });
        let err = FalHandle::from_raw_meta(&bad).unwrap_err();
        assert!(err.to_string().contains("status_url"));
    }

    #[test]
    fn poll_input_job_carries_handle_for_reconstruction() {
        // 构造一个"从 store 读出"的 Job, 验证 poll 入参确实带着句柄可被还原。
        // 不打真实网络: 只验证 poll 还原句柄那一步的入参形态成立。
        let handle = FalHandle {
            status_url: "https://queue.fal.run/x/status".to_string(),
            response_url: "https://queue.fal.run/x".to_string(),
            cancel_url: None,
        };
        let job = Job {
            id: "req-9".to_string(),
            provider: PROVIDER_NAME.to_string(),
            status: JobStatus::Queued,
            outputs: Vec::new(),
            error: None,
            raw_meta: handle.to_raw_meta("req-9"),
        };
        let parsed = FalHandle::from_raw_meta(&job.raw_meta).expect("poll 应能从入参 Job 还原句柄");
        assert_eq!(parsed.status_url, "https://queue.fal.run/x/status");
        assert!(parsed.cancel_url.is_none());
    }
}
