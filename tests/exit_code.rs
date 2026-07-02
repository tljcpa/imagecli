//! 退出码契约冒烟测试(落地 D-006 的退出码契约)。
//!
//! 思路: 以子进程方式启动编译好的 imagecli 二进制, 跑 generate 但故意不给 API key。
//! provider 在 submit 内取 key 失败 -> 返回中文指引错误 -> run_batch 收到 Err ->
//! cmd_generate 置 had_error -> bail -> main 以退出码 1 收口。断言子进程退出码非零。
//!
//! 不打真实网络: 缺 key 在网络请求之前就短路返回, 全程离线。
//! 用临时 IMAGECLI_DB_PATH 隔离, 避免污染用户默认 store。

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// 生成唯一临时 db 路径, 避免并发测试互踩。
fn temp_db_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("imagecli_exit_{}_{}_{}.db", tag, std::process::id(), nanos))
}

/// 跑一次无 key 的 generate, 返回子进程退出码(None 表示被信号终止)。
fn run_generate_without_key(provider: &str) -> (Option<i32>, String) {
    let bin = env!("CARGO_BIN_EXE_imagecli");
    let db = temp_db_path(provider);
    let mut cmd = Command::new(bin);
    cmd.args(["generate", "--provider", provider, "--prompt", "a red fox in snow"])
        .env("IMAGECLI_DB_PATH", db.to_string_lossy().to_string());
    // 清空所有可能的 key 来源(含大陆 5 家), 确保走无 key 失败路径
    for var in [
        "AGNES_API_KEY",
        "IMAGECLI_AGNES_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "IMAGECLI_GOOGLE_KEY",
        "ARK_API_KEY",
        "VOLC_API_KEY",
        "IMAGECLI_VOLC_KEY",
        "STEPFUN_API_KEY",
        "IMAGECLI_STEPFUN_KEY",
        "ZHIPU_API_KEY",
        "GLM_API_KEY",
        "IMAGECLI_ZHIPU_KEY",
        "PPIO_API_KEY",
        "IMAGECLI_PPIO_KEY",
        "SILICONFLOW_API_KEY",
        "IMAGECLI_SILICONFLOW_KEY",
    ] {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("启动 imagecli generate 失败");
    let _ = std::fs::remove_file(&db);
    // 合并 stdout+stderr: 缺 key 的中文指引作为单任务错误打在 stdout(report),
    // 最终 "部分或全部任务失败" 收口打在 stderr; 退出码契约验证只需两者其一含指引。
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code(), combined)
}

/// 无 key 的 agnes generate 必须以非零退出码结束。
#[test]
fn agnes_generate_without_key_exits_nonzero() {
    let (code, stderr) = run_generate_without_key("agnes");
    assert_ne!(
        code,
        Some(0),
        "无 key 的 agnes generate 退出码应非零, 实得 {:?}; stderr={}",
        code,
        stderr
    );
    // 兼带验证错误是中文指引而非 panic
    assert!(
        stderr.contains("AGNES_API_KEY") || stderr.contains("API key"),
        "应给出无 key 的中文指引, 实得 stderr={}",
        stderr
    );
}

/// 无 key 的 google generate 同样必须以非零退出码结束(回归保护)。
#[test]
fn google_generate_without_key_exits_nonzero() {
    let (code, stderr) = run_generate_without_key("google");
    assert_ne!(
        code,
        Some(0),
        "无 key 的 google generate 退出码应非零, 实得 {:?}; stderr={}",
        code,
        stderr
    );
}

/// 大陆 5 家(火山/StepFun/智谱/PPIO/SiliconFlow)无 key 时均须非零退出 + 中文 key 指引。
/// 逐家以子进程跑 generate, 验证 D-010/D-012 接入的无 key 失败路径全程离线、不 panic。
#[test]
fn cn_providers_generate_without_key_exit_nonzero_with_hint() {
    let cases = [
        ("volcengine", "ARK_API_KEY"),
        ("stepfun", "STEPFUN_API_KEY"),
        ("zhipu", "ZHIPU_API_KEY"),
        ("ppio", "PPIO_API_KEY"),
        ("siliconflow", "SILICONFLOW_API_KEY"),
    ];
    for (provider, expect_var) in cases {
        let (code, combined) = run_generate_without_key(provider);
        assert_ne!(
            code,
            Some(0),
            "无 key 的 {} generate 退出码应非零, 实得 {:?}; out={}",
            provider,
            code,
            combined
        );
        // 中文 key 指引应点名该家的环境变量(证明走的是无 key 短路, 而非别处崩)
        assert!(
            combined.contains(expect_var) || combined.contains("API key"),
            "{} 应给出无 key 中文指引(含 {}), 实得 out={}",
            provider,
            expect_var,
            combined
        );
    }
}

/// 无 key 的 seedance text2video 必须非零退出 + 中文 key 指引(D-014 视频地基验收)。
///
/// 走 `--capability text2video`(seedance 真支持的能力, 不会被能力校验提前拦),
/// 故失败必来自 submit 取 key 失败的中文短路, 全程离线、不打网络、不 panic。
#[test]
fn seedance_text2video_without_key_exits_nonzero_with_hint() {
    let bin = env!("CARGO_BIN_EXE_imagecli");
    let db = temp_db_path("seedance");
    let mut cmd = Command::new(bin);
    cmd.args([
        "generate",
        "--provider",
        "seedance",
        "--capability",
        "text2video",
        "--prompt",
        "a cat surfing on a wave",
    ])
    .env("IMAGECLI_DB_PATH", db.to_string_lossy().to_string());
    // 清空 seedance 的全部 key 来源(与 volcengine 共享 ARK_API_KEY)
    for var in ["ARK_API_KEY", "IMAGECLI_ARK_KEY", "IMAGECLI_SEEDANCE_KEY"] {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("启动 imagecli generate 失败");
    let _ = std::fs::remove_file(&db);
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));

    assert_ne!(
        out.status.code(),
        Some(0),
        "无 key 的 seedance text2video 退出码应非零, 实得 {:?}; out={}",
        out.status.code(),
        combined
    );
    assert!(
        combined.contains("ARK_API_KEY") || combined.contains("API key"),
        "seedance 应给出无 key 中文指引(含 ARK_API_KEY), 实得 out={}",
        combined
    );
}

/// 对 provider 不支持的能力组合, 必须给清晰中文错误 + 非零退出(修复 help 误导问题)。
/// 用一个图像 provider(volcengine)请求 text2video: 应被能力校验拦下, 报"不支持能力"。
#[test]
fn unsupported_capability_combo_errors_clearly() {
    let bin = env!("CARGO_BIN_EXE_imagecli");
    let db = temp_db_path("unsupported_cap");
    let mut cmd = Command::new(bin);
    cmd.args([
        "generate",
        "--provider",
        "volcengine",
        "--capability",
        "text2video",
        "--prompt",
        "x",
    ])
    .env("IMAGECLI_DB_PATH", db.to_string_lossy().to_string());
    // 即便给了 key 也应被能力校验提前拦下(校验在取 key 之前), 这里仍清空以稳态。
    for var in ["ARK_API_KEY", "VOLC_API_KEY", "IMAGECLI_VOLC_KEY"] {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("启动 imagecli generate 失败");
    let _ = std::fs::remove_file(&db);
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));

    assert_ne!(
        out.status.code(),
        Some(0),
        "不支持的能力组合退出码应非零, 实得 {:?}; out={}",
        out.status.code(),
        combined
    );
    assert!(
        combined.contains("不支持能力"),
        "应给出'不支持能力'的清晰中文错误而非误导, 实得 out={}",
        combined
    );
}
