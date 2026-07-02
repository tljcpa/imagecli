//! 错误重试分类(D-006 工程十条之"退避重试与幂等重提")。
//!
//! 这里回答一个核心问题: 一个失败到底"该不该重试"。把它收敛成一个纯函数
//! `classify_error`, 让 route 编排层据此决定"同 provider 退避重试" 还是 "直接切 fallback /
//! 失败"。分类依据按可靠性从高到低三档:
//!   1. 结构化 HTTP 状态码(`HttpError`, transport 层在非 2xx 时发出, 最权威);
//!   2. reqwest 传输层错误(连接/超时, 网络抖动, 天然可重试);
//!   3. 错误文本启发式(各 provider 自己 bail 的中文/英文消息兜底)。
//!
//! 设计取舍: 未知错误默认 **不可重试**(NonRetryable)。理由: 生成类请求无幂等键,
//! 盲目重试既浪费配额又可能产生重复产物; 把握不准时宁可直接走 fallback 或如实失败,
//! 也不对同一 provider 反复打。真正"已知可恢复"(限流/5xx/超时/网络)才判可重试。

use thiserror::Error;

/// 结构化 HTTP 错误: transport 层在收到非 2xx 响应时发出, 携带状态码原文。
///
/// 为什么要它: 各 transport 之前一律 `anyhow::bail!("...HTTP {status}...")` 丢成纯字符串,
/// 上层只能靠正则猜状态码, 既脆又易错。改成带类型的错误后, `classify_error` 可直接
/// `downcast_ref::<HttpError>()` 拿到 u16 状态码做精确判定; Display 仍保留与旧消息一致的
/// 文本形态(`{context}: HTTP {status} - {body}`), 不破坏既有人类可读输出与日志。
#[derive(Debug, Error)]
#[error("{context}: HTTP {status} - {body}")]
pub struct HttpError {
    /// HTTP 状态码(如 429 / 503 / 401)。
    pub status: u16,
    /// 上下文短语(如 "提交失败" / "查询状态失败"), 保留旧消息语义。
    pub context: String,
    /// 响应体文本(服务端给出的错误详情, 便于排查; 可能为空)。
    pub body: String,
}

impl HttpError {
    /// 便捷构造: 由 reqwest 状态码与上下文/响应体生成。
    pub fn new(status: u16, context: impl Into<String>, body: impl Into<String>) -> HttpError {
        HttpError {
            status,
            context: context.into(),
            body: body.into(),
        }
    }
}

/// 重试分类结果。只有两态: 该重试 / 不该重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// 可重试: 限流(429)、服务端错误(5xx)、请求超时(408)、网络抖动(连接/超时)。
    Retryable,
    /// 不可重试: 鉴权失败(401/403)、参数错误(其余 4xx)、缺 key、逻辑错误、未知。
    NonRetryable,
}

impl RetryClass {
    /// 是否可重试(便于 if 判断, 避免到处写 == Retryable)。
    pub fn is_retryable(&self) -> bool {
        match self {
            RetryClass::Retryable => true,
            RetryClass::NonRetryable => false,
        }
    }
}

/// 按 HTTP 状态码判定是否可重试(纯函数, 便于单测)。
///
/// 429(Too Many Requests, 限流)、408(Request Timeout)、5xx(服务端错误)可重试;
/// 其余(尤其 401/403 鉴权、400/404/422 参数)不可重试——重试也只会同样失败。
pub fn classify_status(status: u16) -> RetryClass {
    if status == 429 || status == 408 {
        return RetryClass::Retryable;
    }
    if (500..=599).contains(&status) {
        return RetryClass::Retryable;
    }
    RetryClass::NonRetryable
}

/// 文本启发式判定(纯函数, 第三档兜底): 各 provider 自己 bail 出的消息没有结构化状态码时用。
///
/// 命中"限流/超时/网络/配额/可灵并发 1303"等已知可恢复信号 -> 可重试; 命中"鉴权/缺 key/
/// 参数非法"等明确不可恢复信号 -> 不可重试; 都不命中 -> 不可重试(保守默认, 见模块注释)。
pub fn classify_text(msg: &str) -> RetryClass {
    let lower = msg.to_ascii_lowercase();

    // 明确不可重试信号优先(避免一条消息同时含 "rate" 与 "unauthorized" 时误判)。
    // 鉴权/缺 key/参数错误: 重试无意义。
    let non_retryable_markers = [
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "api key",
        "鉴权",
        "未授权",
        "无权限",
        "缺 key",
        "缺少 key",
        "未配置",
        "invalid request",
        "invalid parameter",
        "参数",
    ];
    for m in non_retryable_markers.iter() {
        if lower.contains(m) {
            return RetryClass::NonRetryable;
        }
    }

    // 可重试信号: 限流 / 超时 / 网络 / 配额并发。
    let retryable_markers = [
        "429",
        "too many requests",
        "rate limit",
        "ratelimit",
        "限流",
        "限频",
        "timeout",
        "timed out",
        "超时",
        "connection",
        "connect error",
        "连接",
        "网络",
        "temporarily",
        "service unavailable",
        "503",
        "502",
        "500",
        "504",
        "1303", // 可灵: 并发任务数超限, 稍后可重试
        "concurrency",
        "并发",
    ];
    for m in retryable_markers.iter() {
        if lower.contains(m) {
            return RetryClass::Retryable;
        }
    }

    // 未知: 保守不重试(交给 fallback 或如实失败)。
    RetryClass::NonRetryable
}

/// 对一个 anyhow 错误做重试分类(主入口)。
///
/// 遍历整条错误链(`err.chain()`), 因为 provider 常用 `.context(...)` 包裹底层错误,
/// 只看最外层会漏判。逐层尝试:
///   1. downcast 成 `HttpError` -> 按状态码判(最权威);
///   2. downcast 成 `reqwest::Error` -> 连接/超时/请求构造错误天然可重试;
///
/// 链上都拿不到结构化错误时, 退到对整链文本做启发式(`classify_text`)。
pub fn classify_error(err: &anyhow::Error) -> RetryClass {
    for cause in err.chain() {
        // 第一档: 结构化 HTTP 状态码。
        if let Some(http) = cause.downcast_ref::<HttpError>() {
            return classify_status(http.status);
        }
        // 第二档: reqwest 传输错误。超时与连接错误是网络抖动, 可重试。
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if re.is_timeout() || re.is_connect() || re.is_request() {
                return RetryClass::Retryable;
            }
            // reqwest 携带状态码(如 error_for_status 派生)时也据此判。
            if let Some(status) = re.status() {
                return classify_status(status.as_u16());
            }
        }
    }
    // 第三档: 整链文本启发式。用 {:#} 展开 context 链, 信息最全。
    classify_text(&format!("{:#}", err))
}

/// 判断错误是否疑似"配额/余额/并发耗尽"(纯函数), 用于给用户更有针对性的中文建议。
///
/// 与 classify_error 正交: 配额耗尽既可能是 429(可重试, 稍后重试)也可能是 402/余额不足
/// (不可重试, 需充值或切 fallback)。这里只负责"是不是配额类", 由调用方据此追加建议文案。
pub fn looks_like_quota_exhausted(err: &anyhow::Error) -> bool {
    // 结构化: 402 Payment Required / 429 Too Many Requests 常对应配额或限流。
    for cause in err.chain() {
        if let Some(http) = cause.downcast_ref::<HttpError>() {
            if http.status == 402 || http.status == 429 {
                return true;
            }
        }
    }
    let lower = format!("{:#}", err).to_ascii_lowercase();
    let markers = [
        "quota",
        "配额",
        "insufficient",
        "余额",
        "balance",
        "out of credit",
        "credits",
        "rate limit",
        "限流",
        "1303",
        "并发",
        "concurrency",
        "402",
        "429",
    ];
    markers.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_429_and_5xx_are_retryable() {
        assert_eq!(classify_status(429), RetryClass::Retryable);
        assert_eq!(classify_status(408), RetryClass::Retryable);
        assert_eq!(classify_status(500), RetryClass::Retryable);
        assert_eq!(classify_status(503), RetryClass::Retryable);
        assert_eq!(classify_status(599), RetryClass::Retryable);
    }

    #[test]
    fn status_4xx_auth_and_param_not_retryable() {
        assert_eq!(classify_status(400), RetryClass::NonRetryable);
        assert_eq!(classify_status(401), RetryClass::NonRetryable);
        assert_eq!(classify_status(403), RetryClass::NonRetryable);
        assert_eq!(classify_status(404), RetryClass::NonRetryable);
        assert_eq!(classify_status(422), RetryClass::NonRetryable);
    }

    #[test]
    fn http_error_downcast_drives_classification() {
        // 结构化 HttpError 经 anyhow 包裹后仍可被 classify_error 还原状态码。
        let e: anyhow::Error = HttpError::new(503, "查询状态失败", "upstream down").into();
        assert_eq!(classify_error(&e), RetryClass::Retryable);
        let e2: anyhow::Error = HttpError::new(401, "提交失败", "bad key").into();
        assert_eq!(classify_error(&e2), RetryClass::NonRetryable);
    }

    #[test]
    fn http_error_survives_context_wrapping() {
        // provider 常用 .context() 包裹底层错误; 遍历错误链仍能 downcast 到 HttpError。
        use anyhow::Context;
        let base: anyhow::Result<()> = Err(HttpError::new(429, "提交失败", "slow down").into());
        let wrapped = base.context("agnes 提交任务时出错").unwrap_err();
        assert_eq!(classify_error(&wrapped), RetryClass::Retryable);
    }

    #[test]
    fn text_heuristic_retryable_and_not() {
        assert_eq!(classify_text("提交失败: HTTP 429 - too many requests"), RetryClass::Retryable);
        assert_eq!(classify_text("连接超时 timed out"), RetryClass::Retryable);
        assert_eq!(classify_text("可灵返回 1303 并发任务数超限"), RetryClass::Retryable);
        assert_eq!(classify_text("HTTP 401 unauthorized"), RetryClass::NonRetryable);
        assert_eq!(classify_text("缺 key, 请先配置"), RetryClass::NonRetryable);
        // 未知错误 -> 保守不重试
        assert_eq!(classify_text("某种说不清的错误"), RetryClass::NonRetryable);
    }

    #[test]
    fn quota_detection() {
        let e: anyhow::Error = HttpError::new(429, "提交失败", "rate limited").into();
        assert!(looks_like_quota_exhausted(&e));
        let e2 = anyhow::anyhow!("insufficient balance, 余额不足");
        assert!(looks_like_quota_exhausted(&e2));
        let e3 = anyhow::anyhow!("普通参数错误");
        assert!(!looks_like_quota_exhausted(&e3));
    }
}
