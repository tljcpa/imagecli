//! 任务编排内核: 并发提交 + 指数退避(带 jitter)轮询 + 可取消。
//!
//! 这是项目内核(DECISIONS D-005)。上层 CLI 把一批 GenRequest 交给 runner,
//! runner 负责: 受限并发地 submit, 然后对每个未终结的 Job 按退避策略 poll 到终态或超时。

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::core::provider::{GenRequest, Job, JobStatus, Provider};
use crate::core::store::{now_unix, JobRecord, JobStore};

/// 轮询/并发策略配置。所有时间相关参数集中在此, 便于 CLI 暴露成 flag。
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// 最大并发任务数(bounded concurrency)
    pub concurrency: usize,
    /// 轮询初始间隔
    pub base_delay: Duration,
    /// 轮询间隔上限(退避封顶)
    pub max_delay: Duration,
    /// 单任务总超时(从 submit 起算)
    pub timeout: Duration,
    /// 退避倍率(每次乘以它)
    pub backoff_factor: f64,
}

impl Default for RunConfig {
    fn default() -> RunConfig {
        RunConfig {
            concurrency: 4,
            base_delay: Duration::from_millis(800),
            max_delay: Duration::from_secs(15),
            timeout: Duration::from_secs(600),
            backoff_factor: 2.0,
        }
    }
}

/// 计算下一次轮询的退避延迟: 指数退避 + 满抖动(full jitter)。
///
/// 满抖动公式: sleep = random(0, min(max_delay, base * factor^attempt))。
/// 满抖动比"固定 + 小抖动"更能打散并发任务的轮询时刻, 避免对 provider 形成同步脉冲。
/// 这里把上限算出来后在 [0, 上限] 取随机, 用 Decimal 不必要(非金额), 但避免 f64 误差累积:
/// 我们只用 f64 做一次性的上限计算, 不参与金额, 符合规则(金额才禁 f64)。
pub fn next_backoff(cfg: &RunConfig, attempt: u32) -> Duration {
    // base * factor^attempt, 用 powi 计算指数
    let base_ms = cfg.base_delay.as_millis() as f64;
    let raw = base_ms * cfg.backoff_factor.powi(attempt as i32);
    // 封顶到 max_delay
    let max_ms = cfg.max_delay.as_millis() as f64;
    let mut capped = raw;
    if capped > max_ms {
        capped = max_ms;
    }
    // 满抖动: 在 [0, capped] 取随机
    let mut rng = rand::thread_rng();
    let jittered = rng.gen_range(0.0..=capped);
    Duration::from_millis(jittered as u64)
}

/// 对单个已提交的 Job 轮询到终态或超时, 并把每次状态变化写回 store。
///
/// store 集中持有任务状态(D-007): 每次轮询前从 store 取最新 Job(其 raw_meta 携带句柄)
/// 传给 provider.poll(&job), poll 后用 update_status 写回。这样新进程也能用同一 store 续查。
///
/// 取消语义: MVP 先用超时 + 退避轮询; 显式 cancel 信号留待后续(provider.cancel 已就绪)。
pub async fn poll_to_terminal(
    provider: Arc<dyn Provider>,
    store: &JobStore,
    mut job: Job,
    cfg: &RunConfig,
) -> anyhow::Result<Job> {
    // 已经是终态直接返回(同步 provider 的 submit 可能直接给终态)
    if job.status.is_terminal() {
        return Ok(job);
    }

    let start = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        // 超时检查: 从 submit 起算
        if start.elapsed() >= cfg.timeout {
            anyhow::bail!(
                "任务 {} 轮询超时({}s), 最后状态: {}",
                job.id,
                cfg.timeout.as_secs(),
                job.status.as_str()
            );
        }

        // 退避等待后再 poll
        let delay = next_backoff(cfg, attempt);
        tokio::time::sleep(delay).await;
        attempt = attempt.saturating_add(1);

        // 每次轮询前从 store 取最新 Job(句柄在 raw_meta 里), 容忍记录被并发更新。
        // 取不到(理论上不应发生, submit 已存)则退回用上一轮内存中的 job。
        let latest = match store.get(&job.id)? {
            Some(rec) => rec.to_job(),
            None => job.clone(),
        };

        // 真正轮询一次, 入参是带句柄的完整 Job
        let polled = provider.poll(&latest).await?;
        job = polled;

        // 把本轮结果写回 store: 状态 + 错误 + 产物(result_json)。
        // 序列化失败折叠成 None(不应发生), 不阻断轮询。
        let result_json = serde_json::to_value(&job.outputs).ok();
        store.update_status(&job.id, job.status, job.error.as_deref(), result_json.as_ref())?;

        // 穷尽 match: 终态退出, 非终态继续
        match job.status {
            JobStatus::Succeeded => return Ok(job),
            JobStatus::Failed => {
                let msg = job.error.clone().unwrap_or_else(|| "未知错误".to_string());
                anyhow::bail!("任务 {} 失败: {}", job.id, msg);
            }
            JobStatus::Queued => continue,
            JobStatus::Running => continue,
        }
    }
}

/// 并发地提交并等待一批请求。返回与输入等长的结果向量(保持顺序)。
///
/// 用 Semaphore 做有界并发: 同时在飞的任务数不超过 cfg.concurrency。
/// 单个任务失败不影响其他任务, 各自的 Result 独立返回(部分失败可被上层区分)。
pub async fn run_batch(
    provider: Arc<dyn Provider>,
    store: Arc<JobStore>,
    requests: Vec<GenRequest>,
    cfg: RunConfig,
) -> Vec<anyhow::Result<Job>> {
    // 信号量限制并发
    let sem = Arc::new(Semaphore::new(cfg.concurrency));
    let cfg = Arc::new(cfg);

    // 为每个请求生成一个带索引的 future, 索引用于最后排序还原顺序
    let mut handles = Vec::with_capacity(requests.len());
    for (idx, req) in requests.into_iter().enumerate() {
        let sem = Arc::clone(&sem);
        let cfg = Arc::clone(&cfg);
        let provider = Arc::clone(&provider);
        let store = Arc::clone(&store);

        // spawn 独立任务; 内部先拿信号量许可再 submit, 控制并发
        let handle = tokio::spawn(async move {
            // 许可在作用域内持有, drop 时自动归还
            let _permit = sem
                .acquire()
                .await
                .expect("信号量不会被关闭");
            // submit -> save -> poll -> update 全链路
            let result = submit_and_wait(provider, &store, req, &cfg).await;
            (idx, result)
        });
        handles.push(handle);
    }

    // 收集所有结果, 按 idx 还原顺序
    let mut collected: Vec<(usize, anyhow::Result<Job>)> = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(pair) => collected.push(pair),
            Err(join_err) => {
                // tokio 任务 panic / 被取消, 包成普通错误。索引已丢失, 追加到末尾。
                collected.push((usize::MAX, Err(anyhow::anyhow!("任务执行异常: {}", join_err))));
            }
        }
    }
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, r)| r).collect()
}

/// 只做"提交 + 存库"一步(不轮询), 返回带句柄的 Job。
///
/// 拆出它是为了让 route 编排层把"提交重试"与"轮询重试"分开处理(D-006 幂等要求):
/// 提交失败可安全重发(生成类无幂等键, 每次重发是新任务); 但一旦提交成功、句柄已落库,
/// 后续只该重试 **轮询**(复用已存句柄), 绝不重新 submit, 以免产生重复任务/重复扣费。
/// capability/model 由调用方传入(Job 本身不带这些编排层元信息), 用于落库记录。
pub async fn submit_once(
    provider: Arc<dyn Provider>,
    store: &JobStore,
    req: GenRequest,
    capability: crate::core::provider::Capability,
    model: &str,
) -> anyhow::Result<Job> {
    // 请求原文落库便于复现/审计; 序列化失败折叠成 None, 不阻断主流程。
    let request_json = serde_json::to_value(&req).ok();
    // 提交。异步 provider 返回 Queued/Running, 同步 provider 直接给终态。
    let job = provider.submit(req).await?;
    // 立刻存库: 句柄(raw_meta)在此持久化, 之后重试轮询/换进程都能据此续查。
    let now = now_unix();
    let record = JobRecord::from_job(&job, capability, model, request_json, now, now);
    store.save(&record)?;
    Ok(job)
}

/// 单请求的 submit + 存库 + 轮询闭环。
///
/// 流程: submit 拿到带句柄的 Job -> 立刻 store.save(持久化句柄, 跨进程可续) -> 退避轮询,
/// 每轮从 store 读最新 Job、poll、update_status 写回。
pub async fn submit_and_wait(
    provider: Arc<dyn Provider>,
    store: &JobStore,
    req: GenRequest,
    cfg: &RunConfig,
) -> anyhow::Result<Job> {
    // submit 前先记下编排层元信息(capability/model/原始请求), 因为 req 会被 submit 消费。
    let capability = req.capability;
    let model = req.model.clone();
    // 请求原文落库便于复现/审计; 序列化失败折叠成 None, 不阻断主流程。
    let request_json = serde_json::to_value(&req).ok();

    // 提交。异步 provider 返回 Queued/Running, 同步 provider 直接给终态。
    let job = provider.submit(req).await?;

    // 立刻存库: 句柄(raw_meta)在此持久化, 之后即使换进程也能 status/download。
    let now = now_unix();
    let record = JobRecord::from_job(&job, capability, &model, request_json, now, now);
    store.save(&record)?;

    // 轮询到终态(内部每轮写回 store)
    poll_to_terminal(provider, store, job, cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_by_max_delay() {
        // 退避抖动后绝不应超过 max_delay
        let cfg = RunConfig {
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(5),
            backoff_factor: 2.0,
            ..RunConfig::default()
        };
        // 取很大的 attempt, raw 会爆表, 但必须被 max_delay 封顶
        for attempt in 0..20 {
            let d = next_backoff(&cfg, attempt);
            assert!(
                d <= cfg.max_delay,
                "attempt={} 退避 {:?} 超过上限 {:?}",
                attempt,
                d,
                cfg.max_delay
            );
        }
    }

    #[test]
    fn backoff_attempt_zero_within_base() {
        // attempt=0 时上限是 base_delay, 抖动结果不超过 base
        let cfg = RunConfig {
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
            ..RunConfig::default()
        };
        for _ in 0..50 {
            let d = next_backoff(&cfg, 0);
            assert!(d <= cfg.base_delay);
        }
    }
}
