//! 密钥读取: 按 provider 命名空间, 环境变量优先, keyring 回退。
//!
//! 优先级(从高到低):
//! 1. IMAGECLI_{PROVIDER}_KEY  —— 本项目专用变量, 最高优先, 避免和其他工具串味
//! 2. {PROVIDER}_KEY           —— 兼容各家官方 SDK 习惯(如 FAL_KEY)
//! 3. 系统 keyring             —— 持久化回退, service 名固定前缀
//!
//! 设计为可注入环境查询函数, 便于单测在不污染真实环境变量的前提下覆盖优先级逻辑。

use anyhow::Context;

/// keyring 中用的 service 名前缀, 加上 provider 名构成完整 service。
const KEYRING_SERVICE_PREFIX: &str = "imagecli";
/// keyring 里的 username(本工具固定一个逻辑账户)。
const KEYRING_USER: &str = "default";

/// 抽象一个"按变量名取环境变量"的查询器, 便于测试替换。
pub trait EnvLookup {
    fn get(&self, name: &str) -> Option<String>;
}

/// 真实环境变量查询器。
pub struct SystemEnv;

impl EnvLookup for SystemEnv {
    fn get(&self, name: &str) -> Option<String> {
        // std::env::var 在变量不存在或非 UTF-8 时返回 Err, .ok() 把 Err 折叠成 None
        std::env::var(name).ok()
    }
}

/// 计算某 provider 的两个候选环境变量名。
/// 例如 provider="fal" -> ("IMAGECLI_FAL_KEY", "FAL_KEY")。
pub fn env_var_names(provider: &str) -> (String, String) {
    let upper = provider.to_ascii_uppercase();
    let primary = format!("IMAGECLI_{}_KEY", upper);
    let secondary = format!("{}_KEY", upper);
    (primary, secondary)
}

/// 纯逻辑: 仅按环境变量优先级解析密钥(不碰 keyring)。
/// 抽出来便于单测验证优先级, 不依赖系统 keyring 后端是否可用。
pub fn resolve_from_env<E: EnvLookup>(env: &E, provider: &str) -> Option<String> {
    let (primary, secondary) = env_var_names(provider);
    // IMAGECLI_ 专用变量优先
    if let Some(v) = env.get(&primary) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    // 退而求其次用通用 {PROVIDER}_KEY
    if let Some(v) = env.get(&secondary) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    None
}

/// 完整解析: 先环境变量, 再 keyring。
/// 返回 None 表示彻底没找到, 由调用方给中文报错(提示设置 IMAGECLI_{P}_KEY)。
pub fn resolve_key(provider: &str) -> Option<String> {
    // 第一、二级: 环境变量
    if let Some(v) = resolve_from_env(&SystemEnv, provider) {
        return Some(v);
    }
    // 第三级: keyring。后端不可用时不应崩溃, Err 折叠成 None(取不到密钥)。
    load_from_keyring(provider).unwrap_or_default()
}

/// 解析密钥, 找不到时返回带中文指引的错误(generate 路径用这个)。
pub fn require_key(provider: &str) -> anyhow::Result<String> {
    let (primary, _secondary) = env_var_names(provider);
    resolve_key(provider).with_context(|| {
        format!(
            "未找到 {} 的 API key。请设置环境变量 {} (或用 `imagecli` 写入系统 keyring)。",
            provider, primary
        )
    })
}

/// Google Gemini 的密钥候选环境变量, 按优先级从高到低。
///
/// 为什么 google 单列、不走通用 env_var_names: 官方约定用 `GEMINI_API_KEY` / `GOOGLE_API_KEY`
/// (注意是 `_API_KEY` 后缀, 与本项目通用的 `{PROVIDER}_KEY` 规则不同, 通用规则会算成 `GOOGLE_KEY`)。
/// `IMAGECLI_GOOGLE_KEY` 仍置顶, 维持本模块"项目专用变量最高优先, 避免与其他工具串味"的既有约定;
/// 其后按官方推荐接 `GEMINI_API_KEY`, 再兼容 `GOOGLE_API_KEY`。
pub const GOOGLE_KEY_ENV_CANDIDATES: [&str; 3] =
    ["IMAGECLI_GOOGLE_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"];

/// 缺 key 时的统一中文指引(抽成常量便于单测断言, 不依赖真实 env)。
pub const GOOGLE_KEY_MISSING_HINT: &str =
    "未找到 Google Gemini 的 API key。请设置环境变量 GEMINI_API_KEY(或 GOOGLE_API_KEY / IMAGECLI_GOOGLE_KEY)。";

/// 通用: 按给定候选环境变量名(优先级从高到低)解析密钥(不碰 keyring)。
///
/// 抽成通用函数后, google 与 OpenAI 兼容模板(agnes 等)的 "key 来源" 都用它驱动——
/// 各自只需提供一份候选变量名列表, 不再重复写遍历逻辑。空白值视为未设置, 继续往下找。
pub fn resolve_candidates_from_env<E: EnvLookup>(env: &E, candidates: &[&str]) -> Option<String> {
    for name in candidates.iter() {
        if let Some(v) = env.get(name) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 通用: 完整解析(先候选环境变量, 再 keyring)。供 OpenAI 兼容模板按 config 驱动。
pub fn resolve_candidates_key(candidates: &[&str], keyring_service: &str) -> Option<String> {
    if let Some(v) = resolve_candidates_from_env(&SystemEnv, candidates) {
        return Some(v);
    }
    load_from_keyring(keyring_service).unwrap_or_default()
}

/// 通用: 解析密钥, 找不到时返回带中文指引的错误(模板 submit 用这个)。
pub fn require_candidates_key(
    candidates: &[&str],
    keyring_service: &str,
    hint: &str,
) -> anyhow::Result<String> {
    resolve_candidates_key(candidates, keyring_service).with_context(|| hint.to_string())
}

/// 纯逻辑: 仅按候选环境变量优先级解析 google 密钥(不碰 keyring)。
/// 抽出来便于单测验证优先级与"缺三个 env 返回 None", 不依赖系统 keyring 后端。
pub fn resolve_google_from_env<E: EnvLookup>(env: &E) -> Option<String> {
    resolve_candidates_from_env(env, &GOOGLE_KEY_ENV_CANDIDATES)
}

/// agnes(OpenAI 兼容)的密钥候选环境变量, 按优先级从高到低。
///
/// 为什么 AGNES_API_KEY 置顶(而非沿用 google 那样 IMAGECLI_ 优先): D-009 与主控联调约定
/// 用 `AGNES_API_KEY=<key>` 注入, 官方/直觉变量名优先更顺手; 仍兼容 `IMAGECLI_AGNES_KEY`
/// 作为本项目命名空间回退, 避免与其他工具串味。
pub const AGNES_KEY_ENV_CANDIDATES: [&str; 2] = ["AGNES_API_KEY", "IMAGECLI_AGNES_KEY"];

/// 缺 agnes key 时的统一中文指引(抽成常量便于单测断言, 不依赖真实 env)。
pub const AGNES_KEY_MISSING_HINT: &str =
    "未找到 agnes 的 API key。请设置环境变量 AGNES_API_KEY(或 IMAGECLI_AGNES_KEY)。";

/// 纯逻辑: 仅按候选环境变量解析 agnes 密钥(不碰 keyring), 便于单测验证优先级。
pub fn resolve_agnes_from_env<E: EnvLookup>(env: &E) -> Option<String> {
    resolve_candidates_from_env(env, &AGNES_KEY_ENV_CANDIDATES)
}

/// 解析 agnes 密钥, 找不到时返回带中文指引的错误(agnes provider 的 submit 经模板用这个)。
pub fn require_agnes_key() -> anyhow::Result<String> {
    require_candidates_key(&AGNES_KEY_ENV_CANDIDATES, "agnes", AGNES_KEY_MISSING_HINT)
}

// ===== 大陆 5 家 OpenAI 兼容 provider 的密钥候选(D-010 / D-012) =====
// 统一约定: 官方/直觉变量名优先, 末位接本项目命名空间 IMAGECLI_{P}_KEY 作回退(避免与其他工具串味)。
// 全部只从 env(再 keyring)取, 绝不写死任何 key; 缺 key 时由模板 submit 给中文指引、退出码非零。

/// 火山引擎方舟(Seedream)密钥候选: 官方 ARK_API_KEY 优先, 兼容 VOLC_API_KEY, 末位项目命名空间。
pub const VOLCENGINE_KEY_ENV_CANDIDATES: [&str; 3] =
    ["ARK_API_KEY", "VOLC_API_KEY", "IMAGECLI_VOLC_KEY"];
/// 缺火山 key 的中文指引。
pub const VOLCENGINE_KEY_MISSING_HINT: &str =
    "未找到火山引擎方舟的 API key。请设置环境变量 ARK_API_KEY(或 VOLC_API_KEY / IMAGECLI_VOLC_KEY)。";

/// 阶跃 StepFun 密钥候选。
pub const STEPFUN_KEY_ENV_CANDIDATES: [&str; 2] = ["STEPFUN_API_KEY", "IMAGECLI_STEPFUN_KEY"];
/// 缺 StepFun key 的中文指引。
pub const STEPFUN_KEY_MISSING_HINT: &str =
    "未找到阶跃 StepFun 的 API key。请设置环境变量 STEPFUN_API_KEY(或 IMAGECLI_STEPFUN_KEY)。";

/// 智谱 CogView 密钥候选: 官方 ZHIPU_API_KEY 优先, 兼容 GLM_API_KEY, 末位项目命名空间。
pub const ZHIPU_KEY_ENV_CANDIDATES: [&str; 3] =
    ["ZHIPU_API_KEY", "GLM_API_KEY", "IMAGECLI_ZHIPU_KEY"];
/// 缺智谱 key 的中文指引。
pub const ZHIPU_KEY_MISSING_HINT: &str =
    "未找到智谱 CogView 的 API key。请设置环境变量 ZHIPU_API_KEY(或 GLM_API_KEY / IMAGECLI_ZHIPU_KEY)。";

/// PPIO 派欧云密钥候选。
pub const PPIO_KEY_ENV_CANDIDATES: [&str; 2] = ["PPIO_API_KEY", "IMAGECLI_PPIO_KEY"];
/// 缺 PPIO key 的中文指引。
pub const PPIO_KEY_MISSING_HINT: &str =
    "未找到 PPIO 派欧云的 API key。请设置环境变量 PPIO_API_KEY(或 IMAGECLI_PPIO_KEY)。";

/// SiliconFlow 硅基流动密钥候选。
pub const SILICONFLOW_KEY_ENV_CANDIDATES: [&str; 2] =
    ["SILICONFLOW_API_KEY", "IMAGECLI_SILICONFLOW_KEY"];
/// 缺 SiliconFlow key 的中文指引。
pub const SILICONFLOW_KEY_MISSING_HINT: &str =
    "未找到 SiliconFlow 硅基流动的 API key。请设置环境变量 SILICONFLOW_API_KEY(或 IMAGECLI_SILICONFLOW_KEY)。";

// ===== 海外 provider 密钥候选(D-011): OpenAI 官方 + Replicate =====
// 同样约定: 官方/直觉变量名优先, 末位接本项目命名空间 IMAGECLI_{P}_KEY 作回退。
// 只从 env(再 keyring)取, 绝不写死任何 key; 缺 key 时由各 provider 给中文指引、退出码非零。

/// OpenAI 官方密钥候选: 官方 OPENAI_API_KEY 优先, 末位项目命名空间。
pub const OPENAI_KEY_ENV_CANDIDATES: [&str; 2] = ["OPENAI_API_KEY", "IMAGECLI_OPENAI_KEY"];
/// 缺 OpenAI key 的中文指引。
pub const OPENAI_KEY_MISSING_HINT: &str =
    "未找到 OpenAI 的 API key。请设置环境变量 OPENAI_API_KEY(或 IMAGECLI_OPENAI_KEY)。";

/// Replicate 密钥候选: 官方 REPLICATE_API_TOKEN 优先, 末位项目命名空间。
/// 注意 Replicate 官方变量名是 `REPLICATE_API_TOKEN`(TOKEN 后缀, 非 _KEY),
/// 与本项目通用 `{PROVIDER}_KEY` 规则不同, 故与 google 一样单列候选常量。
pub const REPLICATE_KEY_ENV_CANDIDATES: [&str; 2] =
    ["REPLICATE_API_TOKEN", "IMAGECLI_REPLICATE_KEY"];
/// 缺 Replicate key 的中文指引。
pub const REPLICATE_KEY_MISSING_HINT: &str =
    "未找到 Replicate 的 API token。请设置环境变量 REPLICATE_API_TOKEN(或 IMAGECLI_REPLICATE_KEY)。";

/// 火山方舟 Seedance(视频)密钥候选: 与 volcengine 同火山账号, 官方 ARK_API_KEY 优先,
/// 再接本项目命名空间 IMAGECLI_ARK_KEY, 末位 provider 专用 IMAGECLI_SEEDANCE_KEY。
/// 注意与 VOLCENGINE_KEY_ENV_CANDIDATES 共享 ARK_API_KEY(同账号), 但各自有独立回退变量。
pub const SEEDANCE_KEY_ENV_CANDIDATES: [&str; 3] =
    ["ARK_API_KEY", "IMAGECLI_ARK_KEY", "IMAGECLI_SEEDANCE_KEY"];
/// 缺 Seedance key 的中文指引。
pub const SEEDANCE_KEY_MISSING_HINT: &str =
    "未找到火山方舟 Seedance 的 API key。请设置环境变量 ARK_API_KEY(或 IMAGECLI_ARK_KEY / IMAGECLI_SEEDANCE_KEY)。";

// ===== 可灵 Kling(D-014, JWT 鉴权)的双密钥候选: AccessKey + SecretKey =====
// 可灵需要 AK + SK 两个密钥(本地现算 HS256 JWT: AK 作 iss, SK 作签名密钥), 与本模块原有
// "单 key/provider" 不同。处理方式: 不改既有单 key 抽象, 而是给可灵单列两份候选变量名列表,
// 各走通用 resolve_candidates_*/require_candidates_key, 一份取 AK、一份取 SK。
// 约定: 官方/直觉变量名 KLING_ACCESS_KEY / KLING_SECRET_KEY 优先, 末位接本项目命名空间
// IMAGECLI_KLING_AK / IMAGECLI_KLING_SK 作回退。只从 env(再 keyring)取, 绝不写死。

/// 可灵 AccessKey 候选。
pub const KLING_AK_ENV_CANDIDATES: [&str; 2] = ["KLING_ACCESS_KEY", "IMAGECLI_KLING_AK"];
/// 可灵 SecretKey 候选。
pub const KLING_SK_ENV_CANDIDATES: [&str; 2] = ["KLING_SECRET_KEY", "IMAGECLI_KLING_SK"];
/// 缺可灵 AccessKey 的中文指引。
pub const KLING_AK_MISSING_HINT: &str =
    "未找到可灵 Kling 的 AccessKey。请设置环境变量 KLING_ACCESS_KEY(或 IMAGECLI_KLING_AK), 并一并设置 KLING_SECRET_KEY。";
/// 缺可灵 SecretKey 的中文指引。
pub const KLING_SK_MISSING_HINT: &str =
    "未找到可灵 Kling 的 SecretKey。请设置环境变量 KLING_SECRET_KEY(或 IMAGECLI_KLING_SK), 并一并设置 KLING_ACCESS_KEY。";

// ===== 即梦 visual(D-014, 火山 AK/SK V4 签名)的双密钥候选: AccessKeyId + SecretAccessKey =====
// 即梦 visual 与火山方舟 Ark 可能不是同一账号体系(方舟 ARK_API_KEY 是 Bearer 单 key,
// 即梦 visual 是火山 IAM 的 AK/SK 对做 V4 签名), 故用独立的 JIMENG_ 前缀, 不复用 ARK/VOLC。
// 约定: JIMENG_ACCESS_KEY / JIMENG_SECRET_KEY 优先, 兼容通用 VOLC_ACCESS_KEY / VOLC_SECRET_KEY,
// 末位接本项目命名空间 IMAGECLI_JIMENG_AK / IMAGECLI_JIMENG_SK。只从 env(再 keyring)取, 绝不写死。

/// 即梦 AccessKeyId 候选。
pub const JIMENG_AK_ENV_CANDIDATES: [&str; 3] =
    ["JIMENG_ACCESS_KEY", "VOLC_ACCESS_KEY", "IMAGECLI_JIMENG_AK"];
/// 即梦 SecretAccessKey 候选。
pub const JIMENG_SK_ENV_CANDIDATES: [&str; 3] =
    ["JIMENG_SECRET_KEY", "VOLC_SECRET_KEY", "IMAGECLI_JIMENG_SK"];
/// 缺即梦 AccessKeyId 的中文指引。
pub const JIMENG_AK_MISSING_HINT: &str =
    "未找到即梦 visual 的 AccessKeyId。请设置环境变量 JIMENG_ACCESS_KEY(或 VOLC_ACCESS_KEY / IMAGECLI_JIMENG_AK), 并一并设置 JIMENG_SECRET_KEY。";
/// 缺即梦 SecretAccessKey 的中文指引。
pub const JIMENG_SK_MISSING_HINT: &str =
    "未找到即梦 visual 的 SecretAccessKey。请设置环境变量 JIMENG_SECRET_KEY(或 VOLC_SECRET_KEY / IMAGECLI_JIMENG_SK), 并一并设置 JIMENG_ACCESS_KEY。";

/// 完整解析 google 密钥: 先候选环境变量, 再 keyring(service 名沿用 "google")。
pub fn resolve_google_key() -> Option<String> {
    if let Some(v) = resolve_google_from_env(&SystemEnv) {
        return Some(v);
    }
    load_from_keyring("google").unwrap_or_default()
}

/// 解析 google 密钥, 找不到时返回带中文指引的错误(google provider 的 submit 用这个)。
pub fn require_google_key() -> anyhow::Result<String> {
    resolve_google_key().with_context(|| GOOGLE_KEY_MISSING_HINT.to_string())
}

/// 从系统 keyring 读取。后端不可用(如无 keyutils 权限)时返回 Err, 上层折叠为 None。
fn load_from_keyring(provider: &str) -> anyhow::Result<Option<String>> {
    let service = format!("{}-{}", KEYRING_SERVICE_PREFIX, provider);
    let entry = keyring::Entry::new(&service, KEYRING_USER)?;
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("读取 keyring 失败: {}", e)),
    }
}

/// 把密钥写入系统 keyring(供未来 `imagecli keys set` 子命令用; 当前预留)。
pub fn store_in_keyring(provider: &str, secret: &str) -> anyhow::Result<()> {
    let service = format!("{}-{}", KEYRING_SERVICE_PREFIX, provider);
    let entry = keyring::Entry::new(&service, KEYRING_USER)?;
    entry.set_password(secret)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 假环境查询器, 用 map 模拟环境变量。
    struct FakeEnv {
        vars: HashMap<String, String>,
    }

    impl FakeEnv {
        fn new() -> FakeEnv {
            FakeEnv {
                vars: HashMap::new(),
            }
        }
        fn with(mut self, k: &str, v: &str) -> FakeEnv {
            self.vars.insert(k.to_string(), v.to_string());
            self
        }
    }

    impl EnvLookup for FakeEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
    }

    #[test]
    fn var_names_are_namespaced() {
        let (p, s) = env_var_names("fal");
        assert_eq!(p, "IMAGECLI_FAL_KEY");
        assert_eq!(s, "FAL_KEY");
    }

    #[test]
    fn primary_env_wins_over_secondary() {
        // 两个都给, IMAGECLI_ 专用变量应胜出
        let env = FakeEnv::new()
            .with("IMAGECLI_FAL_KEY", "primary-key")
            .with("FAL_KEY", "secondary-key");
        let got = resolve_from_env(&env, "fal");
        assert_eq!(got.as_deref(), Some("primary-key"));
    }

    #[test]
    fn falls_back_to_secondary() {
        // 只有通用变量时用它
        let env = FakeEnv::new().with("FAL_KEY", "secondary-key");
        let got = resolve_from_env(&env, "fal");
        assert_eq!(got.as_deref(), Some("secondary-key"));
    }

    #[test]
    fn empty_value_is_ignored() {
        // 空白值视为未设置, 继续往下找
        let env = FakeEnv::new()
            .with("IMAGECLI_FAL_KEY", "   ")
            .with("FAL_KEY", "real");
        let got = resolve_from_env(&env, "fal");
        assert_eq!(got.as_deref(), Some("real"));
    }

    #[test]
    fn none_when_nothing_set() {
        let env = FakeEnv::new();
        assert!(resolve_from_env(&env, "fal").is_none());
    }

    #[test]
    fn google_key_missing_when_three_envs_absent() {
        // 三个候选 env 全缺 -> None(对应无 key 的中文报错路径)
        let env = FakeEnv::new();
        assert!(resolve_google_from_env(&env).is_none());
        // 指引文案为中文且点名三个变量
        assert!(GOOGLE_KEY_MISSING_HINT.contains("GEMINI_API_KEY"));
        assert!(GOOGLE_KEY_MISSING_HINT.contains("GOOGLE_API_KEY"));
        assert!(GOOGLE_KEY_MISSING_HINT.contains("IMAGECLI_GOOGLE_KEY"));
    }

    #[test]
    fn google_key_priority_project_namespace_first() {
        // 项目专用变量优先于官方变量(维持既有"避免串味"约定)
        let env = FakeEnv::new()
            .with("IMAGECLI_GOOGLE_KEY", "proj-key")
            .with("GEMINI_API_KEY", "gemini-key")
            .with("GOOGLE_API_KEY", "google-key");
        assert_eq!(resolve_google_from_env(&env).as_deref(), Some("proj-key"));
    }

    #[test]
    fn agnes_key_priority_and_missing() {
        // AGNES_API_KEY 优先于 IMAGECLI_AGNES_KEY
        let env = FakeEnv::new()
            .with("AGNES_API_KEY", "agnes-key")
            .with("IMAGECLI_AGNES_KEY", "proj-key");
        assert_eq!(resolve_agnes_from_env(&env).as_deref(), Some("agnes-key"));
        // 只有项目命名空间变量时用它(回退)
        let env2 = FakeEnv::new().with("IMAGECLI_AGNES_KEY", "proj-key");
        assert_eq!(resolve_agnes_from_env(&env2).as_deref(), Some("proj-key"));
        // 两个都缺 -> None(对应无 key 中文报错路径)
        let env3 = FakeEnv::new();
        assert!(resolve_agnes_from_env(&env3).is_none());
        // 指引文案为中文且点名两个变量
        assert!(AGNES_KEY_MISSING_HINT.contains("AGNES_API_KEY"));
        assert!(AGNES_KEY_MISSING_HINT.contains("IMAGECLI_AGNES_KEY"));
    }

    #[test]
    fn cn_providers_key_priority_and_missing_hints() {
        // 火山: ARK_API_KEY 优先于 VOLC_API_KEY 优先于 IMAGECLI_VOLC_KEY
        let env = FakeEnv::new()
            .with("ARK_API_KEY", "ark")
            .with("VOLC_API_KEY", "volc")
            .with("IMAGECLI_VOLC_KEY", "proj");
        assert_eq!(
            resolve_candidates_from_env(&env, &VOLCENGINE_KEY_ENV_CANDIDATES).as_deref(),
            Some("ark")
        );
        // 智谱: 缺官方时回退 GLM_API_KEY
        let env2 = FakeEnv::new().with("GLM_API_KEY", "glm");
        assert_eq!(
            resolve_candidates_from_env(&env2, &ZHIPU_KEY_ENV_CANDIDATES).as_deref(),
            Some("glm")
        );
        // 全缺 -> None(对应无 key 中文报错路径), 逐家验证
        let empty = FakeEnv::new();
        assert!(resolve_candidates_from_env(&empty, &STEPFUN_KEY_ENV_CANDIDATES).is_none());
        assert!(resolve_candidates_from_env(&empty, &PPIO_KEY_ENV_CANDIDATES).is_none());
        assert!(resolve_candidates_from_env(&empty, &SILICONFLOW_KEY_ENV_CANDIDATES).is_none());
        // 指引文案为中文且点名各自变量
        assert!(VOLCENGINE_KEY_MISSING_HINT.contains("ARK_API_KEY"));
        assert!(STEPFUN_KEY_MISSING_HINT.contains("STEPFUN_API_KEY"));
        assert!(ZHIPU_KEY_MISSING_HINT.contains("ZHIPU_API_KEY"));
        assert!(PPIO_KEY_MISSING_HINT.contains("PPIO_API_KEY"));
        assert!(SILICONFLOW_KEY_MISSING_HINT.contains("SILICONFLOW_API_KEY"));
    }

    #[test]
    fn seedance_key_priority_and_missing_hint() {
        // ARK_API_KEY 优先于 IMAGECLI_ARK_KEY 优先于 IMAGECLI_SEEDANCE_KEY。
        let env = FakeEnv::new()
            .with("ARK_API_KEY", "ark")
            .with("IMAGECLI_ARK_KEY", "proj-ark")
            .with("IMAGECLI_SEEDANCE_KEY", "proj-seed");
        assert_eq!(
            resolve_candidates_from_env(&env, &SEEDANCE_KEY_ENV_CANDIDATES).as_deref(),
            Some("ark")
        );
        // 只有 provider 专用变量时用它(回退)
        let env2 = FakeEnv::new().with("IMAGECLI_SEEDANCE_KEY", "proj-seed");
        assert_eq!(
            resolve_candidates_from_env(&env2, &SEEDANCE_KEY_ENV_CANDIDATES).as_deref(),
            Some("proj-seed")
        );
        // 全缺 -> None(对应无 key 中文报错路径)
        let empty = FakeEnv::new();
        assert!(resolve_candidates_from_env(&empty, &SEEDANCE_KEY_ENV_CANDIDATES).is_none());
        // 指引文案为中文且点名三个变量
        assert!(SEEDANCE_KEY_MISSING_HINT.contains("ARK_API_KEY"));
        assert!(SEEDANCE_KEY_MISSING_HINT.contains("IMAGECLI_ARK_KEY"));
        assert!(SEEDANCE_KEY_MISSING_HINT.contains("IMAGECLI_SEEDANCE_KEY"));
    }

    #[test]
    fn overseas_providers_key_priority_and_missing_hints() {
        // OpenAI: OPENAI_API_KEY 优先于 IMAGECLI_OPENAI_KEY
        let env = FakeEnv::new()
            .with("OPENAI_API_KEY", "sk-official")
            .with("IMAGECLI_OPENAI_KEY", "proj");
        assert_eq!(
            resolve_candidates_from_env(&env, &OPENAI_KEY_ENV_CANDIDATES).as_deref(),
            Some("sk-official")
        );
        // Replicate: REPLICATE_API_TOKEN 优先于 IMAGECLI_REPLICATE_KEY
        let env2 = FakeEnv::new()
            .with("REPLICATE_API_TOKEN", "r8_official")
            .with("IMAGECLI_REPLICATE_KEY", "proj");
        assert_eq!(
            resolve_candidates_from_env(&env2, &REPLICATE_KEY_ENV_CANDIDATES).as_deref(),
            Some("r8_official")
        );
        // 只有项目命名空间变量时用它(回退)
        let env3 = FakeEnv::new().with("IMAGECLI_REPLICATE_KEY", "proj-only");
        assert_eq!(
            resolve_candidates_from_env(&env3, &REPLICATE_KEY_ENV_CANDIDATES).as_deref(),
            Some("proj-only")
        );
        // 全缺 -> None(对应无 key 中文报错路径)
        let empty = FakeEnv::new();
        assert!(resolve_candidates_from_env(&empty, &OPENAI_KEY_ENV_CANDIDATES).is_none());
        assert!(resolve_candidates_from_env(&empty, &REPLICATE_KEY_ENV_CANDIDATES).is_none());
        // 指引文案为中文且点名各自变量
        assert!(OPENAI_KEY_MISSING_HINT.contains("OPENAI_API_KEY"));
        assert!(OPENAI_KEY_MISSING_HINT.contains("IMAGECLI_OPENAI_KEY"));
        assert!(REPLICATE_KEY_MISSING_HINT.contains("REPLICATE_API_TOKEN"));
        assert!(REPLICATE_KEY_MISSING_HINT.contains("IMAGECLI_REPLICATE_KEY"));
    }

    #[test]
    fn google_key_falls_back_to_gemini_then_google() {
        // 无项目变量时优先 GEMINI_API_KEY
        let env = FakeEnv::new()
            .with("GEMINI_API_KEY", "gemini-key")
            .with("GOOGLE_API_KEY", "google-key");
        assert_eq!(resolve_google_from_env(&env).as_deref(), Some("gemini-key"));
        // 只有 GOOGLE_API_KEY 时用它
        let env2 = FakeEnv::new().with("GOOGLE_API_KEY", "google-key");
        assert_eq!(resolve_google_from_env(&env2).as_deref(), Some("google-key"));
    }
}
