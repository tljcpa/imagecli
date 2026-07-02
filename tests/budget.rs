//! 预算护栏集成冒烟(D-006: 成本预检与预算护栏)。
//!
//! 以子进程方式跑编译好的 imagecli 二进制, 验证两条护栏的"进程级"契约:
//!   1. --dry-run: 无 key 也能跑出成本预估、退出 0, 且不产生任何 store 记录(不打网络)。
//!   2. --max-cost: 预估超上限时, 在 submit 之前就拒绝执行(非零退出 + 中文提示),
//!      同样不产生 store 记录(护栏短路在开库之前)。
//!
//! 全程离线: dry-run 在调用 provider 之前返回; max-cost 超限同样在 submit 之前 bail。
//! 用临时 IMAGECLI_DB_PATH 隔离, 既避免污染用户 store, 又能反查"是否留下脏记录"。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// 生成唯一临时 db 路径, 避免并发测试互踩。
fn temp_db_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("imagecli_budget_{}_{}_{}.db", tag, std::process::id(), nanos))
}

/// 跑一条 imagecli 命令, 清空所有 key 来源, 返回 (退出码, stdout+stderr 合并)。
fn run_cli(db: &Path, args: &[&str]) -> (Option<i32>, String) {
    let bin = env!("CARGO_BIN_EXE_imagecli");
    let out = Command::new(bin)
        .args(args)
        .env("IMAGECLI_DB_PATH", db.to_string_lossy().to_string())
        .env_remove("AGNES_API_KEY")
        .env_remove("IMAGECLI_AGNES_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("IMAGECLI_GOOGLE_KEY")
        .env_remove("FAL_KEY")
        .env_remove("IMAGECLI_FAL_KEY")
        .output()
        .expect("启动 imagecli 失败");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code(), combined)
}

/// 用 `list --json` 反查该 db 里的任务条数(粗略数 job_id 出现次数足够本测断言"是否为空")。
fn list_is_empty(db: &Path) -> bool {
    let (_code, out) = run_cli(db, &["--json", "list"]);
    // 空 store 的 list --json 形如 {"jobs": []}; 用是否含 "job_id" 判定有无记录。
    !out.contains("\"job_id\"")
}

/// dry-run: 无 key 也应退出 0, 打印成本预估, 且不留 store 记录。
#[test]
fn dry_run_no_key_exits_zero_estimates_cost_no_store() {
    let db = temp_db_path("dryrun");
    let (code, out) = run_cli(
        &db,
        &[
            "--json",
            "generate",
            "--provider",
            "agnes",
            "--prompt",
            "a",
            "--prompt",
            "b",
            "--dry-run",
        ],
    );
    assert_eq!(code, Some(0), "dry-run 应退出 0, 实得 {:?}; out={}", code, out);
    assert!(out.contains("\"dry_run\""), "应输出 dry_run 标志; out={}", out);
    assert!(out.contains("\"task_count\""), "应输出 task_count; out={}", out);
    assert!(out.contains("\"estimated_cost\""), "应输出 estimated_cost; out={}", out);
    // agnes 免费, 两个任务预估成本应为 0。
    assert!(out.contains("\"estimated_cost\": \"0\""), "agnes 两任务预估应为 0; out={}", out);
    // dry-run 不应触发缺 key 报错(没有真的去调 provider)。
    assert!(!out.contains("API key"), "dry-run 不应出现缺 key 报错; out={}", out);
    // dry-run 不写库。
    assert!(list_is_empty(&db), "dry-run 不应产生 store 记录");
    let _ = std::fs::remove_file(&db);
}

/// max-cost 超限: 应非零退出 + 中文提示, 且不留 store 记录(拒绝发生在开库前)。
#[test]
fn max_cost_over_budget_rejects_nonzero_no_store() {
    let db = temp_db_path("maxcost");
    // fal 文生图占位单价 0.025; 4 个任务预估 0.10, 远超 --max-cost 0.01 -> 拒绝。
    let (code, out) = run_cli(
        &db,
        &[
            "generate",
            "--provider",
            "fal",
            "--prompt",
            "a",
            "--prompt",
            "b",
            "--prompt",
            "c",
            "--prompt",
            "d",
            "--max-cost",
            "0.01",
        ],
    );
    assert_ne!(code, Some(0), "超预算应非零退出, 实得 {:?}; out={}", code, out);
    assert!(out.contains("超过 --max-cost"), "应给出中文超预算提示; out={}", out);
    // 拒绝在 submit/开库之前发生, 不应留下脏记录。
    assert!(list_is_empty(&db), "超预算拒绝不应产生 store 记录");
    let _ = std::fs::remove_file(&db);
}

/// max-cost 充足: 预估未超上限时不应被护栏拦截(此处用 dry-run 验证护栏放行而不真打网络)。
#[test]
fn max_cost_under_budget_passes_guard() {
    let db = temp_db_path("under");
    // fal 4 任务预估 0.10; --max-cost 1.00 充足。配合 --dry-run 避免真实提交/缺 key。
    let (code, out) = run_cli(
        &db,
        &[
            "generate",
            "--provider",
            "fal",
            "--prompt",
            "a",
            "--prompt",
            "b",
            "--prompt",
            "c",
            "--prompt",
            "d",
            "--max-cost",
            "1.00",
            "--dry-run",
        ],
    );
    // 未超预算 + dry-run -> 退出 0, 不出现拒绝提示。
    assert_eq!(code, Some(0), "未超预算应放行(dry-run 退出 0), 实得 {:?}; out={}", code, out);
    assert!(!out.contains("超过 --max-cost"), "未超预算不应出现拒绝提示; out={}", out);
    let _ = std::fs::remove_file(&db);
}
