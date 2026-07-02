//! 同步 HTTP 传输(D-003 的 http-sync 维度): submit 一次 POST 直接拿终态结果, poll 为 no-op。
//!
//! 形态对标"一次请求拿结果"的同步接口(Google Gemini generateContent、OpenAI Images 等)。
//! 本模块只负责通用的"同步 POST + 自定义鉴权头 + 超时 + 错误体解析", 不绑定具体 provider
//! 的 endpoint 与请求/响应结构; 具体 provider(如 google.rs)在其上薄薄一层做 JSON 翻译。
//!
//! 与 http_queue 的区别: http_queue 是 submit->poll status->fetch result 三段式异步队列;
//! http_sync 一次 POST 即终态, 没有 status_url/response_url 句柄, 无需轮询。

use std::time::Duration;

/// 同步请求默认超时(秒)。图像生成同步接口可能耗时较久, 给一个偏宽的默认值。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 同步 HTTP 客户端。持有 reqwest::Client、自定义鉴权头(名+值)与超时。
///
/// 鉴权头做成"名 + 值"两段而非固定 Authorization: 因为各家同步接口鉴权头不同
/// (Gemini 用 `x-goog-api-key: {key}`, OpenAI 用 `Authorization: Bearer {key}`),
/// 通用层不该硬编码头名。
pub struct HttpSyncClient {
    client: reqwest::Client,
    /// 鉴权头名, 如 "x-goog-api-key"
    auth_header_name: String,
    /// 鉴权头值, 如 api key 原文(或 "Bearer {key}")
    auth_header_value: String,
    /// 单次请求超时
    timeout: Duration,
}

impl HttpSyncClient {
    /// 用给定的鉴权头(名+值)构造, 超时取默认值。
    pub fn new(
        client: reqwest::Client,
        auth_header_name: impl Into<String>,
        auth_header_value: impl Into<String>,
    ) -> HttpSyncClient {
        HttpSyncClient {
            client,
            auth_header_name: auth_header_name.into(),
            auth_header_value: auth_header_value.into(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// 覆盖默认超时(builder 风格, 便于按 provider 调整)。
    pub fn with_timeout(mut self, timeout: Duration) -> HttpSyncClient {
        self.timeout = timeout;
        self
    }

    /// 同步 POST: 把 body 以 JSON 发到 url, 直接返回解析后的响应 JSON。
    ///
    /// 非 2xx 时把响应体读出来并入中文错误(便于排查鉴权失败/参数非法/配额耗尽)。
    /// 这是同步 provider 的唯一网络动作: 一次往返即拿到终态结果。
    pub async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .client
            .post(url)
            .header(&self.auth_header_name, &self.auth_header_value)
            .timeout(self.timeout)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            // 把错误响应体并入错误信息(Gemini 会在 body 里给出 error.message)。
            // 发结构化 HttpError(而非纯字符串 bail): 上层 retry 分类可 downcast 拿状态码,
            // 精确区分 429/5xx(可重试)与 401/4xx(不可重试)。Display 文本与旧消息一致。
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::core::retry::HttpError::new(status.as_u16(), "同步请求失败", text).into());
        }
        let parsed = resp.json::<serde_json::Value>().await?;
        Ok(parsed)
    }
}
