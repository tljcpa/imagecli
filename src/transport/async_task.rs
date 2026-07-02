//! 通用 async-task 异步任务骨架(D-014: 视频/异步任务的可复用地基)。
//!
//! 形态对标"提交 -> 拿 task_id -> 轮询查询 -> 取产物 URL"的异步任务三段式,
//! 覆盖所有视频 provider(Ark Seedance / 可灵 / 即梦 visual)与部分异步图像。
//! 与 http_queue(fal 风格: 提交即返回 status_url/response_url)的区别:
//! 这里只拿到一个 task_id, 查询 URL 由 provider 用 task_id 自行拼装, 更贴近
//! Ark/可灵/即梦这类"任务资源"风格的 REST API。
//!
//! ============== 三个扩展点(后续即梦/可灵复用本骨架只需注入这三样)==============
//!
//! 1. 鉴权注入(`TaskAuth` trait): 给一个待发请求(方法/URL/请求体)算出要附加的
//!    HTTP 头。本棒先落地 Bearer(`BearerAuth`); 后续:
//!      - 可灵: 本地 HS256 JWT(iss=AK / exp=+1800 / nbf=-5, SK 签名)-> 实现一个
//!        `JwtAuth`, headers() 里现算 JWT 串塞进 Authorization: Bearer。
//!      - 即梦 visual: 火山 AK/SK V4 签名(HMAC-SHA256, 依赖 method/path/body/时间戳)->
//!        实现一个 `VolcV4Auth`, headers() 里按规范算出 Authorization + X-Date 等签名头。
//!
//!    正因签名可能依赖请求体与方法, headers() 的入参才带上 method/url/body, 而非
//!    只返回一个静态头(Bearer 忽略这三个入参即可)。
//!
//! 2. 状态字段映射(`StatusMapping`): 各家 status 字符串不同(Ark: queued/running/
//!    succeeded/failed; 可灵: submitted/processing/succeed/failed), 用一份"哪些算排队/
//!    执行中/成功"的配置把原始字符串收敛到归一化 JobStatus, 其余(含显式失败与未知)
//!    一律 Failed(穷尽兜底, 不把脏状态当运行中)。
//!
//! 3. 产物字段路径(`extract_urls_at` 的 JSON Pointer 列表): 产物 URL 在响应里的位置
//!    各家不同(Ark: /content/video_url; 可灵: /data/task_result/videos/0/url)。
//!    provider 传入候选 pointer 列表即可, 不必重写解析。
//!
//! 句柄(task_id + 查询 URL)随 Job.raw_meta 落进 store 跨进程流转(`TaskHandle`),
//! provider 自身无状态(D-007); 轮询退避复用 runner 已有逻辑。

use serde_json::{json, Value};

use crate::core::provider::{Asset, AssetKind, JobStatus};

/// 鉴权注入点(扩展点 1)。
///
/// 给一个待发请求算出需附加的 HTTP 头 (name, value) 列表。
/// - method: "POST" / "GET" / "DELETE" 等(大写)。
/// - url:    完整请求 URL。
/// - body:   请求体字节(GET/DELETE 通常为空切片)。
///
/// 之所以把 method/url/body 都传进来: 像火山 AK/SK V4、可灵 JWT 这类签名鉴权,
/// 签名内容依赖请求本身(方法、路径、甚至请求体哈希、时间戳)。Bearer 不依赖这些,
/// 实现时忽略入参直接返回固定头即可。
pub trait TaskAuth: Send + Sync {
    /// 算出本次请求需附加的 HTTP 头。失败(如签名出错)返回中文错误。
    fn headers(&self, method: &str, url: &str, body: &[u8]) -> anyhow::Result<Vec<(String, String)>>;
}

/// Bearer 鉴权: 固定 `Authorization: Bearer <token>`。本棒(Seedance)用它。
///
/// 后续可灵的 JwtAuth、即梦的 VolcV4Auth 是同一 trait 的另两个实现, AsyncTaskClient 不变。
pub struct BearerAuth {
    token: String,
}

impl BearerAuth {
    /// 用已取得的 API key/token 构造。token 只从调用方(provider 读 env)注入, 本模块不碰 key 来源。
    pub fn new(token: impl Into<String>) -> BearerAuth {
        BearerAuth {
            token: token.into(),
        }
    }
}

impl TaskAuth for BearerAuth {
    fn headers(&self, _method: &str, _url: &str, _body: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
        // Bearer 与请求内容无关, 忽略 method/url/body, 返回固定授权头。
        Ok(vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        )])
    }
}

/// 状态字段映射配置(扩展点 2)。
///
/// 列出哪些原始状态字符串算"排队中 / 执行中 / 成功"; 其余(含显式 failed/error 与任何
/// 未知字符串)一律映射为 Failed。匹配大小写不敏感。
pub struct StatusMapping {
    /// 映射为 Queued 的原始状态(小写写法即可, 匹配时大小写不敏感)。
    pub queued: &'static [&'static str],
    /// 映射为 Running 的原始状态。
    pub running: &'static [&'static str],
    /// 映射为 Succeeded 的原始状态。
    pub succeeded: &'static [&'static str],
}

impl StatusMapping {
    /// 把原始状态字符串收敛到归一化 JobStatus(纯函数, 便于离线单测)。
    ///
    /// 判定顺序: succeeded -> queued -> running -> 兜底 Failed。
    /// 兜底 Failed 覆盖显式失败(failed/error/cancelled)与一切未知串, 保证不漏判终态。
    pub fn map(&self, raw: &str) -> JobStatus {
        let lowered = raw.to_ascii_lowercase();
        if self.succeeded.iter().any(|s| *s == lowered) {
            return JobStatus::Succeeded;
        }
        if self.queued.iter().any(|s| *s == lowered) {
            return JobStatus::Queued;
        }
        if self.running.iter().any(|s| *s == lowered) {
            return JobStatus::Running;
        }
        // 显式 failed/cancelled 以及任何未知状态都视为失败终态。
        JobStatus::Failed
    }
}

/// 任务句柄: 一次提交后需记住的最小信息, 随 Job.raw_meta 跨进程流转(D-007)。
///
/// 与 fal 的 FalHandle / replicate 的 ReplicateHandle 同模式, 但更通用:
/// 只存 task_id 与查询 URL(查询 URL 由 provider 用 task_id 拼好后存入, poll 直接复用,
/// 避免 poll 再依赖 provider 的 base_url 常量)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskHandle {
    /// 异步任务 id(provider 返回的 task id)。
    pub task_id: String,
    /// 轮询查询用的完整 URL。
    pub query_url: String,
}

impl TaskHandle {
    /// 序列化进 Job.raw_meta。submit 时写, poll/cancel 时读。
    pub fn to_raw_meta(&self) -> Value {
        json!({
            "task_id": self.task_id,
            "query_url": self.query_url,
        })
    }

    /// 从 Job.raw_meta 还原句柄。缺 query_url 时给清晰中文错误(句柄已丢失)。
    pub fn from_raw_meta(raw_meta: &Value) -> anyhow::Result<TaskHandle> {
        let task_id = raw_meta
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let query_url = raw_meta
            .get("query_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Job.raw_meta 缺少 query_url, 无法轮询异步任务(句柄已丢失)")
            })?
            .to_string();
        Ok(TaskHandle {
            task_id,
            query_url,
        })
    }
}

/// 通用 async-task HTTP 客户端: 持有 reqwest::Client 与可注入的鉴权器。
///
/// 只负责"协议形态"的通用部分(提交 POST / 查询 GET / 取消 DELETE、附加鉴权头、
/// 非 2xx 中文报错), 不绑定具体 provider 的端点与字段; 各 provider 在其上薄薄一层。
pub struct AsyncTaskClient {
    http: reqwest::Client,
    /// 鉴权注入点(Box<dyn> 以便不同 provider 注入 Bearer/JWT/V4 等不同实现)。
    auth: Box<dyn TaskAuth>,
}

impl AsyncTaskClient {
    /// 用给定鉴权器构造。auth 由各 provider 决定(本棒注入 BearerAuth)。
    pub fn new(http: reqwest::Client, auth: Box<dyn TaskAuth>) -> AsyncTaskClient {
        AsyncTaskClient { http, auth }
    }

    /// 提交任务: POST 到 submit_url, body 为 provider 已构造好的 JSON, 返回原始响应 JSON
    /// (task_id 等由 provider 自行从中解析)。
    pub async fn submit_task(&self, submit_url: &str, body: &Value) -> anyhow::Result<Value> {
        // 先把 body 序列化成字节: 既用于发送, 也可能被签名鉴权器用来算请求体哈希。
        let bytes = serde_json::to_vec(body)?;
        let headers = self.auth.headers("POST", submit_url, &bytes)?;
        let mut rb = self
            .http
            .post(submit_url)
            .header("Content-Type", "application/json")
            .body(bytes);
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        if !status.is_success() {
            // 结构化 HttpError: 供上层 retry 分类精确判定状态码(下同)。
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::core::retry::HttpError::new(status.as_u16(), "提交异步任务失败", text).into());
        }
        let parsed = resp.json::<Value>().await?;
        Ok(parsed)
    }

    /// 查询任务: GET query_url, 返回原始响应 JSON(状态/产物由 provider 解析)。
    pub async fn query_task(&self, query_url: &str) -> anyhow::Result<Value> {
        let headers = self.auth.headers("GET", query_url, &[])?;
        let mut rb = self.http.get(query_url);
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::core::retry::HttpError::new(status.as_u16(), "查询异步任务失败", text).into());
        }
        let parsed = resp.json::<Value>().await?;
        Ok(parsed)
    }

    /// 取消任务: DELETE delete_url(Ark 等用 DELETE 取消任务资源)。尽力而为。
    pub async fn delete_task(&self, delete_url: &str) -> anyhow::Result<()> {
        let headers = self.auth.headers("DELETE", delete_url, &[])?;
        let mut rb = self.http.delete(delete_url);
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("取消异步任务失败: HTTP {} - {}", status, text);
        }
        Ok(())
    }
}

/// 从结果 JSON 按一组候选 JSON Pointer 路径抽取产物 URL, 收集成 Asset(扩展点 3, 纯函数)。
///
/// 每个 pointer 指向的值支持三种形态:
/// - 字符串: 直接当 URL。
/// - 字符串数组: 逐个当 URL。
/// - 对象 / 对象数组: 取其中的 "url" 字段(部分 API 把产物包成 {"url": ...})。
///
/// 命中第一个非空 pointer 后即停(避免同一产物被多个候选路径重复收集);
/// 全部 pointer 都取不到则返回空向量, 由调用方据响应兜底报错。
pub fn extract_urls_at(result: &Value, pointers: &[&str], kind: AssetKind) -> Vec<Asset> {
    for ptr in pointers.iter() {
        if let Some(found) = result.pointer(ptr) {
            let mut out = Vec::new();
            collect_urls(found, kind, &mut out);
            if !out.is_empty() {
                return out;
            }
        }
    }
    Vec::new()
}

/// 递归从一个值里收集 URL(字符串 / 数组 / 对象三种形态)。
fn collect_urls(value: &Value, kind: AssetKind, out: &mut Vec<Asset>) {
    match value {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(Asset::from_url(kind, s.clone()));
            }
        }
        Value::Array(arr) => {
            for item in arr.iter() {
                collect_urls(item, kind, out);
            }
        }
        Value::Object(obj) => {
            if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    out.push(Asset::from_url(kind, url.to_string()));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_auth_returns_fixed_authorization_header() {
        let auth = BearerAuth::new("sk-xyz");
        let headers = auth.headers("POST", "https://x/tasks", b"{}").expect("Bearer 不应失败");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer sk-xyz");
    }

    #[test]
    fn status_mapping_normalizes_ark_style_states() {
        // 仿 Ark Seedance: queued/running/succeeded/failed/cancelled。
        let m = StatusMapping {
            queued: &["queued"],
            running: &["running"],
            succeeded: &["succeeded"],
        };
        assert_eq!(m.map("queued"), JobStatus::Queued);
        assert_eq!(m.map("running"), JobStatus::Running);
        assert_eq!(m.map("succeeded"), JobStatus::Succeeded);
        // 显式失败 + 取消 + 未知 -> 全部 Failed(穷尽兜底)
        assert_eq!(m.map("failed"), JobStatus::Failed);
        assert_eq!(m.map("cancelled"), JobStatus::Failed);
        assert_eq!(m.map("whatever"), JobStatus::Failed);
        assert_eq!(m.map(""), JobStatus::Failed);
    }

    #[test]
    fn status_mapping_is_case_insensitive() {
        let m = StatusMapping {
            queued: &["submitted"],
            running: &["processing"],
            succeeded: &["succeed"],
        };
        assert_eq!(m.map("PROCESSING"), JobStatus::Running);
        assert_eq!(m.map("Succeed"), JobStatus::Succeeded);
        // 非终态判定(轮询应继续)
        assert!(!m.map("submitted").is_terminal());
        assert!(!m.map("processing").is_terminal());
    }

    #[test]
    fn handle_roundtrips_through_raw_meta() {
        let h = TaskHandle {
            task_id: "cgt-123".to_string(),
            query_url: "https://ark/api/v3/contents/generations/tasks/cgt-123".to_string(),
        };
        let raw = h.to_raw_meta();
        let restored = TaskHandle::from_raw_meta(&raw).expect("应能还原句柄");
        assert_eq!(restored, h);
    }

    #[test]
    fn handle_from_raw_meta_errors_when_query_url_missing() {
        let bad = json!({ "task_id": "cgt-123" });
        let err = TaskHandle::from_raw_meta(&bad).unwrap_err();
        assert!(err.to_string().contains("query_url"));
    }

    #[test]
    fn extract_urls_reads_nested_pointer_string() {
        // Ark Seedance 风格: 产物视频在 /content/video_url。
        let result = json!({
            "id": "cgt-1",
            "status": "succeeded",
            "content": { "video_url": "https://tos-cn/seedance/out.mp4" }
        });
        let outputs = extract_urls_at(&result, &["/content/video_url", "/video_url"], AssetKind::Video);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, AssetKind::Video);
        assert_eq!(outputs[0].url.as_deref(), Some("https://tos-cn/seedance/out.mp4"));
    }

    #[test]
    fn extract_urls_falls_back_to_second_pointer() {
        // 第一个 pointer 取不到时, 退到第二个候选路径。
        let result = json!({ "video_url": "https://x/v.mp4" });
        let outputs = extract_urls_at(&result, &["/content/video_url", "/video_url"], AssetKind::Video);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].url.as_deref(), Some("https://x/v.mp4"));
    }

    #[test]
    fn extract_urls_handles_array_and_object_forms() {
        // 数组形态(可灵风格 videos 列表)
        let arr = json!({ "videos": ["https://x/a.mp4", { "url": "https://x/b.mp4" }] });
        let outputs = extract_urls_at(&arr, &["/videos"], AssetKind::Video);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].url.as_deref(), Some("https://x/a.mp4"));
        assert_eq!(outputs[1].url.as_deref(), Some("https://x/b.mp4"));
        // 全部 pointer 取不到 -> 空
        let empty = extract_urls_at(&json!({}), &["/content/video_url"], AssetKind::Video);
        assert!(empty.is_empty());
    }
}
