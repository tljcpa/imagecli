//! MCP(Model Context Protocol)server: 让 imagecli 能被 Claude Code / Cursor 等 agent
//! 直接当工具调用(落地 D-006 的 agent-first 定位)。
//!
//! 传输与协议: stdio 上的 JSON-RPC 2.0, 消息按"一行一条 JSON"(newline-delimited)分隔
//! —— 这是 MCP stdio transport 的约定(不是 LSP 的 Content-Length 分帧)。实现 MCP 核心
//! 三方法 initialize / tools/list / tools/call, 外加可选 ping; notifications/* 通知不回应。
//!
//! 为什么自实现而不引官方 `rmcp`:
//! - MCP 核心就这几个方法, JSON-RPC over stdio 自实现总量很小、行为完全可控;
//! - rmcp 仍在快速演进、API 不稳, 且会拖入一批宏与传输层依赖; 本项目已有 serde_json/tokio,
//!   自实现零新增 crate, 与"单二进制、依赖面最小"的工程取向一致(对齐 D-001)。
//!
//! 工具 handler 的复用策略(关键: 不重写生成链路):
//! - generate_image / generate_video: 以"自调用子进程"方式跑 `imagecli --json generate ...`,
//!   完整复用 cmd_generate 的路由/故障转移/退避重试/预算护栏/下载链路。子进程有独立 stdout,
//!   天然避免与本 server 占用的 stdio JSON-RPC 通道串台(cmd_generate 内部大量 println! 到 stdout)。
//! - list_providers / list_models / get_job / list_jobs: 只读, 直接调 core(registry / catalog /
//!   JobStore)在内存里拼 JSON, 不打印 stdout, 也不碰网络。
//!
//! 凭证: server 不接收 key 参数, 一律沿用进程环境变量(agent 启动 `imagecli mcp` 时注入 env),
//! 子进程继承之。工具描述里也提醒"key 走环境变量"。

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::core::catalog;
use crate::core::registry::Registry;
use crate::core::store::{JobFilter, JobRecord, JobStore};

/// 本 server 声明的协议版本(客户端未指明时的回退)。
/// MCP 用日期串标识版本; 若客户端在 initialize 里给了 protocolVersion, 我们回显它(向后兼容)。
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// 启动 stdio MCP server: 从 stdin 逐行读 JSON-RPC 请求, 处理后把响应逐行写 stdout。
///
/// 循环到 stdin EOF(管道关闭 / 客户端退出)为止。每条消息独立处理, 顺序串行
/// (MCP 不要求并发; 串行最简单且对本工具的调用频率完全够用)。
pub async fn serve() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        // read_line 读到换行或 EOF。返回 0 字节表示 EOF, 退出循环。
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        // 空行(心跳/对齐)直接跳过, 不当错误。
        if trimmed.is_empty() {
            continue;
        }
        // 处理一条消息; 通知类(无 id)返回 None, 不写任何响应。
        if let Some(resp) = handle_line(trimmed).await {
            let s = serde_json::to_string(&resp)?;
            stdout.write_all(s.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            // 必须 flush: 客户端按行同步等待响应, 不 flush 会死锁。
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// 解析一行文本为 JSON-RPC 消息并分发。解析失败回 -32700(parse error, id=null)。
async fn handle_line(line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => handle_message(v).await,
        Err(e) => Some(err_response(
            Value::Null,
            -32700,
            format!("JSON 解析错误: {}", e),
        )),
    }
}

/// 分发一条已解析的 JSON-RPC 消息。
///
/// 通知(无 `id` 字段)按 JSON-RPC 规范不产生响应: 任何错误也静默(返回 None)。
/// 请求(有 `id`)总产生一个响应(成功 result 或 error)。
async fn handle_message(msg: Value) -> Option<Value> {
    // id 缺失 => 通知; 存在(含显式 null)=> 请求。用 get 而非 is_null 区分"没有键"与"键为 null"。
    let is_notification = msg.get("id").is_none();
    let id = msg.get("id").cloned().unwrap_or(Value::Null);

    let method = match msg.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => {
            if is_notification {
                return None;
            }
            return Some(err_response(id, -32600, "缺少 method 字段".to_string()));
        }
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // 通知: 只处理已知通知(如 notifications/initialized), 一律不回应。
    if is_notification {
        return None;
    }

    match method {
        "initialize" => Some(ok_response(id, initialize_result(&params))),
        "ping" => Some(ok_response(id, json!({}))),
        "tools/list" => Some(ok_response(id, tools_list_result())),
        "tools/call" => Some(handle_tools_call(id, &params).await),
        // 未知方法: JSON-RPC 标准 -32601。
        _ => Some(err_response(
            id,
            -32601,
            format!("未知方法: {}", method),
        )),
    }
}

/// 构造 initialize 响应: server 信息 + 能力声明 + 协议版本。
fn initialize_result(params: &Value) -> Value {
    // 回显客户端请求的 protocolVersion(若给了且为字符串), 否则用默认值。
    let version = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {
            // 只声明 tools 能力; 不提供 resources/prompts。listChanged=false: 工具集静态。
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "imagecli",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// 构造 tools/list 响应。
fn tools_list_result() -> Value {
    json!({ "tools": tool_definitions() })
}

/// 全部工具的定义(name/description/inputSchema)。描述对齐 SKILL.md, 并显式标注
/// "消耗额度"与"key 走环境变量"。
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "generate_image",
            "description": "文生图/图生图: 提交生成任务并轮询到终态, 默认下载产物到 out_dir。\
                注意: 会消耗对应 provider 的额度/credits(免费层如 agnes/google 除外); \
                批量前先用 dry_run 预估成本或先生成一张确认。\
                API key 一律走环境变量(如 AGNES_API_KEY / GEMINI_API_KEY / FAL_KEY), 不接受 key 参数。\
                返回 imagecli 的稳定 --json 结构(results[].status / saved / job_id / error 等)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "文本提示词(必填)" },
                    "provider": { "type": "string", "description": "provider 名, 如 agnes/google/fal/volcengine; 省略则按配置默认->内置默认(agnes)" },
                    "model": { "type": "string", "description": "provider 内 model id; 省略按能力取默认" },
                    "capability": { "type": "string", "description": "能力, 默认 text2image; 图生图用 image2image(配合 input)" },
                    "size": { "type": "string", "description": "尺寸(如 1024x1024), 作为自由参数透传给 provider" },
                    "input": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "输入素材(图生图用): 本地图片路径或 http(s) URL, 可多个"
                    },
                    "params": {
                        "type": "object",
                        "description": "其他自由参数(seed/aspect_ratio 等), 直接透传给 provider"
                    },
                    "out_dir": { "type": "string", "description": "产物下载目录, 默认 ./out" },
                    "dry_run": { "type": "boolean", "description": "只预估成本与任务数, 不真实提交、不消耗额度" }
                },
                "required": ["prompt"]
            }
        }),
        json!({
            "name": "generate_video",
            "description": "文生视频/图生视频: 提交异步生成任务并轮询到终态, 默认下载产物。\
                能力默认 text2video(provider 如 seedance/kling); 图生视频用 image2video 并给 input。\
                注意: 视频生成会消耗额度且耗时较长; 批量前先 dry_run 预估或先生成一条确认。\
                API key 走环境变量(如 ARK/可灵的 key), 不接受 key 参数。返回 imagecli 的 --json 结构。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "文本提示词(必填)" },
                    "provider": { "type": "string", "description": "provider 名, 如 seedance/kling" },
                    "model": { "type": "string", "description": "provider 内 model id; 省略按能力取默认" },
                    "capability": { "type": "string", "description": "能力, 默认 text2video; 图生视频用 image2video" },
                    "size": { "type": "string", "description": "尺寸/分辨率, 作为自由参数透传" },
                    "input": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "输入素材(图生视频用): 本地图片路径或 http(s) URL"
                    },
                    "params": {
                        "type": "object",
                        "description": "其他自由参数(duration/fps 等), 透传给 provider"
                    },
                    "out_dir": { "type": "string", "description": "产物下载目录, 默认 ./out" },
                    "dry_run": { "type": "boolean", "description": "只预估, 不真实提交、不消耗额度" }
                },
                "required": ["prompt"]
            }
        }),
        json!({
            "name": "list_providers",
            "description": "列出所有已注册 provider 及其支持的能力, available 表示当前环境是否已配置该 \
                provider 的 API key(走环境变量)。只读, 不消耗额度。",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_models",
            "description": "列出统一模型目录(catalog): 每条含 provider/model/能力/估算成本(USD, 字符串精确)/\
                available(有无 key)。只读, 不消耗额度。用于在生成前选 provider/model。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "可选: 只列该 provider 的 model" }
                }
            }
        }),
        json!({
            "name": "get_job",
            "description": "按 job_id 查本地任务库(SQLite)里某任务的状态与产物。只读, 返回库内快照\
                (不向 provider 刷新, 不打网络)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "任务 id(必填)" }
                },
                "required": ["job_id"]
            }
        }),
        json!({
            "name": "list_jobs",
            "description": "列出本地任务库里的任务, 可按 status(queued/running/succeeded/failed)与 \
                capability 过滤, limit 限制条数。只读。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "按状态过滤" },
                    "capability": { "type": "string", "description": "按能力过滤" },
                    "limit": { "type": "integer", "description": "最多返回多少条" }
                }
            }
        }),
    ]
}

/// 分发 tools/call: 取工具名 + arguments, 路由到对应 handler。
async fn handle_tools_call(id: Value, params: &Value) -> Value {
    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return err_response(id, -32602, "tools/call 缺少 name 字段".to_string()),
    };
    // arguments 缺省视为空对象。
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match name {
        // ---- 只读工具: 直接调 core 拼 JSON, 不打印 stdout、不打网络 ----
        "list_providers" => ok_response(id, tool_result_ok(tool_list_providers())),
        "list_models" => ok_response(id, tool_result_ok(tool_list_models(&args))),
        "get_job" => match tool_get_job(&args) {
            Ok(v) => ok_response(id, tool_result_ok(v)),
            Err(e) => err_response(id, -32602, e),
        },
        "list_jobs" => match tool_list_jobs(&args) {
            Ok(v) => ok_response(id, tool_result_ok(v)),
            Err(e) => err_response(id, -32602, e),
        },
        // ---- 生成工具: 构造 CLI 参数后自调用子进程, 复用 cmd_generate 全链路 ----
        "generate_image" => match build_generate_args("text2image", &args) {
            Ok(cli_args) => ok_response(id, run_generate_subprocess(cli_args).await),
            Err(e) => err_response(id, -32602, e),
        },
        "generate_video" => match build_generate_args("text2video", &args) {
            Ok(cli_args) => ok_response(id, run_generate_subprocess(cli_args).await),
            Err(e) => err_response(id, -32602, e),
        },
        _ => err_response(id, -32602, format!("未知工具: {}", name)),
    }
}

/// list_providers handler: 复用 registry, 输出与 `imagecli providers --json` 同构。
fn tool_list_providers() -> Value {
    let registry = Registry::build_default();
    let mut arr = Vec::new();
    for name in registry.list_names() {
        if let Some(p) = registry.get(&name) {
            let caps: Vec<&str> = p.capabilities().iter().map(|c| c.as_str()).collect();
            arr.push(json!({
                "name": name,
                "capabilities": caps,
                "available": p.has_key(),
            }));
        }
    }
    json!({ "providers": arr })
}

/// list_models handler: 复用 catalog 聚合, 可选按 provider 过滤。输出与 `imagecli models --json` 同源。
fn tool_list_models(args: &Value) -> Value {
    let registry = Registry::build_default();
    let all = catalog::build_catalog(&registry);
    let provider_filter = args.get("provider").and_then(|v| v.as_str());
    let filtered: Vec<catalog::ModelEntry> = match provider_filter {
        Some(p) => all.into_iter().filter(|e| e.provider == p).collect(),
        None => all,
    };
    catalog::catalog_to_json(&filtered)
}

/// get_job handler: 按 job_id 查 store, 返回任务状态与产物(库内快照)。
fn tool_get_job(args: &Value) -> Result<Value, String> {
    let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Err("缺少必填参数 job_id".to_string()),
    };
    let store = JobStore::open().map_err(|e| format!("打开任务库失败: {}", e))?;
    let rec = store
        .get(job_id)
        .map_err(|e| format!("查询任务失败: {}", e))?;
    match rec {
        Some(r) => Ok(record_to_json(&r)),
        None => Ok(json!({ "found": false, "job_id": job_id })),
    }
}

/// list_jobs handler: 按 status/capability/limit 过滤查 store。
fn tool_list_jobs(args: &Value) -> Result<Value, String> {
    let filter = JobFilter {
        status: args
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        capability: args
            .get("capability")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        limit: args.get("limit").and_then(|v| v.as_i64()),
        offset: None,
    };
    let store = JobStore::open().map_err(|e| format!("打开任务库失败: {}", e))?;
    let records = store
        .list(&filter)
        .map_err(|e| format!("列出任务失败: {}", e))?;
    let arr: Vec<Value> = records.iter().map(record_to_json).collect();
    Ok(json!({ "jobs": arr }))
}

/// 把一条 JobRecord 序列化成稳定 JSON(与 cli 的 record_to_json 同字段)。
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

/// 由工具 arguments 构造 `imagecli --json generate ...` 的命令行参数(纯函数, 便于离线单测)。
///
/// default_capability: 该工具的默认能力(generate_image=text2image / generate_video=text2video),
/// 用户在 arguments.capability 显式给值则覆盖之。
/// prompt 为必填; 缺失给清晰中文错误(走 JSON-RPC -32602)。
fn build_generate_args(default_capability: &str, args: &Value) -> Result<Vec<String>, String> {
    // 顶层 --json(全局 flag)+ generate 子命令。
    let mut out: Vec<String> = vec!["--json".to_string(), "generate".to_string()];

    // 能力: arguments.capability 覆盖默认。
    let capability = args
        .get("capability")
        .and_then(|v| v.as_str())
        .unwrap_or(default_capability);
    out.push("--capability".to_string());
    out.push(capability.to_string());

    // prompt(必填)。
    let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p,
        _ => return Err("缺少必填参数 prompt".to_string()),
    };
    out.push("--prompt".to_string());
    out.push(prompt.to_string());

    // provider(可选)。
    if let Some(p) = args.get("provider").and_then(|v| v.as_str()) {
        out.push("--provider".to_string());
        out.push(p.to_string());
    }
    // model(可选)。
    if let Some(m) = args.get("model").and_then(|v| v.as_str()) {
        out.push("--model".to_string());
        out.push(m.to_string());
    }
    // size: 作为自由参数透传(--param size=<v>), 由各 provider 自行解释。
    if let Some(s) = args.get("size").and_then(|v| v.as_str()) {
        out.push("--param".to_string());
        out.push(format!("size={}", s));
    }
    // input(可选, 数组): 每个一条 --input。
    if let Some(items) = args.get("input").and_then(|v| v.as_array()) {
        for it in items {
            if let Some(s) = it.as_str() {
                out.push("--input".to_string());
                out.push(s.to_string());
            }
        }
    }
    // params(可选, 对象): 每个键值一条 --param key=value。
    // value 为字符串则取原文; 其他类型(数字/布尔/对象/数组)按紧凑 JSON 序列化,
    // 与 CLI parse_params 的"先试 JSON 再退字符串"对称。
    if let Some(obj) = args.get("params").and_then(|v| v.as_object()) {
        for (k, v) in obj.iter() {
            let val_str = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push("--param".to_string());
            out.push(format!("{}={}", k, val_str));
        }
    }
    // out_dir(可选)。
    if let Some(d) = args.get("out_dir").and_then(|v| v.as_str()) {
        out.push("--out-dir".to_string());
        out.push(d.to_string());
    }
    // dry_run(可选): 复用 CLI --dry-run, 只估不跑、不消耗额度。
    if args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        out.push("--dry-run".to_string());
    }

    Ok(out)
}

/// 自调用子进程跑 generate, 返回 MCP tool 结果。
///
/// 为什么走子进程: cmd_generate 内部大量 println! 到 stdout, 而本 server 的 stdout 已被
/// JSON-RPC 通道占用; 子进程有独立 stdout, 既复用了完整生成链路又不串台。子进程继承本进程
/// 环境变量(含各 provider 的 key), 凭证沿用环境变量这一约定自然成立。
async fn run_generate_subprocess(cli_args: Vec<String>) -> Value {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return tool_result_err(format!("无法定位 imagecli 自身可执行文件: {}", e));
        }
    };
    let output = tokio::process::Command::new(exe)
        .args(&cli_args)
        .output()
        .await;
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let success = o.status.success();
            // 退出码即契约: 0=全部成功; 非零=至少一个任务失败/出错。
            // stdout 是 --json 结构, 尝试解析为 structuredContent; 失败则只放文本。
            let structured = serde_json::from_str::<Value>(stdout.trim()).ok();
            let mut text = stdout;
            if !success && !stderr.trim().is_empty() {
                // 失败时把 stderr(中文错误/可观测性提示)也带上, 便于 agent 看清原因。
                text.push_str("\n[stderr]\n");
                text.push_str(&stderr);
            }
            tool_result(text, !success, structured)
        }
        Err(e) => tool_result_err(format!("启动 generate 子进程失败: {}", e)),
    }
}

// ---------- JSON-RPC / MCP 响应构造小工具 ----------

/// 成功响应。
fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// 错误响应(JSON-RPC error 对象)。
fn err_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// 成功的 tool 调用结果: 把一个 JSON 值同时放进 text content(便于纯文本客户端)与
/// structuredContent(便于结构化客户端)。isError=false。
fn tool_result_ok(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    tool_result(text, false, Some(value))
}

/// 失败的 tool 调用结果(执行层错误, 非协议错误): isError=true, 只放文本说明。
fn tool_result_err(message: String) -> Value {
    tool_result(message, true, None)
}

/// 通用 tool 结果构造。content 至少含一个 text 块; structured 非空时附 structuredContent。
fn tool_result(text: String, is_error: bool, structured: Option<Value>) -> Value {
    let mut result = json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    });
    if let Some(s) = structured {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("structuredContent".to_string(), s);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用唯一临时 db 隔离 store 相关测试, 避免污染真实任务库与并发互踩。
    fn set_temp_db() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir()
            .join(format!("imagecli_mcp_{}_{}.db", std::process::id(), nanos));
        std::env::set_var("IMAGECLI_DB_PATH", &path);
        path
    }

    #[test]
    fn initialize_echoes_protocol_version_and_reports_server_info() {
        // 客户端给了 protocolVersion -> 回显; 同时声明 tools 能力与 serverInfo。
        let params = json!({ "protocolVersion": "2025-06-18" });
        let r = initialize_result(&params);
        assert_eq!(r["protocolVersion"], "2025-06-18");
        assert_eq!(r["serverInfo"]["name"], "imagecli");
        assert!(r["capabilities"]["tools"].is_object());
        // 未给版本 -> 用默认。
        let r2 = initialize_result(&json!({}));
        assert_eq!(r2["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_contains_all_six_tools_with_valid_schema() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools 应为数组");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "generate_image",
            "generate_video",
            "list_providers",
            "list_models",
            "get_job",
            "list_jobs",
        ] {
            assert!(names.contains(&expected), "应含工具 {}", expected);
        }
        // 每个工具的 inputSchema 必须是 object 类型的合法 JSON Schema, 且有 description。
        for t in tools {
            assert!(t["description"].is_string(), "工具应有 description");
            let schema = &t["inputSchema"];
            assert_eq!(schema["type"], "object", "inputSchema.type 应为 object");
            assert!(schema["properties"].is_object(), "inputSchema 应有 properties");
        }
        // 生成类工具的成本/凭证提示应在描述里(对齐验收: 描述写明会消耗额度 + key 走环境变量)。
        let gen_img = tools.iter().find(|t| t["name"] == "generate_image").unwrap();
        let desc = gen_img["description"].as_str().unwrap();
        assert!(desc.contains("消耗"), "generate_image 描述应提示消耗额度");
        assert!(desc.contains("环境变量"), "generate_image 描述应提示 key 走环境变量");
        // prompt 必填体现在 required。
        assert_eq!(gen_img["inputSchema"]["required"][0], "prompt");
    }

    #[tokio::test]
    async fn handle_message_ping_returns_empty_result() {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
        let resp = handle_message(msg).await.expect("ping 应有响应");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert!(resp["result"].is_object());
    }

    #[tokio::test]
    async fn handle_message_unknown_method_returns_method_not_found() {
        let msg = json!({ "jsonrpc": "2.0", "id": 2, "method": "no/such" });
        let resp = handle_message(msg).await.unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn handle_message_notification_yields_no_response() {
        // 无 id => 通知; 不产生任何响应。
        let msg = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_message(msg).await.is_none());
    }

    #[tokio::test]
    async fn handle_line_parse_error_returns_parse_error_code() {
        let resp = handle_line("{ not json").await.unwrap();
        assert_eq!(resp["error"]["code"], -32700);
        assert_eq!(resp["id"], Value::Null);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_invalid_params() {
        let params = json!({ "name": "nope", "arguments": {} });
        let resp = handle_tools_call(json!(5), &params).await;
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn tools_call_get_job_missing_id_errors() {
        let params = json!({ "name": "get_job", "arguments": {} });
        let resp = handle_tools_call(json!(6), &params).await;
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn tools_call_list_providers_routes_and_returns_content() {
        // 路由到 list_providers, 返回 tool 结果(含 content 与 structuredContent)。
        let params = json!({ "name": "list_providers", "arguments": {} });
        let resp = handle_tools_call(json!(7), &params).await;
        let result = &resp["result"];
        assert_eq!(result["isError"], false);
        assert!(result["content"][0]["text"].is_string());
        // structuredContent 里应能看到已注册 provider(如 agnes)。
        let providers = result["structuredContent"]["providers"]
            .as_array()
            .expect("应有 providers 数组");
        assert!(providers.iter().any(|p| p["name"] == "agnes"));
        // 每条带 available 字段(有无 key)。
        assert!(providers[0]["available"].is_boolean());
    }

    #[tokio::test]
    async fn tools_call_list_models_filters_by_provider() {
        let params = json!({ "name": "list_models", "arguments": { "provider": "agnes" } });
        let resp = handle_tools_call(json!(8), &params).await;
        let models = resp["result"]["structuredContent"]["models"]
            .as_array()
            .expect("应有 models 数组");
        assert!(!models.is_empty(), "agnes 应至少有一个 model");
        for m in models {
            assert_eq!(m["provider"], "agnes");
        }
    }

    #[tokio::test]
    async fn tools_call_list_jobs_on_empty_store_returns_empty() {
        let path = set_temp_db();
        let params = json!({ "name": "list_jobs", "arguments": {} });
        let resp = handle_tools_call(json!(9), &params).await;
        let jobs = resp["result"]["structuredContent"]["jobs"]
            .as_array()
            .expect("应有 jobs 数组");
        assert!(jobs.is_empty(), "空库应返回空 jobs");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn tools_call_get_job_not_found_returns_found_false() {
        let path = set_temp_db();
        let params = json!({ "name": "get_job", "arguments": { "job_id": "missing-xyz" } });
        let resp = handle_tools_call(json!(10), &params).await;
        assert_eq!(resp["result"]["structuredContent"]["found"], false);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn build_generate_args_image_defaults_and_required_prompt() {
        // 默认能力 text2image; prompt 必填。
        let args = json!({ "prompt": "a red fox" });
        let v = build_generate_args("text2image", &args).unwrap();
        // 形如: --json generate --capability text2image --prompt "a red fox"
        assert_eq!(v[0], "--json");
        assert_eq!(v[1], "generate");
        let cap_pos = v.iter().position(|s| s == "--capability").unwrap();
        assert_eq!(v[cap_pos + 1], "text2image");
        let prompt_pos = v.iter().position(|s| s == "--prompt").unwrap();
        assert_eq!(v[prompt_pos + 1], "a red fox");
        // 缺 prompt -> Err。
        assert!(build_generate_args("text2image", &json!({})).is_err());
    }

    #[test]
    fn build_generate_args_video_default_capability_and_passthrough() {
        // generate_video 默认 text2video; provider/model/size/input/params/out_dir/dry_run 全透传。
        let args = json!({
            "prompt": "a cat surfing",
            "provider": "seedance",
            "model": "some-model",
            "size": "1280x720",
            "input": ["https://x/in.png", "/local/a.png"],
            "params": { "duration": 5, "ratio": "16:9" },
            "out_dir": "./vids",
            "dry_run": true
        });
        let v = build_generate_args("text2video", &args).unwrap();
        let joined = v.join(" ");
        assert!(joined.contains("--capability text2video"));
        assert!(joined.contains("--provider seedance"));
        assert!(joined.contains("--model some-model"));
        assert!(joined.contains("--param size=1280x720"));
        assert!(joined.contains("--input https://x/in.png"));
        assert!(joined.contains("--input /local/a.png"));
        // 字符串 param 取原文, 数字 param 取 JSON 数字串。
        assert!(joined.contains("--param ratio=16:9"));
        assert!(joined.contains("--param duration=5"));
        assert!(joined.contains("--out-dir ./vids"));
        assert!(v.iter().any(|s| s == "--dry-run"));
    }

    #[test]
    fn build_generate_args_capability_override() {
        // 显式 capability 覆盖默认(图生图)。
        let args = json!({ "prompt": "x", "capability": "image2image" });
        let v = build_generate_args("text2image", &args).unwrap();
        let cap_pos = v.iter().position(|s| s == "--capability").unwrap();
        assert_eq!(v[cap_pos + 1], "image2image");
    }
}
