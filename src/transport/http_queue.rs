//! 通用 HTTP 队列传输(D-003 的 http-queue 维度)。
//!
//! 形态对标 fal Queue API: submit -> 拿到 status_url/response_url -> 轮询 status_url ->
//! 终态后 GET response_url 取结果。本模块只负责"协议形态"的通用部分(HTTP 收发、
//! 鉴权头、状态体解析), 不绑定具体 provider 的 model 名与参数; fal.rs 在其上薄薄一层。

use serde::Deserialize;

/// 提交后 provider 返回的队列句柄。fal 的提交响应即此形态。
#[derive(Debug, Clone, Deserialize)]
pub struct QueueSubmitResponse {
    /// 队列内请求 id
    pub request_id: String,
    /// 轮询状态用的完整 URL
    pub status_url: String,
    /// 取最终结果用的完整 URL
    pub response_url: String,
    /// 取消用的 URL(部分 provider 提供)
    #[serde(default)]
    pub cancel_url: Option<String>,
}

/// 队列状态响应。fal 的 status 端点返回 `status` 字段为 IN_QUEUE/IN_PROGRESS/COMPLETED。
#[derive(Debug, Clone, Deserialize)]
pub struct QueueStatusResponse {
    /// 原始状态字符串
    pub status: String,
    /// 队列位置(可选, 仅排队时有)
    #[serde(default)]
    pub queue_position: Option<i64>,
}

/// 通用 HTTP 队列客户端。持有 reqwest::Client 与鉴权头值。
pub struct HttpQueueClient {
    client: reqwest::Client,
    /// 鉴权头的值, 例如 "Key xxxxx"(fal 用 Authorization: Key ...)
    auth_header_value: String,
}

impl HttpQueueClient {
    /// 用给定的鉴权头值构造。auth_header_value 由各 provider 决定(fal: "Key {api_key}")。
    pub fn new(client: reqwest::Client, auth_header_value: String) -> HttpQueueClient {
        HttpQueueClient {
            client,
            auth_header_value,
        }
    }

    /// 提交任务: POST 到 submit_url, body 为 provider 已构造好的 JSON。
    pub async fn submit(
        &self,
        submit_url: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<QueueSubmitResponse> {
        let resp = self
            .client
            .post(submit_url)
            .header("Authorization", &self.auth_header_value)
            .json(body)
            .send()
            .await?;
        // 非 2xx 时把响应体读出来塞进错误, 便于排查(鉴权失败/参数非法等)
        let status = resp.status();
        if !status.is_success() {
            // 结构化 HttpError: 供上层 retry 分类精确判定状态码(下同)。
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::core::retry::HttpError::new(status.as_u16(), "提交失败", text).into());
        }
        let parsed = resp.json::<QueueSubmitResponse>().await?;
        Ok(parsed)
    }

    /// 轮询状态: GET status_url。
    pub async fn poll_status(&self, status_url: &str) -> anyhow::Result<QueueStatusResponse> {
        let resp = self
            .client
            .get(status_url)
            .header("Authorization", &self.auth_header_value)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::core::retry::HttpError::new(status.as_u16(), "查询状态失败", text).into());
        }
        let parsed = resp.json::<QueueStatusResponse>().await?;
        Ok(parsed)
    }

    /// 取结果: GET response_url, 返回原始 JSON(由 provider 解析产物结构)。
    pub async fn fetch_result(&self, response_url: &str) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .client
            .get(response_url)
            .header("Authorization", &self.auth_header_value)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::core::retry::HttpError::new(status.as_u16(), "取结果失败", text).into());
        }
        let parsed = resp.json::<serde_json::Value>().await?;
        Ok(parsed)
    }

    /// 取消: PUT/POST cancel_url(fal 用 PUT)。
    pub async fn cancel(&self, cancel_url: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .put(cancel_url)
            .header("Authorization", &self.auth_header_value)
            .send()
            .await?;
        // 取消是尽力而为, 非 2xx 也只记录不强报错(任务可能已终结)
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("取消失败: HTTP {} - {}", status, text);
        }
        Ok(())
    }
}
