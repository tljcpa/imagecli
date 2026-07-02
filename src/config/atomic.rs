//! atomic: 配置/状态文件的"写入防损坏"基础设施(落地 D-013 优先项②)。
//!
//! 背景(切肤之痛): 本项目 2026-06-26 曾因磁盘写满(ENOSPC), 用 read-modify-write
//! 直接覆盖写 DECISIONS.md, 写入中途失败把文件截断到只剩 1 行。本模块借鉴 Claude Code
//! saveGlobalConfig 的设计思想(原子写 + 时间戳备份 + 损坏防护), 用 Rust std 自行实现,
//! 不引入任何第三方 crate, 也不复制其代码——只学"为什么这么做"。
//!
//! 三道防线:
//! - 原子写: 先写同目录临时文件, flush + sync_all 真正落盘, 再 rename 原子替换目标。
//!   rename 在同一文件系统上是原子操作: 要么旧文件、要么新文件, 不会出现"写了一半"的中间态。
//!   即便写临时文件时磁盘满/进程被杀, 原目标文件纹丝不动。
//! - 时间戳备份 + 轮转: 覆盖前先把现有目标复制成 `<name>.bak.<unix秒>`, 只保留最近 N 个,
//!   超出删最旧。给"内容逻辑写错(非 IO 中断)"留一条人工回滚的后路。
//! - 损坏防护: 见 settings.rs 的 load——解析失败返回错误而非静默用默认值覆盖,
//!   避免把用户配置抹掉(对应 Claude Code 的 auth-loss 防护思想)。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;

/// 备份文件名中固定的中缀: `<目标文件名>.bak.<unix秒>`。
const BACKUP_INFIX: &str = ".bak.";

/// 原子写入: 把 `contents` 安全写到 `path`, 写入中断绝不破坏 `path` 原有内容。
///
/// 步骤:
/// 1. 确保父目录存在;
/// 2. 在同目录写一个唯一命名的临时文件(`<name>.tmp.<pid>.<nanos>`);
/// 3. 写完后 flush(刷用户态缓冲) + sync_all(刷内核页缓存到磁盘), 确保数据真正落盘;
/// 4. rename 临时文件覆盖目标——同一文件系统上 rename 原子, 这一步要么成功要么不变。
///
/// 任何一步失败都会尝试清理临时文件, 不留垃圾, 也不触碰原目标文件。
pub fn atomic_write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    // 第 1 步: 父目录不存在则创建(首次写配置时目录还不存在)
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }
    }

    // 第 2 步: 计算同目录下的唯一临时文件路径
    // 必须同目录: 跨目录(如 /tmp -> 目标)的 rename 可能跨文件系统, 那样 rename 退化为
    // "复制 + 删除", 不再原子, 也可能 EXDEV 直接失败。同目录才保证 rename 原子语义。
    let tmp_path = temp_path_for(path);

    // 第 3 步: 写临时文件。用块作用域确保 File 在 rename 前被 drop(关闭句柄),
    // 否则 Windows 上持有打开句柄的文件无法被 rename 覆盖(Linux 宽松, 但统一写法更稳)。
    {
        // create_new=true: 若临时名已存在则报错而非覆盖, 避免撞到别的并发写入者的临时文件
        let mut file: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .with_context(|| format!("创建临时文件失败: {}", tmp_path.display()))?;

        // 写全部字节; write_all 内部循环直到写完或出错
        if let Err(e) = file.write_all(contents) {
            // 写失败: 清理临时文件后把错误抛上去, 原目标不受影响
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| format!("写临时文件失败: {}", tmp_path.display()));
        }

        // flush: 把 Rust/libc 用户态缓冲推给内核
        if let Err(e) = file.flush() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| format!("flush 临时文件失败: {}", tmp_path.display()));
        }

        // sync_all: 强制内核把数据 + 元数据写到物理磁盘。
        // 这一步是"防断电/防崩溃"的关键: 不 sync 的话 rename 后数据可能还在页缓存,
        // 此刻掉电会得到一个 rename 成功但内容为空的文件(经典的 ext4 0 字节陷阱)。
        if let Err(e) = file.sync_all() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| format!("sync 临时文件到磁盘失败: {}", tmp_path.display()));
        }
    }

    // 第 4 步: 原子 rename 覆盖目标
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        // rename 失败也要清理临时文件, 避免残留
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).with_context(|| {
            format!(
                "原子替换目标文件失败: {} -> {}",
                tmp_path.display(),
                path.display()
            )
        });
    }

    Ok(())
}

/// 写前备份 + 轮转: 若 `path` 已存在, 复制成带时间戳的备份, 并把同名备份保留到最近 `max_backups` 个。
///
/// - `now_secs`: 备份时间戳(unix 秒), 由外部传入便于测试; 生产用 `now_unix_secs()`。
/// - 目标不存在时直接返回 Ok(首次写, 无可备份)。
/// - 备份失败不应阻断主流程的写入决策, 但这里仍把错误抛上去, 由调用方决定是否容忍。
///   (settings 的调用方选择: 备份失败只警告不致命, 见 settings.rs。)
pub fn backup_and_rotate(path: &Path, max_backups: usize, now_secs: u64) -> anyhow::Result<()> {
    // 目标不存在: 没有东西可备份, 直接成功返回
    if !path.exists() {
        return Ok(());
    }

    // 备份文件名: 在原文件名后接 `.bak.<秒>`, 放同目录
    let file_name = path
        .file_name()
        .context("目标路径没有文件名, 无法生成备份名")?
        .to_string_lossy()
        .to_string();
    let parent = path.parent().map(PathBuf::from).unwrap_or_default();
    let backup_name = format!("{}{}{}", file_name, BACKUP_INFIX, now_secs);
    let backup_path = parent.join(&backup_name);

    // 复制现有目标到备份。copy 会读全量再写, 备份是旁路文件, 即便它写坏也不影响主文件。
    // 若同一秒内重复备份(backup_path 已存在), copy 直接覆盖同名备份, 语义上等价(同一秒同内容)。
    std::fs::copy(path, &backup_path)
        .with_context(|| format!("复制备份失败: {} -> {}", path.display(), backup_path.display()))?;

    // 轮转: 清理超出 max_backups 的旧备份
    prune_backups(&parent, &file_name, max_backups)?;
    Ok(())
}

/// 轮转辅助: 扫描 `dir` 下所有 `<base_name>.bak.<秒>` 备份, 按时间戳降序, 只保留最近 `max_keep` 个。
fn prune_backups(dir: &Path, base_name: &str, max_keep: usize) -> anyhow::Result<()> {
    // 备份名前缀, 例如 "config.toml.bak."
    let prefix = format!("{}{}", base_name, BACKUP_INFIX);

    // 收集 (时间戳, 完整路径) 列表
    let mut backups: Vec<(u64, PathBuf)> = Vec::new();
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // 目录读不了(理论上不该, 因为刚写过): 不阻断主流程, 放弃轮转
        Err(_) => return Ok(()),
    };
    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        // 只认精确前缀匹配, 且后缀必须能解析成 u64 时间戳, 避免误删别的文件
        if let Some(rest) = name.strip_prefix(&prefix) {
            if let Ok(ts) = rest.parse::<u64>() {
                backups.push((ts, entry.path()));
            }
        }
    }

    // 数量没超额: 无需删除
    if backups.len() <= max_keep {
        return Ok(());
    }

    // 按时间戳降序排序(最新在前), 删除排在 max_keep 之后的旧备份
    backups.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, old_path) in backups.into_iter().skip(max_keep) {
        // 删除失败不致命(可能被别的进程清了), 忽略即可
        let _ = std::fs::remove_file(&old_path);
    }
    Ok(())
}

/// 生成同目录下唯一的临时文件路径: `<name>.tmp.<pid>.<nanos>`。
///
/// pid + 纳秒时间戳两重唯一化: 同一进程内连续两次写也不会撞名(纳秒变化),
/// 多进程并发写也不会撞(pid 不同)。
fn temp_path_for(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 在原文件名后接临时后缀, 仍放同目录(用 with 拼接保证父目录一致)
    let file_name = match path.file_name() {
        Some(n) => n.to_string_lossy().to_string(),
        // 极端兜底: 没文件名就用固定名
        None => "imagecli_atomic".to_string(),
    };
    let tmp_name = format!("{}.tmp.{}.{}", file_name, pid, nanos);
    let parent = path.parent().map(PathBuf::from).unwrap_or_default();
    parent.join(tmp_name)
}

/// 取当前 unix 秒(供 backup_and_rotate 的生产调用方使用)。
/// 与 store::now_unix 同源思路: 允许用 SystemTime, 不引入隐藏时间依赖。
pub fn now_unix_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个隔离的临时测试目录(进程 id + 纳秒, 避免并发互踩)。
    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("imagecli_atomic_test_{}_{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_roundtrip() {
        // 原子写后内容应与写入一致
        let dir = temp_dir();
        let path = dir.join("data.txt");
        atomic_write(&path, b"hello atomic").unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "hello atomic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_creates_parent_dir() {
        // 父目录不存在也应被自动创建
        let dir = temp_dir();
        let path = dir.join("sub").join("nested").join("data.txt");
        atomic_write(&path, b"x").unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        // 写成功后同目录不应残留 .tmp.* 临时文件
        let dir = temp_dir();
        let path = dir.join("data.txt");
        atomic_write(&path, b"content").unwrap();
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "不应残留临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_overwrites_existing_intact() {
        // 覆盖已有文件: 新内容完整、无残留旧内容
        let dir = temp_dir();
        let path = dir.join("data.txt");
        atomic_write(&path, b"old longer content").unwrap();
        atomic_write(&path, b"new").unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_skipped_when_target_missing() {
        // 目标不存在时备份是 no-op, 不报错也不产生备份文件
        let dir = temp_dir();
        let path = dir.join("config.toml");
        backup_and_rotate(&path, 5, 1000).unwrap();
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 0, "不该产生任何备份");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_creates_timestamped_copy() {
        // 目标存在时备份生成 `<name>.bak.<ts>` 且内容与原文件一致
        let dir = temp_dir();
        let path = dir.join("config.toml");
        atomic_write(&path, b"v1").unwrap();
        backup_and_rotate(&path, 5, 12345).unwrap();
        let bak = dir.join("config.toml.bak.12345");
        assert!(bak.exists(), "应生成时间戳备份");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_rotation_keeps_only_n_newest() {
        // 写 N+2 次(每次用递增时间戳备份), 最终只剩最近 N 个备份
        let dir = temp_dir();
        let path = dir.join("config.toml");
        let max = 5usize;
        // 先建初始文件
        atomic_write(&path, b"init").unwrap();
        // 模拟 7 次"备份现有 -> 写新内容", 时间戳 1..=7
        for ts in 1..=(max as u64 + 2) {
            backup_and_rotate(&path, max, ts).unwrap();
            atomic_write(&path, format!("v{}", ts).as_bytes()).unwrap();
        }
        // 统计备份文件
        let baks: Vec<u64> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_prefix("config.toml.bak.")
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .collect();
        assert_eq!(baks.len(), max, "应只保留最近 {} 个备份", max);
        // 保留的应是时间戳最大的 N 个(3..=7), 最旧的 1、2 被删
        let mut sorted = baks.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![3, 4, 5, 6, 7]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_ignores_unrelated_files() {
        // 轮转只动 `<name>.bak.<数字>` 模式, 不误删其它文件
        let dir = temp_dir();
        let path = dir.join("config.toml");
        atomic_write(&path, b"x").unwrap();
        // 放一个无关文件和一个不合模式的伪备份
        atomic_write(&dir.join("other.txt"), b"keep me").unwrap();
        atomic_write(&dir.join("config.toml.bak.notanumber"), b"keep me too").unwrap();
        // 触发多次备份轮转
        for ts in 1..=8u64 {
            backup_and_rotate(&path, 3, ts).unwrap();
        }
        assert!(dir.join("other.txt").exists(), "无关文件不该被删");
        assert!(
            dir.join("config.toml.bak.notanumber").exists(),
            "非数字后缀的伪备份不该被删"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
