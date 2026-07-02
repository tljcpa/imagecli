//! Google Gemini provider(D-008 首个真实端到端 provider, 走 D-003 的 http-sync 传输)。
//!
//! 协议形态(Gemini generateContent REST):
//! 1. 提交: POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent
//!    带 `x-goog-api-key: {API_KEY}`, body 为 contents/parts + generationConfig.responseModalities。
//! 2. 响应: 一次返回终态结果, 图片以 base64 内嵌在
//!    candidates[0].content.parts[].inlineData{ mimeType, data } 里(无可再下载的 URL)。
//!
//! 与 fal(http-queue)的根本差异: Gemini 是同步的, 没有 status_url/response_url 句柄,
//! 一次 POST 即终态。故 submit 直接拿结果、解码 base64、落盘, 返回 Succeeded 的 Job;
//! poll 为 no-op(传入已终态的 Job 原样返回), cancel 无意义(同步无在途任务)。
//!
//! 产物落盘与持久化取舍(D-008 的架构债处理):
//! Gemini 产物是响应内的大块 base64 字节。若把它当 InlineBytes 直接塞进 Job.outputs 返回,
//! 编排层 runner 会在 submit 后立刻 store.save, 把这堆 base64 灌进 SQLite(result_json)。
//! 为避免大 base64 进库, 这里在 submit 内就把字节解码落盘到稳定的数据目录(跨进程可见),
//! 再把 outputs 收敛成 LocalPath 素材返回 —— 入库的只是本地路径字符串(小), 且 status/download
//! 在别的进程也能据此路径找到产物。raw_meta 也只存元信息(model/mime/文本), 不含 base64。

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

use crate::config::keys;
use crate::core::download::ext_from_mime;
use crate::core::provider::{Asset, AssetKind, Capability, GenRequest, Job, JobStatus, Provider};
use crate::transport::http_sync::HttpSyncClient;

/// provider 名(注册表 key / Job.provider)。选 "google" 而非 "gemini":
/// 与 D-008 的密钥命名空间(IMAGECLI_GOOGLE_KEY / GOOGLE_API_KEY)一致, 且未来 Imagen 等
/// 同属 Google 的模型可复用同一 provider 命名空间。
const PROVIDER_NAME: &str = "google";
/// Gemini REST 根地址(generateContent 在 /{model}:generateContent)。
const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
/// 鉴权头名(官方约定)。
const AUTH_HEADER: &str = "x-goog-api-key";
/// text2image 默认 model。
pub const DEFAULT_T2I_MODEL: &str = "gemini-2.5-flash-image";

/// Google provider 实现。无状态: 只持有 HTTP 客户端与能力声明。
pub struct GoogleProvider {
    http: reqwest::Client,
    caps: Vec<Capability>,
}

impl GoogleProvider {
    /// 构造一个默认 google provider。
    pub fn new() -> GoogleProvider {
        GoogleProvider {
            http: reqwest::Client::new(),
            // Gemini 2.5 Flash Image(Nano Banana)同一 model 既能文生图也能图像编辑(图生图):
            // 输入图作为 inline_data part 塞进请求, 复用同一 generateContent 端点与 model。
            // 输入来源约束(诚实标注): Gemini inline 接口只吃内联字节(base64), 不接受远程 URL。
            // 本 CLI 的 load_input_asset 会把本地文件读成内联字节、URL 维持 from_url;
            // 故 i2i 实际生效的是本地图输入(URL 输入不会被塞进 inline_data, 见 build_gemini_request_body)。
            caps: vec![Capability::Text2Image, Capability::Image2Image],
        }
    }
}

impl Default for GoogleProvider {
    fn default() -> GoogleProvider {
        GoogleProvider::new()
    }
}

/// 由 GenRequest 构造 Gemini generateContent 请求体(纯函数, 便于离线单测)。
///
/// 结构:
/// {
///   "contents": [ { "parts": [ {"text": prompt}, {"inlineData": {...}}? ] } ],
///   "generationConfig": { "responseModalities": ["TEXT","IMAGE"] }
/// }
/// - prompt 写入一个 text part(text2image 必有)。
/// - 输入素材中"带内联字节"的图片写入 inline_data part(为图生图/多图融合预留;
///   当前 CLI 只喂 URL, 一般为空)。URL 形态的输入 Gemini inline 接口用不上, 跳过。
///
/// 用 camelCase(generationConfig/responseModalities/inlineData/mimeType): REST proto-JSON
/// 两种写法都收, 这里用官方文档示例的规范写法。
pub fn build_gemini_request_body(req: &GenRequest) -> Value {
    // parts 数组: 先文本, 再可选内联图片
    let mut parts: Vec<Value> = Vec::new();

    if let Some(prompt) = &req.prompt {
        parts.push(json!({ "text": prompt }));
    }

    // 仅把"已携带内联字节"的图片输入转成 inline_data part(base64 编码原始字节)。
    for asset in req.inputs.iter() {
        let is_image = matches!(asset.kind, AssetKind::Image);
        if !is_image {
            continue;
        }
        if let Some(inline) = &asset.inline {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&inline.data);
            parts.push(json!({
                "inlineData": {
                    "mimeType": inline.mime,
                    "data": b64,
                }
            }));
        }
    }

    json!({
        "contents": [ { "parts": parts } ],
        "generationConfig": { "responseModalities": ["TEXT", "IMAGE"] }
    })
}

/// 从 Gemini 响应里抽取图片产物: 返回 (mime, 已解码字节) 列表(纯函数, 便于离线单测)。
///
/// 路径: candidates[].content.parts[].inlineData{ mimeType, data(base64) }。
/// 同时容忍 snake_case(inline_data / mime_type), 因 proto-JSON 两种写法都合法。
/// 解析不到任何图片返回空向量(由调用方决定是否报错并附带文本说明)。
pub fn parse_gemini_image_parts(resp: &Value) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();

    let candidates = match resp.get("candidates").and_then(|v| v.as_array()) {
        Some(c) => c,
        None => return Ok(out),
    };

    for cand in candidates {
        let parts = cand
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array());
        let parts = match parts {
            Some(p) => p,
            None => continue,
        };
        for part in parts {
            // 容忍 camelCase 与 snake_case 两种写法
            let inline = part.get("inlineData").or_else(|| part.get("inline_data"));
            let inline = match inline {
                Some(v) => v,
                None => continue,
            };
            let mime = inline
                .get("mimeType")
                .or_else(|| inline.get("mime_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let data_b64 = match inline.get("data").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            // base64 解码; 解码失败给清晰中文错误(响应损坏)
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|e| anyhow::anyhow!("Gemini 产物 base64 解码失败: {}", e))?;
            out.push((mime, bytes));
        }
    }

    Ok(out)
}

/// 抽取响应里的纯文本片段(若有), 用于无图片时的报错上下文与 raw_meta 记录。
fn extract_response_text(resp: &Value) -> Option<String> {
    let candidates = resp.get("candidates")?.as_array()?;
    let mut texts: Vec<String> = Vec::new();
    for cand in candidates {
        if let Some(parts) = cand
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !t.trim().is_empty() {
                        texts.push(t.to_string());
                    }
                }
            }
        }
    }
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// 生成一个本进程唯一的 job id(Gemini 无队列请求 id, 自造一个稳定标识)。
fn generate_job_id() -> String {
    use rand::Rng;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 加一个随机后缀, 防止同一纳秒内并发提交撞 id
    let suffix: u32 = rand::thread_rng().gen();
    format!("google-{}-{:08x}", nanos, suffix)
}

/// 产物落盘的稳定目录: XDG data dir 下 imagecli/artifacts/{job_id}。
///
/// 为什么落到数据目录而非 CWD: outputs 收敛成 LocalPath 后会被 store 持久化,
/// 别的进程(status/download)要据此路径找到产物, 必须是稳定、不随 CWD 漂移的位置。
/// CLI 的 --out-dir 仍生效: download 阶段会把这里的本地产物复制进 --out-dir。
fn artifacts_dir(job_id: &str) -> anyhow::Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "imagecli")
        .ok_or_else(|| anyhow::anyhow!("无法确定用户数据目录(XDG data dir), 无法落盘 Gemini 产物"))?;
    Ok(dirs.data_dir().join("artifacts").join(job_id))
}

#[async_trait]
impl Provider for GoogleProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // Gemini 2.5 Flash Image 同一 model 既支持文生图也支持图像编辑(图生图):
        // 一条 entry 同时声明 Text2Image 与 Image2Image(与即梦 4.0 同 model 双能力同构)。
        let mut entry = crate::core::catalog::ModelEntry::single(
            PROVIDER_NAME,
            DEFAULT_T2I_MODEL,
            Some("gemini-image"),
            Capability::Text2Image,
        );
        entry.capabilities.push(Capability::Image2Image);
        vec![entry]
    }

    fn has_key(&self) -> bool {
        // google 走官方候选变量(GEMINI_API_KEY / GOOGLE_API_KEY / IMAGECLI_GOOGLE_KEY / keyring)
        keys::resolve_google_key().is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        // 占位 schema: Gemini 参数随 model 变, MVP 先给静态描述, 不打网络。
        Ok(json!({
            "provider": PROVIDER_NAME,
            "model": model,
            "note": "占位 schema; Gemini generateContent 参数见官方文档",
            "common_params": {
                "prompt": "string, 文本提示词(text part)",
                "responseModalities": "固定为 [TEXT, IMAGE] 以让模型返回图片"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        // 1. 取 key(无 key 在此返回中文指引, 绝不 panic)
        let key = keys::require_google_key()?;

        // 2. 构造 URL 与请求体
        let url = format!("{}/{}:generateContent", GEMINI_BASE, req.model);
        let body = build_gemini_request_body(&req);

        // 3. 同步 POST 一次拿终态结果
        let client = HttpSyncClient::new(self.http.clone(), AUTH_HEADER, key);
        let resp = client.post_json(&url, &body).await?;

        // 4. 解析 base64 图片产物
        let image_parts = parse_gemini_image_parts(&resp)?;
        if image_parts.is_empty() {
            // 没有图片: 可能被安全策略拦截或仅返回文本, 给出文本上下文便于排查
            let text_ctx = extract_response_text(&resp).unwrap_or_else(|| "(无文本)".to_string());
            anyhow::bail!("Gemini 未返回图片产物。模型文本响应: {}", text_ctx);
        }

        // 5. 落盘: 解码字节写进稳定数据目录, outputs 收敛为 LocalPath(避免大 base64 进库)
        let job_id = generate_job_id();
        let out_dir = artifacts_dir(&job_id)?;
        tokio::fs::create_dir_all(&out_dir).await?;

        let mut outputs: Vec<Asset> = Vec::new();
        let mut mime_types: Vec<String> = Vec::new();
        for (index, (mime, bytes)) in image_parts.iter().enumerate() {
            let ext = ext_from_mime(mime, AssetKind::Image);
            let filename = format!("{}_{}.{}", job_id, index, ext);
            let dest = out_dir.join(filename);
            tokio::fs::write(&dest, bytes).await?;
            outputs.push(Asset::from_path(AssetKind::Image, dest));
            mime_types.push(mime.clone());
        }

        // 6. raw_meta 只存元信息(不含 base64): 同步 provider 无句柄, status_url 等留空
        let raw_meta = json!({
            "model": req.model,
            "mime_types": mime_types,
            "image_count": outputs.len(),
            "response_text": extract_response_text(&resp),
        });

        Ok(Job {
            id: job_id,
            provider: PROVIDER_NAME.to_string(),
            status: JobStatus::Succeeded,
            outputs,
            error: None,
            raw_meta,
        })
    }

    async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
        // 同步 provider: submit 已返回终态, poll 为 no-op, 原样返回传入的(终态)Job。
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
    use crate::core::provider::Capability;

    #[test]
    fn build_body_has_text_and_modalities() {
        let req = GenRequest {
            capability: Capability::Text2Image,
            model: DEFAULT_T2I_MODEL.to_string(),
            prompt: Some("a red fox in snow".to_string()),
            inputs: Vec::new(),
            params: serde_json::Map::new(),
        };
        let body = build_gemini_request_body(&req);

        // 文本 part
        let text = body["contents"][0]["parts"][0]["text"].as_str();
        assert_eq!(text, Some("a red fox in snow"));
        // responseModalities 固定包含 IMAGE
        let mods = body["generationConfig"]["responseModalities"].as_array().unwrap();
        let mods: Vec<&str> = mods.iter().filter_map(|v| v.as_str()).collect();
        assert!(mods.contains(&"IMAGE"));
        assert!(mods.contains(&"TEXT"));
    }

    #[test]
    fn build_body_includes_inline_image_input() {
        // 带内联字节的图片输入 -> inline_data part(图生图预留路径)
        let req = GenRequest {
            capability: Capability::Image2Image,
            model: DEFAULT_T2I_MODEL.to_string(),
            prompt: Some("make it night".to_string()),
            inputs: vec![Asset::from_inline_bytes(
                AssetKind::Image,
                "image/png",
                vec![1, 2, 3, 4],
            )],
            params: serde_json::Map::new(),
        };
        let body = build_gemini_request_body(&req);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        // 第 0 个是 text, 第 1 个应是 inlineData
        let inline = &parts[1]["inlineData"];
        assert_eq!(inline["mimeType"].as_str(), Some("image/png"));
        // base64("\x01\x02\x03\x04") == "AQIDBA=="
        assert_eq!(inline["data"].as_str(), Some("AQIDBA=="));
    }

    #[test]
    fn parse_response_decodes_inline_base64() {
        // 1x1 png 的 base64 fixture
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQAY3Y2wAAAAAElFTkSuQmCC";
        let resp = json!({
            "candidates": [
                { "content": { "parts": [
                    { "text": "here is your image" },
                    { "inlineData": { "mimeType": "image/png", "data": b64 } }
                ] } }
            ]
        });
        let parts = parse_gemini_image_parts(&resp).expect("应能解析");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, "image/png");
        // 解码字节非空且以 PNG 魔数开头(\x89PNG)
        assert!(!parts[0].1.is_empty());
        assert_eq!(&parts[0].1[0..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn parse_response_tolerates_snake_case() {
        let b64 = "AQIDBA=="; // [1,2,3,4]
        let resp = json!({
            "candidates": [
                { "content": { "parts": [
                    { "inline_data": { "mime_type": "image/jpeg", "data": b64 } }
                ] } }
            ]
        });
        let parts = parse_gemini_image_parts(&resp).expect("应能解析 snake_case");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, "image/jpeg");
        assert_eq!(parts[0].1, vec![1, 2, 3, 4]);
    }

    #[test]
    fn capabilities_declare_text2image_and_image2image() {
        // google 应同时声明 t2i 与 i2i(Gemini 2.5 Flash Image 同 model 双能力)。
        let p = GoogleProvider::new();
        assert!(p.capabilities().contains(&Capability::Text2Image));
        assert!(p.capabilities().contains(&Capability::Image2Image));
    }

    #[test]
    fn catalog_entry_declares_both_capabilities() {
        // 目录条目应把两种能力都挂在同一条(同 model)。
        let p = GoogleProvider::new();
        let cat = p.catalog();
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0].model_id, DEFAULT_T2I_MODEL);
        assert!(cat[0].capabilities.contains(&Capability::Text2Image));
        assert!(cat[0].capabilities.contains(&Capability::Image2Image));
    }

    #[test]
    fn i2i_request_body_carries_inline_data_part() {
        // i2i: 本地图作为内联字节输入 -> 请求体带 inlineData part(base64+mime)。
        let req = GenRequest {
            capability: Capability::Image2Image,
            model: DEFAULT_T2I_MODEL.to_string(),
            prompt: Some("turn the sky purple".to_string()),
            inputs: vec![Asset::from_inline_bytes(
                AssetKind::Image,
                "image/jpeg",
                vec![9, 8, 7, 6],
            )],
            params: serde_json::Map::new(),
        };
        let body = build_gemini_request_body(&req);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        // 有且仅有一个 inlineData part(URL 输入不进 inline, 这里全是本地字节)。
        let inline_parts: Vec<&Value> = parts.iter().filter(|p| p.get("inlineData").is_some()).collect();
        assert_eq!(inline_parts.len(), 1);
        assert_eq!(inline_parts[0]["inlineData"]["mimeType"].as_str(), Some("image/jpeg"));
        // base64([9,8,7,6]) == "CQgHBg=="
        assert_eq!(inline_parts[0]["inlineData"]["data"].as_str(), Some("CQgHBg=="));
    }

    #[test]
    fn url_input_is_not_inlined_for_gemini() {
        // 诚实边界: Gemini inline 接口不吃远程 URL, URL 形态输入不会被塞进 inline_data。
        let req = GenRequest {
            capability: Capability::Image2Image,
            model: DEFAULT_T2I_MODEL.to_string(),
            prompt: Some("edit this".to_string()),
            inputs: vec![Asset::from_url(AssetKind::Image, "https://x/in.png".to_string())],
            params: serde_json::Map::new(),
        };
        let body = build_gemini_request_body(&req);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        // 只有 text part, 没有任何 inlineData(URL 输入被跳过)。
        assert!(parts.iter().all(|p| p.get("inlineData").is_none()));
        assert_eq!(parts[0]["text"].as_str(), Some("edit this"));
    }

    #[test]
    fn parse_response_without_image_is_empty() {
        // 只有文本, 无 inlineData -> 空向量(由 submit 决定报错)
        let resp = json!({
            "candidates": [ { "content": { "parts": [ { "text": "sorry, blocked" } ] } } ]
        });
        let parts = parse_gemini_image_parts(&resp).expect("空也应 Ok");
        assert!(parts.is_empty());
        // 文本上下文可抽出
        assert_eq!(extract_response_text(&resp).as_deref(), Some("sorry, blocked"));
    }
}
