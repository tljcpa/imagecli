//! 即梦 visual 图像 provider —— D-014 独立 provider, 对接火山「即梦 visual」(非方舟 Ark)。
//!
//! 与方舟 Ark(Bearer, OpenAI drop-in)是两条独立产品线: 即梦 visual 走火山 IAM 的
//! AK/SK **V4 签名**(HMAC-SHA256), Action 风格异步任务(提交->task_id->查询->取产物),
//! 字段全自有(req_key/width/height/scale, 而非 OpenAI 的 size/n)。故独立 provider 不混进 volcengine。
//!
//! 复用 async_task 骨架的方式: 鉴权注入点换成 `VolcV4Auth`; 因即梦的「查询结果」也是 POST(带 body)
//! 而非 GET, poll 同样走骨架的 `submit_task`(POST)而非 `query_task`(GET)。
//!
//! 协议形态(host = https://visual.volcengineapi.com, region=cn-north-1, service=cv):
//! - 提交: POST /?Action=CVSync2AsyncSubmitTask&Version=2022-08-31
//!   body: `{ "req_key":"jimeng_t2i_v40", "prompt":..., "width":..., "height":..., "scale":..., "seed":... }`
//!   响应: `{ code, data:{ task_id } }`。
//! - 查询: POST /?Action=CVSync2AsyncGetResult&Version=2022-08-31
//!   body: `{ "req_key":"jimeng_t2i_v40", "task_id":<id> }`
//!   响应: `data.status` ∈ in_queue/generating/done/not_found/expired;
//!   产物在 `data.image_urls`(URL 数组)或 `data.binary_data_base64`(base64 数组)。
//!
//! V4 签名(难点, 务必字段顺序/编码不错):
//!   - 派生签名密钥链(火山变体, **secret key 不加 AWS4 前缀, 直接用原 sk 起链**):
//!     kDate    = HMAC-SHA256(sk, ShortDate[YYYYMMDD]);
//!     kRegion  = HMAC-SHA256(kDate, "cn-north-1");
//!     kService = HMAC-SHA256(kRegion, "cv");
//!     kSigning = HMAC-SHA256(kService, "request")
//!   - canonical request = method\nURI\nquery\ncanonicalHeaders\nsignedHeaders\nhashedPayload
//!     signedHeaders 固定 content-type;host;x-content-sha256;x-date(小写、排序)。
//!   - stringToSign = "HMAC-SHA256\n" + XDate + "\n" + scope + "\n" + sha256hex(canonicalRequest)
//!     scope = ShortDate/region/service/request。
//!   - signature = hex(HMAC-SHA256(kSigning, stringToSign))
//!   - Authorization: HMAC-SHA256 Credential=<ak>/<scope>, SignedHeaders=<sh>, Signature=<sig>
//!   - 另附 X-Date / X-Content-Sha256 头(Host/Content-Type 由 client 发送, 值需与签名一致)。
//!
//! 凭证只从环境变量取(JIMENG_ACCESS_KEY + JIMENG_SECRET_KEY, 或 VOLC_ACCESS_KEY/SECRET_KEY,
//! 或 IMAGECLI_JIMENG_AK/SK), 绝不写死; SecretAccessKey 永不上行(只参与本地签名)。

use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::keys;
use crate::core::provider::{
    Asset, AssetKind, Capability, GenRequest, InputImage, Job, JobStatus, Provider,
};
use crate::transport::async_task::{
    extract_urls_at, AsyncTaskClient, StatusMapping, TaskAuth, TaskHandle,
};

/// provider 名(注册表 key / Job.provider)。
pub const PROVIDER_NAME: &str = "jimeng";
/// 即梦 visual host。
const HOST: &str = "visual.volcengineapi.com";
/// 即梦 visual 根地址。
const BASE_URL: &str = "https://visual.volcengineapi.com";
/// V4 签名 region(即梦 visual 固定 cn-north-1)。
const REGION: &str = "cn-north-1";
/// V4 签名 service(即梦 visual 固定 cv = computer vision)。
const SERVICE: &str = "cv";
/// API 版本(放 query 串)。
const API_VERSION: &str = "2022-08-31";
/// 提交任务的 Action。
const ACTION_SUBMIT: &str = "CVSync2AsyncSubmitTask";
/// 查询结果的 Action。
const ACTION_GET_RESULT: &str = "CVSync2AsyncGetResult";

/// 文生图默认 req_key(即梦图片生成 4.0)。即梦用 req_key 而非 model 字段选模型。
/// 这里把它当作"model"暴露(catalog/--model 用), 提交时写进 body 的 req_key。
pub const DEFAULT_T2I_MODEL: &str = "jimeng_t2i_v40";

/// 即梦 visual 异步状态映射(扩展点 2)。
/// in_queue=排队, generating=执行中, done=成功; not_found/expired/失败/未知 -> Failed(穷尽兜底)。
const STATUS_MAPPING: StatusMapping = StatusMapping {
    queued: &["in_queue"],
    running: &["generating"],
    succeeded: &["done"],
};

/// 产物图片 URL 在响应里的候选路径(扩展点 3): data.image_urls(URL 字符串数组)。
const IMAGE_URL_POINTERS: &[&str] = &["/data/image_urls"];

/// HMAC-SHA256 类型别名。
type HmacSha256 = Hmac<Sha256>;

// ===================== V4 签名: 纯函数部分(便于离线单测/锁定算法) =====================

/// SHA256 十六进制摘要(小写)。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 单步 HMAC-SHA256, 返回原始字节(供签名密钥链逐级派生)。
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// 派生 V4 签名密钥(火山变体: sk 不加前缀, 直接起链)。纯函数, 便于单测中间值。
///
/// kDate=HMAC(sk, ShortDate) -> kRegion=HMAC(kDate, region) -> kService=HMAC(kRegion, service)
/// -> kSigning=HMAC(kService, "request")。
fn derive_signing_key(sk: &str, short_date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(sk.as_bytes(), short_date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"request")
}

/// 构造 canonical request 字符串(纯函数)。
///
/// signedHeaders 固定为 content-type;host;x-content-sha256;x-date(已小写排序)。
/// canonicalHeaders 每行 `name:value\n`, 顺序与 signedHeaders 一致。
fn build_canonical_request(
    method: &str,
    uri: &str,
    canonical_query: &str,
    host: &str,
    content_type: &str,
    x_content_sha256: &str,
    x_date: &str,
) -> String {
    let canonical_headers = format!(
        "content-type:{}\nhost:{}\nx-content-sha256:{}\nx-date:{}\n",
        content_type, host, x_content_sha256, x_date
    );
    let signed_headers = "content-type;host;x-content-sha256;x-date";
    // method\nURI\nquery\ncanonicalHeaders\nsignedHeaders\nhashedPayload
    // 注意 canonical_headers 自身以 \n 结尾, 与 signedHeaders 之间再隔一个 \n(AWS sigv4 同构)。
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, uri, canonical_query, canonical_headers, signed_headers, x_content_sha256
    )
}

/// 构造 stringToSign(纯函数)。
fn build_string_to_sign(x_date: &str, scope: &str, canonical_request_hash: &str) -> String {
    format!(
        "HMAC-SHA256\n{}\n{}\n{}",
        x_date, scope, canonical_request_hash
    )
}

/// 完整 V4 签名(纯函数: 给定 x_date 与 body 可复现, 便于用已知向量锁定算法)。
///
/// 返回 (Authorization 头值, X-Content-Sha256 头值)。
#[allow(clippy::too_many_arguments)]
pub fn sign_v4(
    ak: &str,
    sk: &str,
    region: &str,
    service: &str,
    method: &str,
    uri: &str,
    canonical_query: &str,
    host: &str,
    content_type: &str,
    body: &[u8],
    x_date: &str,
) -> (String, String) {
    let short_date = &x_date[..8];
    let x_content_sha256 = sha256_hex(body);
    let canonical_request = build_canonical_request(
        method,
        uri,
        canonical_query,
        host,
        content_type,
        &x_content_sha256,
        x_date,
    );
    let cr_hash = sha256_hex(canonical_request.as_bytes());
    let scope = format!("{}/{}/{}/request", short_date, region, service);
    let string_to_sign = build_string_to_sign(x_date, &scope, &cr_hash);
    let signing_key = derive_signing_key(sk, short_date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "HMAC-SHA256 Credential={}/{}, SignedHeaders=content-type;host;x-content-sha256;x-date, Signature={}",
        ak, scope, signature
    );
    (authorization, x_content_sha256)
}

/// 把 Unix 秒(UTC)格式化为 V4 要求的 `YYYYMMDDTHHMMSSZ`(纯函数, 不依赖 chrono)。
///
/// 用 Howard Hinnant 的 civil-from-days 算法把"自纪元的天数"还原成年月日, 避免引第三方时间库。
pub fn format_x_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // civil_from_days(纪元 1970-01-01 为第 0 天), 内部以 3 月为年首消除闰年分支。
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year, month, day, hour, minute, second
    )
}

/// 火山 AK/SK V4 鉴权器(TaskAuth 的 V4 实现)。持有 AK/SK 与 region/service/host。
pub struct VolcV4Auth {
    access_key: String,
    secret_key: String,
    region: String,
    service: String,
    host: String,
}

impl VolcV4Auth {
    /// 用 AK/SK 构造(region/service/host 取即梦 visual 固定值)。
    pub fn new(access_key: impl Into<String>, secret_key: impl Into<String>) -> VolcV4Auth {
        VolcV4Auth {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: REGION.to_string(),
            service: SERVICE.to_string(),
            host: HOST.to_string(),
        }
    }
}

impl TaskAuth for VolcV4Auth {
    fn headers(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
    ) -> anyhow::Result<Vec<(String, String)>> {
        // 从 url 解析 path 与 query(签名依赖 canonical URI/query)。
        let (uri, raw_query) = split_path_query(url);
        let canonical_query = canonicalize_query(&raw_query);

        // 当前 UTC 时间 -> X-Date。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| anyhow::anyhow!("系统时钟异常, 无法生成即梦 V4 签名时间戳: {}", e))?
            .as_secs();
        let x_date = format_x_date(now);

        // Content-Type 必须与 client 实际发送一致(AsyncTaskClient 的 POST 固定 application/json)。
        let content_type = "application/json";
        let (authorization, x_content_sha256) = sign_v4(
            &self.access_key,
            &self.secret_key,
            &self.region,
            &self.service,
            method,
            &uri,
            &canonical_query,
            &self.host,
            content_type,
            body,
            &x_date,
        );

        Ok(vec![
            ("Authorization".to_string(), authorization),
            ("X-Date".to_string(), x_date),
            ("X-Content-Sha256".to_string(), x_content_sha256),
        ])
    }
}

/// 从完整 URL 拆出 (path, raw_query)。path 缺省为 "/"。
fn split_path_query(url: &str) -> (String, String) {
    // 去掉 scheme://host 前缀
    let after_scheme = match url.find("://") {
        Some(pos) => &url[pos + 3..],
        None => url,
    };
    let path_and_query = match after_scheme.find('/') {
        Some(pos) => &after_scheme[pos..],
        None => "/",
    };
    match path_and_query.find('?') {
        Some(pos) => (
            path_and_query[..pos].to_string(),
            path_and_query[pos + 1..].to_string(),
        ),
        None => (path_and_query.to_string(), String::new()),
    }
}

/// 把原始 query 串规范化: 拆成 (key,value) 对, 按 key 排序, 各自百分号编码后重组。
fn canonicalize_query(raw_query: &str) -> String {
    if raw_query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in raw_query.split('&') {
        if part.is_empty() {
            continue;
        }
        match part.find('=') {
            Some(pos) => {
                let k = &part[..pos];
                let v = &part[pos + 1..];
                pairs.push((percent_encode(k), percent_encode(v)));
            }
            None => pairs.push((percent_encode(part), String::new())),
        }
    }
    // 按编码后的 key 排序(V4 要求 query 参数有序)。
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// V4 风格百分号编码: 仅 A-Z a-z 0-9 - _ . ~ 不编码, 其余按字节转 %XX(大写十六进制)。
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || b == b'-'
            || b == b'_'
            || b == b'.'
            || b == b'~';
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

// ===================== provider 实现 =====================

/// 即梦 provider。无状态: 只持有 HTTP 客户端与能力声明。
pub struct JimengProvider {
    http: reqwest::Client,
    caps: Vec<Capability>,
}

impl JimengProvider {
    /// 构造默认即梦 provider。声明文生图 + 图生图(即梦 4.0 同一 req_key 既能 t2i 也能 i2i,
    /// i2i 经 binary_data_base64[本地图] 或 image_urls[远程图] 喂参考图)。
    pub fn new() -> JimengProvider {
        JimengProvider {
            http: reqwest::Client::new(),
            caps: vec![Capability::Text2Image, Capability::Image2Image],
        }
    }

    /// 取 V4 鉴权器(需 AK + SK)。任一缺失返回带中文指引的错误, 绝不 panic、绝不写死。
    fn v4_auth(&self) -> anyhow::Result<VolcV4Auth> {
        let ak = keys::require_candidates_key(
            &keys::JIMENG_AK_ENV_CANDIDATES,
            PROVIDER_NAME,
            keys::JIMENG_AK_MISSING_HINT,
        )?;
        let sk = keys::require_candidates_key(
            &keys::JIMENG_SK_ENV_CANDIDATES,
            PROVIDER_NAME,
            keys::JIMENG_SK_MISSING_HINT,
        )?;
        Ok(VolcV4Auth::new(ak, sk))
    }

    /// 构造一个带 V4 鉴权的 async-task 客户端。
    fn task_client(&self) -> anyhow::Result<AsyncTaskClient> {
        let auth = self.v4_auth()?;
        Ok(AsyncTaskClient::new(self.http.clone(), Box::new(auth)))
    }
}

impl Default for JimengProvider {
    fn default() -> JimengProvider {
        JimengProvider::new()
    }
}

/// 拼 Action 端点 URL(提交/查询同 host 同 path, 仅 Action 不同)。
fn action_url(action: &str) -> String {
    format!("{}/?Action={}&Version={}", BASE_URL, action, API_VERSION)
}

/// 由 GenRequest 构造即梦提交请求体(纯函数, 便于离线单测)。
///
/// 形态: `{ "req_key":<model>, "prompt":..., <透传自由参数 width/height/scale/seed 等> }`。
///   - req_key 取 req.model(即梦用 req_key 选模型, 默认 jimeng_t2i_v40)。
///   - prompt 存在才写。
///   - params 自由参数(width/height/scale/seed/return_url 等)整体并入, 用户 --param 覆盖同名默认。
pub fn build_jimeng_submit_body(req: &GenRequest) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("req_key".to_string(), json!(req.model));
    if let Some(prompt) = &req.prompt {
        body.insert("prompt".to_string(), json!(prompt));
    }

    // i2i 喂图: 遍历图片输入, 按形态分流。
    // - 远程 URL -> image_urls 数组(即梦 visual 的参考图 URL 字段);
    // - 本地字节 -> binary_data_base64 数组(即梦 visual 的内联 base64 参考图字段)。
    // 两个数组各自非空才写, 互不排斥(可混填 URL 图与本地图)。t2i 无输入时两数组皆空, 行为不变。
    let mut image_urls: Vec<String> = Vec::new();
    let mut binary_b64: Vec<String> = Vec::new();
    for asset in req.inputs.iter() {
        if !matches!(asset.kind, AssetKind::Image) {
            continue;
        }
        match asset.as_input_image() {
            Some(InputImage::Url(u)) => image_urls.push(u.to_string()),
            Some(InputImage::Bytes { base64, .. }) => binary_b64.push(base64),
            None => {}
        }
    }
    if !image_urls.is_empty() {
        body.insert("image_urls".to_string(), json!(image_urls));
    }
    if !binary_b64.is_empty() {
        body.insert("binary_data_base64".to_string(), json!(binary_b64));
    }

    for (k, v) in req.params.iter() {
        body.insert(k.clone(), v.clone());
    }
    Value::Object(body)
}

/// 构造即梦查询结果请求体(纯函数): `{ "req_key":<model>, "task_id":<id> }`。
pub fn build_jimeng_query_body(req_key: &str, task_id: &str) -> Value {
    json!({ "req_key": req_key, "task_id": task_id })
}

/// 把即梦的状态字符串映射到归一化 JobStatus(纯函数包装)。
pub fn map_jimeng_status(raw: &str) -> JobStatus {
    STATUS_MAPPING.map(raw)
}

/// 从即梦查询结果抽取图片产物(纯函数, 便于离线单测)。
///
/// 优先 data.image_urls(URL 数组); 若无 URL 则回退 data.binary_data_base64(base64 数组),
/// 解码成 inline 字节 Asset 走骨架已有的 inline 落盘路径。
pub fn extract_jimeng_outputs(result: &Value) -> Vec<Asset> {
    // 先试 URL 形态。
    let urls = extract_urls_at(result, IMAGE_URL_POINTERS, AssetKind::Image);
    if !urls.is_empty() {
        return urls;
    }
    // 回退 base64 内联字节。
    let mut out: Vec<Asset> = Vec::new();
    if let Some(arr) = result
        .pointer("/data/binary_data_base64")
        .and_then(|v| v.as_array())
    {
        for item in arr.iter() {
            if let Some(b64) = item.as_str() {
                if b64.is_empty() {
                    continue;
                }
                // 标准 base64(带或不带填充)解码; 解码失败则跳过该条(不中断其余产物)。
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .or_else(|_| {
                        base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64)
                    });
                if let Ok(bytes) = decoded {
                    // 即梦图片默认 jpeg/png, 这里统一标 image/jpeg(落盘按 mime 推扩展名)。
                    out.push(Asset::from_inline_bytes(AssetKind::Image, "image/jpeg", bytes));
                }
            }
        }
    }
    out
}

/// 从即梦响应解析状态字符串(data.status)。
fn parse_status(resp: &Value) -> &str {
    resp.pointer("/data/status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// 抽取响应里可能的报错文本(失败时给排查上下文)。即梦顶层有 message; 兜底 stringify。
fn extract_error_text(resp: &Value) -> String {
    if let Some(msg) = resp
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "Success")
    {
        return msg.to_string();
    }
    resp.to_string()
}

#[async_trait]
impl Provider for JimengProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        // 即梦 4.0 同一 req_key 既能 t2i 也能 i2i: 一条 entry 同时声明两种能力。
        let mut entry = crate::core::catalog::ModelEntry::single(
            PROVIDER_NAME,
            DEFAULT_T2I_MODEL,
            Some("jimeng"),
            Capability::Text2Image,
        );
        entry.capabilities.push(Capability::Image2Image);
        vec![entry]
    }

    fn has_key(&self) -> bool {
        keys::resolve_candidates_key(&keys::JIMENG_AK_ENV_CANDIDATES, PROVIDER_NAME).is_some()
            && keys::resolve_candidates_key(&keys::JIMENG_SK_ENV_CANDIDATES, PROVIDER_NAME).is_some()
    }

    async fn schema(&self, model: &str) -> anyhow::Result<Value> {
        Ok(json!({
            "provider": PROVIDER_NAME,
            "model": model,
            "note": "占位 schema; 即梦 visual 真实参数以火山「视觉智能」控制台为准",
            "common_params": {
                "req_key": "string, 模型标识(如 jimeng_t2i_v40)",
                "prompt": "string, 文本提示词",
                "width": "int, 生成宽度",
                "height": "int, 生成高度",
                "scale": "number, 文本影响程度",
                "seed": "int, 随机种子(-1 随机)"
            }
        }))
    }

    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job> {
        let client = self.task_client()?;
        // req_key 即 model, 后续查询也要用, 记进句柄。
        let req_key = req.model.clone();
        let body = build_jimeng_submit_body(&req);
        let resp = client.submit_task(&action_url(ACTION_SUBMIT), &body).await?;

        // 解析 task_id(即梦在 data.task_id)。
        let task_id = resp
            .pointer("/data/task_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("即梦提交未返回 task_id(响应: {}), 无法轮询", resp)
            })?
            .to_string();

        // 句柄: query_url 指向 GetResult 端点(POST), req_key 一并存入 raw_meta 供 poll 构造查询体。
        let handle = TaskHandle {
            task_id: task_id.clone(),
            query_url: action_url(ACTION_GET_RESULT),
        };
        let mut raw_meta = handle.to_raw_meta();
        if let Some(obj) = raw_meta.as_object_mut() {
            obj.insert("req_key".to_string(), json!(req_key));
        }

        Ok(Job {
            id: task_id,
            provider: PROVIDER_NAME.to_string(),
            status: JobStatus::Queued,
            outputs: Vec::new(),
            error: None,
            raw_meta,
        })
    }

    async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
        let handle = TaskHandle::from_raw_meta(&job.raw_meta)?;
        // req_key 从句柄取; 缺失则回退默认(老句柄兼容)。
        let req_key = job
            .raw_meta
            .get("req_key")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_T2I_MODEL)
            .to_string();
        let client = self.task_client()?;

        // 即梦查询结果是 POST(带 body), 故走 submit_task(POST)而非骨架的 query_task(GET)。
        let query_body = build_jimeng_query_body(&req_key, &handle.task_id);
        let polled = client.submit_task(&handle.query_url, &query_body).await?;

        let raw_status = parse_status(&polled);
        let status = map_jimeng_status(raw_status);

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
                next.outputs = extract_jimeng_outputs(&polled);
                if let Some(obj) = next.raw_meta.as_object_mut() {
                    obj.insert("result".to_string(), polled.clone());
                }
            }
            JobStatus::Failed => {
                next.error = Some(format!(
                    "即梦任务失败({}): {}",
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
        // 即梦 visual 异步任务无通用取消端点; 尽力而为, 直接 Ok。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn capabilities_declare_text2image_and_image2image() {
        let p = JimengProvider::new();
        assert!(p.capabilities().contains(&Capability::Text2Image));
        // 即梦 4.0 同源支持 i2i(经 binary_data_base64 / image_urls 喂参考图)。
        assert!(p.capabilities().contains(&Capability::Image2Image));
        // 视频能力仍不声明(D-014: 即梦图像 provider 不混视频)。
        assert!(!p.capabilities().contains(&Capability::Text2Video));
    }

    #[test]
    fn build_submit_body_i2i_local_bytes_to_binary_base64() {
        // 本地图(内联字节)输入 -> binary_data_base64 数组; 不应出现 image_urls。
        let mut req = t2i_req("改成水彩风格");
        req.capability = Capability::Image2Image;
        req.inputs = vec![Asset::from_inline_bytes(
            AssetKind::Image,
            "image/png",
            vec![1u8, 2, 3, 4],
        )];
        let body = build_jimeng_submit_body(&req);
        // base64(0x01020304) = "AQIDBA=="
        let arr = body
            .get("binary_data_base64")
            .and_then(|v| v.as_array())
            .expect("应有 binary_data_base64 数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], json!("AQIDBA=="));
        assert!(body.get("image_urls").is_none());
    }

    #[test]
    fn build_submit_body_i2i_url_to_image_urls() {
        // 远程 URL 输入 -> image_urls 数组; 不应出现 binary_data_base64。
        let mut req = t2i_req("加一只猫");
        req.capability = Capability::Image2Image;
        req.inputs = vec![Asset::from_url(AssetKind::Image, "https://x/ref.png")];
        let body = build_jimeng_submit_body(&req);
        let arr = body
            .get("image_urls")
            .and_then(|v| v.as_array())
            .expect("应有 image_urls 数组");
        assert_eq!(arr[0], json!("https://x/ref.png"));
        assert!(body.get("binary_data_base64").is_none());
    }

    #[test]
    fn v4_signature_matches_known_vector() {
        // 已知向量(由独立 python 参考实现生成, 锁定 V4 算法不被无意改动):
        // ak=AKLTtest sk=c2VjcmV0 region=cn-north-1 service=cv
        // method=POST uri=/ query=Action=CVSync2AsyncSubmitTask&Version=2022-08-31
        // host=visual.volcengineapi.com content_type=application/json
        // body={"req_key":"jimeng_t2i_v40"} x_date=20240101T000000Z
        let body = br#"{"req_key":"jimeng_t2i_v40"}"#;
        let (auth, xcsha) = sign_v4(
            "AKLTtest",
            "c2VjcmV0",
            "cn-north-1",
            "cv",
            "POST",
            "/",
            "Action=CVSync2AsyncSubmitTask&Version=2022-08-31",
            "visual.volcengineapi.com",
            "application/json",
            body,
            "20240101T000000Z",
        );
        assert_eq!(
            xcsha,
            "b1e23e1cb883cff93cc72b847e0d63ccea7f5586b3dd734101edf7d7402fb605"
        );
        assert_eq!(
            auth,
            "HMAC-SHA256 Credential=AKLTtest/20240101/cn-north-1/cv/request, \
SignedHeaders=content-type;host;x-content-sha256;x-date, \
Signature=e8bcf6ac179226df9aa6f5de8b7c496e23a3923bf4d97efc58d827502f23245e"
        );
    }

    #[test]
    fn signing_key_chain_intermediate_is_stable() {
        // 锁定派生密钥链(火山变体, sk 不加前缀)的最终 kSigning 中间值。
        let key = derive_signing_key("c2VjcmV0", "20240101", "cn-north-1", "cv");
        assert_eq!(
            hex::encode(key),
            "4eb7fd5aca5f45c3778d583c0d99ecf11933093a078185afe75738b185cbb80c"
        );
    }

    #[test]
    fn canonical_request_hash_is_stable() {
        let cr = build_canonical_request(
            "POST",
            "/",
            "Action=CVSync2AsyncSubmitTask&Version=2022-08-31",
            "visual.volcengineapi.com",
            "application/json",
            "b1e23e1cb883cff93cc72b847e0d63ccea7f5586b3dd734101edf7d7402fb605",
            "20240101T000000Z",
        );
        assert_eq!(
            sha256_hex(cr.as_bytes()),
            "5292f8e227ed353e5d675ec5bf88e5f3e9aa7d6342d08a36e983a351f5fc8f3f"
        );
    }

    #[test]
    fn format_x_date_known_epochs() {
        // 1970-01-01T00:00:00Z
        assert_eq!(format_x_date(0), "19700101T000000Z");
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(format_x_date(1_704_067_200), "20240101T000000Z");
        // 2024-01-01T00:00:00Z + 时分秒
        assert_eq!(format_x_date(1_704_067_200 + 3661), "20240101T010101Z");
    }

    #[test]
    fn canonicalize_query_sorts_and_encodes() {
        // 乱序输入应按 key 排序输出
        assert_eq!(
            canonicalize_query("Version=2022-08-31&Action=CVSync2AsyncSubmitTask"),
            "Action=CVSync2AsyncSubmitTask&Version=2022-08-31"
        );
        // 空 query
        assert_eq!(canonicalize_query(""), "");
    }

    #[test]
    fn split_path_query_parses_action_url() {
        let (path, query) = split_path_query(
            "https://visual.volcengineapi.com/?Action=CVSync2AsyncSubmitTask&Version=2022-08-31",
        );
        assert_eq!(path, "/");
        assert_eq!(query, "Action=CVSync2AsyncSubmitTask&Version=2022-08-31");
    }

    #[test]
    fn v4_auth_returns_three_headers() {
        let auth = VolcV4Auth::new("ak", "sk");
        let headers = auth
            .headers(
                "POST",
                "https://visual.volcengineapi.com/?Action=CVSync2AsyncSubmitTask&Version=2022-08-31",
                b"{}",
            )
            .expect("V4 签名不应失败");
        // Authorization / X-Date / X-Content-Sha256 三个头都在
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"Authorization"));
        assert!(names.contains(&"X-Date"));
        assert!(names.contains(&"X-Content-Sha256"));
        let auth_val = &headers.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        assert!(auth_val.starts_with("HMAC-SHA256 Credential=ak/"));
    }

    #[test]
    fn build_submit_body_uses_req_key() {
        let mut req = t2i_req("a red panda");
        req.params.insert("width".to_string(), json!(1024));
        req.params.insert("height".to_string(), json!(1024));
        let body = build_jimeng_submit_body(&req);
        assert_eq!(body.get("req_key").unwrap(), &json!(DEFAULT_T2I_MODEL));
        assert_eq!(body.get("prompt").unwrap(), &json!("a red panda"));
        assert_eq!(body.get("width").unwrap(), &json!(1024));
    }

    #[test]
    fn build_query_body_has_req_key_and_task_id() {
        let body = build_jimeng_query_body("jimeng_t2i_v40", "task-123");
        assert_eq!(body.get("req_key").unwrap(), &json!("jimeng_t2i_v40"));
        assert_eq!(body.get("task_id").unwrap(), &json!("task-123"));
    }

    #[test]
    fn status_mapping_jimeng_specific() {
        assert_eq!(map_jimeng_status("in_queue"), JobStatus::Queued);
        assert_eq!(map_jimeng_status("generating"), JobStatus::Running);
        assert_eq!(map_jimeng_status("done"), JobStatus::Succeeded);
        // not_found / expired / 未知 -> Failed
        assert_eq!(map_jimeng_status("not_found"), JobStatus::Failed);
        assert_eq!(map_jimeng_status("expired"), JobStatus::Failed);
        assert_eq!(map_jimeng_status("whatever"), JobStatus::Failed);
    }

    #[test]
    fn extract_outputs_prefers_image_urls() {
        let result = json!({
            "code": 10000,
            "data": {
                "status": "done",
                "image_urls": ["https://vis/out1.jpg", "https://vis/out2.jpg"]
            }
        });
        let outputs = extract_jimeng_outputs(&result);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].kind, AssetKind::Image);
        assert_eq!(outputs[0].url.as_deref(), Some("https://vis/out1.jpg"));
    }

    #[test]
    fn extract_outputs_falls_back_to_binary_base64() {
        // 无 image_urls 时解码 binary_data_base64 成 inline 字节。
        let raw = b"hello-jpeg-bytes";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let result = json!({
            "data": {
                "status": "done",
                "binary_data_base64": [b64]
            }
        });
        let outputs = extract_jimeng_outputs(&result);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, AssetKind::Image);
        // inline 字节应解码回原文
        let inline = outputs[0].inline.as_ref().expect("应为 inline 字节产物");
        assert_eq!(inline.data, raw);
        assert_eq!(inline.mime, "image/jpeg");
    }

    #[test]
    fn extract_outputs_empty_when_neither() {
        assert!(extract_jimeng_outputs(&json!({ "data": { "status": "generating" } })).is_empty());
    }

    #[test]
    fn action_url_is_well_formed() {
        assert_eq!(
            action_url(ACTION_SUBMIT),
            "https://visual.volcengineapi.com/?Action=CVSync2AsyncSubmitTask&Version=2022-08-31"
        );
        assert_eq!(
            action_url(ACTION_GET_RESULT),
            "https://visual.volcengineapi.com/?Action=CVSync2AsyncGetResult&Version=2022-08-31"
        );
    }
}
