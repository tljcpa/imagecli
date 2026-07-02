//! 跨进程持久化冒烟测试(落地 D-007 的核心验收)。
//!
//! 思路: 本测试进程(进程 A)用 JobStore 写一条人造任务记录到一个临时 db;
//! 然后以子进程方式(进程 B)启动编译好的 imagecli 二进制, 用 status / list 子命令
//! 去读同一个 db。如果进程 B 能读到进程 A 写的任务, 就证明状态确实跨进程持久化了。
//!
//! 不打真实网络: 人造记录直接是 succeeded 终态, status 命令对终态不会再向 provider 轮询。

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use imagecli::core::provider::{Asset, AssetKind, Capability, Job, JobStatus};
use imagecli::core::store::{JobRecord, JobStore};
use serde_json::json;

/// 生成一个唯一的临时 db 路径, 避免并发测试互相踩。
fn temp_db_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("imagecli_xproc_{}_{}.db", std::process::id(), nanos))
}

/// 构造一条已成功的人造任务记录(带句柄与一个产物 URL)。
fn make_succeeded_record(job_id: &str) -> JobRecord {
    // raw_meta 里塞 fal 风格的句柄, 模拟 submit 后落库的形态
    let job = Job {
        id: job_id.to_string(),
        provider: "fal".to_string(),
        status: JobStatus::Succeeded,
        outputs: vec![Asset::from_url(AssetKind::Image, "https://cdn.fal.ai/fake/out.png")],
        error: None,
        raw_meta: json!({
            "request_id": job_id,
            "status_url": "https://queue.fal.run/x/status",
            "response_url": "https://queue.fal.run/x",
            "cancel_url": null,
        }),
    };
    JobRecord::from_job(&job, Capability::Text2Image, "fal-ai/flux/dev", None, 1700000000, 1700000000)
}

/// 手动跨进程冒烟用的"播种"步骤(默认 #[ignore], 仅手动驱动)。
/// 进程 A: 用 IMAGECLI_DB_PATH 指定的 db 写一条 succeeded 任务, 不清理。
/// 之后在另一个 shell 里用 `cargo run -- list/status` 作为进程 B 读取, 证明跨进程可读。
#[test]
#[ignore]
fn seed_for_manual_demo() {
    let store = JobStore::open().expect("打开 store 失败(请设 IMAGECLI_DB_PATH)");
    let rec = make_succeeded_record("manual-demo-001");
    store.save(&rec).expect("写入记录失败");
    eprintln!("seeded job manual-demo-001 into store");
}

/// 进程 A 写 -> 进程 B(子进程二进制) 读, 验证 list 与 status 都能看到该任务。
#[test]
fn job_persists_across_processes() {
    let db_path = temp_db_path();
    let db_str = db_path.to_string_lossy().to_string();
    let job_id = "xproc-job-001";

    // ---- 进程 A: 写入 ----
    {
        // 用 IMAGECLI_DB_PATH 指向临时 db, 与二进制共享同一份存储
        let store = JobStore::open_at(&db_path).expect("进程A: 打开 store 失败");
        let rec = make_succeeded_record(job_id);
        store.save(&rec).expect("进程A: 写入记录失败");
        // store 在此 drop, 连接关闭, 数据已落盘
    }

    // 编译好的二进制路径由 cargo 在集成测试时通过环境变量注入
    let bin = env!("CARGO_BIN_EXE_imagecli");

    // ---- 进程 B-1: list --json 应能列出该任务 ----
    let list_out = Command::new(bin)
        .args(["list", "--json"])
        .env("IMAGECLI_DB_PATH", &db_str)
        .output()
        .expect("进程B: 启动 imagecli list 失败");
    assert!(
        list_out.status.success(),
        "list 退出码非 0: stderr={}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        list_stdout.contains(job_id),
        "进程B 的 list 输出未包含进程A 写的任务 {}: {}",
        job_id,
        list_stdout
    );

    // ---- 进程 B-2: status <id> --json 应能读到该任务且为 succeeded ----
    let status_out = Command::new(bin)
        .args(["status", job_id, "--json"])
        .env("IMAGECLI_DB_PATH", &db_str)
        .output()
        .expect("进程B: 启动 imagecli status 失败");
    assert!(
        status_out.status.success(),
        "status 退出码非 0: stderr={}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_stdout.contains(job_id) && status_stdout.contains("succeeded"),
        "进程B 的 status 输出不符合预期: {}",
        status_stdout
    );

    // ---- 进程 B-3: list --status failed 应过滤掉该 succeeded 任务 ----
    let filtered = Command::new(bin)
        .args(["list", "--status", "failed", "--json"])
        .env("IMAGECLI_DB_PATH", &db_str)
        .output()
        .expect("进程B: 启动 imagecli list --status failed 失败");
    let filtered_stdout = String::from_utf8_lossy(&filtered.stdout);
    assert!(
        !filtered_stdout.contains(job_id),
        "status=failed 过滤不应返回 succeeded 任务: {}",
        filtered_stdout
    );

    // 清理
    let _ = std::fs::remove_file(&db_path);
}
