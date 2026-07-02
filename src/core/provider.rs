//! Provider 契约与核心归一化类型。
//!
//! 这里是整个项目的"语义中枢": 不管底层是 fal 的 queue、OpenAI 的同步接口、
//! 还是未来的子进程 CLI, 上层编排器只面对本文件定义的 Capability/JobStatus/
//! GenRequest/Job/Asset 这套统一词汇(对应 DECISIONS D-003 两维抽象、D-005 Submit+Poll)。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 能力维度: 一个 provider 在某个 model 上能干什么。
/// 这是 D-003 的"能力(Capability)"正交维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    /// 文生图
    Text2Image,
    /// 图生图(参考图驱动)
    Image2Image,
    /// 文生视频
    Text2Video,
    /// 图生视频
    Image2Video,
    /// 多帧合成视频
    FramesToVideo,
    /// 超分辨率
    Upscale,
}

impl Capability {
    /// 把命令行里用户给的字符串解析成能力枚举。
    /// 用显式 match 而不是 try_from 宏, 便于给出中文错误。
    pub fn parse(s: &str) -> anyhow::Result<Capability> {
        // 统一转小写, 容忍用户大小写混写
        let lowered = s.to_ascii_lowercase();
        match lowered.as_str() {
            "text2image" | "t2i" => Ok(Capability::Text2Image),
            "image2image" | "i2i" => Ok(Capability::Image2Image),
            "text2video" | "t2v" => Ok(Capability::Text2Video),
            "image2video" | "i2v" => Ok(Capability::Image2Video),
            "framestovideo" | "f2v" => Ok(Capability::FramesToVideo),
            "upscale" => Ok(Capability::Upscale),
            _ => {
                // 不 panic, 给清晰中文报错
                anyhow::bail!("未知能力: {} (可选: text2image/image2image/text2video/image2video/framestovideo/upscale)", s)
            }
        }
    }

    /// 给人看的稳定字符串名(也用于 --json 输出)。
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::Text2Image => "text2image",
            Capability::Image2Image => "image2image",
            Capability::Text2Video => "text2video",
            Capability::Image2Video => "image2video",
            Capability::FramesToVideo => "framestovideo",
            Capability::Upscale => "upscale",
        }
    }
}

/// 归一化任务状态机。
/// 不同 provider 各有各的状态字符串(fal 是 IN_QUEUE/IN_PROGRESS/COMPLETED,
/// Replicate 是 starting/processing/succeeded/...), 全部收敛到这四态。
/// 用 sum type + 穷尽 match, 编译期保证所有分支被处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    /// 排队中, 尚未开始执行
    Queued,
    /// 正在执行
    Running,
    /// 成功完成(终态)
    Succeeded,
    /// 失败(终态)
    Failed,
}

impl JobStatus {
    /// 是否为终态。终态意味着轮询应当停止。
    pub fn is_terminal(&self) -> bool {
        match self {
            JobStatus::Queued => false,
            JobStatus::Running => false,
            JobStatus::Succeeded => true,
            JobStatus::Failed => true,
        }
    }

    /// 稳定字符串名, 用于 --json 输出。
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
        }
    }

    /// 从稳定字符串名还原(store 反序列化用)。
    /// 未知字符串归 Failed: 宁可当失败终态停止轮询, 也不把脏状态当运行中。
    pub fn parse(s: &str) -> JobStatus {
        match s {
            "queued" => JobStatus::Queued,
            "running" => JobStatus::Running,
            "succeeded" => JobStatus::Succeeded,
            _ => JobStatus::Failed,
        }
    }
}

/// 素材类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    Image,
    Video,
    Audio,
}

/// 内联字节产物: 产物直接以字节内嵌在响应里(典型: Gemini 的 base64 inlineData),
/// 不是可再次下载的 URL。data 存"已解码"的原始字节(不是 base64 字符串),
/// mime 用于落盘时推断扩展名(image/png -> png 等)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineBytes {
    /// MIME 类型, 如 "image/png"
    pub mime: String,
    /// 已解码的原始字节
    pub data: Vec<u8>,
}

/// 素材的来源视角(借用), 供 download/落盘逻辑做穷尽 match。
///
/// 为什么用"借用枚举 + 三个可选字段"而不是直接把 Asset 改成 enum:
/// 既有代码与全部既有测试都按 `asset.url` / `asset.local_path` 直接读字段,
/// 直接换成 enum 会推翻这套访问面与一批测试。这里折中:
/// 字段保持向后兼容(新增一个 `inline`), 同时用 `source()` 返回一个借用枚举,
/// 让 download 侧仍能享受"穷尽 match 三种来源"的编译期保证。
/// 取舍: 牺牲一点"类型层面互斥"(三个 Option 理论上可同时为 Some),
/// 换取零破坏地扩展既有抽象; 互斥性由构造函数 + source() 的优先级约定来保证。
pub enum AssetSource<'a> {
    /// 内联字节(Gemini 等同步内嵌产物)
    Inline(&'a InlineBytes),
    /// 远程 URL(需下载)
    Url(&'a str),
    /// 本地已落盘路径
    LocalPath(&'a Path),
    /// 三者皆空(异常素材)
    Empty,
}

/// 素材: 可能是本地文件(待上传的参考图 / 已落盘产物)、远程 URL(产物 / 已上传输入),
/// 或内联字节产物(Gemini 的 base64 图片)。三种来源由 `source()` 归一成 AssetSource。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub kind: AssetKind,
    /// 本地路径, 输入参考素材或已落盘产物时常用
    pub local_path: Option<PathBuf>,
    /// 远程 URL, 产物或已上传素材
    pub url: Option<String>,
    /// 内联字节产物(如 Gemini inlineData); 落盘后通常收敛为 local_path 再持久化
    #[serde(default)]
    pub inline: Option<InlineBytes>,
}

impl Asset {
    /// 构造一个仅含远程 URL 的素材(典型: 下载产物)。
    pub fn from_url(kind: AssetKind, url: impl Into<String>) -> Asset {
        Asset {
            kind,
            local_path: None,
            url: Some(url.into()),
            inline: None,
        }
    }

    /// 构造一个仅含本地路径的素材(典型: 待上传输入 / 已落盘产物)。
    pub fn from_path(kind: AssetKind, path: PathBuf) -> Asset {
        Asset {
            kind,
            local_path: Some(path),
            url: None,
            inline: None,
        }
    }

    /// 构造一个内联字节素材(典型: Gemini base64 产物解码后的字节)。
    pub fn from_inline_bytes(kind: AssetKind, mime: impl Into<String>, data: Vec<u8>) -> Asset {
        Asset {
            kind,
            local_path: None,
            url: None,
            inline: Some(InlineBytes {
                mime: mime.into(),
                data,
            }),
        }
    }

    /// 把三个可选字段归一成单一来源枚举, 供落盘逻辑穷尽处理。
    /// 优先级: inline > url > local_path。
    /// 理由: inline 是"必须立刻落盘否则丢"的瞬时字节, 优先级最高;
    /// 收敛为 local_path 后 inline 已为 None, 自然落到 LocalPath 分支。
    pub fn source(&self) -> AssetSource<'_> {
        if let Some(b) = &self.inline {
            return AssetSource::Inline(b);
        }
        if let Some(u) = &self.url {
            return AssetSource::Url(u);
        }
        if let Some(p) = &self.local_path {
            return AssetSource::LocalPath(p);
        }
        AssetSource::Empty
    }

    /// 把一个"输入素材"归一成 provider 的 i2i 喂图形态(对应本地图 i2i 支持)。
    ///
    /// 与 source() 的区别: source() 服务"产物落盘"(优先 inline 立即落盘), 本方法服务
    /// "输入图喂给请求体"。两种喂法:
    /// - 已是远程 URL(url 字段) -> InputImage::Url, 各家直接透传到 image/image_urls 字段;
    /// - 本地字节(inline 字段, 由 CLI 读取本地文件时填入) -> InputImage::Bytes,
    ///   base64 已编码 + 携带 mime, 各家自行决定塞 raw base64(即梦 binary_data_base64、
    ///   可灵 image)还是 data URI(`data:<mime>;base64,...`, 如 Seedream 的 image)。
    ///
    /// 优先级 url > inline: 与喂图语义一致(已是 URL 则无需再 base64)。
    /// local_path 形态(只有路径、未读字节)不在此处理 —— CLI 加载本地输入图时已读成 inline,
    /// 故这里遇到纯 local_path 返回 None(避免在纯函数 build_body 里做文件 IO)。
    pub fn as_input_image(&self) -> Option<InputImage<'_>> {
        if let Some(u) = &self.url {
            return Some(InputImage::Url(u));
        }
        if let Some(b) = &self.inline {
            let base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &b.data);
            return Some(InputImage::Bytes {
                base64,
                mime: &b.mime,
            });
        }
        None
    }
}

/// 输入图喂给 provider i2i 请求体的归一形态。
/// 由 `Asset::as_input_image` 产出, 各 provider 的 build_body 据此塞自家字段。
pub enum InputImage<'a> {
    /// 已是远程 URL, 直接透传(provider 的 image / image_urls 字段)。
    Url(&'a str),
    /// 本地字节: base64 已编码 + mime。
    /// 各家自行决定用 raw base64(即梦/可灵)还是 data URI(`data:<mime>;base64,...`)。
    Bytes { base64: String, mime: &'a str },
}

impl InputImage<'_> {
    /// 取一个 raw base64 字符串(URL 形态返回 None)。即梦/可灵塞 base64 字段时用。
    pub fn as_raw_base64(&self) -> Option<&str> {
        match self {
            InputImage::Url(_) => None,
            InputImage::Bytes { base64, .. } => Some(base64),
        }
    }

    /// 归一成"可直接放进 image 字段的字符串": URL 原样; 字节形态拼成 data URI。
    /// Seedream 等接受 URL 或 data URI 的 OpenAI 兼容 i2i 用这个。
    pub fn to_image_field_string(&self) -> String {
        match self {
            InputImage::Url(u) => (*u).to_string(),
            InputImage::Bytes { base64, mime } => {
                format!("data:{};base64,{}", mime, base64)
            }
        }
    }
}

/// 归一化生成请求。上层 CLI 把用户输入翻译成它, provider 再翻译成自家协议。
/// Serialize: 便于 store 把请求原文落库(request_json), 供复现/审计。
#[derive(Debug, Clone, Serialize)]
pub struct GenRequest {
    pub capability: Capability,
    /// provider 内部的 endpoint / model id, 例如 "fal-ai/flux/dev"
    pub model: String,
    pub prompt: Option<String>,
    /// 参考图/视频/音频等输入素材
    pub inputs: Vec<Asset>,
    /// 自由参数(image_size/seed/num_images/aspect_ratio...), 直接透传给 provider
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// 归一化任务句柄。submit/poll 都返回它, download 消费它的 outputs。
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub provider: String,
    pub status: JobStatus,
    /// 完成后的产物素材(URL 列表)
    pub outputs: Vec<Asset>,
    /// 失败时的错误描述
    pub error: Option<String>,
    /// 原始返回体, 保留给调试 / --json 透传(状态/响应原文)
    pub raw_meta: serde_json::Value,
}

/// Provider 契约。对应 D-005: 核心是 submit + poll。
/// 同步 provider 的 submit 直接返回终态, poll 为 no-op; 异步 provider 走真实轮询。
#[async_trait]
pub trait Provider: Send + Sync {
    /// provider 名(注册表 key, 也是 Job.provider)。
    fn name(&self) -> &str;

    /// 本 provider 支持的能力列表。
    fn capabilities(&self) -> &[Capability];

    /// 自报某 model 的参数 schema。fal 先返回静态占位, 后续可接 fal 的 schema 接口。
    async fn schema(&self, model: &str) -> anyhow::Result<serde_json::Value>;

    /// 提交任务。异步 provider 返回 Queued/Running 的 Job; 同步 provider 直接返回终态。
    async fn submit(&self, req: GenRequest) -> anyhow::Result<Job>;

    /// 轮询任务状态。入参是从 store 读出的完整 Job(其 raw_meta 携带 provider 句柄),
    /// provider 据此还原自家轮询所需 URL, 自身保持无状态(D-007)。
    /// 同步 provider 实现为直接返回当前(终态)Job。
    async fn poll(&self, job: &Job) -> anyhow::Result<Job>;

    /// 取消任务。从 job.raw_meta 取 cancel_url。不支持取消的 provider 返回 Ok(())(尽力而为)。
    async fn cancel(&self, job: &Job) -> anyhow::Result<()>;

    /// 自报本 provider 暴露的模型目录(D-011): 每条声明 provider/model/alias/能力/估算成本。
    /// 默认返回空向量, 让未来新增的 provider 即使未声明也能编译; 已落地的 provider 都应覆盖,
    /// 至少给出 text2image 的默认 model 一条。available 字段由聚合层(catalog)按 has_key 覆盖,
    /// provider 在此不必关心。
    fn catalog(&self) -> Vec<crate::core::catalog::ModelEntry> {
        Vec::new()
    }

    /// 自报当前是否能取到本 provider 的 API key(env/keyring)。
    /// 各 provider 知道自己的 key 来源(见 config::keys), 故由 provider 自报而非聚合层硬编码。
    /// 默认 false(保守: 未知 key 来源视为不可用), 已落地的 provider 都应覆盖。
    fn has_key(&self) -> bool {
        false
    }
}

/// 上传抽象。并非所有 provider 都需要(纯 URL 输入的不需要);
/// fal 用 storage upload 把本地文件换成可访问 URL。
#[async_trait]
pub trait Uploader: Send + Sync {
    async fn upload(&self, local: &std::path::Path) -> anyhow::Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_input_image_url_asset_yields_url() {
        let a = Asset::from_url(AssetKind::Image, "https://x/in.png");
        match a.as_input_image() {
            Some(InputImage::Url(u)) => assert_eq!(u, "https://x/in.png"),
            _ => panic!("URL 素材应归一为 InputImage::Url"),
        }
    }

    #[test]
    fn as_input_image_inline_asset_yields_base64() {
        // 0x01020304 的标准 base64 是 "AQIDBA=="
        let a = Asset::from_inline_bytes(AssetKind::Image, "image/png", vec![1, 2, 3, 4]);
        match a.as_input_image() {
            Some(InputImage::Bytes { base64, mime }) => {
                assert_eq!(base64, "AQIDBA==");
                assert_eq!(mime, "image/png");
            }
            _ => panic!("内联字节素材应归一为 InputImage::Bytes"),
        }
    }

    #[test]
    fn as_input_image_local_path_only_yields_none() {
        // 只有 local_path、未读字节: build_body 纯函数阶段无法编码, 返回 None。
        let a = Asset::from_path(AssetKind::Image, std::path::PathBuf::from("/tmp/x.png"));
        assert!(a.as_input_image().is_none());
    }

    #[test]
    fn input_image_to_field_string_forms() {
        // URL 原样; 字节形态拼 data URI。
        let url = InputImage::Url("https://x/a.png");
        assert_eq!(url.to_image_field_string(), "https://x/a.png");
        assert_eq!(url.as_raw_base64(), None);

        let bytes = InputImage::Bytes {
            base64: "AQIDBA==".to_string(),
            mime: "image/png",
        };
        assert_eq!(
            bytes.to_image_field_string(),
            "data:image/png;base64,AQIDBA=="
        );
        assert_eq!(bytes.as_raw_base64(), Some("AQIDBA=="));
    }
}
