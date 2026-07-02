//! 可灵 Kling 视频 provider —— D-014 的第二个视频 provider, 验证 async-task 骨架的鉴权注入点。
//!
//! 与 Seedance(Bearer)不同, 可灵用**本地现算的 HS256 JWT** 鉴权:
//! 用 AccessKey 作 iss、SecretKey 作 HMAC-SHA256 签名密钥, 在每次发请求时本地拼出
//! 一个短时效 JWT 塞进 `Authorization: Bearer <jwt>`。SecretKey 永不上行(只参与本地签名)。
//!
//! 这正是 async_task 骨架 `TaskAuth` 扩展点的意义: 骨架(提交->task_id->轮询->取产物)不变,
//! 只把"算鉴权头"这件事换成 `JwtAuth`。
//!
//! 协议形态(base = https://api.klingai.com, 另有 api-singapore.klingai.com):
//! - 文生视频提交: POST /v1/videos/text2video; 查询 GET /v1/videos/text2video/{task_id}
//! - 图生视频提交: POST /v1/videos/image2video; 查询 GET /v1/videos/image2video/{task_id}
//! - 请求体: `model_name`(注意不是 model!)、prompt、negative_prompt、mode、duration、aspect_ratio;
//!   图生视频再加 image。
//! - 提交响应信封: `{code, message, data:{task_id, task_status}}`。
//! - 查询响应: `data.task_status` ∈ submitted/processing/succeed/failed(注意是 succeed 不是 succeeded);
//!   产物视频在 `data.task_result.videos[].url`。
//!
//! 凭证只从环境变量取(KLING_ACCESS_KEY + KLING_SECRET_KEY, 或 IMAGECLI_KLING_AK/SK), 绝不写死。

use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::config::keys;
use crate::core::provider::{
    Asset, AssetKind, Capability, GenRequest, InputImage, Job, JobStatus, Provider,
};
use crate::transport::async_task::{
    extract_urls_at, AsyncTaskClient, StatusMapping, TaskAuth, TaskHandle,
};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "kling";
/// 可灵 API 根地址(默认大陆站, 可用环境变量 IMAGECLI_KLING_BASE_URL 覆盖为新加坡站)。
const DEFAULT_BASE_URL: &str = "https://api.klingai.com";
/// 覆盖 base_url 的环境变量名(可灵另有 https://api-singapore.klingai.com 新加坡站)。
const BASE_URL_ENV: &str = "IMAGECLI_KLING_BASE_URL";
/// 文生视频路径段。
const T2V_PATH: &str = "/v1/videos/text2video";
/// 图生视频路径段。
const I2V_PATH: &str = "/v1/videos/image2video";

/// JWT 有效期(秒): exp = now + 1800(30 分钟), 与可灵示例一致。
const JWT_TTL_SECS: u64 = 1800;
/// JWT 生效前置宽限(秒): nbf = now - 5, 抵消轻微时钟漂移避免 "token not yet valid"。
const JWT_NBF_SKEW_SECS: u64 = 5;

/// 文生视频默认 model_name(可被 --model 覆盖)。
///
/// 注意: 可灵字段名是 `model_name` 而非通用的 `model`; model 版本(kling-v1-6 / kling-v2-1-master 等)
/// 随控制台更新, 这里给一个合理默认, 用户可用 `--model kling-...` 覆盖, 不硬依赖某确切版本。
pub const DEFAULT_T2V_MODEL: &str = "kling-v2-1-master";
/// 图生视频默认 model_name(图生视频在较老版本上更稳, 取 v1-6 作默认, 可被 --model 覆盖)。
pub const DEFAULT_I2V_MODEL: &str = "kling-v1-6";

/// 可灵任务状态字段映射(扩展点 2)。
/// submitted=排队, processing=执行中, succeed=成功(注意是 succeed 不是 succeeded);
/// failed 及任何未知 -> Failed(穷尽兜底)。
const STATUS_MAPPING: StatusMapping = StatusMapping {
    queued: &["submitted"],
    running: &["processing"],
    succeeded: &["succeed"],
};

/// 产物视频 URL 在响应里的候选路径(扩展点 3): data.task_result.videos[](每个元素含 url)。
const VIDEO_URL_POINTERS: &[&str] = &["/data/task_result/videos"];

/// HMAC-SHA256 类型别名(可灵 JWT 与火山 V4 同算法, 这里取 SHA256 变体)。
type HmacSha256 = Hmac<Sha256>;

/// 可灵 JWT 鉴权器(TaskAuth 的 JWT 实现): 持有 AccessKey 与 SecretKey, headers() 里本地现算 HS256 JWT。
///
/// 为什么不引 jsonwebtoken: payload 固定只有 iss/exp/nbf 三个字段, 自己用 hmac+sha2+base64 拼更轻、
/// 依赖更少, 且能把"算 JWT"做成可离线单测的纯函数。SecretKey 只在本地参与签名, 绝不上行。
pub struct JwtAuth {
    /// AccessKey, 作 JWT payload 的 iss。
    access_key: String,
    /// SecretKey, 作 HMAC-SHA256 的签名密钥(永不上行)。
    secret_key: String,
}

impl JwtAuth {
    /// 用 AK/SK 构造。AK/SK 只从调用方(provider 读 env)注入, 本模块不碰来源。
    pub fn new(access_key: impl Into<String>, secret_key: impl Into<String>) -> JwtAuth {
        JwtAuth {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
        }
    }
}

impl TaskAuth for JwtAuth {
    fn headers(
        &self,
        _method: &str,
        _url: &str,
        _body: &[u8],
    ) -> anyhow::Result<Vec<(String, String)>> {
        // JWT 与请求方法/URL/body 无关(只依赖 AK/SK 与当前时间), 忽略这三个入参。
        let now = current_unix_secs()?;
        let jwt = build_kling_jwt(&self.access_key, &self.secret_key, now);
        Ok(vec![("Authorization".to_string(), format!("Bearer {}", jwt))])
    }
}

/// 取当前 Unix 时间戳(秒)。系统时钟早于 1970 时返回中文错误(不 panic)。
fn current_unix_secs() -> anyhow::Result<u64> {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("系统时钟异常, 无法生成可灵 JWT 时间戳: {}", e))?;
    Ok(dur.as_secs())
}

/// base64url 无填充编码(JWT 标准用 URL-safe 且去掉 '=' 填充)。
fn b64url(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

/// 本地现算可灵 HS256 JWT(纯函数, 便于离线单测: 固定 ak/sk/now 可复现签名)。
///
/// header = {"alg":"HS256","typ":"JWT"}; payload = {"iss":ak, "exp":now+1800, "nbf":now-5};
/// 签名 = HMAC-SHA256(base64url(header) + "." + base64url(payload), sk);
/// 最终 JWT = signing_input + "." + base64url(签名)。
pub fn build_kling_jwt(access_key: &str, secret_key: &str, now_secs: u64) -> String {
    // header: 字段顺序固定, 直接手写紧凑 JSON(避免序列化器给字段重排带来的签名不稳定)。
    let header = json!({ "alg": "HS256", "typ": "JWT" });
    let exp = now_secs + JWT_TTL_SECS;
    // nbf 用饱和减法防止极端早期时间下溢(now < 5 秒的理论情形)。
    let nbf = now_secs.saturating_sub(JWT_NBF_SKEW_SECS);
    let payload = json!({ "iss": access_key, "exp": exp, "nbf": nbf });

    // 紧凑序列化(serde_json 默认无多余空格), 再 base64url。
    let header_b64 = b64url(serde_json::to_string(&header).unwrap_or_default().as_bytes());
    let payload_b64 = b64url(serde_json::to_string(&payload).unwrap_or_default().as_bytes());
    let signing_input = format!("{}.{}", header_b64, payload_b64);

    // HMAC-SHA256(signing_input, sk)。new_from_slice 对任意长度 key 都成立, 不会失败。
    let mut mac =
        HmacSha256::new_from_slice(secret_key.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(signing_input.as_bytes());
    let sig = mac.finalize().into_bytes();
    let sig_b64 = b64url(&sig);

    format!("{}.{}", signing_input, sig_b64)
}

/// Kling provider 实现。无状态: 只持有 HTTP 客户端与能力声明(句柄随 Job.raw_meta 流转)。
pub struct KlingProvider {
    http: reqwest::Client,
    caps: Vec<Capability>,
}

impl KlingProvider {
    /// 构造默认 Kling provider。声明文生视频 + 图生视频两种能力。
    pub fn new() -> KlingProvider {
        KlingProvider {
            http: reqwest::Client::new(),
            caps: vec![Capability::Text2Video, Capability::Image2Video],
        }
    }

    /// 取 JWT 鉴权器(需 AK + SK 两个密钥)。任一缺失返回带中文指引的错误, 绝不 panic、绝不写死。
    fn jwt_auth(&self) -> anyhow::Result<JwtAuth> {
        let ak = keys::require_candidates_key(
            &keys::KLING_AK_ENV_CANDIDATES,
            PROVIDER_NAME,
            keys::KLING_AK_MISSING_HINT,
        )?;
        let sk = keys::require_candidates_key(
            &keys::KLING_SK_ENV_CANDIDATES,
            PROVIDER_NAME,
            keys::KLING_SK_MISSING_HINT,
        )?;
        Ok(JwtAuth::new(ak, sk))
    }

    /// 构造一个带 JWT 鉴权的 async-task 客户端。
    fn task_client(&self) -> anyhow::Result<AsyncTaskClient> {
        let auth = self.jwt_auth()?;
        Ok(AsyncTaskClient::new(self.http.clone(), Box::new(auth)))
    }
}

impl Default for KlingProvider {
    fn default() -> KlingProvider {
        KlingProvider::new()
    }
}

/// 读取生效的 base_url(允许用环境变量覆盖为新加坡站, 否则用默认大陆站)。
fn base_url() -> String {
    match std::env::var(BASE_URL_ENV) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_BASE_URL.to_string(),
    }
}

/// 由能力选定资源路径段(文生视频 / 图生视频)。
fn path_for(capability: Capability) -> &'static str {
    match capability {
        Capability::Image2Video => I2V_PATH,
        // 其余(主要是 Text2Video)走文生视频路径。
        _ => T2V_PATH,
    }
}

/// 提交端点 URL(按能力选 t2v/i2v 路径)。
fn submit_url(capability: Capability) -> String {
    format!("{}{}", base_url(), path_for(capability))
}

/// 用 task_id 拼查询端点 URL(查询路径与提交同段, 末尾接 /{task_id})。
fn query_url(capability: Capability, task_id: &str) -> String {
    format!("{}{}/{}", base_url(), path_for(capability), task_id)
}

/// 由 GenRequest 构造可灵提交请求体(纯函数, 便于离线单测)。
///
/// 形态: `{ "model_name": <model>, "prompt": ..., <透传自由参数>, 图生视频再加 "image": <url> }`。
///   - 注意字段名是 `model_name`(可灵特有), 不是通用的 `model`。
///   - prompt 存在则写入; 没有则不写(图生视频可纯图驱动)。
///   - 图生视频: 取第一个 URL 形态的图片输入写入 `image`(本地路径需先上传, 与其他 provider 一致)。
///   - params 里的自由参数(negative_prompt / mode / duration / aspect_ratio / cfg_scale 等)整体并入顶层透传,
///     用户 --param 优先级最高, 覆盖同名默认。
pub fn build_kling_body(req: &GenRequest) -> Value {
    let mut body = serde_json::Map::new();

    // 可灵字段名为 model_name(与 OpenAI/Ark 的 model 不同)。
    body.insert("model_name".to_string(), json!(req.model));

    // 文本提示词(存在才写)。
    if let Some(prompt) = &req.prompt {
        body.insert("prompt".to_string(), json!(prompt));
    }

    // 图生视频: 取第一个图片输入作为 image。可灵 image 字段接受图片 URL 或 raw base64,
    // 故远程 URL 直接透传、本地图(内联字节)用 raw base64(非 data URI, 可灵要纯 base64)。
    if matches!(req.capability, Capability::Image2Video) {
        for asset in req.inputs.iter() {
            if !matches!(asset.kind, AssetKind::Image) {
                continue;
            }
            match asset.as_input_image() {
                Some(InputImage::Url(u)) => {
                    body.insert("image".to_string(), json!(u));
                    break;
                }
                Some(InputImage::Bytes { base64, .. }) => {
                    body.insert("image".to_string(), json!(base64));
                    break;
                }
                None => {}
            }
        }
    }

    // 透传自由参数(negative_prompt/mode/duration/aspect_ratio 等), 用户 --param 覆盖同名默认。
    for (k, v) in req.params.iter() {
        body.insert(k.clone(), v.clone());
    }

    Value::Object(body)
}

/// 把可灵的状态字符串映射到归一化 JobStatus(纯函数包装, 便于离线单测)。
pub fn map_kling_status(raw: &str) -> JobStatus {
    STATUS_MAPPING.map(raw)
}

/// 从可灵查询结果里抽取视频产物(纯函数, 便于离线单测)。
pub fn extract_kling_outputs(result: &Value) -> Vec<Asset> {
    extract_urls_at(result, VIDEO_URL_POINTERS, AssetKind::Video)
}

/// 从可灵响应里解析 task_status 字符串(提交体与查询体都在 data.task_status)。
fn parse_task_status(resp: &Value) -> &str {
    resp.pointer("/data/task_status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// 抽取响应里可能的报错文本(失败时给排查上下文)。
/// 可灵信封顶层有 message; data 里失败时可能有 task_status_msg。兜底 stringify。
fn extract_error_text(resp: &Value) -> String {
    if let Some(msg) = resp
        .pointer("/data/task_status_msg")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return msg.to_string();
    }
    if let Some(msg) = resp
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return msg.to_string();
    }
    resp.to_string()
}

#[async_trait]
impl Provider for KlingProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // 暴露文生视频(alias "kling")与图生视频两条默认 model。
        vec![
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_T2V_MODEL,
                Some("kling"),
                Capability::Text2Video,
            ),
            crate::core::catalog::ModelEntry::single(
                PROVIDER_NAME,
                DEFAULT_I2V_MODEL,
                Some("kling-i2v"),
                Capability::Image2Video,
            ),
        ]
    }

    fn has_key(&self) -> bool {
        // 需 AK 与 SK 同时可取才算有 key。
        keys::resolve_candidates_key(&keys::KLING_AK_ENV_CANDIDATES, PROVIDER_NAME).is_some()
            && keys::resolve_candidates_key(&keys::KLING_SK_ENV_CANDIDATES, PROVIDER_NAME).is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        Ok(json!({
            "provider": PROVIDER_NAME,
            "model": model,
            "note": "占位 schema; 可灵真实参数随 model 而异, 以可灵开放平台控制台为准",
            "common_params": {
                "model_name": "string, 模型版本(如 kling-v1-6 / kling-v2-1-master)",
                "prompt": "string, 文本提示词",
                "negative_prompt": "string, 负向提示词",
                "mode": "string, std(标准) / pro(高表现)",
                "duration": "string, 视频时长秒数, 5 或 10",
                "aspect_ratio": "string, 画面比例, 如 16:9 / 9:16 / 1:1"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        // 鉴权(无 AK/SK 在此返回中文错误, 不 panic、不写死)。
        let client = self.task_client()?;

        // 记下能力, 用于选 t2v/i2v 端点并拼查询 URL。
        let capability = req.capability;
        let body = build_kling_body(&req);
        let resp = client.submit_task(&submit_url(capability), &body).await?;

        // 解析 task id(可灵在 data.task_id)。缺 id 无法后续轮询, 给清晰中文错误。
        let task_id = resp
            .pointer("/data/task_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("可灵提交未返回 task id(响应: {}), 无法轮询", resp)
            })?
            .to_string();

        // 句柄: task_id + 查询 URL(查询 URL 已含 t2v/i2v 路径), 随 Job.raw_meta 跨进程流转(D-007)。
        let handle = TaskHandle {
            task_id: task_id.clone(),
            query_url: query_url(capability, &task_id),
        };

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
        let handle = TaskHandle::from_raw_meta(&job.raw_meta)?;
        let client = self.task_client()?;

        let polled = client.query_task(&handle.query_url).await?;
        let raw_status = parse_task_status(&polled);
        let status = map_kling_status(raw_status);

        // 保留句柄不丢, 合并本次原始状态。
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

        match status {
            JobStatus::Succeeded => {
                next.outputs = extract_kling_outputs(&polled);
                if let Some(obj) = next.raw_meta.as_object_mut() {
                    obj.insert("result".to_string(), polled.clone());
                }
            }
            JobStatus::Failed => {
                next.error = Some(format!(
                    "可灵任务失败({}): {}",
                    raw_status,
                    extract_error_text(&polled)
                ));
            }
            JobStatus::Queued => {}
            JobStatus::Running => {}
        }

        Ok(next)
    }

    async fn cancel(&self, _job: &Job) -> anyhow::Result<()> {
        // 可灵开放平台未提供通用任务取消端点; 尽力而为, 直接返回 Ok。
        Ok(())
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
    fn capabilities_declare_video_not_image() {
        let p = KlingProvider::new();
        assert!(p.capabilities().contains(&Capability::Text2Video));
        assert!(p.capabilities().contains(&Capability::Image2Video));
        assert!(!p.capabilities().contains(&Capability::Text2Image));
    }

    #[test]
    fn jwt_has_three_segments_and_decodable_payload() {
        // 固定 ak/sk/now -> 可重现的 JWT。验证三段结构 + payload 解码出 iss/exp/nbf。
        let now = 1_700_000_000u64;
        let jwt = build_kling_jwt("my-ak", "my-sk", now);
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT 必须是 header.payload.sig 三段");

        // 解码 header
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[0])
            .expect("header 应能 base64url 解码");
        let header: Value = serde_json::from_slice(&header_bytes).expect("header 应是 JSON");
        assert_eq!(header["alg"], json!("HS256"));
        assert_eq!(header["typ"], json!("JWT"));

        // 解码 payload, 校验三个字段
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .expect("payload 应能 base64url 解码");
        let payload: Value = serde_json::from_slice(&payload_bytes).expect("payload 应是 JSON");
        assert_eq!(payload["iss"], json!("my-ak"));
        assert_eq!(payload["exp"], json!(now + JWT_TTL_SECS));
        assert_eq!(payload["nbf"], json!(now - JWT_NBF_SKEW_SECS));
    }

    #[test]
    fn jwt_is_deterministic_for_fixed_inputs() {
        // 同 ak/sk/now 必须得到完全相同的 JWT(签名可复现, 便于回归)。
        let now = 1_700_000_000u64;
        let a = build_kling_jwt("ak", "sk", now);
        let b = build_kling_jwt("ak", "sk", now);
        assert_eq!(a, b);
        // 已知向量(锁定签名算法不被无意改动): ak="ak" sk="sk" now=1700000000。
        assert_eq!(
            a,
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJleHAiOjE3MDAwMDE4MDAsImlzcyI6ImFrIiwibmJmIjoxNjk5OTk5OTk1fQ.h2Ib2cfDM6pOpdPWBmbO7GDw67oBqBvAPTDCgWpgtek"
        );
    }

    #[test]
    fn jwt_auth_returns_bearer_header() {
        let auth = JwtAuth::new("ak", "sk");
        let headers = auth.headers("POST", "https://x/y", b"{}").expect("JWT 不应失败");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Authorization");
        assert!(headers[0].1.starts_with("Bearer "));
        // Bearer 后是三段 JWT
        let token = headers[0].1.trim_start_matches("Bearer ");
        assert_eq!(token.split('.').count(), 3);
    }

    #[test]
    fn build_body_uses_model_name_not_model() {
        // 可灵字段是 model_name, 不是 model。
        let body = build_kling_body(&t2v_req("a cat surfing"));
        assert_eq!(body.get("model_name").unwrap(), &json!(DEFAULT_T2V_MODEL));
        assert!(body.get("model").is_none());
        assert_eq!(body.get("prompt").unwrap(), &json!("a cat surfing"));
    }

    #[test]
    fn build_body_i2v_appends_image_url() {
        let req = GenRequest {
            capability: Capability::Image2Video,
            model: DEFAULT_I2V_MODEL.to_string(),
            prompt: Some("make it move".to_string()),
            inputs: vec![Asset::from_url(AssetKind::Image, "https://x/in.png")],
            params: serde_json::Map::new(),
        };
        let body = build_kling_body(&req);
        assert_eq!(body.get("image").unwrap(), &json!("https://x/in.png"));
        assert_eq!(body.get("model_name").unwrap(), &json!(DEFAULT_I2V_MODEL));
    }

    #[test]
    fn build_body_i2v_local_bytes_to_raw_base64() {
        // 本地图(内联字节)输入 -> image 字段填 raw base64(非 data URI)。
        let req = GenRequest {
            capability: Capability::Image2Video,
            model: DEFAULT_I2V_MODEL.to_string(),
            prompt: Some("make it move".to_string()),
            inputs: vec![Asset::from_inline_bytes(
                AssetKind::Image,
                "image/png",
                vec![1u8, 2, 3, 4],
            )],
            params: serde_json::Map::new(),
        };
        let body = build_kling_body(&req);
        // base64(0x01020304)="AQIDBA==" 且不带 data: 前缀。
        assert_eq!(body.get("image").unwrap(), &json!("AQIDBA=="));
    }

    #[test]
    fn build_body_passes_through_params() {
        let mut req = t2v_req("dog");
        req.params.insert("mode".to_string(), json!("pro"));
        req.params.insert("duration".to_string(), json!("10"));
        req.params
            .insert("aspect_ratio".to_string(), json!("9:16"));
        let body = build_kling_body(&req);
        assert_eq!(body.get("mode").unwrap(), &json!("pro"));
        assert_eq!(body.get("duration").unwrap(), &json!("10"));
        assert_eq!(body.get("aspect_ratio").unwrap(), &json!("9:16"));
    }

    #[test]
    fn status_mapping_kling_specific() {
        // 可灵: submitted/processing/succeed(注意 succeed 不是 succeeded)。
        assert_eq!(map_kling_status("submitted"), JobStatus::Queued);
        assert_eq!(map_kling_status("processing"), JobStatus::Running);
        assert_eq!(map_kling_status("succeed"), JobStatus::Succeeded);
        // succeeded(多了 ed)不被识别为成功, 落入 Failed 兜底(防止误判)。
        assert_eq!(map_kling_status("succeeded"), JobStatus::Failed);
        assert_eq!(map_kling_status("failed"), JobStatus::Failed);
        assert_eq!(map_kling_status("whatever"), JobStatus::Failed);
        // 大小写不敏感
        assert_eq!(map_kling_status("PROCESSING"), JobStatus::Running);
    }

    #[test]
    fn extract_outputs_reads_task_result_videos() {
        let result = json!({
            "code": 0,
            "data": {
                "task_status": "succeed",
                "task_result": {
                    "videos": [
                        { "id": "v1", "url": "https://kl/out1.mp4" },
                        { "id": "v2", "url": "https://kl/out2.mp4" }
                    ]
                }
            }
        });
        let outputs = extract_kling_outputs(&result);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].kind, AssetKind::Video);
        assert_eq!(outputs[0].url.as_deref(), Some("https://kl/out1.mp4"));
        assert_eq!(outputs[1].url.as_deref(), Some("https://kl/out2.mp4"));
    }

    #[test]
    fn extract_outputs_empty_and_error_text() {
        assert!(extract_kling_outputs(&json!({ "data": { "task_status": "processing" } })).is_empty());
        let err = json!({
            "message": "envelope msg",
            "data": { "task_status": "failed", "task_status_msg": "prompt rejected" }
        });
        // data.task_status_msg 优先
        assert_eq!(extract_error_text(&err), "prompt rejected");
    }

    #[test]
    fn parse_task_status_reads_data_field() {
        let resp = json!({ "data": { "task_id": "t1", "task_status": "processing" } });
        assert_eq!(parse_task_status(&resp), "processing");
    }

    #[test]
    fn urls_are_well_formed_for_both_capabilities() {
        // 默认 base, 文生视频与图生视频路径正确。
        assert_eq!(
            submit_url(Capability::Text2Video),
            "https://api.klingai.com/v1/videos/text2video"
        );
        assert_eq!(
            submit_url(Capability::Image2Video),
            "https://api.klingai.com/v1/videos/image2video"
        );
        assert_eq!(
            query_url(Capability::Text2Video, "abc"),
            "https://api.klingai.com/v1/videos/text2video/abc"
        );
    }
}
