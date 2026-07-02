//! 多 provider 路由 + 故障转移 + 重试编排(D-006 工程十条核心三块)。
//!
//! 这是在 runner(单 provider 的并发提交 + 退避轮询)之上的一层"候选链编排":
//! 每个生成请求带一条 **候选 provider 链**(主 + 备), runner 负责"对一个 provider 跑通
//! submit/poll", 本模块负责"主失败就退避重试、再不行就切下一个候选"。
//!
//! 与批量(fan-out)正交: 上层把 N 个 prompt fan-out 成 N 个 `RequestTemplate`,
//! 每个 template 各自独立走同一条候选链(`run_unit`); `run_batch_routed` 用信号量做有界并发,
//! 与 runner::run_batch 同构。一个 prompt 走到哪个候选、重试了几次, 互不影响别的 prompt。
//!
//! 三个关键区分(逐一对应 D-006 的要求):
//! 1. **可重试 vs 不可重试**: 由 retry::classify_error 判(429/5xx/超时/网络 可重试;
//!    鉴权/参数/缺 key 不可重试)。不可重试错误不浪费退避, 直接切 fallback。
//! 2. **提交重试 vs 轮询重试(幂等)**: 生成类请求无幂等键, 重新 submit = 新任务(可能重复扣费)。
//!    故"提交失败"才重发 submit; 一旦提交成功、句柄落库, 后续只重试 **轮询**(复用已存句柄),
//!    绝不重新 submit。两类退避分开计数。
//! 3. **轮询通信失败 vs 任务终态失败**: 前者(网络抖动)可重试轮询; 后者(任务真的 Failed)
//!    重试轮询无意义, 直接切 fallback。靠重读 store 里该任务的状态来区分。

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::core::provider::{Asset, Capability, GenRequest, Job, JobStatus, Provider};
use crate::core::retry::{classify_error, looks_like_quota_exhausted};
use crate::core::runner::{next_backoff, poll_to_terminal, submit_once, RunConfig};
use crate::core::store::JobStore;

/// 一个候选: provider 实例 + 它在本次能力下要用的 model。
///
/// model 必须随候选走, 因为不同 provider 的默认 model 不同(fal-ai/flux/dev vs
/// agnes-image-2.1-flash...), 切到下一家时请求体里的 model 也要随之换成那家的。
#[derive(Clone)]
pub struct Candidate {
    pub name: String,
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

/// 一个生成单元的模板(fan-out 的最小单位): 共享 capability/inputs/params, prompt 各异。
///
/// 不含 model: model 由候选注入(见 Candidate)。一个 template 对应一个 prompt 的"作业",
/// 它会被原样喂给候选链里的每一家(只换 model)。
#[derive(Clone)]
pub struct RequestTemplate {
    pub capability: Capability,
    pub prompt: Option<String>,
    pub inputs: Vec<Asset>,
    pub params: serde_json::Map<String, Value>,
}

impl RequestTemplate {
    /// 针对某个候选(注入其 model)构造一个具体 GenRequest。
    fn to_request(&self, model: &str) -> GenRequest {
        GenRequest {
            capability: self.capability,
            model: model.to_string(),
            prompt: self.prompt.clone(),
            inputs: self.inputs.clone(),
            params: self.params.clone(),
        }
    }
}

/// 路由/重试配置。run 是底层并发与退避(复用 runner), retries 是"每个 provider 的重试次数"。
#[derive(Clone)]
pub struct RouteConfig {
    /// 底层 runner 配置(并发/退避/超时)。
    pub run: RunConfig,
    /// 每个 provider 对可重试错误的额外重试次数。N=2 表示最多 1+2=3 次尝试。
    pub retries: u32,
}

/// 一条尝试事件(可观测性): 记录某次 submit/poll 的 provider/model/阶段/耗时/关联 id/结果。
///
/// Serialize 以便直接进 --json 的 events; 同时有 human 文本格式(format_event)供 verbose 打 stderr。
#[derive(Debug, Clone, Serialize)]
pub struct AttemptEvent {
    /// 本次事件所属 provider。
    pub provider: String,
    /// 本次事件所用 model。
    pub model: String,
    /// 阶段: "submit" / "poll" / "fallback"。
    pub phase: String,
    /// 该 provider 内该阶段的第几次尝试(1 基)。
    pub attempt: u32,
    /// 是否成功。
    pub ok: bool,
    /// 失败时是否被判为可重试(成功或 fallback 事件为 None)。
    pub retryable: Option<bool>,
    /// 关联的 provider 侧请求 id(fal request_id / replicate prediction_id /
    /// 异步 task_id / 火山 logid 等), 从 Job.raw_meta 提取, 便于和各家后台对账。
    pub request_id: Option<String>,
    /// 本次尝试耗时(毫秒)。
    pub elapsed_ms: u128,
    /// 失败时的错误摘要(成功为 None)。
    pub error: Option<String>,
}

/// 一个生成单元跑完候选链后的结果(含可观测性元数据)。
pub struct UnitOutcome {
    /// 最终结果: 某候选成功的 Job, 或全部候选失败的错误。
    pub result: anyhow::Result<Job>,
    /// 实际产出结果(或最后尝试)的 provider 名。
    pub provider_used: Option<String>,
    /// 实际所用 model。
    pub model_used: Option<String>,
    /// 总提交尝试次数(跨所有候选累加)。
    pub attempts: u32,
    /// 本单元总耗时(毫秒)。
    pub elapsed_ms: u128,
    /// 在产出最终结果的 provider 之前, 已失败被跳过的 provider 名(发生过切换才非空)。
    pub fallback_from: Vec<String>,
    /// 全过程尝试事件流(verbose / --json 用)。
    pub events: Vec<AttemptEvent>,
    /// 该单元对应的 prompt(回填报告用)。
    pub prompt: Option<String>,
    /// 是否疑似配额/限流耗尽(给中文建议用): 任一候选错误命中配额信号即置位。
    pub quota_hint: bool,
}

/// 从 Job.raw_meta 提取各家的关联请求 id(纯函数, 便于单测)。
///
/// 各 provider 句柄字段名不同: fal=request_id, replicate=prediction_id, 异步骨架=task_id,
/// 火山=logid。按优先级取第一个非空字符串, 取不到返回 None(同步 provider 可能没有)。
pub fn extract_request_id(raw_meta: &Value) -> Option<String> {
    const KEYS: &[&str] = &["request_id", "prediction_id", "task_id", "logid", "id", "req_key"];
    for k in KEYS.iter() {
        if let Some(s) = raw_meta.get(*k).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 把一条事件格式化成 verbose stderr 的一行(不含 key, 安全)。
pub fn format_event(ev: &AttemptEvent) -> String {
    let mut s = format!(
        "[trace] provider={} model={} phase={} attempt={} {} {}ms",
        ev.provider,
        ev.model,
        ev.phase,
        ev.attempt,
        match ev.ok {
            true => "OK",
            false => "ERR",
        },
        ev.elapsed_ms,
    );
    if let Some(rid) = &ev.request_id {
        s.push_str(&format!(" request_id={}", rid));
    }
    if let Some(r) = ev.retryable {
        s.push_str(match r {
            true => " (可重试)",
            false => " (不可重试)",
        });
    }
    if let Some(e) = &ev.error {
        // 错误可能很长, 截断到 200 字符避免刷屏。
        let trimmed: String = e.chars().take(200).collect();
        s.push_str(&format!(" error={}", trimmed));
    }
    s
}

/// 对单个生成单元跑候选链: 主失败(退避重试后仍失败 / 不可重试)就切下一个候选。
///
/// 返回 UnitOutcome(永不 panic; 全部候选失败时 result 为 Err)。
pub async fn run_unit(
    chain: &[Candidate],
    store: &JobStore,
    tmpl: &RequestTemplate,
    cfg: &RouteConfig,
) -> UnitOutcome {
    let unit_start = Instant::now();
    let mut events: Vec<AttemptEvent> = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    let mut total_attempts: u32 = 0;
    let mut last_err: Option<anyhow::Error> = None;
    let mut quota_hint = false;

    // 空候选链(不应发生, 上层已校验)兜底成失败。
    if chain.is_empty() {
        return UnitOutcome {
            result: Err(anyhow::anyhow!("无可用候选 provider(候选链为空)")),
            provider_used: None,
            model_used: None,
            attempts: 0,
            elapsed_ms: unit_start.elapsed().as_millis(),
            fallback_from: Vec::new(),
            events,
            prompt: tmpl.prompt.clone(),
            quota_hint,
        };
    }

    for cand in chain.iter() {
        tried.push(cand.name.clone());

        // ---------- 阶段一: 提交(可重试; 每次重发都是新任务, 因生成类无幂等键)----------
        let mut submit_attempt: u32 = 0;
        let submitted: Option<Job> = loop {
            submit_attempt += 1;
            total_attempts += 1;
            let t0 = Instant::now();
            let req = tmpl.to_request(&cand.model);
            match submit_once(
                Arc::clone(&cand.provider),
                store,
                req,
                tmpl.capability,
                &cand.model,
            )
            .await
            {
                Ok(job) => {
                    events.push(AttemptEvent {
                        provider: cand.name.clone(),
                        model: cand.model.clone(),
                        phase: "submit".to_string(),
                        attempt: submit_attempt,
                        ok: true,
                        retryable: None,
                        request_id: extract_request_id(&job.raw_meta),
                        elapsed_ms: t0.elapsed().as_millis(),
                        error: None,
                    });
                    break Some(job);
                }
                Err(e) => {
                    let class = classify_error(&e);
                    if looks_like_quota_exhausted(&e) {
                        quota_hint = true;
                    }
                    events.push(AttemptEvent {
                        provider: cand.name.clone(),
                        model: cand.model.clone(),
                        phase: "submit".to_string(),
                        attempt: submit_attempt,
                        ok: false,
                        retryable: Some(class.is_retryable()),
                        request_id: None,
                        elapsed_ms: t0.elapsed().as_millis(),
                        error: Some(format!("{:#}", e)),
                    });
                    last_err = Some(e);
                    // 可重试且还有重试预算 -> 退避后重发 submit(新任务)。
                    if class.is_retryable() && submit_attempt <= cfg.retries {
                        let delay = next_backoff(&cfg.run, submit_attempt - 1);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    // 不可重试 / 预算耗尽 -> 放弃本 provider 的提交。
                    break None;
                }
            }
        };

        let job = match submitted {
            Some(j) => j,
            None => {
                // 本候选提交彻底失败 -> 若还有下一家, 记一次 fallback 事件, 切换。
                push_fallback_event(&mut events, cand, chain, &tried);
                continue;
            }
        };

        // ---------- 阶段二: 轮询(只重试轮询, 绝不重新 submit; 复用已落库句柄)----------
        let mut poll_attempt: u32 = 0;
        loop {
            poll_attempt += 1;
            let t0 = Instant::now();
            match poll_to_terminal(Arc::clone(&cand.provider), store, job.clone(), &cfg.run).await {
                Ok(done) => {
                    // poll_to_terminal 只在成功终态返回 Ok; 但"入口即终态 Failed"(同步 provider
                    // 直接给 Failed)也会原样 Ok 返回, 这里显式区分。
                    if done.status == JobStatus::Succeeded {
                        events.push(AttemptEvent {
                            provider: cand.name.clone(),
                            model: cand.model.clone(),
                            phase: "poll".to_string(),
                            attempt: poll_attempt,
                            ok: true,
                            retryable: None,
                            request_id: extract_request_id(&done.raw_meta),
                            elapsed_ms: t0.elapsed().as_millis(),
                            error: None,
                        });
                        return success_outcome(
                            done, cand, tried, total_attempts, unit_start, events, tmpl, quota_hint,
                        );
                    }
                    // 入口即终态 Failed: 任务真失败, 重试轮询无意义 -> 切 fallback。
                    let msg = done.error.clone().unwrap_or_else(|| "任务终态失败".to_string());
                    events.push(AttemptEvent {
                        provider: cand.name.clone(),
                        model: cand.model.clone(),
                        phase: "poll".to_string(),
                        attempt: poll_attempt,
                        ok: false,
                        retryable: Some(false),
                        request_id: extract_request_id(&done.raw_meta),
                        elapsed_ms: t0.elapsed().as_millis(),
                        error: Some(msg.clone()),
                    });
                    last_err = Some(anyhow::anyhow!("{} 任务失败: {}", cand.name, msg));
                    break;
                }
                Err(e) => {
                    // 区分"任务真的终态失败"与"轮询通信抖动": 重读 store 看状态是否已落 failed。
                    let terminal_failed = match store.get(&job.id) {
                        Ok(Some(rec)) => rec.status == "failed",
                        _ => false,
                    };
                    let class = classify_error(&e);
                    if looks_like_quota_exhausted(&e) {
                        quota_hint = true;
                    }
                    // 终态失败不可重试轮询; 否则按错误分类决定是否重试轮询。
                    let can_retry_poll =
                        !terminal_failed && class.is_retryable() && poll_attempt <= cfg.retries;
                    events.push(AttemptEvent {
                        provider: cand.name.clone(),
                        model: cand.model.clone(),
                        phase: "poll".to_string(),
                        attempt: poll_attempt,
                        ok: false,
                        retryable: Some(can_retry_poll),
                        request_id: extract_request_id(&job.raw_meta),
                        elapsed_ms: t0.elapsed().as_millis(),
                        error: Some(format!("{:#}", e)),
                    });
                    last_err = Some(e);
                    if can_retry_poll {
                        // 重试轮询: 复用已落库的同一任务句柄, 不重新 submit(幂等)。
                        let delay = next_backoff(&cfg.run, poll_attempt - 1);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    break;
                }
            }
        }

        // 走到这说明本候选(轮询阶段)失败 -> 若还有下一家, 记 fallback 事件并切换。
        push_fallback_event(&mut events, cand, chain, &tried);
        // 继续下一候选。
    }

    // 全部候选失败。
    let provider_used = tried.last().cloned();
    let model_used = chain
        .iter()
        .find(|c| Some(&c.name) == provider_used.as_ref())
        .map(|c| c.model.clone());
    let fallback_from = fallback_from_of(&tried);
    UnitOutcome {
        result: Err(last_err.unwrap_or_else(|| anyhow::anyhow!("所有候选 provider 均失败"))),
        provider_used,
        model_used,
        attempts: total_attempts,
        elapsed_ms: unit_start.elapsed().as_millis(),
        fallback_from,
        events,
        prompt: tmpl.prompt.clone(),
        quota_hint,
    }
}

/// 成功收尾: 组装成功的 UnitOutcome。
#[allow(clippy::too_many_arguments)]
fn success_outcome(
    done: Job,
    cand: &Candidate,
    tried: Vec<String>,
    total_attempts: u32,
    unit_start: Instant,
    events: Vec<AttemptEvent>,
    tmpl: &RequestTemplate,
    quota_hint: bool,
) -> UnitOutcome {
    let fallback_from = fallback_from_of(&tried);
    UnitOutcome {
        result: Ok(done),
        provider_used: Some(cand.name.clone()),
        model_used: Some(cand.model.clone()),
        attempts: total_attempts,
        elapsed_ms: unit_start.elapsed().as_millis(),
        fallback_from,
        events,
        prompt: tmpl.prompt.clone(),
        quota_hint,
    }
}

/// fallback_from = 已尝试候选里除"最后一个(产出最终结果者)"之外的全部。
fn fallback_from_of(tried: &[String]) -> Vec<String> {
    if tried.len() <= 1 {
        return Vec::new();
    }
    tried[..tried.len() - 1].to_vec()
}

/// 若当前候选不是链上最后一家, 记一条 fallback 切换事件(便于 verbose/审计看到"切了")。
fn push_fallback_event(
    events: &mut Vec<AttemptEvent>,
    cand: &Candidate,
    chain: &[Candidate],
    tried: &[String],
) {
    let is_last = tried.len() >= chain.len();
    if is_last {
        return;
    }
    events.push(AttemptEvent {
        provider: cand.name.clone(),
        model: cand.model.clone(),
        phase: "fallback".to_string(),
        attempt: 0,
        ok: false,
        retryable: None,
        request_id: None,
        elapsed_ms: 0,
        error: Some(format!("{} 失败, 切换下一个候选", cand.name)),
    });
}

/// 有界并发地跑一批生成单元, 每个单元各走同一条候选链。返回与输入等长同序的结果。
pub async fn run_batch_routed(
    chain: Vec<Candidate>,
    store: Arc<JobStore>,
    templates: Vec<RequestTemplate>,
    cfg: RouteConfig,
) -> Vec<UnitOutcome> {
    let sem = Arc::new(Semaphore::new(cfg.run.concurrency.max(1)));
    let chain = Arc::new(chain);
    let cfg = Arc::new(cfg);

    let mut handles = Vec::with_capacity(templates.len());
    for (idx, tmpl) in templates.into_iter().enumerate() {
        let sem = Arc::clone(&sem);
        let chain = Arc::clone(&chain);
        let cfg = Arc::clone(&cfg);
        let store = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("信号量不会被关闭");
            let outcome = run_unit(&chain, &store, &tmpl, &cfg).await;
            (idx, outcome)
        });
        handles.push(handle);
    }

    let mut collected: Vec<(usize, UnitOutcome)> = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(pair) => collected.push(pair),
            Err(join_err) => {
                // tokio 任务异常: 包成一个失败 UnitOutcome, 索引置末尾。
                collected.push((
                    usize::MAX,
                    UnitOutcome {
                        result: Err(anyhow::anyhow!("任务执行异常: {}", join_err)),
                        provider_used: None,
                        model_used: None,
                        attempts: 0,
                        elapsed_ms: 0,
                        fallback_from: Vec::new(),
                        events: Vec::new(),
                        prompt: None,
                        quota_hint: false,
                    },
                ));
            }
        }
    }
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, o)| o).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{Asset, AssetKind};
    use crate::core::retry::HttpError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// 极小退避 + 短超时的测试 RunConfig, 避免单测真睡几百毫秒。
    fn fast_run() -> RunConfig {
        RunConfig {
            concurrency: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            timeout: Duration::from_secs(5),
            backoff_factor: 2.0,
        }
    }

    fn temp_store() -> (Arc<JobStore>, std::path::PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir()
            .join(format!("imagecli_route_{}_{}.db", std::process::id(), nanos));
        let store = JobStore::open_at(&path).expect("打开临时 store 失败");
        (Arc::new(store), path)
    }

    fn tmpl(prompt: &str) -> RequestTemplate {
        RequestTemplate {
            capability: Capability::Text2Image,
            prompt: Some(prompt.to_string()),
            inputs: Vec::new(),
            params: serde_json::Map::new(),
        }
    }

    /// 可编程的假 provider: 按配置在 submit 阶段返回成功/可重试错/不可重试错, 并计数。
    struct FakeProvider {
        name: String,
        // 行为: "ok" 提交即成功终态; "retryable" 总返回 503; "nonretryable" 总返回 401;
        //       "retryable_then_ok" 前 fail_times 次 503, 之后成功。
        behavior: String,
        fail_times: usize,
        submit_calls: Arc<AtomicUsize>,
        poll_calls: Arc<AtomicUsize>,
        caps: Vec<Capability>,
    }

    impl FakeProvider {
        fn new(name: &str, behavior: &str) -> FakeProvider {
            FakeProvider {
                name: name.to_string(),
                behavior: behavior.to_string(),
                fail_times: 0,
                submit_calls: Arc::new(AtomicUsize::new(0)),
                poll_calls: Arc::new(AtomicUsize::new(0)),
                caps: vec![Capability::Text2Image],
            }
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> &[Capability] {
            &self.caps
        }
        async fn schema(&self, _model: &str) -> anyhow::Result<Value> {
            Ok(Value::Null)
        }
        async fn submit(&self, _req: GenRequest) -> anyhow::Result<Job> {
            let n = self.submit_calls.fetch_add(1, Ordering::SeqCst) + 1;
            match self.behavior.as_str() {
                "ok" => Ok(self.success_job()),
                "retryable" => Err(HttpError::new(503, "提交失败", "busy").into()),
                "nonretryable" => Err(HttpError::new(401, "提交失败", "bad key").into()),
                "retryable_then_ok" => {
                    if n <= self.fail_times {
                        Err(HttpError::new(503, "提交失败", "busy").into())
                    } else {
                        Ok(self.success_job())
                    }
                }
                // 提交成功但留在 Running, 交给 poll 决定(用于 poll 重试测试)。
                "submit_ok_poll" => Ok(self.running_job()),
                _ => Ok(self.success_job()),
            }
        }
        async fn poll(&self, job: &Job) -> anyhow::Result<Job> {
            let n = self.poll_calls.fetch_add(1, Ordering::SeqCst) + 1;
            match self.behavior.as_str() {
                "submit_ok_poll" => {
                    // 前 fail_times 次轮询返回可重试错(不改 store 状态, 保持非终态),
                    // 之后成功。证明"poll 失败重试 poll 而非重新 submit"。
                    if n <= self.fail_times {
                        Err(HttpError::new(503, "查询状态失败", "busy").into())
                    } else {
                        let mut done = job.clone();
                        done.status = JobStatus::Succeeded;
                        done.outputs = vec![Asset::from_url(AssetKind::Image, "https://x/ok.png")];
                        Ok(done)
                    }
                }
                _ => Ok(job.clone()),
            }
        }
        async fn cancel(&self, _job: &Job) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl FakeProvider {
        fn success_job(&self) -> Job {
            Job {
                id: format!("{}-job-{}", self.name, self.submit_calls.load(Ordering::SeqCst)),
                provider: self.name.clone(),
                status: JobStatus::Succeeded,
                outputs: vec![Asset::from_url(AssetKind::Image, "https://x/ok.png")],
                error: None,
                raw_meta: serde_json::json!({ "request_id": "rid-abc" }),
            }
        }
        fn running_job(&self) -> Job {
            Job {
                id: format!("{}-job-{}", self.name, self.submit_calls.load(Ordering::SeqCst)),
                provider: self.name.clone(),
                status: JobStatus::Running,
                outputs: Vec::new(),
                error: None,
                raw_meta: serde_json::json!({ "task_id": "tid-xyz", "query_url": "https://x/q" }),
            }
        }
    }

    fn cand(p: FakeProvider) -> Candidate {
        let name = p.name.clone();
        Candidate {
            name,
            provider: Arc::new(p),
            model: "m".to_string(),
        }
    }

    #[tokio::test]
    async fn primary_fails_falls_back_to_secondary() {
        // 主 nonretryable 失败 -> 切备 ok 成功; provider_used=备, fallback_from=[主]。
        let (store, path) = temp_store();
        let chain = vec![
            cand(FakeProvider::new("primary", "nonretryable")),
            cand(FakeProvider::new("backup", "ok")),
        ];
        let cfg = RouteConfig { run: fast_run(), retries: 2 };
        let out = run_unit(&chain, &store, &tmpl("x"), &cfg).await;
        assert!(out.result.is_ok(), "应回退成功");
        assert_eq!(out.provider_used.as_deref(), Some("backup"));
        assert_eq!(out.fallback_from, vec!["primary".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn nonretryable_is_not_retried() {
        // 不可重试错误: 主只 submit 一次就切备(不浪费重试)。
        let (store, path) = temp_store();
        let prov = FakeProvider::new("primary", "nonretryable");
        let calls = Arc::clone(&prov.submit_calls);
        let chain = vec![cand(prov), cand(FakeProvider::new("backup", "ok"))];
        let cfg = RouteConfig { run: fast_run(), retries: 3 };
        let _ = run_unit(&chain, &store, &tmpl("x"), &cfg).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "不可重试错误不应重试 submit");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn retryable_retries_n_times_then_fails() {
        // 可重试错误: retries=2 -> 共 1+2=3 次 submit, 仍失败(单候选链)。
        let (store, path) = temp_store();
        let prov = FakeProvider::new("only", "retryable");
        let calls = Arc::clone(&prov.submit_calls);
        let chain = vec![cand(prov)];
        let cfg = RouteConfig { run: fast_run(), retries: 2 };
        let out = run_unit(&chain, &store, &tmpl("x"), &cfg).await;
        assert!(out.result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3, "1 次初次 + 2 次重试 = 3");
        assert_eq!(out.attempts, 3);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn retryable_then_succeeds_no_fallback() {
        // 前 2 次 503 可重试, 第 3 次成功; retries=2 够用, 不切 fallback。
        let (store, path) = temp_store();
        let mut prov = FakeProvider::new("only", "retryable_then_ok");
        prov.fail_times = 2;
        let calls = Arc::clone(&prov.submit_calls);
        let chain = vec![cand(prov)];
        let cfg = RouteConfig { run: fast_run(), retries: 2 };
        let out = run_unit(&chain, &store, &tmpl("x"), &cfg).await;
        assert!(out.result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(out.fallback_from.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn poll_failure_retries_poll_not_submit() {
        // 提交成功(1 次 submit), 轮询前 2 次可重试错, 第 3 次成功。
        // 断言: submit 只调 1 次(不因 poll 失败重新 submit), poll 调 3 次。
        let (store, path) = temp_store();
        let mut prov = FakeProvider::new("only", "submit_ok_poll");
        prov.fail_times = 2;
        let submit_calls = Arc::clone(&prov.submit_calls);
        let poll_calls = Arc::clone(&prov.poll_calls);
        let chain = vec![cand(prov)];
        let cfg = RouteConfig { run: fast_run(), retries: 3 };
        let out = run_unit(&chain, &store, &tmpl("x"), &cfg).await;
        assert!(out.result.is_ok(), "轮询重试后应成功");
        assert_eq!(submit_calls.load(Ordering::SeqCst), 1, "poll 失败绝不应重新 submit");
        assert_eq!(poll_calls.load(Ordering::SeqCst), 3, "轮询应重试到第 3 次成功");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn all_candidates_fail_reports_last_and_fallback_chain() {
        // 两家都失败: provider_used=最后一家, fallback_from=[第一家]。
        let (store, path) = temp_store();
        let chain = vec![
            cand(FakeProvider::new("a", "nonretryable")),
            cand(FakeProvider::new("b", "nonretryable")),
        ];
        let cfg = RouteConfig { run: fast_run(), retries: 1 };
        let out = run_unit(&chain, &store, &tmpl("x"), &cfg).await;
        assert!(out.result.is_err());
        assert_eq!(out.provider_used.as_deref(), Some("b"));
        assert_eq!(out.fallback_from, vec!["a".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extract_request_id_picks_known_keys() {
        assert_eq!(
            extract_request_id(&serde_json::json!({ "request_id": "r1" })).as_deref(),
            Some("r1")
        );
        assert_eq!(
            extract_request_id(&serde_json::json!({ "prediction_id": "p1" })).as_deref(),
            Some("p1")
        );
        assert_eq!(
            extract_request_id(&serde_json::json!({ "task_id": "t1" })).as_deref(),
            Some("t1")
        );
        assert!(extract_request_id(&serde_json::json!({ "nope": "x" })).is_none());
    }
}
