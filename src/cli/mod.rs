//! CLI 层: 把用户命令行输入翻译成统一的 GenRequest / Job 操作, 调度 core 内核。
//!
//! 子命令: generate / status / download / providers / models。
//! 设计原则(对应 REQUIREMENTS): 稳定的 --json 机器输出契约, 退出码有契约,
//! 错误一律给中文提示而非 panic。

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::config::settings::Settings;
use crate::core::catalog::{self, ModelEntry};
use crate::core::download::download_job_outputs;
use crate::core::pricing;
use crate::core::provider::{Asset, AssetKind, Capability, GenRequest, Job, JobStatus};
use crate::core::registry::Registry;
use crate::core::route::{self, Candidate, RequestTemplate, RouteConfig};
use crate::core::runner::RunConfig;
use crate::core::store::{now_unix, JobFilter, JobRecord, JobStore};
use crate::providers::{
    agnes, fal, google, jimeng, kling, openai, ppio, replicate, seedance, siliconflow, stepfun,
    volcengine, zhipu,
};

/// generate 未显式 provider 且配置也无默认时的内置回退 provider。
///
/// 选 agnes 而非 fal(B 发现的问题): fal 需海外付费 key, 新用户默认会直接失败;
/// agnes 是 D-009 的免费层, "免费且常可用", 作为最后兜底最不容易让新手卡住。
/// 完整回退链: CLI flag > 配置文件默认 > 本内置默认(agnes)。
const BUILTIN_DEFAULT_PROVIDER: &str = "agnes";

/// imagecli: 通用多 provider 图像/视频生成 CLI。
#[derive(Debug, Parser)]
#[command(name = "imagecli", version, about = "通用多 provider 图像/视频生成 CLI")]
pub struct Cli {
    /// 输出机器可解析的 JSON(稳定契约), 默认输出人类可读文本。
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// 顶层子命令。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 生成: 提交一个或多个任务, 轮询到终态, 默认下载产物。
    Generate(GenerateArgs),
    /// 查询某任务状态: 从本地 store 读出任务并向 provider 刷新一次(跨进程可用)。
    Status(StatusArgs),
    /// 下载: 从 store 读出已成功任务的产物落盘; 或直接下载给定 URL。
    Download(DownloadArgs),
    /// 列出本地 store 里的任务, 支持按状态/能力过滤。
    List(ListArgs),
    /// 列出已注册的 provider 及其能力。
    Providers,
    /// 列出某 provider 的可用 model(复用统一目录, 按 provider 过滤)。
    Models(ModelsArgs),
    /// "/model 式" 统一模型选择器(D-011): 无参进交互菜单(TTY)/列表(无 TTY);
    /// 带 `<provider/model>` 或 `<alias>` 则非交互直设默认并持久化。
    Model(ModelArgs),
    /// 启动 MCP server(stdio 上的 JSON-RPC 2.0): 让 Claude Code / Cursor 等 agent
    /// 把 imagecli 当工具直接调用(生成图像/视频、查任务)。落地 D-006 agent-first 定位。
    Mcp,
}

/// generate 子命令参数。
#[derive(Debug, clap::Args)]
pub struct GenerateArgs {
    /// provider 名。不给则按"配置文件默认 -> 内置默认(agnes)"回退(见 BUILTIN_DEFAULT_PROVIDER)。
    #[arg(long)]
    pub provider: Option<String>,

    /// 候选 provider 故障转移链: 主 provider 失败时按序切这些备用(可重复或逗号分隔)。
    /// 例: `--fallback agnes,replicate` 或 `--fallback agnes --fallback replicate`。
    /// 每个 prompt 各自独立走"主 + 这些备"的候选链(与批量 fan-out 正交)。
    #[arg(long = "fallback", value_name = "PROVIDER", value_delimiter = ',')]
    pub fallback: Vec<String>,

    /// 对"可重试错误"(HTTP 429/5xx、超时、网络抖动)的每 provider 重试次数。
    /// 默认 2(即最多 1 次初次 + 2 次重试)。鉴权/参数等不可重试错误不受此影响。
    #[arg(long, default_value_t = 2)]
    pub retries: u32,

    /// 详细模式: 把每阶段(submit/poll/fallback)的 provider/model/耗时/关联 request_id/
    /// 重试与切换事件打到 stderr(不污染 --json 的 stdout)。
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// 能力: text2image/image2image/text2video/image2video/framestovideo/upscale。
    #[arg(long, default_value = "text2image")]
    pub capability: String,

    /// provider 内的 model id; 不给则按能力取默认(text2image -> fal-ai/flux/dev)。
    #[arg(long)]
    pub model: Option<String>,

    /// 文本提示词(可重复)。给多个则批量 fan-out, 每个 prompt 一个 Job 并发跑。
    #[arg(long = "prompt")]
    pub prompts: Vec<String>,

    /// 从文件读取一批 prompt: 一行一个, 空行与以 # 开头的注释行忽略。
    /// 与 --prompt 可叠加(先文件后命令行, 全部合并)。
    #[arg(long = "prompts-file", value_name = "PATH")]
    pub prompts_file: Option<PathBuf>,

    /// 只预估不提交: 打印本次将提交的任务数与预估成本合计(Decimal), 不调用任何 provider, 退出 0。
    #[arg(long)]
    pub dry_run: bool,

    /// 预算上限(USD): 预估总成本超过该值则拒绝执行(非零退出)。Decimal 精确比较。
    #[arg(long, value_name = "AMOUNT")]
    pub max_cost: Option<Decimal>,

    /// 输入素材(可重复, 用于图生图/图生视频)。可填本地图片路径或 http(s) URL:
    /// 本地路径会读取字节并按各 provider 能力 base64 内联进请求(即梦/可灵/Seedream 已支持)。
    #[arg(long = "input", value_name = "PATH_OR_URL")]
    pub inputs: Vec<String>,

    /// 自由参数, 形如 key=value(可重复)。value 先尝试按 JSON 解析, 失败则当字符串。
    #[arg(long = "param", value_name = "KEY=VALUE")]
    pub params: Vec<String>,

    /// 产物下载目录。
    #[arg(long, default_value = "./out")]
    pub out_dir: PathBuf,

    /// 最大并发任务数。
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    /// 只提交+轮询, 不下载产物。
    #[arg(long)]
    pub no_download: bool,
}

/// status 子命令参数。
#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// provider 名。
    #[arg(long, default_value = "fal")]
    pub provider: String,
    /// 任务 id。
    pub job_id: String,
}

/// download 子命令参数。
#[derive(Debug, clap::Args)]
pub struct DownloadArgs {
    /// 任务 id: 从 store 读出该任务(需已成功)并下载其产物。
    /// 也用作文件名前缀。
    #[arg(long, default_value = "download")]
    pub job_id: String,
    /// 直接下载这些产物 URL(可重复); 给了就不查 store, 走纯 URL 下载。
    #[arg(long = "url", value_name = "URL")]
    pub urls: Vec<String>,
    /// 下载目录。
    #[arg(long, default_value = "./out")]
    pub out_dir: PathBuf,
}

/// list 子命令参数。
#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// 按状态过滤: queued/running/succeeded/failed。
    #[arg(long)]
    pub status: Option<String>,
    /// 按能力过滤: text2image/image2image/...。
    #[arg(long)]
    pub capability: Option<String>,
    /// 最多返回多少条。
    #[arg(long)]
    pub limit: Option<i64>,
    /// 跳过前多少条(配合 limit 分页)。
    #[arg(long)]
    pub offset: Option<i64>,
}

/// models 子命令参数。
#[derive(Debug, clap::Args)]
pub struct ModelsArgs {
    /// provider 名。
    #[arg(long, default_value = "fal")]
    pub provider: String,
}

/// model 子命令参数(/model 式选择器)。
#[derive(Debug, clap::Args)]
pub struct ModelArgs {
    /// 要设为默认的目标: `<provider/model>` 或 `<alias>`(如 agnes / agnes/agnes-image-2.1-flash)。
    /// 不给则: TTY 下进交互菜单; 无 TTY 下打印目录列表 + 设置提示(D-011 无 TTY 降级)。
    pub selector: Option<String>,

    /// 仅列出目录, 不进交互(即使在 TTY 也只打印)。
    #[arg(long)]
    pub list: bool,
}

/// CLI 入口分发。返回进程退出码语义由 main 决定(Err -> 非零)。
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let registry = Registry::build_default();
    match cli.command {
        Command::Generate(args) => cmd_generate(&registry, args, cli.json).await,
        Command::Status(args) => cmd_status(&registry, args, cli.json).await,
        Command::Download(args) => cmd_download(args, cli.json).await,
        Command::List(args) => cmd_list(args, cli.json),
        Command::Providers => cmd_providers(&registry, cli.json),
        Command::Models(args) => cmd_models(&registry, args, cli.json),
        Command::Model(args) => cmd_model(&registry, args, cli.json),
        // mcp 子命令仅注册 + 转发: 实际 server 实现全在 crate::mcp, cli 这里不掺逻辑。
        Command::Mcp => crate::mcp::serve().await,
    }
}

/// 打开默认(或 IMAGECLI_DB_PATH 指定的)任务 store。各命令公用。
fn open_store() -> anyhow::Result<JobStore> {
    JobStore::open()
}

/// 把 key=value 字符串解析进 params map。value 先试 JSON, 失败当字符串。
fn parse_params(raw: &[String]) -> anyhow::Result<serde_json::Map<String, Value>> {
    let mut map = serde_json::Map::new();
    for item in raw {
        // 按第一个 '=' 切分, value 里允许再含 '='
        let pos = match item.find('=') {
            Some(p) => p,
            None => anyhow::bail!("参数格式应为 key=value, 收到: {}", item),
        };
        let key = item[..pos].trim().to_string();
        let val_str = &item[pos + 1..];
        if key.is_empty() {
            anyhow::bail!("参数 key 不能为空: {}", item);
        }
        // 先尝试解析成 JSON(数字/布尔/对象/数组), 失败则按裸字符串
        let value = match serde_json::from_str::<Value>(val_str) {
            Ok(v) => v,
            Err(_) => Value::String(val_str.to_string()),
        };
        map.insert(key, value);
    }
    Ok(map)
}

/// 解析"本次生效的 provider"(纯函数, 便于离线单测回退链)。
///
/// 回退链(D-011): CLI flag > 配置文件默认 > 内置默认(agnes)。
/// 内置默认选 agnes 而非 fal: fal 需付费 key, 新用户默认会失败; agnes 免费常可用。
fn resolve_effective_provider(flag: Option<&str>, cfg_default: Option<&str>) -> String {
    if let Some(p) = flag {
        return p.to_string();
    }
    if let Some(p) = cfg_default {
        return p.to_string();
    }
    BUILTIN_DEFAULT_PROVIDER.to_string()
}

/// 解析"本次生效的 model"(纯函数, 便于离线单测)。
///
/// 优先级: CLI --model > 配置默认 model(仅当配置默认 provider == 生效 provider 时才用,
/// 避免把别的 provider 的默认 model 误套到当前 provider)> None(交给 default_model_for 兜底)。
fn resolve_effective_model(
    effective_provider: &str,
    flag_model: Option<&str>,
    cfg_default_provider: Option<&str>,
    cfg_default_model: Option<&str>,
) -> Option<String> {
    if let Some(m) = flag_model {
        return Some(m.to_string());
    }
    // 配置里的默认 model 必须与生效 provider 同属, 才可复用
    if let (Some(cp), Some(cm)) = (cfg_default_provider, cfg_default_model) {
        if cp == effective_provider {
            return Some(cm.to_string());
        }
    }
    None
}

/// 按能力取默认 model。各 provider 按其声明的能力给默认 model
/// (fal/replicate 除 text2image 外还有 text2video/image2video 默认视频 endpoint)。
fn default_model_for(provider: &str, capability: Capability) -> Option<String> {
    match provider {
        "fal" => match capability {
            Capability::Text2Image => Some(fal::DEFAULT_T2I_MODEL.to_string()),
            // fal 视频仍走同一 Queue API, 只是 endpoint/产物不同(D-011 复用现有 provider 加 video)。
            Capability::Text2Video => Some(fal::DEFAULT_T2V_MODEL.to_string()),
            Capability::Image2Video => Some(fal::DEFAULT_I2V_MODEL.to_string()),
            // 超分: clarity-upscaler, 仍走 Queue API, 输入 image_url、产物 image.url。
            Capability::Upscale => Some(fal::DEFAULT_UPSCALE_MODEL.to_string()),
            _ => None,
        },
        "google" => match capability {
            Capability::Text2Image => Some(google::DEFAULT_T2I_MODEL.to_string()),
            // Gemini 图像编辑复用同一 model(Nano Banana 同端点吃 inline_data 输入图)。
            Capability::Image2Image => Some(google::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        "agnes" => match capability {
            Capability::Text2Image => Some(agnes::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        // D-010/D-012: 火山 Seedream 4.0 同端点支持 t2i 与 i2i(同一 model)。
        "volcengine" => match capability {
            Capability::Text2Image => Some(volcengine::DEFAULT_T2I_MODEL.to_string()),
            Capability::Image2Image => Some(volcengine::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        "stepfun" => match capability {
            Capability::Text2Image => Some(stepfun::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        "zhipu" => match capability {
            Capability::Text2Image => Some(zhipu::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        "ppio" => match capability {
            Capability::Text2Image => Some(ppio::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        "siliconflow" => match capability {
            Capability::Text2Image => Some(siliconflow::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        // D-011 海外: OpenAI 官方(gpt-image-1) + Replicate(flux-schnell), 均仅声明 text2image
        "openai" => match capability {
            Capability::Text2Image => Some(openai::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        "replicate" => match capability {
            Capability::Text2Image => Some(replicate::DEFAULT_T2I_MODEL.to_string()),
            // Replicate 视频仍走 prediction 异步, 只是 model/产物不同(D-011 复用现有 provider 加 video)。
            Capability::Text2Video => Some(replicate::DEFAULT_T2V_MODEL.to_string()),
            Capability::Image2Video => Some(replicate::DEFAULT_I2V_MODEL.to_string()),
            // 超分: real-esrgan, 仍走 prediction 异步, 输入 image、output 是更高清图 url。
            Capability::Upscale => Some(replicate::DEFAULT_UPSCALE_MODEL.to_string()),
            _ => None,
        },
        // D-014 视频: 火山方舟 Seedance, 文生视频/图生视频各有默认 model。
        "seedance" => match capability {
            Capability::Text2Video => Some(seedance::DEFAULT_T2V_MODEL.to_string()),
            Capability::Image2Video => Some(seedance::DEFAULT_I2V_MODEL.to_string()),
            _ => None,
        },
        // D-014 视频: 可灵 Kling, 文生视频/图生视频各有默认 model_name。
        "kling" => match capability {
            Capability::Text2Video => Some(kling::DEFAULT_T2V_MODEL.to_string()),
            Capability::Image2Video => Some(kling::DEFAULT_I2V_MODEL.to_string()),
            _ => None,
        },
        // D-014 图像: 即梦 visual, 文生图 + 图生图(4.0 同一 req_key 既能 t2i 也能 i2i)。
        "jimeng" => match capability {
            Capability::Text2Image => Some(jimeng::DEFAULT_T2I_MODEL.to_string()),
            Capability::Image2Image => Some(jimeng::DEFAULT_T2I_MODEL.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// 校验所选 provider 是否声明支持目标能力(纯函数, 便于离线单测)。
///
/// 修复 B 早先发现的"CLI help 列了全部能力但 provider 不真支持"的误导:
/// 对 provider 不支持的能力, 给清晰中文错误(列出它真正支持的能力 + 引导 `imagecli providers`),
/// 而不是放任后续 default_model_for 报出含糊的"无默认 model", 让用户误以为只是少给了 --model。
fn ensure_capability_supported(
    provider_name: &str,
    supported: &[Capability],
    capability: Capability,
) -> anyhow::Result<()> {
    if supported.contains(&capability) {
        return Ok(());
    }
    let caps: Vec<&str> = supported.iter().map(|c| c.as_str()).collect();
    anyhow::bail!(
        "provider {} 不支持能力 {}。它支持: {}。请改用支持该能力的 provider(`imagecli providers` 查看各家能力)。",
        provider_name,
        capability.as_str(),
        caps.join(", ")
    )
}

/// 解析 prompts-file 文本内容为 prompt 列表(纯函数, 便于离线单测)。
///
/// 规则: 一行一个 prompt; 去除首尾空白后, 空行与以 '#' 开头的注释行忽略。
/// 故意只看"trim 后是否以 # 开头"判注释, 不做行内注释切分(prompt 里可能合法含 #)。
fn parse_prompts_content(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

/// 读取并解析 prompts-file。读不到文件给中文报错, 不 panic。
fn parse_prompts_file(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 prompts-file 失败 {}: {}", path.display(), e))?;
    Ok(parse_prompts_content(&content))
}

/// 判别一个 --input 值是否为远程 URL(http/https 开头, 大小写不敏感)。
/// 纯函数, 便于离线单测本地路径 vs URL 的判别边界。
fn is_remote_url(s: &str) -> bool {
    let lowered = s.to_ascii_lowercase();
    if lowered.starts_with("http://") {
        return true;
    }
    if lowered.starts_with("https://") {
        return true;
    }
    false
}

/// 由本地文件扩展名推断 MIME(供本地图 base64 内联时标注; data URI / 落盘扩展名都依赖它)。
/// 未知扩展名回退 image/png(多数 i2i 接口接受 png)。纯函数, 便于离线单测。
fn mime_from_path(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}

/// 把一个 --input 值加载成输入素材。
///
/// 判别本地路径 vs URL:
/// - http(s) URL: 维持现状, 构造 Asset::from_url 直接透传(各家用 image_url / image 字段);
/// - 本地文件: 读取原始字节 + 按扩展名推断 mime, 存为内联字节素材(Asset::from_inline_bytes)。
///   各 provider 的 build_body 经 Asset::as_input_image 把它 base64 编码塞自家 i2i 喂图字段。
///   这样 build_body 保持纯函数(不在其中做文件 IO), 文件读取集中在 CLI 加载阶段。
///
/// 既不是 http(s) URL、本地又不存在该文件时, 给清晰中文报错(让用户改 URL 或检查路径)。
fn load_input_asset(raw: &str) -> anyhow::Result<Asset> {
    if is_remote_url(raw) {
        return Ok(Asset::from_url(AssetKind::Image, raw.to_string()));
    }
    let path = std::path::Path::new(raw);
    if !path.exists() {
        anyhow::bail!(
            "输入素材既不是 http(s) URL, 本地也不存在该文件: {} (请检查路径, 或改用 URL)",
            raw
        );
    }
    if !path.is_file() {
        anyhow::bail!("输入素材路径存在但不是文件(可能是目录): {}", raw);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("读取本地输入图失败 {}: {}", raw, e))?;
    let mime = mime_from_path(path);
    Ok(Asset::from_inline_bytes(AssetKind::Image, mime, bytes))
}

/// 由一批 prompt fan-out 成等量 GenRequest(纯函数, 便于离线单测请求构造)。
///
/// prompts 为空时退回单个 prompt=None 的请求, 兼容"只靠输入素材(图生图/图生视频)
/// 不带文本"的旧用法。否则每个 prompt 一个请求, capability/model/inputs/params 共享。
fn build_requests(
    capability: Capability,
    model: &str,
    prompts: &[String],
    inputs: &[Asset],
    params: &serde_json::Map<String, Value>,
) -> Vec<GenRequest> {
    let mut requests = Vec::new();
    if prompts.is_empty() {
        requests.push(GenRequest {
            capability,
            model: model.to_string(),
            prompt: None,
            inputs: inputs.to_vec(),
            params: params.clone(),
        });
        return requests;
    }
    for p in prompts.iter() {
        requests.push(GenRequest {
            capability,
            model: model.to_string(),
            prompt: Some(p.clone()),
            inputs: inputs.to_vec(),
            params: params.clone(),
        });
    }
    requests
}

/// 由一批 prompt fan-out 成等量 RequestTemplate(路由层的 fan-out 单位, 纯函数便于单测)。
///
/// 与 build_requests 同构, 但不含 model: model 由候选链注入(不同 provider 默认 model 不同)。
/// prompts 为空时退回单个 prompt=None 模板, 兼容"纯输入素材(图生图)"用法。
fn build_templates(
    capability: Capability,
    prompts: &[String],
    inputs: &[Asset],
    params: &serde_json::Map<String, Value>,
) -> Vec<RequestTemplate> {
    let mut out = Vec::new();
    if prompts.is_empty() {
        out.push(RequestTemplate {
            capability,
            prompt: None,
            inputs: inputs.to_vec(),
            params: params.clone(),
        });
        return out;
    }
    for p in prompts.iter() {
        out.push(RequestTemplate {
            capability,
            prompt: Some(p.clone()),
            inputs: inputs.to_vec(),
            params: params.clone(),
        });
    }
    out
}

/// 为某个候选 provider 解析它该用的 model(纯函数)。
///
/// 不接受 CLI `--model`(那是给主 provider 的, 套到备用家会无效): 只用 "配置默认 model
/// (当且仅当配置默认 provider 就是这家)" 或 "按能力的内置默认 model"。取不到返回 None。
fn resolve_model_for_provider(
    provider_name: &str,
    capability: Capability,
    cfg_default_provider: Option<&str>,
    cfg_default_model: Option<&str>,
) -> Option<String> {
    if let Some(m) =
        resolve_effective_model(provider_name, None, cfg_default_provider, cfg_default_model)
    {
        return Some(m);
    }
    default_model_for(provider_name, capability)
}

/// 候选被跳过的原因(纯枚举, 便于给中文说明与单测)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackSkip {
    /// provider 未注册。
    UnknownProvider,
    /// provider 不支持该能力。
    Unsupported,
    /// provider 该能力无可用默认 model。
    NoModel,
}

impl FallbackSkip {
    fn reason_cn(&self) -> &'static str {
        match self {
            FallbackSkip::UnknownProvider => "未注册的 provider",
            FallbackSkip::Unsupported => "不支持该能力",
            FallbackSkip::NoModel => "该能力无可用默认 model",
        }
    }
}

/// 判一个 fallback 候选能否加入候选链(纯函数, 便于离线单测跳过逻辑)。
///
/// 三关: 存在 -> 支持目标能力 -> 有可用 model。任一不过给出跳过原因(由调用方记 note 并说明)。
fn classify_fallback(
    exists: bool,
    supported: Option<&[Capability]>,
    capability: Capability,
    model: &Option<String>,
) -> Result<(), FallbackSkip> {
    if !exists {
        return Err(FallbackSkip::UnknownProvider);
    }
    match supported {
        Some(caps) if caps.contains(&capability) => {}
        _ => return Err(FallbackSkip::Unsupported),
    }
    if model.is_none() {
        return Err(FallbackSkip::NoModel);
    }
    Ok(())
}

/// generate 实现。
async fn cmd_generate(registry: &Registry, args: GenerateArgs, as_json: bool) -> anyhow::Result<()> {
    // 读持久化默认配置(缺文件视为无默认, 不报错)。
    let settings = Settings::load()?;

    // 解析生效 provider: CLI flag > 配置默认 > 内置默认(agnes)。
    let provider_name = resolve_effective_provider(
        args.provider.as_deref(),
        settings.default_provider.as_deref(),
    );

    // 取 provider
    let provider = match registry.get(&provider_name) {
        Some(p) => p,
        None => anyhow::bail!(
            "未知 provider: {} (已注册: {})",
            provider_name,
            registry.list_names().join(", ")
        ),
    };

    // 解析能力
    let capability = Capability::parse(&args.capability)?;

    // 能力支持校验(打通 video capability 的关键): provider 必须真声明支持该能力,
    // 否则给清晰中文错误而非误导(修复 help 列了能力但 provider 不支持的问题)。
    ensure_capability_supported(&provider_name, provider.capabilities(), capability)?;

    // 确定 model: CLI --model > 配置默认 model(同 provider 时)> 按能力取内置默认。
    let model = match resolve_effective_model(
        &provider_name,
        args.model.as_deref(),
        settings.default_provider.as_deref(),
        settings.default_model.as_deref(),
    ) {
        Some(m) => m,
        None => match default_model_for(&provider_name, capability) {
            Some(m) => m,
            None => anyhow::bail!(
                "未指定 model 且 {}/{} 无默认 model, 请用 --model 指定",
                provider_name,
                capability.as_str()
            ),
        },
    };

    // 输入素材: 判别本地路径 vs URL。URL 直接透传; 本地文件读取字节存为内联素材,
    // 供各 provider 的 build_body base64 编码塞进自家 i2i 喂图字段(详见 load_input_asset)。
    let mut inputs = Vec::new();
    for raw in args.inputs.iter() {
        inputs.push(load_input_asset(raw)?);
    }

    // 自由参数
    let params = parse_params(&args.params)?;

    // 收集这批 prompt: 先文件后命令行, 全部合并(可叠加)。
    let mut prompts: Vec<String> = Vec::new();
    if let Some(file) = &args.prompts_file {
        prompts.extend(parse_prompts_file(file)?);
    }
    prompts.extend(args.prompts.iter().cloned());

    // fan-out 成等量 RequestTemplate(prompts 为空则退回单个无 prompt 模板, 兼容图生图旧用法)。
    // 路由层会让每个模板各自独立走候选链(主 + fallback), 与批量 fan-out 正交。
    let templates = build_templates(capability, &prompts, &inputs, &params);
    let task_count = templates.len();

    // 预算护栏(D-006, Decimal): 估算本批总成本。单价见 pricing 表(多为粗估占位)。
    let estimated_cost = pricing::estimate_total(&provider_name, &model, capability, task_count);

    // dry-run: 只估不跑, 不打开 store、不调用任何 provider, 直接退出 0。
    if args.dry_run {
        if as_json {
            print_json(&json!({
                "dry_run": true,
                "provider": provider_name,
                "model": model,
                "task_count": task_count,
                "estimated_cost": estimated_cost.to_string(),
                "prompts": prompts,
            }));
        } else {
            println!(
                "[dry-run] 本次将提交 {} 个任务, 预估成本合计 {} USD (provider={}, model={})",
                task_count, estimated_cost, provider_name, model
            );
        }
        return Ok(());
    }

    // --max-cost: 预估超过上限则拒绝执行(非零退出), 在 submit/开库之前短路, 不留脏记录。
    if let Some(max) = args.max_cost {
        if estimated_cost > max {
            anyhow::bail!(
                "预估总成本 {} USD 超过 --max-cost {} 上限, 已拒绝执行(共 {} 个任务, provider={})。\
                 如确需执行请调高 --max-cost 或减少任务数; 用 --dry-run 可先预览成本。",
                estimated_cost,
                max,
                task_count,
                provider_name
            );
        }
    }

    // ---------- 构建候选 provider 链: 主(已校验) + 各 fallback(逐家过"存在/支持/有 model")----------
    // primary 已在上方做过 ensure_capability_supported 与 model 解析, 直接作为链首。
    let mut chain: Vec<Candidate> = vec![Candidate {
        name: provider_name.clone(),
        provider: Arc::clone(&provider),
        model: model.clone(),
    }];
    // 记录被跳过的 fallback(原因)+ 实际生效的候选链名(用于 --json 与人类提示)。
    let mut skipped: Vec<Value> = Vec::new();
    // 去重: 主已在链中; 重复的 fallback(或与主同名)静默跳过。
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(provider_name.clone());
    for raw in args.fallback.iter() {
        let fname = raw.trim().to_string();
        if fname.is_empty() {
            continue;
        }
        if !seen.insert(fname.clone()) {
            continue;
        }
        let fprovider = registry.get(&fname);
        let supported = fprovider.as_ref().map(|p| p.capabilities());
        let fmodel = resolve_model_for_provider(
            &fname,
            capability,
            settings.default_provider.as_deref(),
            settings.default_model.as_deref(),
        );
        match classify_fallback(fprovider.is_some(), supported, capability, &fmodel) {
            Ok(()) => {
                // 上面四关都过, fprovider/fmodel 必为 Some。
                chain.push(Candidate {
                    name: fname.clone(),
                    provider: fprovider.expect("classify 已确认存在"),
                    model: fmodel.expect("classify 已确认有 model"),
                });
            }
            Err(reason) => {
                let note = format!("跳过 fallback {}: {}", fname, reason.reason_cn());
                if args.verbose {
                    eprintln!("[fallback] {}", note);
                }
                skipped.push(json!({ "provider": fname, "reason": reason.reason_cn() }));
            }
        }
    }

    // verbose: 打印生效候选链(stderr, 不污染 --json stdout)。
    if args.verbose {
        let chain_names: Vec<&str> = chain.iter().map(|c| c.name.as_str()).collect();
        eprintln!("[chain] 候选链: {}", chain_names.join(" -> "));
    }

    // 打开任务 store(跨进程持久化句柄与状态, D-007)
    let store = Arc::new(open_store()?);

    // 跑路由编排: 每个模板各自走候选链(主失败 -> 退避重试 -> 切 fallback), 有界并发。
    let route_cfg = RouteConfig {
        run: RunConfig {
            concurrency: args.concurrency,
            ..RunConfig::default()
        },
        retries: args.retries,
    };
    let outcomes =
        route::run_batch_routed(chain, Arc::clone(&store), templates, route_cfg).await;

    // 处理结果(outcomes 与输入模板等长同序)
    let mut report_items = Vec::new();
    let mut had_error = false;
    let mut success_count: usize = 0;
    let mut fail_count: usize = 0;

    // 下载用的 HTTP 客户端
    let dl_client = reqwest::Client::new();

    for outcome in outcomes.into_iter() {
        // 完整解构, 既能 move 出 result, 又能用其余可观测性字段。
        let route::UnitOutcome {
            result,
            provider_used,
            model_used,
            attempts,
            elapsed_ms,
            fallback_from,
            events,
            prompt,
            quota_hint,
        } = outcome;
        let prompt_ref = prompt.as_deref();

        // verbose: 把本单元的尝试事件逐条打到 stderr(含 request_id/重试/切换), 不进 --json stdout。
        if args.verbose {
            for ev in events.iter() {
                eprintln!("{}", route::format_event(ev));
            }
        }
        // 发生过 fallback 切换: 人类输出提示"主 X 失败, 已用备 Y"(stderr, 避免污染 json)。
        if !fallback_from.is_empty() {
            if let Some(used) = &provider_used {
                eprintln!(
                    "[fallback] 主 {} 失败, 已切换到 {}",
                    fallback_from.join("/"),
                    used
                );
            }
        }
        // 配额/限流耗尽: 给针对性中文建议(切 fallback 或稍后重试)。
        if quota_hint {
            eprintln!(
                "[提示] 检测到疑似配额/限流耗尽(如免费额度满 / 可灵 1303 并发超限): \
                 建议稍后重试, 或用 --fallback 指定备用 provider。"
            );
        }

        match result {
            Ok(job) => {
                // 退出码契约(D-006): 任务最终为 Failed 也必须计入失败, 使进程返回非零。
                if job.status == JobStatus::Failed {
                    had_error = true;
                    fail_count += 1;
                } else {
                    success_count += 1;
                }
                // 默认下载产物
                let mut saved_paths: Vec<String> = Vec::new();
                if !args.no_download && !job.outputs.is_empty() {
                    match download_job_outputs(&dl_client, &job, &args.out_dir).await {
                        Ok(paths) => {
                            for p in paths {
                                saved_paths.push(p.to_string_lossy().to_string());
                            }
                        }
                        Err(e) => {
                            had_error = true;
                            saved_paths.push(format!("下载失败: {}", e));
                        }
                    }
                }
                let mut item = job_to_json(&job, &saved_paths, prompt_ref);
                augment_route_fields(
                    &mut item,
                    provider_used.as_deref(),
                    model_used.as_deref(),
                    attempts,
                    elapsed_ms,
                    &fallback_from,
                );
                report_items.push(item);
            }
            Err(e) => {
                had_error = true;
                fail_count += 1;
                let mut item = json!({
                    "prompt": prompt_ref,
                    "job_id": Value::Null,
                    "status": "failed",
                    "saved": Vec::<String>::new(),
                    "error": e.to_string(),
                });
                augment_route_fields(
                    &mut item,
                    provider_used.as_deref(),
                    model_used.as_deref(),
                    attempts,
                    elapsed_ms,
                    &fallback_from,
                );
                report_items.push(item);
            }
        }
    }

    // 输出
    if as_json {
        let out = json!({
            "results": report_items,
            "task_count": task_count,
            "succeeded": success_count,
            "failed": fail_count,
            "estimated_cost": estimated_cost.to_string(),
            "skipped_fallbacks": skipped,
        });
        print_json(&out);
    } else {
        for item in report_items.iter() {
            print_generate_human(item);
        }
        // 人类可读汇总: 成功/失败计数 + 预估成本。
        println!(
            "汇总: 成功 {} / 失败 {} / 共 {} 个任务; 预估成本合计 {} USD",
            success_count, fail_count, task_count, estimated_cost
        );
    }

    if had_error {
        anyhow::bail!("部分或全部任务失败, 详见上方输出");
    }
    Ok(())
}

/// 给一条 generate 结果 JSON 补充路由可观测性字段(provider_used/attempts/elapsed_ms/fallback_from)。
///
/// provider_used: 实际产出结果(或最后尝试)的 provider; fallback_from: 切换前已失败的 provider 列表
/// (空表示没发生切换)。这些是稳定的 --json 契约新增字段, 既有字段不动。
fn augment_route_fields(
    item: &mut Value,
    provider_used: Option<&str>,
    model_used: Option<&str>,
    attempts: u32,
    elapsed_ms: u128,
    fallback_from: &[String],
) {
    if let Some(obj) = item.as_object_mut() {
        obj.insert("provider_used".to_string(), json!(provider_used));
        obj.insert("model_used".to_string(), json!(model_used));
        obj.insert("attempts".to_string(), json!(attempts));
        // elapsed_ms 可能超过 u64? 实践不会(单任务毫秒级), 但 u128 序列化为数字更稳妥。
        obj.insert("elapsed_ms".to_string(), json!(elapsed_ms as u64));
        obj.insert("fallback_from".to_string(), json!(fallback_from));
    }
}

/// status 实现: 从 store 读出任务, 向 provider 刷新一次, 写回 store。跨进程可用。
async fn cmd_status(registry: &Registry, args: StatusArgs, as_json: bool) -> anyhow::Result<()> {
    let store = open_store()?;

    // 从 store 取记录。取不到说明本机从未提交过该任务(或用错了 db 路径)。
    let rec = match store.get(&args.job_id)? {
        Some(r) => r,
        None => anyhow::bail!(
            "未在本地 store 找到任务 {} (它可能在别的机器/别的 IMAGECLI_DB_PATH 下提交)",
            args.job_id
        ),
    };

    // 用记录里登记的 provider(而非命令行猜测)取 provider 实现。
    let provider = match registry.get(&rec.provider) {
        Some(p) => p,
        None => anyhow::bail!("任务 {} 的 provider {} 未注册", args.job_id, rec.provider),
    };

    // 还原成带句柄的运行视角 Job
    let mut job = rec.to_job();

    // 非终态才向 provider 刷新; 终态直接用 store 里的快照(省一次网络)。
    if !job.status.is_terminal() {
        let polled = provider.poll(&job).await?;
        // 用刷新后的 Job 重建记录写回(保留原 capability/model/created_at, 更新 raw_meta/状态/产物)。
        let updated = JobRecord::from_job(
            &polled,
            Capability::parse(&rec.capability)?,
            &rec.model,
            rec.request_json.clone(),
            rec.created_at,
            now_unix(),
        );
        store.save(&updated)?;
        job = polled;
    }

    if as_json {
        print_json(&job_to_json(&job, &[], None));
    } else {
        println!("任务 {} 状态: {}", job.id, job.status.as_str());
        if let Some(err) = &job.error {
            println!("错误: {}", err);
        }
        for (i, asset) in job.outputs.iter().enumerate() {
            if let Some(url) = &asset.url {
                println!("  产物 #{}: {}", i, url);
            }
        }
    }
    Ok(())
}

/// download 实现: 给了 --url 就直接下这些 URL; 否则从 store 读出已成功任务的产物落盘。
async fn cmd_download(args: DownloadArgs, as_json: bool) -> anyhow::Result<()> {
    // 组装一个待下载的 Job: 要么来自显式 URL, 要么来自 store。
    let job = if !args.urls.is_empty() {
        // 直接 URL 模式: 拼临时 Job 复用 download_job_outputs 的命名/落盘逻辑
        let outputs: Vec<Asset> = args
            .urls
            .iter()
            .map(|u| Asset::from_url(AssetKind::Image, u.clone()))
            .collect();
        Job {
            id: args.job_id.clone(),
            provider: "download".to_string(),
            status: crate::core::provider::JobStatus::Succeeded,
            outputs,
            error: None,
            raw_meta: Value::Null,
        }
    } else {
        // store 模式: 按 job_id 取记录, 必须是已成功且有产物。
        let store = open_store()?;
        let rec = match store.get(&args.job_id)? {
            Some(r) => r,
            None => anyhow::bail!(
                "未在本地 store 找到任务 {}; 或用 --url 直接指定要下载的链接",
                args.job_id
            ),
        };
        let job = rec.to_job();
        if job.status != crate::core::provider::JobStatus::Succeeded {
            anyhow::bail!(
                "任务 {} 当前状态为 {}, 尚未成功, 无产物可下载(可先 `imagecli status {}` 刷新)",
                args.job_id,
                job.status.as_str(),
                args.job_id
            );
        }
        if job.outputs.is_empty() {
            anyhow::bail!("任务 {} 已成功但无产物可下载", args.job_id);
        }
        job
    };

    let client = reqwest::Client::new();
    let paths = download_job_outputs(&client, &job, &args.out_dir).await?;
    let path_strs: Vec<String> = paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
    if as_json {
        print_json(&json!({ "saved": path_strs }));
    } else {
        for p in path_strs.iter() {
            println!("已保存: {}", p);
        }
    }
    Ok(())
}

/// list 实现: 按过滤条件列出 store 里的任务。过滤在 SQL 完成。
fn cmd_list(args: ListArgs, as_json: bool) -> anyhow::Result<()> {
    let store = open_store()?;
    let filter = JobFilter {
        status: args.status,
        capability: args.capability,
        limit: args.limit,
        offset: args.offset,
    };
    let records = store.list(&filter)?;

    if as_json {
        let arr: Vec<Value> = records.iter().map(record_to_json).collect();
        print_json(&json!({ "jobs": arr }));
    } else {
        if records.is_empty() {
            println!("(store 中无匹配任务)");
        }
        for r in records.iter() {
            println!(
                "{:<24} {:<10} {:<12} {:<10} {}",
                r.job_id,
                r.provider,
                r.capability,
                r.status,
                r.model
            );
        }
    }
    Ok(())
}

/// 把一条 JobRecord 序列化成稳定 JSON(list --json 用)。
fn record_to_json(rec: &JobRecord) -> Value {
    json!({
        "job_id": rec.job_id,
        "provider": rec.provider,
        "model": rec.model,
        "capability": rec.capability,
        "status": rec.status,
        "error": rec.error,
        "created_at": rec.created_at,
        "updated_at": rec.updated_at,
    })
}

/// providers 实现: 列出注册表里的 provider 及能力。
fn cmd_providers(registry: &Registry, as_json: bool) -> anyhow::Result<()> {
    let names = registry.list_names();
    if as_json {
        let mut arr = Vec::new();
        for name in names.iter() {
            if let Some(p) = registry.get(name) {
                let caps: Vec<&str> = p.capabilities().iter().map(|c| c.as_str()).collect();
                arr.push(json!({ "name": name, "capabilities": caps }));
            }
        }
        print_json(&json!({ "providers": arr }));
    } else {
        if names.is_empty() {
            println!("(无已注册 provider)");
        }
        for name in names.iter() {
            if let Some(p) = registry.get(name) {
                let caps: Vec<&str> = p.capabilities().iter().map(|c| c.as_str()).collect();
                println!("{:<10} 能力: {}", name, caps.join(", "));
            }
        }
    }
    Ok(())
}

/// models 实现: 复用统一目录(catalog), 按 provider 过滤。
///
/// 与 `model`(单数, /model 式选择器)的分工:
/// - `models --provider X`: 只读, 列出单个 provider 的模型(本函数)。
/// - `model [<provider/model>]`: 跨 provider 选择 + 设默认并持久化(cmd_model)。
///
/// 两者共用同一目录来源(Provider::catalog), 模型声明不重复造。
fn cmd_models(registry: &Registry, args: ModelsArgs, as_json: bool) -> anyhow::Result<()> {
    // 聚合全目录后按 provider 过滤(available 已按各 provider 有无 key 填好)。
    let all = catalog::build_catalog(registry);
    let filtered: Vec<&ModelEntry> = all.iter().filter(|e| e.provider == args.provider).collect();

    if as_json {
        // 复用 catalog_to_json 的稳定契约; 这里对过滤后的子集渲染。
        let owned: Vec<ModelEntry> = filtered.iter().map(|e| (*e).clone()).collect();
        let mut v = catalog::catalog_to_json(&owned);
        // 附带 provider 字段, 兼容旧 `models --json` 输出形态(顶层带 provider)。
        if let Some(obj) = v.as_object_mut() {
            obj.insert("provider".to_string(), json!(args.provider));
        }
        print_json(&v);
    } else {
        if filtered.is_empty() {
            println!("provider {} 暂无内置 model 清单", args.provider);
        }
        for e in filtered.iter() {
            println!("{}", catalog::format_entry_label(e));
        }
    }
    Ok(())
}

/// model 实现(/model 式选择器, D-011)。三模式:
/// 1. 带 selector(`<provider/model>` 或 alias): 非交互校验存在性 -> 设默认 -> 持久化。
/// 2. 无 selector + (无 TTY 或 --list): 打印目录 + 设置提示(--json 输出目录)。无 TTY 降级。
/// 3. 无 selector + TTY + 非 --list: dialoguer 交互菜单, 选中即设默认并持久化。
fn cmd_model(registry: &Registry, args: ModelArgs, as_json: bool) -> anyhow::Result<()> {
    let entries = catalog::build_catalog(registry);

    // 模式 1: 带参直设
    if let Some(sel) = args.selector.as_deref() {
        let entry = match catalog::resolve_selection(&entries, sel) {
            Some(e) => e.clone(),
            None => anyhow::bail!(
                "未找到匹配的模型: {} (可用 `imagecli model --list` 查看目录; 形如 provider/model 或 alias)",
                sel
            ),
        };
        return set_default_and_report(&entry, as_json);
    }

    // 模式 2: 无 TTY 或 --list -> 只打印目录(降级)
    let is_tty = std::io::stdout().is_terminal();
    if args.list || !is_tty {
        if as_json {
            print_json(&catalog::catalog_to_json(&entries));
        } else {
            print_catalog_human(&entries);
            println!();
            println!("用 `imagecli model <provider/model>` 或 `imagecli model <alias>` 设置默认模型。");
        }
        return Ok(());
    }

    // 模式 3: 交互菜单(仅 TTY)
    let selected = match interactive_pick(&entries)? {
        Some(e) => e,
        None => {
            // 用户取消(Esc): 不改默认, 友好提示。
            println!("已取消, 默认模型未改变。");
            return Ok(());
        }
    };
    set_default_and_report(&selected, as_json)
}

/// 把选中的条目写入持久化默认配置并打印确认(set 路径共用)。
fn set_default_and_report(entry: &ModelEntry, as_json: bool) -> anyhow::Result<()> {
    let settings = Settings {
        default_provider: Some(entry.provider.clone()),
        default_model: Some(entry.model_id.clone()),
    };
    settings.save()?;
    let path = Settings::resolve_path()?;
    if as_json {
        print_json(&json!({
            "set_default": true,
            "provider": entry.provider,
            "model": entry.model_id,
            "qualified": entry.qualified(),
            "available": entry.available,
            "config_path": path.to_string_lossy(),
        }));
    } else {
        println!(
            "已设默认模型: {}{}",
            entry.qualified(),
            match entry.available {
                true => String::new(),
                false => "  (注意: 当前缺 key, generate 时需先配置该 provider 的 API key)".to_string(),
            }
        );
        println!("已写入配置: {}", path.display());
    }
    Ok(())
}

/// 人类可读地打印整张目录(无 TTY 降级与 --list 共用)。
fn print_catalog_human(entries: &[ModelEntry]) {
    if entries.is_empty() {
        println!("(目录为空: 无已注册 provider)");
        return;
    }
    // 按 provider 分组打印(目录已按 provider 名排序聚合)。
    let mut last_provider = String::new();
    for e in entries.iter() {
        if e.provider != last_provider {
            println!("[{}]", e.provider);
            last_provider = e.provider.clone();
        }
        println!("  {}", catalog::format_entry_label(e));
    }
}

/// 交互式选择器(仅在 TTY 下调用, 离线测试不覆盖此路径)。
///
/// 返回 Ok(Some(entry)) 表示选中; Ok(None) 表示用户取消(Esc)。
/// 用 dialoguer::Select; 标签复用 format_entry_label, 与列表展示一致。
fn interactive_pick(entries: &[ModelEntry]) -> anyhow::Result<Option<ModelEntry>> {
    if entries.is_empty() {
        anyhow::bail!("目录为空, 无可选模型(无已注册 provider)");
    }
    let labels: Vec<String> = entries.iter().map(catalog::format_entry_label).collect();
    let selection = dialoguer::Select::new()
        .with_prompt("选择默认模型(方向键移动, 回车确认, Esc 取消)")
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(|e| anyhow::anyhow!("交互选择失败: {}", e))?;
    match selection {
        Some(idx) => Ok(Some(entries[idx].clone())),
        None => Ok(None),
    }
}

/// 把 Job 序列化成稳定的 JSON 报告项。prompt 为该任务对应的提示词(批量回填, 可为 None)。
fn job_to_json(job: &Job, saved_paths: &[String], prompt: Option<&str>) -> Value {
    let outputs: Vec<Value> = job
        .outputs
        .iter()
        .map(|a| json!({ "kind": format!("{:?}", a.kind), "url": a.url }))
        .collect();
    json!({
        "id": job.id,
        "job_id": job.id,
        "prompt": prompt,
        "provider": job.provider,
        "status": job.status.as_str(),
        "outputs": outputs,
        "saved": saved_paths,
        "error": job.error,
    })
}

/// 人类可读地打印一条 generate 结果。
fn print_generate_human(item: &Value) {
    let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let job_id = item.get("job_id").and_then(|v| v.as_str()).unwrap_or("-");
    if let Some(prompt) = item.get("prompt").and_then(|v| v.as_str()) {
        println!("状态: {} | job_id: {} | prompt: {}", status, job_id, prompt);
    } else {
        println!("状态: {} | job_id: {}", status, job_id);
    }
    if let Some(err) = item.get("error").and_then(|v| v.as_str()) {
        println!("  错误: {}", err);
    }
    if let Some(saved) = item.get("saved").and_then(|v| v.as_array()) {
        for s in saved {
            if let Some(path) = s.as_str() {
                println!("  已保存: {}", path);
            }
        }
    }
}

/// 统一的 JSON 打印(pretty, 末尾换行)。
fn print_json(v: &Value) {
    // serde_json::to_string_pretty 失败的概率极低(仅 IO/递归), 兜底成紧凑输出
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{}", s),
        Err(_) => println!("{}", v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_remote_url_distinguishes_path_from_url() {
        // http/https(大小写不敏感)判为 URL; 其余(本地路径)判为非 URL。
        assert!(is_remote_url("https://x/a.png"));
        assert!(is_remote_url("http://x/a.png"));
        assert!(is_remote_url("HTTPS://X/A.PNG"));
        assert!(!is_remote_url("./a.png"));
        assert!(!is_remote_url("/root/a.png"));
        assert!(!is_remote_url("a.png"));
        // data URI 与 ftp 不当作可下载 URL(本实现只透传 http/https)
        assert!(!is_remote_url("data:image/png;base64,AAAA"));
    }

    #[test]
    fn mime_from_path_maps_known_extensions() {
        use std::path::Path;
        assert_eq!(mime_from_path(Path::new("a.png")), "image/png");
        assert_eq!(mime_from_path(Path::new("a.JPG")), "image/jpeg");
        assert_eq!(mime_from_path(Path::new("a.jpeg")), "image/jpeg");
        assert_eq!(mime_from_path(Path::new("a.webp")), "image/webp");
        assert_eq!(mime_from_path(Path::new("a.gif")), "image/gif");
        // 未知/无扩展名回退 png
        assert_eq!(mime_from_path(Path::new("a.bmpx")), "image/png");
        assert_eq!(mime_from_path(Path::new("noext")), "image/png");
    }

    #[test]
    fn load_input_asset_url_stays_url() {
        // URL 输入: 维持 from_url, url 字段非空、inline 为空。
        let asset = load_input_asset("https://x/in.png").expect("URL 应直接透传");
        assert_eq!(asset.url.as_deref(), Some("https://x/in.png"));
        assert!(asset.inline.is_none());
    }

    #[test]
    fn load_input_asset_local_reads_bytes_into_inline() {
        // 本地文件: 读取字节 -> inline(mime 按扩展名), url 为空。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir()
            .join(format!("imagecli_in_{}_{}.png", std::process::id(), nanos));
        std::fs::write(&path, [1u8, 2, 3, 4]).expect("写测试 png");

        let asset = load_input_asset(path.to_str().unwrap()).expect("本地图应能加载");
        let inline = asset.inline.as_ref().expect("本地图应存为 inline 字节");
        assert_eq!(inline.mime, "image/png");
        assert_eq!(inline.data, vec![1u8, 2, 3, 4]);
        assert!(asset.url.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_input_asset_missing_local_file_errors() {
        // 既非 URL、本地又不存在 -> 报错(给清晰中文提示)。
        let err = load_input_asset("/root/definitely_missing_imagecli_xyz.png").unwrap_err();
        assert!(err.to_string().contains("不存在"));
    }

    #[test]
    fn parse_prompts_content_skips_empty_and_comments() {
        // 空行、纯空白行、# 注释行都应被忽略; 其余按行 trim 后保留。
        let content = "\
a red fox
\x20\x20
# 这是注释, 应忽略
  a blue whale
   # 带前导空白的注释也忽略
last one
";
        let prompts = parse_prompts_content(content);
        assert_eq!(prompts, vec!["a red fox", "a blue whale", "last one"]);
    }

    #[test]
    fn parse_prompts_content_keeps_hash_inside_prompt() {
        // 只把"trim 后以 # 开头"的整行当注释; 行内 # 属于 prompt 内容, 保留。
        let prompts = parse_prompts_content("a cat #1 wearing a hat\n");
        assert_eq!(prompts, vec!["a cat #1 wearing a hat"]);
    }

    #[test]
    fn build_requests_fans_out_one_per_prompt() {
        // 多 prompt -> 等量请求, 顺序一致, capability/model/inputs/params 共享。
        let prompts = vec!["p1".to_string(), "p2".to_string(), "p3".to_string()];
        let inputs = vec![Asset::from_url(AssetKind::Image, "https://x/in.png")];
        let mut params = serde_json::Map::new();
        params.insert("seed".to_string(), json!(7));
        let reqs = build_requests(Capability::Text2Image, "m1", &prompts, &inputs, &params);
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].prompt.as_deref(), Some("p1"));
        assert_eq!(reqs[1].prompt.as_deref(), Some("p2"));
        assert_eq!(reqs[2].prompt.as_deref(), Some("p3"));
        // 每个请求都带上共享的 model/inputs/params。
        for r in reqs.iter() {
            assert_eq!(r.model, "m1");
            assert_eq!(r.inputs.len(), 1);
            assert_eq!(r.params.get("seed"), Some(&json!(7)));
            assert_eq!(r.capability, Capability::Text2Image);
        }
    }

    #[test]
    fn effective_provider_fallback_chain() {
        // CLI flag 最高优先
        assert_eq!(
            resolve_effective_provider(Some("fal"), Some("google")),
            "fal"
        );
        // 无 flag 用配置默认
        assert_eq!(resolve_effective_provider(None, Some("google")), "google");
        // 无 flag 无配置 -> 内置默认 agnes(不是 fal: B 发现 fal 需付费 key 会让新手失败)
        assert_eq!(resolve_effective_provider(None, None), "agnes");
        assert_eq!(resolve_effective_provider(None, None), BUILTIN_DEFAULT_PROVIDER);
    }

    #[test]
    fn effective_model_prefers_flag_then_matching_cfg() {
        // --model 最高优先
        assert_eq!(
            resolve_effective_model("agnes", Some("m-flag"), Some("agnes"), Some("m-cfg")).as_deref(),
            Some("m-flag")
        );
        // 无 --model, 配置默认 provider 与生效 provider 一致 -> 用配置默认 model
        assert_eq!(
            resolve_effective_model("agnes", None, Some("agnes"), Some("m-cfg")).as_deref(),
            Some("m-cfg")
        );
        // 配置默认 provider 与生效 provider 不一致 -> 不套用别家 model(返回 None, 交给按能力兜底)
        assert_eq!(
            resolve_effective_model("fal", None, Some("agnes"), Some("m-cfg")),
            None
        );
        // 完全无配置 -> None
        assert_eq!(resolve_effective_model("fal", None, None, None), None);
    }

    #[test]
    fn capability_check_passes_for_supported_and_errors_for_unsupported() {
        // seedance 真支持视频: text2video / image2video 通过。
        let video_caps = [Capability::Text2Video, Capability::Image2Video];
        assert!(ensure_capability_supported("seedance", &video_caps, Capability::Text2Video).is_ok());
        // 不支持 text2image: 报清晰中文错误, 且列出它真正支持的能力。
        let err = ensure_capability_supported("seedance", &video_caps, Capability::Text2Image)
            .unwrap_err()
            .to_string();
        assert!(err.contains("不支持能力"));
        assert!(err.contains("text2image"));
        assert!(err.contains("text2video"));
        // 反向: 图像 provider 不支持 text2video。
        let img_caps = [Capability::Text2Image];
        let err2 = ensure_capability_supported("fal", &img_caps, Capability::Text2Video)
            .unwrap_err()
            .to_string();
        assert!(err2.contains("不支持能力"));
        assert!(err2.contains("text2video"));
    }

    #[test]
    fn default_model_for_seedance_video_capabilities() {
        // 视频能力路由到 seedance 的默认 model(打通 video capability)。
        assert_eq!(
            default_model_for("seedance", Capability::Text2Video).as_deref(),
            Some(seedance::DEFAULT_T2V_MODEL)
        );
        assert_eq!(
            default_model_for("seedance", Capability::Image2Video).as_deref(),
            Some(seedance::DEFAULT_I2V_MODEL)
        );
        // seedance 不给图像默认 model。
        assert!(default_model_for("seedance", Capability::Text2Image).is_none());
    }

    #[test]
    fn default_model_for_google_t2i_and_i2i_same_model() {
        // Gemini 图像编辑复用同一 model: t2i 与 i2i 都路由到 DEFAULT_T2I_MODEL。
        assert_eq!(
            default_model_for("google", Capability::Text2Image).as_deref(),
            Some(google::DEFAULT_T2I_MODEL)
        );
        assert_eq!(
            default_model_for("google", Capability::Image2Image).as_deref(),
            Some(google::DEFAULT_T2I_MODEL)
        );
        // google 不涉足视频。
        assert!(default_model_for("google", Capability::Text2Video).is_none());
    }

    #[test]
    fn default_model_for_upscale_fal_and_replicate() {
        // 超分能力路由到 fal/replicate 的默认超分 model(打通 upscale capability)。
        assert_eq!(
            default_model_for("fal", Capability::Upscale).as_deref(),
            Some(fal::DEFAULT_UPSCALE_MODEL)
        );
        assert_eq!(
            default_model_for("replicate", Capability::Upscale).as_deref(),
            Some(replicate::DEFAULT_UPSCALE_MODEL)
        );
        // 不支持超分的 provider(如 agnes)无默认超分 model。
        assert!(default_model_for("agnes", Capability::Upscale).is_none());
    }

    #[test]
    fn build_templates_fans_out_one_per_prompt() {
        // 多 prompt -> 等量模板; 无 model 字段(由候选注入)。
        let prompts = vec!["p1".to_string(), "p2".to_string()];
        let inputs = vec![Asset::from_url(AssetKind::Image, "https://x/in.png")];
        let mut params = serde_json::Map::new();
        params.insert("seed".to_string(), json!(7));
        let tmpls = build_templates(Capability::Text2Image, &prompts, &inputs, &params);
        assert_eq!(tmpls.len(), 2);
        assert_eq!(tmpls[0].prompt.as_deref(), Some("p1"));
        assert_eq!(tmpls[1].prompt.as_deref(), Some("p2"));
        assert_eq!(tmpls[0].inputs.len(), 1);
        assert_eq!(tmpls[0].params.get("seed"), Some(&json!(7)));
        // 空 prompts -> 单个无 prompt 模板(兼容图生图)。
        let empty = build_templates(Capability::Image2Image, &[], &inputs, &params);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].prompt.is_none());
    }

    #[test]
    fn classify_fallback_skip_reasons() {
        let model = Some("m".to_string());
        // 未注册 provider。
        assert_eq!(
            classify_fallback(false, None, Capability::Text2Image, &model),
            Err(FallbackSkip::UnknownProvider)
        );
        // 存在但不支持该能力。
        let img_caps = [Capability::Text2Image];
        assert_eq!(
            classify_fallback(true, Some(&img_caps), Capability::Text2Video, &model),
            Err(FallbackSkip::Unsupported)
        );
        // 支持但无可用 model。
        assert_eq!(
            classify_fallback(true, Some(&img_caps), Capability::Text2Image, &None),
            Err(FallbackSkip::NoModel)
        );
        // 三关全过。
        assert_eq!(
            classify_fallback(true, Some(&img_caps), Capability::Text2Image, &model),
            Ok(())
        );
    }

    #[test]
    fn resolve_model_for_provider_ignores_cli_flag_uses_default() {
        // fallback 解析不吃 CLI --model; 用配置(同 provider 时)或按能力默认。
        // 配置默认属于该 provider -> 用配置 model。
        assert_eq!(
            resolve_model_for_provider("agnes", Capability::Text2Image, Some("agnes"), Some("cfg-m"))
                .as_deref(),
            Some("cfg-m")
        );
        // 配置默认属于别家 -> 退到按能力默认(agnes 有 text2image 默认)。
        assert_eq!(
            resolve_model_for_provider("agnes", Capability::Text2Image, Some("fal"), Some("cfg-m"))
                .as_deref(),
            Some(agnes::DEFAULT_T2I_MODEL)
        );
    }

    #[test]
    fn build_requests_empty_prompts_yields_single_promptless_request() {
        // 无 prompt 时退回单个 prompt=None 请求(兼容图生图/图生视频纯输入用法)。
        let inputs = vec![Asset::from_url(AssetKind::Image, "https://x/in.png")];
        let params = serde_json::Map::new();
        let reqs = build_requests(Capability::Image2Image, "m2", &[], &inputs, &params);
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].prompt.is_none());
        assert_eq!(reqs[0].inputs.len(), 1);
    }
}
