//! 子进程传输(D-003 的 subprocess 维度): 跑外部 CLI, 解析其 stdout。
//!
//! 预留给未来"CLI 型后端"(某些自托管/本地推理工具只有命令行接口)。
//! MVP 阶段仅放骨架与占位实现, 保证类型成立、可编译。

use std::process::Stdio;

use tokio::process::Command;

/// 子进程执行结果骨架。
#[derive(Debug, Clone)]
pub struct SubprocessOutput {
    /// 退出码(None 表示被信号终止)
    pub exit_code: Option<i32>,
    /// 标准输出原文
    pub stdout: String,
    /// 标准错误原文
    pub stderr: String,
}

/// 子进程传输客户端: 封装"跑一条命令并捕获输出"。
pub struct SubprocessClient {
    /// 要执行的程序名/路径(如某本地推理 CLI)
    program: String,
}

impl SubprocessClient {
    pub fn new(program: impl Into<String>) -> SubprocessClient {
        SubprocessClient {
            program: program.into(),
        }
    }

    /// 跑一次命令, 捕获 stdout/stderr。
    ///
    /// 这部分是通用的(已可用), 真正"把 stdout 解析成 Job"的语义留给具体 provider 实现。
    pub async fn run(&self, args: &[String]) -> anyhow::Result<SubprocessOutput> {
        let output = Command::new(&self.program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        Ok(SubprocessOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// 把 stdout 解析成结构化结果。
    ///
    /// 占位: 不同 CLI 后端输出格式各异, 没有统一解析逻辑可写, 故先返回未实现错误。
    pub fn parse_stdout(&self, _stdout: &str) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("subprocess transport 的 stdout 解析尚未实现(预留给未来 CLI 型后端)")
    }
}
