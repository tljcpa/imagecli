//! settings: 默认配置(默认 provider + 默认 model)的持久化(落地 D-011)。
//!
//! 与 key 严格分离: key 仍走 env/keyring(见 config::keys), 本文件只存"用户选了哪个
//! 默认 provider/model"。落 XDG config dir(linux: ~/.config/imagecli/config.toml),
//! 与 jobs.db(XDG data dir)分属不同目录, 互不干扰。
//!
//! 设计取舍:
//! - 用 toml(人类可读、可手改), 不用 SQLite——这是单条用户偏好, 不需要并发/查询。
//! - 缺文件视为"无默认"(返回空 Settings), 不报错: 首次使用本就没配置。
//! - IMAGECLI_CONFIG_PATH 环境变量可覆盖路径(测试隔离用, 与 store 的 IMAGECLI_DB_PATH 同源思路)。

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::atomic;

/// 覆盖配置文件路径的环境变量名(测试/隔离用)。
const CONFIG_PATH_ENV: &str = "IMAGECLI_CONFIG_PATH";

/// 配置文件备份保留个数: 每次覆盖写前先备份, 只保留最近 N 个, 超出删最旧。
/// 5 是经验值: 够回溯几次误操作, 又不至于在配置目录堆太多文件。
const MAX_BACKUPS: usize = 5;

/// 持久化的默认配置。两字段都可选: 用户从未设过默认时全为 None。
///
/// default_provider 与 default_model 成对存储: 选择器一次写两者, 保证模型确实属于该 provider。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// 默认 provider 名(如 "agnes")。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// 默认 model id(如 "agnes-image-2.1-flash")。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

impl Settings {
    /// 解析配置文件路径: 优先 IMAGECLI_CONFIG_PATH, 否则 XDG config dir 下 imagecli/config.toml。
    pub fn resolve_path() -> anyhow::Result<PathBuf> {
        if let Ok(p) = std::env::var(CONFIG_PATH_ENV) {
            if !p.trim().is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
        let dirs = directories::ProjectDirs::from("", "", "imagecli")
            .context("无法确定用户配置目录(XDG config dir)")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// 从默认路径加载。文件不存在视为"无默认", 返回空 Settings(不报错)。
    pub fn load() -> anyhow::Result<Settings> {
        let path = Self::resolve_path()?;
        Self::load_from(&path)
    }

    /// 从指定路径加载(便于单测注入临时文件)。不存在返回空 Settings。
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Settings> {
        if !path.exists() {
            return Ok(Settings::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
        // 损坏防护: 解析失败时返回错误并保留原文件, 绝不静默用默认值覆盖写回。
        // 静默默认会把用户配置抹掉(借鉴 Claude Code 读到损坏 auth 状态拒绝覆盖的思想);
        // 这里把决定权交给上层(提示用户手动修复), 而不是在数据层悄悄丢数据。
        Self::parse_toml(&text).with_context(|| {
            format!(
                "配置文件解析失败, 已原样保留未做任何修改: {}。请手动修复或删除该文件后重试。",
                path.display()
            )
        })
    }

    /// 解析 toml 文本为 Settings(纯函数, 便于离线单测往返)。
    pub fn parse_toml(text: &str) -> anyhow::Result<Settings> {
        toml::from_str::<Settings>(text).context("解析配置文件(toml)失败")
    }

    /// 序列化为 toml 文本(纯函数, 便于单测)。
    pub fn to_toml(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).context("序列化配置(toml)失败")
    }

    /// 保存到默认路径(自动创建父目录)。
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::resolve_path()?;
        self.save_to(&path)
    }

    /// 保存到指定路径(自动创建父目录, 便于单测)。
    ///
    /// 写入防损坏(D-013 优先项): 序列化 -> 写前备份并轮转 -> 原子写(临时文件 + sync + rename)。
    /// 父目录由 atomic_write 内部按需创建。
    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        // 先序列化: 序列化失败就不该碰磁盘上的任何东西(连备份都不做, 避免无意义的备份动作)。
        let text = self.to_toml()?;

        // 写前备份现有文件 + 轮转。备份失败只警告不致命:
        // 备份是"额外保险", 不该因为备份不了(如磁盘紧张)就拒绝保存用户的新配置。
        let now = atomic::now_unix_secs();
        if let Err(e) = atomic::backup_and_rotate(path, MAX_BACKUPS, now) {
            eprintln!("警告: 配置写入前备份失败(继续原子写入): {:#}", e);
        }

        // 原子写: 写同目录临时文件 -> flush + sync_all 落盘 -> rename 原子替换。
        // 中途失败(如 ENOSPC)绝不破坏原 config.toml —— 正是本加固要根治的事故场景。
        atomic::atomic_write(path, text.as_bytes())
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_settings_roundtrip() {
        // 空配置 toml 往返仍为空(两字段都 None, skip_serializing_if 不写出)。
        let s = Settings::default();
        let text = s.to_toml().unwrap();
        let back = Settings::parse_toml(&text).unwrap();
        assert_eq!(back, s);
        assert!(back.default_provider.is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        // 写默认 -> 读回: 字段逐一一致(D-011 配置读写往返)。
        let s = Settings {
            default_provider: Some("agnes".to_string()),
            default_model: Some("agnes-image-2.1-flash".to_string()),
        };
        let text = s.to_toml().unwrap();
        let back = Settings::parse_toml(&text).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.default_provider.as_deref(), Some("agnes"));
        assert_eq!(back.default_model.as_deref(), Some("agnes-image-2.1-flash"));
    }

    #[test]
    fn save_to_then_load_from_file_roundtrip() {
        // 落真实临时文件再读回, 覆盖 save_to/load_from 的 IO 路径。
        let dir = std::env::temp_dir().join(format!("imagecli-cfg-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let s = Settings {
            default_provider: Some("fal".to_string()),
            default_model: Some("fal-ai/flux/dev".to_string()),
        };
        s.save_to(&path).unwrap();
        let back = Settings::load_from(&path).unwrap();
        assert_eq!(back, s);
        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_missing_file_is_empty() {
        // 不存在的文件 -> 空 Settings, 不报错(首次使用本就无配置)。
        let path = std::path::Path::new("/nonexistent/imagecli/config.toml");
        let back = Settings::load_from(path).unwrap();
        assert_eq!(back, Settings::default());
    }

    /// 造隔离临时目录(进程 id + 纳秒避免并发互踩)。
    fn temp_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("imagecli-settings-test-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_corrupt_file_errors_and_preserves() {
        // 损坏防护: 放一个坏 toml, load 应返回错误而非静默默认, 且原文件内容保持不变。
        let dir = temp_dir();
        let path = dir.join("config.toml");
        let bad = "default_provider = \n这不是合法 toml [[[";
        std::fs::write(&path, bad).unwrap();

        let result = Settings::load_from(&path);
        assert!(result.is_err(), "损坏配置应返回错误, 不能静默用默认值");

        // 文件未被改动: 内容仍是那段坏 toml(没被默认值覆盖)
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, bad, "load 失败时绝不能修改原文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_backup_of_existing() {
        // 覆盖已有配置时应先生成 .bak 备份(内容为旧值)。
        let dir = temp_dir();
        let path = dir.join("config.toml");
        let v1 = Settings {
            default_provider: Some("agnes".to_string()),
            default_model: None,
        };
        v1.save_to(&path).unwrap();
        // 第二次保存触发对 v1 的备份
        let v2 = Settings {
            default_provider: Some("fal".to_string()),
            default_model: None,
        };
        v2.save_to(&path).unwrap();

        // 主文件应是 v2
        let back = Settings::load_from(&path).unwrap();
        assert_eq!(back.default_provider.as_deref(), Some("fal"));
        // 至少存在一个备份文件, 且其内容能解析回 v1
        let baks: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("config.toml.bak."))
            .collect();
        assert!(!baks.is_empty(), "覆盖写应留下至少一个备份");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_to_uses_atomic_write_no_tmp_residue() {
        // 保存后目录里不应残留 .tmp.* 临时文件(原子写收尾干净)。
        let dir = temp_dir();
        let path = dir.join("config.toml");
        let s = Settings {
            default_provider: Some("agnes".to_string()),
            default_model: Some("m".to_string()),
        };
        s.save_to(&path).unwrap();
        let residue: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(residue.is_empty(), "原子写后不应残留临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
