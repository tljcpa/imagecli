//! MCP stdio server 集成冒烟测试。
//!
//! 以子进程方式启动编译好的 `imagecli mcp`, 通过 stdin 喂入 newline-delimited 的
//! JSON-RPC 请求, 从 stdout 读回响应, 验证:
//!   1. initialize 握手返回 serverInfo + tools 能力 + 回显 protocolVersion;
//!   2. tools/list 列出全部 6 个工具;
//!   3. tools/call 路由到 generate_image 的 dry_run 路径(全程离线、不消耗额度),
//!      返回 isError=false 且结构里带 dry_run 预估。
//!
//! 全程离线: 只用 dry_run 与只读路径, 不打真实网络、不需要任何 provider key。
//! 清空 key 来源, 并用临时 IMAGECLI_DB_PATH 隔离, 避免污染真实任务库。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// 生成唯一临时 db 路径, 避免并发测试互踩。
fn temp_db() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("imagecli_mcp_it_{}_{}.db", std::process::id(), nanos))
}

#[test]
fn stdio_handshake_lists_tools_and_runs_dry_run_call() {
    let bin = env!("CARGO_BIN_EXE_imagecli");
    let db = temp_db();

    let mut child = Command::new(bin)
        .arg("mcp")
        .env("IMAGECLI_DB_PATH", db.to_string_lossy().to_string())
        // 清空所有 key 来源: 证明握手与 dry_run 不依赖任何凭证。
        .env_remove("AGNES_API_KEY")
        .env_remove("IMAGECLI_AGNES_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("IMAGECLI_GOOGLE_KEY")
        .env_remove("FAL_KEY")
        .env_remove("IMAGECLI_FAL_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 imagecli mcp 失败");

    // 写三条请求 + 一条通知, 然后关闭 stdin 触发 server EOF 退出。
    {
        let stdin = child.stdin.as_mut().expect("应能拿到子进程 stdin");
        let lines = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"generate_image","arguments":{"prompt":"a red fox","provider":"agnes","dry_run":true}}}"#,
        ];
        for l in lines {
            stdin.write_all(l.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        stdin.flush().unwrap();
    }
    // drop stdin: 关闭管道, server 读到 EOF 后退出。
    drop(child.stdin.take());

    // 逐行读回响应(通知不产生响应, 故应正好 3 条)。
    let stdout = child.stdout.take().expect("应能拿到子进程 stdout");
    let reader = BufReader::new(stdout);
    let mut responses: Vec<serde_json::Value> = Vec::new();
    for line in reader.lines() {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        responses.push(serde_json::from_str(&line).expect("每行响应应是合法 JSON"));
    }
    let _ = child.wait();
    let _ = std::fs::remove_file(&db);

    assert_eq!(responses.len(), 3, "应正好 3 条响应(通知不回应)");

    // 1) initialize
    let init = &responses[0];
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "imagecli");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // 2) tools/list: 含全部 6 个工具。
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools 应为数组");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "generate_image",
        "generate_video",
        "list_providers",
        "list_models",
        "get_job",
        "list_jobs",
    ] {
        assert!(names.contains(&expected), "tools/list 应含 {}", expected);
    }

    // 3) tools/call generate_image dry_run: 路由成功、未消耗额度、带 dry_run 预估。
    let call = &responses[2]["result"];
    assert_eq!(call["isError"], false);
    assert_eq!(call["structuredContent"]["dry_run"], true);
    assert_eq!(call["structuredContent"]["provider"], "agnes");
}
