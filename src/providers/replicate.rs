//! Replicate provider(D-011 海外, D-012 的 C 类: 异步 prediction 提交+轮询)。
//!
//! WebFetch 核实(2026-06-26, replicate.com/docs/reference/http):
//!   - 鉴权: header `Authorization: Bearer <REPLICATE_API_TOKEN>`(官方文档用 Bearer)。
//!   - 创建 prediction(官方模型): POST https://api.replicate.com/v1/models/{owner}/{name}/predictions
//!     body 仅 `{ "input": { ... } }`(官方模型无需指定 version)。
//!   - 轮询: GET 返回体里的 `urls.get`(或 /v1/predictions/{id}), 拿 `status` 与 `output`。
//!   - 状态: starting / processing / succeeded / failed / canceled。
//!   - 产物: `output` 字段, 文件类是 HTTPS URL(flux-schnell 为 URL 字符串数组)。
//!
//! 与 fal(http-queue)同属 C 类异步, 但 Replicate 自有 schema(提交响应里产物字段是
//! `output`、轮询 URL 在 `urls.get`、状态是小写枚举), 与 http_queue.rs 的
//! QueueSubmitResponse/QueueStatusResponse 结构不一致, 故写专属 provider:
//! HTTP 收发直接用 reqwest, 但把"请求体构造 / 状态映射 / 产物抽取"抽成纯函数离线单测,
//! 把句柄(轮询 URL)随 Job.raw_meta 跨进程流转(与 fal 的 FalHandle 同模式, D-007)。
//!
//! 凭证只从环境变量取(REPLICATE_API_TOKEN 优先, IMAGECLI_REPLICATE_KEY 回退), 绝不写死。

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::config::keys;
use crate::core::provider::{Asset, AssetKind, Capability, GenRequest, Job, JobStatus, Provider};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "replicate";
/// Replicate API 根地址。
const REPLICATE_API_BASE: &str = "https://api.replicate.com/v1";
/// text2image 默认 model(官方模型 owner/name 形式; BFL Flux Schnell, 最快)。
pub const DEFAULT_T2I_MODEL: &str = "black-forest-labs/flux-schnell";

/// 一次提交后需记住的句柄: 后续 poll/cancel 要用到的 URL。
///
/// 与 fal 同模式: 句柄随 Job.raw_meta 落进 store 跨进程流转, provider 本身无状态。
/// poll 用 get_url 查状态/取产物; cancel 用 cancel_url(部分情况可缺)。
#[derive(Debug, Clone)]
struct ReplicateHandle {
    /// prediction id(用于展示与兜底拼 URL)。
    prediction_id: String,
    /// 轮询用的完整 URL(Replicate 返回的 urls.get)。
    get_url: String,
    /// 取消用的 URL(Replicate 返回的 urls.cancel, 可缺)。
    cancel_url: Option<String>,
}

impl ReplicateHandle {
    /// 序列化进 Job.raw_meta。submit 时写, poll/cancel 时读。
    fn to_raw_meta(&self) -> Value {
        json!({
            "prediction_id": self.prediction_id,
            "get_url": self.get_url,
            "cancel_url": self.cancel_url,
        })
    }

    /// 从 Job.raw_meta 还原句柄。缺 get_url 时给清晰中文错误(句柄已丢失)。
    fn from_raw_meta(raw_meta: &Value) -> anyhow::Result<ReplicateHandle> {
        let prediction_id = raw_meta
            .get("prediction_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let get_url = raw_meta
            .get("get_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Job.raw_meta 缺少 get_url, 无法轮询 Replicate 任务(句柄已丢失)")
            })?
            .to_string();
        let cancel_url = raw_meta
            .get("cancel_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(ReplicateHandle {
            prediction_id,
            get_url,
            cancel_url,
        })
    }
}

/// Replicate provider 实现。无状态: 只持有 HTTP 客户端与能力声明。
pub struct ReplicateProvider {
    http: reqwest::Client,
    caps: Vec<Capability>,
}

impl ReplicateProvider {
    /// 构造一个默认 Replicate provider。
    pub fn new() -> ReplicateProvider {
        ReplicateProvider {
            http: reqwest::Client::new(),
            // MVP 声明 text2image(flux-schnell 文生图); 后续可按 model 扩能力。
            caps: vec![Capability::Text2Image],
        }
    }

    /// 取鉴权头值: `Bearer <token>`。无 key 时返回带中文指引的错误, 绝不 panic。
    fn auth_header_value() -> anyhow::Result<String> {
        let key = keys::require_candidates_key(
            &keys::REPLICATE_KEY_ENV_CANDIDATES,
            PROVIDER_NAME,
            keys::REPLICATE_KEY_MISSING_HINT,
        )?;
        Ok(format!("Bearer {}", key))
    }
}

impl Default for ReplicateProvider {
    fn default() -> ReplicateProvider {
        ReplicateProvider::new()
    }
}

/// 把 Replicate 的状态字符串映射到归一化 JobStatus(纯函数, 便于离线单测)。
///
/// Replicate 五态: starting / processing / succeeded / failed / canceled。
/// 映射取舍(沿用 fal 同款语义, 与 JobStatus 的 Queued/Running 区分一致):
///   - starting  -> Queued (prediction 初始化中, 尚未真正跑, 等价排队)
///   - processing-> Running(模型执行中)
///   - succeeded -> Succeeded(终态)
///   - failed / canceled / 任何未知 -> Failed(终态, 穷尽兜底, 不把脏状态当运行中)
///
/// 注: 任务要点称 "starting/processing -> running", 本实现把二者细分为 Queued/Running,
/// 信息更全且与 fal 一致; 关键契约不变(两者皆非终态、succeeded/failed 各自归位)。
pub fn map_replicate_status(raw: &str) -> JobStatus {
    let lowered = raw.to_ascii_lowercase();
    match lowered.as_str() {
        "starting" => JobStatus::Queued,
        "processing" => JobStatus::Running,
        "succeeded" => JobStatus::Succeeded,
        // failed / canceled 以及任何未知状态都视为失败终态
        _ => JobStatus::Failed,
    }
}

/// 由 GenRequest 构造 Replicate 创建 prediction 的请求体(纯函数, 便于离线单测)。
///
/// 形态: `{ "input": { "prompt": ..., <透传的自由参数> } }`。
///   - prompt 存在则写入 input.prompt。
///   - params 里的自由参数(aspect_ratio / num_outputs / seed / ...)并入 input 顶层透传。
///   - 第一张 image 输入的 URL 写入 input.image(供图生图类官方模型用); 纯文生图通常无此项。
pub fn build_prediction_body(req: &GenRequest) -> Value {
    let mut input = Map::new();

    // prompt
    if let Some(prompt) = &req.prompt {
        input.insert("prompt".to_string(), json!(prompt));
    }

    // 透传自由参数(用户 --param 优先级最高, 直接覆盖同名默认)
    for (k, v) in req.params.iter() {
        input.insert(k.clone(), v.clone());
    }

    // 第一张图片输入 -> input.image(若已是 URL)。本地路径需先上传, MVP 仅接受 URL 输入。
    for asset in req.inputs.iter() {
        let is_image = matches!(asset.kind, AssetKind::Image);
        if is_image {
            if let Some(url) = &asset.url {
                input.insert("image".to_string(), json!(url));
                break;
            }
        }
    }

    json!({ "input": Value::Object(input) })
}

/// 从 Replicate 的 `output` 字段抽取产物素材(纯函数, 便于离线单测)。
///
/// Replicate 不同模型 output 形态不一, 这里覆盖常见几种(字符串 / 字符串数组 /
/// 对象数组 / 单对象取 "url" 字段)。flux-schnell 即"字符串数组"形态。
/// 取不到任何 URL 返回空向量, 由调用方据响应兜底报错。kind 统一按 Image
/// (本 provider MVP 只声明 text2image; 接视频模型时再按 model 区分 kind)。
pub fn extract_replicate_outputs(output: &Value) -> Vec<Asset> {
    let mut out = Vec::new();
    push_assets_from_value(output, &mut out);
    out
}

/// 递归地从一个 output 值里收集图片 URL(支持字符串 / 数组 / 对象三种形态)。
fn push_assets_from_value(value: &Value, out: &mut Vec<Asset>) {
    match value {
        // 直接是 URL 字符串
        Value::String(s) => {
            if !s.is_empty() {
                out.push(Asset::from_url(AssetKind::Image, s));
            }
        }
        // 数组: 逐项递归(每项可能是字符串或对象)
        Value::Array(arr) => {
            for item in arr.iter() {
                push_assets_from_value(item, out);
            }
        }
        // 对象: 取 "url" 字段(部分模型把产物包成 { "url": ... })
        Value::Object(obj) => {
            if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    out.push(Asset::from_url(AssetKind::Image, url));
                }
            }
        }
        // 其它类型(null/number/bool): 非产物, 跳过
        _ => {}
    }
}

/// 抽取响应里可能的报错文本(失败或无产物时给排查上下文)。
/// Replicate 失败体: 顶层 `error` 字段(字符串); 兜底直接 stringify。
fn extract_error_text(resp: &Value) -> String {
    if let Some(msg) = resp.get("error").and_then(|v| v.as_str()) {
        if !msg.is_empty() {
            return msg.to_string();
        }
    }
    resp.to_string()
}

#[async_trait]
impl Provider for ReplicateProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // 默认暴露 flux-schnell(alias "flux-schnell", 与 fal 的 "flux" 区分避免歧义)。
        vec![crate::core::catalog::ModelEntry::single(
            PROVIDER_NAME,
            DEFAULT_T2I_MODEL,
            Some("flux-schnell"),
            Capability::Text2Image,
        )]
    }

    fn has_key(&self) -> bool {
        keys::resolve_candidates_key(&keys::REPLICATE_KEY_ENV_CANDIDATES, PROVIDER_NAME).is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        // 占位 schema: Replicate 每个 model 有自己的 OpenAPI input schema, MVP 不打网络。
        Ok(json!({
            "provider": PROVIDER_NAME,
            "model": model,
            "note": "占位 schema; Replicate 真实 input 参数随 model 而异, 见 replicate.com 模型页",
            "common_params": {
                "prompt": "string, 文本提示词",
                "aspect_ratio": "string, 如 1:1 / 16:9(部分模型)",
                "num_outputs": "int, 生成张数(部分模型)",
                "seed": "int, 随机种子"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        // 1. 鉴权(无 key 在此返回中文错误, 不 panic, 绝不写死 key)
        let auth = Self::auth_header_value()?;

        // 2. 官方模型走 /models/{owner}/{name}/predictions, body 仅 { "input": {...} }
        let url = format!("{}/models/{}/predictions", REPLICATE_API_BASE, req.model);
        let body = build_prediction_body(&req);

        // 3. 创建 prediction
        let resp = self
            .http
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await?;
        let http_status = resp.status();
        if !http_status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Replicate 创建 prediction 失败: HTTP {} - {}", http_status, text);
        }
        let created: Value = resp.json().await?;

        // 4. 解析 prediction id / 状态 / 轮询与取消 URL
        let prediction_id = created
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let raw_status = created.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let status = map_replicate_status(raw_status);

        // get_url: 优先用响应里的 urls.get; 缺失时按 id 兜底拼标准 URL
        let get_url = created
            .get("urls")
            .and_then(|u| u.get("get"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}/predictions/{}", REPLICATE_API_BASE, prediction_id));
        let cancel_url = created
            .get("urls")
            .and_then(|u| u.get("cancel"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let handle = ReplicateHandle {
            prediction_id: prediction_id.clone(),
            get_url,
            cancel_url,
        };

        // 5. 若创建即返回终态(小模型偶发同步完成), 顺手解析产物; 否则留待 poll。
        let mut outputs = Vec::new();
        if status == JobStatus::Succeeded {
            if let Some(output) = created.get("output") {
                outputs = extract_replicate_outputs(output);
            }
        }
        let error = match status {
            JobStatus::Failed => Some(format!(
                "Replicate prediction 失败: {}",
                extract_error_text(&created)
            )),
            _ => None,
        };

        Ok(Job {
            id: prediction_id,
            provider: PROVIDER_NAME.to_string(),
            status,
            outputs,
            error,
            raw_meta: handle.to_raw_meta(),
        })
    }

    async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
        // 从入参 Job 的 raw_meta 还原句柄(D-007: provider 无状态, 句柄随 Job 流转)。
        let handle = ReplicateHandle::from_raw_meta(&job.raw_meta)?;
        let auth = Self::auth_header_value()?;

        // GET 轮询 URL 取最新 prediction 状态
        let resp = self
            .http
            .get(&handle.get_url)
            .header("Authorization", &auth)
            .send()
            .await?;
        let http_status = resp.status();
        if !http_status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Replicate 轮询失败: HTTP {} - {}", http_status, text);
        }
        let polled: Value = resp.json().await?;

        let raw_status = polled.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let status = map_replicate_status(raw_status);

        // 保留句柄(get_url 等)不丢, 让后续轮询仍能继续; 合并本次原始状态。
        let mut next_meta = job.raw_meta.clone();
        if let Some(obj) = next_meta.as_object_mut() {
            obj.insert("last_status".to_string(), json!(raw_status));
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
                if let Some(output) = polled.get("output") {
                    next.outputs = extract_replicate_outputs(output);
                }
                if let Some(obj) = next.raw_meta.as_object_mut() {
                    if let Some(output) = polled.get("output") {
                        obj.insert("output".to_string(), output.clone());
                    }
                }
            }
            JobStatus::Failed => {
                next.error = Some(format!(
                    "Replicate prediction 失败({}): {}",
                    raw_status,
                    extract_error_text(&polled)
                ));
            }
            JobStatus::Queued => {}
            JobStatus::Running => {}
        }

        Ok(next)
    }

    async fn cancel(&self, job: &Job) -> anyhow::Result<()> {
        // 句柄缺失或无 cancel_url 都不算硬错误(尽力而为)。
        let handle = match ReplicateHandle::from_raw_meta(&job.raw_meta) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };
        let cancel_url = match handle.cancel_url {
            Some(u) => u,
            None => return Ok(()),
        };
        let auth = Self::auth_header_value()?;
        // Replicate 取消是 POST {cancel_url}
        let resp = self
            .http
            .post(&cancel_url)
            .header("Authorization", &auth)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Replicate 取消失败: HTTP {} - {}", status, text);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AssetSource;

    fn t2i_req(prompt: &str) -> GenRequest {
        GenRequest {
            capability: Capability::Text2Image,
            model: DEFAULT_T2I_MODEL.to_string(),
            prompt: Some(prompt.to_string()),
            inputs: Vec::new(),
            params: serde_json::Map::new(),
        }
    }

    #[test]
    fn status_mapping_covers_five_states() {
        // starting/processing 为非终态; succeeded/failed/canceled 各自归位。
        assert_eq!(map_replicate_status("starting"), JobStatus::Queued);
        assert_eq!(map_replicate_status("processing"), JobStatus::Running);
        assert_eq!(map_replicate_status("succeeded"), JobStatus::Succeeded);
        assert_eq!(map_replicate_status("failed"), JobStatus::Failed);
        assert_eq!(map_replicate_status("canceled"), JobStatus::Failed);
        // starting/processing 必须都是非终态(对应"轮询应继续")
        assert!(!map_replicate_status("starting").is_terminal());
        assert!(!map_replicate_status("processing").is_terminal());
    }

    #[test]
    fn status_mapping_is_case_insensitive_and_unknown_is_failed() {
        assert_eq!(map_replicate_status("PROCESSING"), JobStatus::Running);
        assert_eq!(map_replicate_status("Succeeded"), JobStatus::Succeeded);
        // 未知状态归 Failed(穷尽兜底)
        assert_eq!(map_replicate_status("whatever"), JobStatus::Failed);
        assert_eq!(map_replicate_status(""), JobStatus::Failed);
    }

    #[test]
    fn build_body_wraps_input_with_prompt_and_params() {
        // Replicate 请求体必须把字段包进 "input" 对象。
        let mut req = t2i_req("a red fox in snow");
        req.params.insert("aspect_ratio".to_string(), json!("16:9"));
        req.params.insert("num_outputs".to_string(), json!(2));
        let body = build_prediction_body(&req);
        // 顶层只有 input
        let input = body.get("input").expect("应有 input 包裹层");
        assert_eq!(input["prompt"].as_str(), Some("a red fox in snow"));
        assert_eq!(input["aspect_ratio"].as_str(), Some("16:9"));
        assert_eq!(input["num_outputs"].as_i64(), Some(2));
        // 纯文生图无输入, 不应出现 image
        assert!(input.get("image").is_none());
    }

    #[test]
    fn build_body_picks_first_image_input_url() {
        // 图生图: 第一张 URL 形态输入写入 input.image
        let req = GenRequest {
            capability: Capability::Image2Image,
            model: "black-forest-labs/flux-dev".to_string(),
            prompt: Some("make it night".to_string()),
            inputs: vec![Asset::from_url(AssetKind::Image, "https://x/in.png")],
            params: serde_json::Map::new(),
        };
        let body = build_prediction_body(&req);
        assert_eq!(body["input"]["image"].as_str(), Some("https://x/in.png"));
    }

    #[test]
    fn extract_outputs_from_string_array() {
        // flux-schnell 典型 output: URL 字符串数组
        let output = json!([
            "https://replicate.delivery/a.webp",
            "https://replicate.delivery/b.webp"
        ]);
        let outputs = extract_replicate_outputs(&output);
        assert_eq!(outputs.len(), 2);
        match outputs[0].source() {
            AssetSource::Url(u) => assert_eq!(u, "https://replicate.delivery/a.webp"),
            _ => panic!("应为 Url 来源"),
        }
        assert_eq!(outputs[0].kind, AssetKind::Image);
    }

    #[test]
    fn extract_outputs_from_single_string() {
        // 部分模型 output 是单个 URL 字符串
        let output = json!("https://replicate.delivery/single.png");
        let outputs = extract_replicate_outputs(&output);
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            outputs[0].url.as_deref(),
            Some("https://replicate.delivery/single.png")
        );
    }

    #[test]
    fn extract_outputs_from_object_array() {
        // 部分模型把产物包成 { "url": ... } 对象
        let output = json!([{ "url": "https://replicate.delivery/obj.png" }]);
        let outputs = extract_replicate_outputs(&output);
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            outputs[0].url.as_deref(),
            Some("https://replicate.delivery/obj.png")
        );
    }

    #[test]
    fn extract_outputs_empty_when_no_url() {
        // null / 空数组 -> 无产物
        assert!(extract_replicate_outputs(&json!(null)).is_empty());
        assert!(extract_replicate_outputs(&json!([])).is_empty());
        // error 文本可从顶层 error 抽出
        let err_resp = json!({ "status": "failed", "error": "NSFW content detected" });
        assert_eq!(extract_error_text(&err_resp), "NSFW content detected");
    }

    #[test]
    fn handle_roundtrips_through_raw_meta() {
        // submit 写进 raw_meta 的句柄, poll 必须能原样还原(跨进程 store 还原核心)。
        let handle = ReplicateHandle {
            prediction_id: "pred-1".to_string(),
            get_url: "https://api.replicate.com/v1/predictions/pred-1".to_string(),
            cancel_url: Some("https://api.replicate.com/v1/predictions/pred-1/cancel".to_string()),
        };
        let raw_meta = handle.to_raw_meta();
        let restored = ReplicateHandle::from_raw_meta(&raw_meta).expect("应能还原句柄");
        assert_eq!(restored.prediction_id, "pred-1");
        assert_eq!(restored.get_url, handle.get_url);
        assert_eq!(restored.cancel_url, handle.cancel_url);
    }

    #[test]
    fn handle_from_raw_meta_errors_when_get_url_missing() {
        // 句柄丢失(无 get_url)时给清晰错误而非 panic。
        let bad = json!({ "prediction_id": "pred-1" });
        let err = ReplicateHandle::from_raw_meta(&bad).unwrap_err();
        assert!(err.to_string().contains("get_url"));
    }
}
