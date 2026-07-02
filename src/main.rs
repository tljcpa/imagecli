//! imagecli 入口。
//!
//! 职责极薄: 解析命令行 -> 进 tokio 运行时 -> 调度 cli::run。
//! 错误统一在此收口: 打印中文错误到 stderr, 以非零退出码结束(退出码契约)。
//!
//! 模块本体在 `lib.rs`(库形式), 以便集成测试也能复用内核(如跨进程冒烟需要直接用 JobStore)。

use clap::Parser;

use imagecli::cli::{self, Cli};

#[tokio::main]
async fn main() {
    // 解析命令行。clap 在 --help/--version/参数错误时自行打印并退出。
    let cli = Cli::parse();

    // 分发执行。所有业务错误以 anyhow::Result 上抛, 在此统一收口。
    match cli::run(cli).await {
        Ok(()) => {
            // 成功: 退出码 0(进程默认)
        }
        Err(e) => {
            // 失败: 中文错误打到 stderr, 退出码 1。
            // 用 {:#} 展开 anyhow 的 context 链, 便于定位(如缺 key 的指引)。
            eprintln!("错误: {:#}", e);
            std::process::exit(1);
        }
    }
}
