//! 火山方舟 Ark Seedance 视频 provider —— D-014 的首个视频 provider, 验证 async-task 骨架。
//!
//! 与 volcengine(同走 Ark, 但那是图像 Seedream 的 OpenAI drop-in、同步)不同:
//! Seedance 是视频, 走异步任务(提交拿 task_id -> 轮询 -> 取视频 URL), 故复用
//! transport::async_task 通用骨架(D-012 的 C 类), 注入 Bearer 鉴权 + Ark 字段映射。
//!
//! 协议形态(Ark, base_url = https://ark.cn-beijing.volces.com/api/v3):
//!
//! - 提交: POST /contents/generations/tasks; body 为
//!   `{ "model": <model>, "content": [ {type:text,text:prompt}, 图生视频再加 {type:image_url,...} ] }`;
//!   响应含 task id(字段 "id", 形如 "cgt-...")。
//! - 轮询: GET /contents/generations/tasks/{task_id}; 响应 "status" 为
//!   queued/running/succeeded/failed/cancelled, 成功后产物视频 URL 在 "content.video_url"。
//! - 取消: DELETE /contents/generations/tasks/{task_id}(尽力而为)。
//!
//! 鉴权: Authorization: Bearer <ARK_API_KEY>(与 volcengine 同火山账号)。
//! 凭证只从环境变量取(ARK_API_KEY / IMAGECLI_ARK_KEY / IMAGECLI_SEEDANCE_KEY), 绝不写死。
//!
//! 产物 URL 约 24h 过期: 成功后由上层 generate 立即走正常 download 落盘(AssetKind::Video -> .mp4)。

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::keys;
use crate::core::provider::{Asset, AssetKind, Capability, GenRequest, Job, JobStatus, Provider};
use crate::transport::async_task::{
    extract_urls_at, AsyncTaskClient, BearerAuth, StatusMapping, TaskHandle,
};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "seedance";
/// Ark API 根地址(到 /api/v3 为止)。
const BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";
/// 异步任务资源路径。
const TASKS_PATH: &str = "/contents/generations/tasks";

/// 文生视频默认 model。
///
/// 注意: Ark 的 model id 带日期/版本后缀(如 -250428), 后缀会随控制台版本更新而变,
/// **以方舟控制台「在线推理 / 模型广场」实际可用的 model id 为准**, 这里只给一个合理默认,
/// 用户可用 `--model doubao-seedance-x-x-...` 覆盖, 绝不硬依赖某个确切日期后缀。
pub const DEFAULT_T2V_MODEL: &str = "doubao-seedance-1-0-lite-t2v-250428";
/// 图生视频默认 model(同上, 以控制台为准, 可被 --model 覆盖)。
pub const DEFAULT_I2V_MODEL: &str = "doubao-seedance-1-0-lite-i2v-250428";

/// Ark Seedance 任务状态字段映射(扩展点 2)。
/// queued/running/succeeded 各自归位; failed/cancelled 及任何未知 -> Failed(穷尽兜底)。
const STATUS_MAPPING: StatusMapping = StatusMapping {
    queued: &["queued"],
    running: &["running"],
    succeeded: &["succeeded"],
};

/// 产物视频 URL 在响应里的候选路径(扩展点 3): 优先 content.video_url, 兜底顶层 video_url。
const VIDEO_URL_POINTERS: &[&str] = &["/content/video_url", "/video_url"];

/// Seedance provider 实现。无状态: 只持有 HTTP 客户端与能力声明(句柄随 Job.raw_meta 流转)。
pub struct SeedanceProvider {
    http: reqwest::Client,
    caps: Vec<Capability>,
}

impl SeedanceProvider {
    /// 构造默认 Seedance provider。声明文生视频 + 图生视频两种能力。
    pub fn new() -> SeedanceProvider {
        SeedanceProvider {
            http: reqwest::Client::new(),
            caps: vec![Capability::Text2Video, Capability::Image2Video],
        }
    }

    /// 取 Bearer 鉴权器。无 key 时返回带中文指引的错误, 绝不 panic、绝不写死 key。
    fn bearer(&self) -> anyhow::Result<BearerAuth> {
        let key = keys::require_candidates_key(
            &keys::SEEDANCE_KEY_ENV_CANDIDATES,
            PROVIDER_NAME,
            keys::SEEDANCE_KEY_MISSING_HINT,
        )?;
        Ok(BearerAuth::new(key))
    }

    /// 构造一个带 Bearer 鉴权的 async-task 客户端。
    fn task_client(&self) -> anyhow::Result<AsyncTaskClient> {
        let auth = self.bearer()?;
        Ok(AsyncTaskClient::new(self.http.clone(), Box::new(auth)))
    }
}

impl Default for SeedanceProvider {
    fn default() -> SeedanceProvider {
        SeedanceProvider::new()
    }
}

/// 提交端点 URL。
fn submit_url() -> String {
    format!("{}{}", BASE_URL, TASKS_PATH)
}

/// 用 task_id 拼查询/取消端点 URL。
fn task_url(task_id: &str) -> String {
    format!("{}{}/{}", BASE_URL, TASKS_PATH, task_id)
}

/// 由 GenRequest 构造 Seedance 提交请求体(纯函数, 便于离线单测)。
///
/// 形态: `{ "model": <model>, "content": [ ... ], <透传的自由参数> }`。
///   - prompt 存在则作为一条 `{type:"text", text:<prompt>}` 加入 content。
///   - 每个 URL 形态的图片输入作为一条 `{type:"image_url", image_url:{url}}` 加入 content
///     (图生视频用; 纯文生视频通常无图片输入)。
///   - params 里的自由参数(如 resolution / duration / ratio / seed)整体并入顶层透传。
pub fn build_seedance_body(req: &GenRequest) -> Value {
    let mut content: Vec<Value> = Vec::new();

    // 文本提示词
    if let Some(prompt) = &req.prompt {
        content.push(json!({ "type": "text", "text": prompt }));
    }

    // 图片输入(图生视频): 仅接受已是 URL 的输入(本地路径需先上传, 与 fal/replicate 一致)。
    for asset in req.inputs.iter() {
        let is_image = matches!(asset.kind, AssetKind::Image);
        if is_image {
            if let Some(url) = &asset.url {
                content.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
        }
    }

    // 顶层 body: model + content
    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), json!(req.model));
    body.insert("content".to_string(), Value::Array(content));

    // 透传自由参数(用户 --param 优先级最高, 覆盖同名默认)。
    for (k, v) in req.params.iter() {
        body.insert(k.clone(), v.clone());
    }

    Value::Object(body)
}

/// 把 Ark Seedance 的状态字符串映射到归一化 JobStatus(纯函数包装, 便于离线单测)。
pub fn map_seedance_status(raw: &str) -> JobStatus {
    STATUS_MAPPING.map(raw)
}

/// 从 Seedance 查询结果里抽取视频产物(纯函数, 便于离线单测)。
pub fn extract_seedance_outputs(result: &Value) -> Vec<Asset> {
    extract_urls_at(result, VIDEO_URL_POINTERS, AssetKind::Video)
}

/// 抽取响应里可能的报错文本(失败时给排查上下文)。
/// Ark 失败体常见 `error.message` 或顶层 `error`; 兜底直接 stringify。
fn extract_error_text(resp: &Value) -> String {
    if let Some(msg) = resp
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return msg.to_string();
    }
    if let Some(msg) = resp.get("error").and_then(|v| v.as_str()) {
        if !msg.is_empty() {
            return msg.to_string();
        }
    }
    resp.to_string()
}

#[async_trait]
impl Provider for SeedanceProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // 暴露文生视频(alias "seedance")与图生视频两条默认 model。
        // available 由聚合层按 has_key 覆盖; est_cost 走 pricing(视频量级)。
        vec![
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_T2V_MODEL,
                Some("seedance"),
                Capability::Text2Video,
            ),
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_I2V_MODEL,
                Some("seedance-i2v"),
                Capability::Image2Video,
            ),
        ]
    }

    fn has_key(&self) -> bool {
        keys::resolve_candidates_key(&keys::SEEDANCE_KEY_ENV_CANDIDATES, PROVIDER_NAME).is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        // 占位 schema: Ark 真实参数随 model 而异, 以方舟控制台为准, MVP 不打网络。
        Ok(json!({
            "provider": PROVIDER_NAME,
            "model": model,
            "note": "占位 schema; Ark Seedance 真实参数(分辨率/时长/比例)随 model 而异, 以方舟控制台为准",
            "common_params": {
                "prompt": "string, 文本提示词",
                "resolution": "string, 如 720p / 1080p(随 model)",
                "duration": "int, 视频时长秒数(随 model)",
                "ratio": "string, 画面比例, 如 16:9 / 9:16(随 model)"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        // 鉴权(无 key 在此返回中文错误, 不 panic、不写死 key)
        let client = self.task_client()?;

        // 构造请求体并提交
        let body = build_seedance_body(&req);
        let resp = client.submit_task(&submit_url(), &body).await?;

        // 解析 task id(Ark 返回字段 "id")。缺 id 无法后续轮询, 给清晰中文错误。
        let task_id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Seedance 提交未返回 task id(响应: {}), 无法轮询",
                    resp
                )
            })?
            .to_string();

        // 句柄: task_id + 查询 URL, 随 Job.raw_meta 跨进程流转(D-007)。
        let handle = TaskHandle {
            task_id: task_id.clone(),
            query_url: task_url(&task_id),
        };

        // 提交成功后任务处于排队态; 归一化为 Queued。
        Ok(Job {
            id: task_id,
            provider: PROVIDER_NAME.to_string(),
            status: JobStatus::Queued,
            outputs: Vec::new(),
            error: None,
            raw_meta: handle.to_raw_meta(),
        })
    }

    async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
        // 从入参 Job 的 raw_meta 还原句柄(provider 无状态, 句柄随 Job 流转)。
        let handle = TaskHandle::from_raw_meta(&job.raw_meta)?;
        let client = self.task_client()?;

        // GET 查询最新任务状态
        let polled = client.query_task(&handle.query_url).await?;
        let raw_status = polled.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let status = map_seedance_status(raw_status);

        // 保留句柄不丢, 合并本次原始状态; 让后续轮询仍能继续。
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
                next.outputs = extract_seedance_outputs(&polled);
                if let Some(obj) = next.raw_meta.as_object_mut() {
                    obj.insert("result".to_string(), polled.clone());
                }
            }
            JobStatus::Failed => {
                next.error = Some(format!(
                    "Seedance 任务失败({}): {}",
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
        // 句柄缺失不算硬错误(尽力而为)。
        let handle = match TaskHandle::from_raw_meta(&job.raw_meta) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };
        let client = self.task_client()?;
        // Ark 取消是 DELETE 任务资源(查询/取消同 URL)。
        client.delete_task(&handle.query_url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t2v_req(prompt: &str) -> GenRequest {
        GenRequest {
            capability: Capability::Text2Video,
            model: DEFAULT_T2V_MODEL.to_string(),
            prompt: Some(prompt.to_string()),
            inputs: Vec::new(),
            params: serde_json::Map::new(),
        }
    }

    #[test]
    fn capabilities_declare_video() {
        // Seedance 必须声明视频能力, 不声明 text2image(避免 help 误导)。
        let p = SeedanceProvider::new();
        assert!(p.capabilities().contains(&Capability::Text2Video));
        assert!(p.capabilities().contains(&Capability::Image2Video));
        assert!(!p.capabilities().contains(&Capability::Text2Image));
    }

    #[test]
    fn status_mapping_exhaustive() {
        assert_eq!(map_seedance_status("queued"), JobStatus::Queued);
        assert_eq!(map_seedance_status("running"), JobStatus::Running);
        assert_eq!(map_seedance_status("succeeded"), JobStatus::Succeeded);
        // failed / cancelled / 未知 -> Failed
        assert_eq!(map_seedance_status("failed"), JobStatus::Failed);
        assert_eq!(map_seedance_status("cancelled"), JobStatus::Failed);
        assert_eq!(map_seedance_status("whatever"), JobStatus::Failed);
        // 大小写不敏感
        assert_eq!(map_seedance_status("RUNNING"), JobStatus::Running);
    }

    #[test]
    fn build_body_t2v_has_model_and_text_content() {
        let body = build_seedance_body(&t2v_req("a cat surfing"));
        assert_eq!(body.get("model").unwrap(), &json!(DEFAULT_T2V_MODEL));
        let content = body.get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], json!({ "type": "text", "text": "a cat surfing" }));
    }

    #[test]
    fn build_body_i2v_appends_image_url_content() {
        // 图生视频: text + image_url 两条 content。
        let req = GenRequest {
            capability: Capability::Image2Video,
            model: DEFAULT_I2V_MODEL.to_string(),
            prompt: Some("make it move".to_string()),
            inputs: vec![Asset::from_url(AssetKind::Image, "https://x/in.png")],
            params: serde_json::Map::new(),
        };
        let body = build_seedance_body(&req);
        let content = body.get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], json!("text"));
        assert_eq!(content[1]["type"], json!("image_url"));
        assert_eq!(content[1]["image_url"]["url"], json!("https://x/in.png"));
    }

    #[test]
    fn build_body_passes_through_params() {
        // 自由参数并入顶层透传。
        let mut req = t2v_req("dog");
        req.params.insert("resolution".to_string(), json!("720p"));
        req.params.insert("duration".to_string(), json!(5));
        let body = build_seedance_body(&req);
        assert_eq!(body.get("resolution").unwrap(), &json!("720p"));
        assert_eq!(body.get("duration").unwrap(), &json!(5));
    }

    #[test]
    fn extract_outputs_reads_content_video_url_as_video_kind() {
        let result = json!({
            "id": "cgt-1",
            "status": "succeeded",
            "content": { "video_url": "https://tos-cn/seedance/out.mp4" }
        });
        let outputs = extract_seedance_outputs(&result);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, AssetKind::Video);
        assert_eq!(outputs[0].url.as_deref(), Some("https://tos-cn/seedance/out.mp4"));
    }

    #[test]
    fn extract_outputs_empty_when_no_video() {
        assert!(extract_seedance_outputs(&json!({ "status": "running" })).is_empty());
        // 失败体的错误文本可从 error.message 抽出
        let err = json!({ "status": "failed", "error": { "message": "prompt rejected" } });
        assert_eq!(extract_error_text(&err), "prompt rejected");
    }

    #[test]
    fn submit_url_and_task_url_are_well_formed() {
        assert_eq!(
            submit_url(),
            "https://ark.cn-beijing.volces.com/api/v3/contents/generations/tasks"
        );
        assert_eq!(
            task_url("cgt-abc"),
            "https://ark.cn-beijing.volces.com/api/v3/contents/generations/tasks/cgt-abc"
        );
    }
}
