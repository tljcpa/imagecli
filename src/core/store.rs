//! JobStore: 任务状态的 SQLite 跨进程持久化(落地 DECISIONS D-007)。
//!
//! 为什么要它: MVP 现状把 fal 句柄(status_url/response_url)存在内存 Mutex<HashMap>,
//! 换一个进程(先 generate 再单独 status/download)就丢了。本模块把每个任务的句柄与状态
//! 落到 SQLite, 让 status/download/list 跨进程可用。
//!
//! 同步性: rusqlite 是同步库。store 的调用点都不在热路径(热路径是 poll 的网络等待),
//! 每次调用只做一条本地 SQL, 临界区极短。这里用 Mutex<Connection> 让 JobStore 满足 Sync,
//! 可直接被 Arc 跨 tokio 任务共享; 不引入 spawn_blocking 以保持简单。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::core::provider::{Asset, Capability, Job, JobStatus};

/// 覆盖 db 路径的环境变量名(测试/隔离用)。
const DB_PATH_ENV: &str = "IMAGECLI_DB_PATH";

/// 取当前 unix 秒。允许用 SystemTime(D-007/规则允许), 不引入隐藏时间依赖。
pub fn now_unix() -> i64 {
    // 从 UNIX_EPOCH 起的秒数; 系统时钟早于 1970 的异常情况折叠成 0
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

/// 一条任务记录, 与数据库 `jobs` 表一一对应。
///
/// 与 core::provider::Job 的区别: JobRecord 是"存储视角", 多出 model/capability/
/// request_json/时间戳等编排层元数据; Job 是"运行视角", 只关心 provider 轮询所需。
#[derive(Debug, Clone)]
pub struct JobRecord {
    pub job_id: String,
    pub provider: String,
    pub model: String,
    pub capability: String,
    pub status: String,
    pub error: Option<String>,
    /// 原始元数据 JSON。fal 在此存 status_url/response_url/cancel_url 句柄。
    pub raw_meta: Value,
    /// 提交时的归一化请求 JSON(便于复现/审计), 可空。
    pub request_json: Option<Value>,
    /// 结果 JSON: 这里存产物素材数组(Vec<Asset> 序列化), 便于 download 跨进程还原。
    pub result_json: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl JobRecord {
    /// 由运行视角的 Job 构造存储记录。capability/model/request_json/时间戳由外部传入,
    /// 因为 Job 本身不携带这些编排层信息。
    pub fn from_job(
        job: &Job,
        capability: Capability,
        model: &str,
        request_json: Option<Value>,
        created_at: i64,
        updated_at: i64,
    ) -> JobRecord {
        // outputs 序列化进 result_json(失败折叠成 None, 不阻断主流程)
        let result_json = serde_json::to_value(&job.outputs).ok();
        JobRecord {
            job_id: job.id.clone(),
            provider: job.provider.clone(),
            model: model.to_string(),
            capability: capability.as_str().to_string(),
            status: job.status.as_str().to_string(),
            error: job.error.clone(),
            raw_meta: job.raw_meta.clone(),
            request_json,
            result_json,
            created_at,
            updated_at,
        }
    }

    /// 还原成运行视角的 Job。raw_meta(含句柄)与 outputs 都从记录恢复,
    /// 这样 provider.poll(&job) 能在新进程里拿到句柄继续轮询。
    pub fn to_job(&self) -> Job {
        // outputs 从 result_json 反序列化; 解析不出就给空向量
        let outputs: Vec<Asset> = match &self.result_json {
            Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
            None => Vec::new(),
        };
        Job {
            id: self.job_id.clone(),
            provider: self.provider.clone(),
            status: JobStatus::parse(&self.status),
            outputs,
            error: self.error.clone(),
            raw_meta: self.raw_meta.clone(),
        }
    }
}

/// list 过滤条件。全部可选, None 表示不限制该维度。
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub status: Option<String>,
    pub capability: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// 任务存储句柄。内部持有一条 SQLite 连接, 用 Mutex 串行化访问以满足 Sync。
pub struct JobStore {
    conn: Mutex<Connection>,
}

impl JobStore {
    /// 解析 db 文件路径: 优先 IMAGECLI_DB_PATH, 否则 XDG data dir 下 imagecli/jobs.db。
    fn resolve_db_path() -> anyhow::Result<PathBuf> {
        // 环境变量覆盖(测试与隔离用)
        if let Ok(p) = std::env::var(DB_PATH_ENV) {
            if !p.trim().is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
        // XDG data dir: linux 下为 ~/.local/share/imagecli
        let dirs = directories::ProjectDirs::from("", "", "imagecli")
            .context("无法确定用户数据目录(XDG data dir)")?;
        let data_dir = dirs.data_dir().to_path_buf();
        Ok(data_dir.join("jobs.db"))
    }

    /// 打开(或创建)默认位置的 store。建表幂等。
    pub fn open() -> anyhow::Result<JobStore> {
        let path = Self::resolve_db_path()?;
        Self::open_at(&path)
    }

    /// 在指定路径打开 store, 父目录不存在则创建。建表幂等。
    pub fn open_at(path: &std::path::Path) -> anyhow::Result<JobStore> {
        // 确保父目录存在
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建数据目录失败: {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开任务数据库失败: {}", path.display()))?;
        Self::init_schema(&conn)?;
        Ok(JobStore {
            conn: Mutex::new(conn),
        })
    }

    /// 建表。CREATE TABLE IF NOT EXISTS 保证幂等。
    fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                job_id       TEXT PRIMARY KEY,
                provider     TEXT NOT NULL,
                model        TEXT NOT NULL,
                capability   TEXT NOT NULL,
                status       TEXT NOT NULL,
                error        TEXT,
                raw_meta     TEXT NOT NULL,
                request_json TEXT,
                result_json  TEXT,
                created_at   INTEGER NOT NULL,
                updated_at   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
            CREATE INDEX IF NOT EXISTS idx_jobs_capability ON jobs(capability);",
        )?;
        Ok(())
    }

    /// upsert 一条记录(按 job_id 主键冲突则整体覆盖)。
    pub fn save(&self, rec: &JobRecord) -> anyhow::Result<()> {
        // JSON 列以字符串形式存储
        let raw_meta = serde_json::to_string(&rec.raw_meta)?;
        let request_json = json_opt_to_string(&rec.request_json)?;
        let result_json = json_opt_to_string(&rec.result_json)?;

        let conn = self.lock();
        conn.execute(
            "INSERT INTO jobs
                (job_id, provider, model, capability, status, error,
                 raw_meta, request_json, result_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(job_id) DO UPDATE SET
                provider=excluded.provider,
                model=excluded.model,
                capability=excluded.capability,
                status=excluded.status,
                error=excluded.error,
                raw_meta=excluded.raw_meta,
                request_json=excluded.request_json,
                result_json=excluded.result_json,
                updated_at=excluded.updated_at",
            params![
                rec.job_id,
                rec.provider,
                rec.model,
                rec.capability,
                rec.status,
                rec.error,
                raw_meta,
                request_json,
                result_json,
                rec.created_at,
                rec.updated_at,
            ],
        )?;
        Ok(())
    }

    /// 按 job_id 取一条记录。不存在返回 None。
    pub fn get(&self, job_id: &str) -> anyhow::Result<Option<JobRecord>> {
        let conn = self.lock();
        let rec = conn
            .query_row(
                "SELECT job_id, provider, model, capability, status, error,
                        raw_meta, request_json, result_json, created_at, updated_at
                 FROM jobs WHERE job_id = ?1",
                params![job_id],
                row_to_record,
            )
            .optional()?;
        Ok(rec)
    }

    /// 更新状态/错误/结果, 并刷新 updated_at。raw_meta 不动(句柄在 submit 时已定, 不变)。
    pub fn update_status(
        &self,
        job_id: &str,
        status: JobStatus,
        error: Option<&str>,
        result_json: Option<&Value>,
    ) -> anyhow::Result<()> {
        let result_str = match result_json {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let now = now_unix();
        let conn = self.lock();
        conn.execute(
            "UPDATE jobs SET status=?1, error=?2, result_json=?3, updated_at=?4 WHERE job_id=?5",
            params![status.as_str(), error, result_str, now, job_id],
        )?;
        Ok(())
    }

    /// 按过滤条件列出记录。过滤在 SQL WHERE 完成, 不全表拉回内存再过滤。
    /// 排序: 按 created_at 倒序(最新在前), 输出稳定。
    pub fn list(&self, filter: &JobFilter) -> anyhow::Result<Vec<JobRecord>> {
        // 动态拼 WHERE。用占位符 + 参数向量, 杜绝 SQL 注入。
        let mut sql = String::from(
            "SELECT job_id, provider, model, capability, status, error,
                    raw_meta, request_json, result_json, created_at, updated_at
             FROM jobs",
        );
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(s) = &filter.status {
            conditions.push(format!("status = ?{}", binds.len() + 1));
            binds.push(Box::new(s.clone()));
        }
        if let Some(c) = &filter.capability {
            conditions.push(format!("capability = ?{}", binds.len() + 1));
            binds.push(Box::new(c.clone()));
        }
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(" ORDER BY created_at DESC");

        // LIMIT/OFFSET 同样走占位符
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT ?{}", binds.len() + 1));
            binds.push(Box::new(limit));
            if let Some(offset) = filter.offset {
                sql.push_str(&format!(" OFFSET ?{}", binds.len() + 1));
                binds.push(Box::new(offset));
            }
        }

        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        // 把 Box<dyn ToSql> 转成 &dyn ToSql 切片喂给 query_map
        let param_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 取连接锁。锁中毒(持锁线程 panic)极不可能, 这里直接 expect。
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("jobs db 连接锁不应被毒化")
    }
}

/// 把可空 JSON 值转成可空字符串(供 SQL 绑定)。
fn json_opt_to_string(v: &Option<Value>) -> anyhow::Result<Option<String>> {
    match v {
        Some(val) => Ok(Some(serde_json::to_string(val)?)),
        None => Ok(None),
    }
}

/// 把字符串 JSON 列解析回 Value; NULL/解析失败折叠为 None。
fn parse_json_column(s: Option<String>) -> Option<Value> {
    match s {
        Some(text) => serde_json::from_str(&text).ok(),
        None => None,
    }
}

/// 把一行 SQL 结果映射成 JobRecord。
fn row_to_record(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
    // raw_meta 列非空; 解析失败给 Null 兜底(不应发生)
    let raw_meta_str: String = row.get(6)?;
    let raw_meta = serde_json::from_str(&raw_meta_str).unwrap_or(Value::Null);
    let request_json: Option<String> = row.get(7)?;
    let result_json: Option<String> = row.get(8)?;
    Ok(JobRecord {
        job_id: row.get(0)?,
        provider: row.get(1)?,
        model: row.get(2)?,
        capability: row.get(3)?,
        status: row.get(4)?,
        error: row.get(5)?,
        raw_meta,
        request_json: parse_json_column(request_json),
        result_json: parse_json_column(result_json),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{Asset, AssetKind};
    use serde_json::json;

    /// 用临时文件路径开一个隔离 store。
    fn temp_store() -> (JobStore, PathBuf) {
        // 用进程 id + 纳秒时间戳拼唯一文件名, 避免并发测试互相踩
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("imagecli_test_{}_{}.db", std::process::id(), nanos));
        let store = JobStore::open_at(&path).expect("打开临时 store 失败");
        (store, path)
    }

    fn sample_job() -> Job {
        Job {
            id: "req-123".to_string(),
            provider: "fal".to_string(),
            status: JobStatus::Queued,
            outputs: Vec::new(),
            error: None,
            raw_meta: json!({
                "status_url": "https://queue.fal.run/x/status",
                "response_url": "https://queue.fal.run/x",
            }),
        }
    }

    #[test]
    fn save_and_get_roundtrip() {
        let (store, path) = temp_store();
        let job = sample_job();
        let rec = JobRecord::from_job(&job, Capability::Text2Image, "fal-ai/flux/dev", None, 100, 100);
        store.save(&rec).unwrap();

        let got = store.get("req-123").unwrap().expect("应能取到刚存的记录");
        assert_eq!(got.job_id, "req-123");
        assert_eq!(got.provider, "fal");
        assert_eq!(got.capability, "text2image");
        assert_eq!(got.status, "queued");
        // raw_meta 句柄应原样还原
        let restored = got.to_job();
        assert_eq!(
            restored.raw_meta.get("status_url").and_then(|v| v.as_str()),
            Some("https://queue.fal.run/x/status")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn update_status_and_result_roundtrip() {
        let (store, path) = temp_store();
        let job = sample_job();
        let rec = JobRecord::from_job(&job, Capability::Text2Image, "m", None, 1, 1);
        store.save(&rec).unwrap();

        // 模拟成功: 写入产物
        let outputs = vec![Asset::from_url(AssetKind::Image, "https://cdn/a.png")];
        let result_json = serde_json::to_value(&outputs).unwrap();
        store
            .update_status("req-123", JobStatus::Succeeded, None, Some(&result_json))
            .unwrap();

        let got = store.get("req-123").unwrap().unwrap();
        assert_eq!(got.status, "succeeded");
        let restored = got.to_job();
        assert_eq!(restored.status, JobStatus::Succeeded);
        assert_eq!(restored.outputs.len(), 1);
        assert_eq!(restored.outputs[0].url.as_deref(), Some("https://cdn/a.png"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_filters_by_status_in_sql() {
        let (store, path) = temp_store();
        // 存三条: 两条 succeeded, 一条 queued
        for (i, st) in [JobStatus::Succeeded, JobStatus::Queued, JobStatus::Succeeded]
            .iter()
            .enumerate()
        {
            let mut job = sample_job();
            job.id = format!("j{}", i);
            job.status = *st;
            let rec = JobRecord::from_job(&job, Capability::Text2Image, "m", None, i as i64, i as i64);
            store.save(&rec).unwrap();
        }

        let filter = JobFilter {
            status: Some("succeeded".to_string()),
            ..JobFilter::default()
        };
        let got = store.list(&filter).unwrap();
        assert_eq!(got.len(), 2, "应只返回两条 succeeded");
        for r in got.iter() {
            assert_eq!(r.status, "succeeded");
        }

        // limit 生效
        let filter2 = JobFilter {
            limit: Some(1),
            ..JobFilter::default()
        };
        let got2 = store.list(&filter2).unwrap();
        assert_eq!(got2.len(), 1);
        let _ = std::fs::remove_file(path);
    }
}
