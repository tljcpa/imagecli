//! 模型目录(catalog): 把所有 provider 暴露的模型聚合成一张统一的表(落地 D-011)。
//!
//! 为什么需要它: 多 provider 的价值只有靠"统一目录 + 易切换默认"才能兑现——
//! 否则几十个模型用户记不住、记不准(D-011 的"接得全 -> 用得顺")。本模块定义
//! 单条目结构 ModelEntry, 并提供"从 registry 聚合所有 provider 的 catalog"的入口,
//! 以及"按 provider/model 或 alias 解析选择"的纯函数(便于离线单测)。
//!
//! 职责边界: provider 声明自己有哪些模型(`Provider::catalog`), 是否有 key 由
//! `Provider::has_key` 自报(各 provider 知道自己的 key 来源, 见 config::keys);
//! 本模块只做"聚合 + 解析 + 渲染", 不碰网络、不读 key 之外的任何东西。

use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::core::provider::Capability;
use crate::core::registry::Registry;

/// 目录里的一条模型条目。聚合后供选择器/列表/--json 使用。
///
/// 字段含义:
/// - provider: 所属 provider 名(注册表 key), 如 "fal"。
/// - model_id: provider 内部的 model 标识, 如 "fal-ai/flux/dev"(可含 '/')。
/// - alias: 便于记忆的短别名(可选), 如 "flux"; 选择器与解析都接受它。
/// - capabilities: 该条目支持的能力(MVP 多为单一 text2image)。
/// - est_cost: 单条任务的预估单价(USD, Decimal), 复用 pricing 表; 免费为 0。
/// - available: 该 provider 当前是否能取到 key(env/keyring); 由聚合时按 provider 自报填入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub provider: String,
    pub model_id: String,
    pub alias: Option<String>,
    pub capabilities: Vec<Capability>,
    pub est_cost: Decimal,
    pub available: bool,
}

impl ModelEntry {
    /// 便捷构造: 单能力条目。est_cost 由 pricing 表按 provider/model/capability 估算。
    /// available 先置 false(占位), 聚合时再按 provider 是否有 key 覆盖。
    pub fn single(
        provider: &str,
        model_id: &str,
        alias: Option<&str>,
        capability: Capability,
    ) -> ModelEntry {
        let est_cost = crate::core::pricing::unit_price(provider, model_id, capability);
        ModelEntry {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            alias: alias.map(|s| s.to_string()),
            capabilities: vec![capability],
            est_cost,
            available: false,
        }
    }

    /// "provider/model_id" 形式的全限定名(选择器解析与展示用)。
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }
}

/// 从 registry 聚合所有已注册 provider 的目录。
///
/// available 由每个 provider 自报(`Provider::has_key`, 读真实 env/keyring)填入。
/// 聚合顺序跟随 registry.list_names()(已排序), 输出稳定。
pub fn build_catalog(registry: &Registry) -> Vec<ModelEntry> {
    // 收集 (provider_name, has_key, 该 provider 的原始条目)
    let mut providers: Vec<(String, bool, Vec<ModelEntry>)> = Vec::new();
    for name in registry.list_names() {
        if let Some(p) = registry.get(&name) {
            providers.push((name, p.has_key(), p.catalog()));
        }
    }
    assemble(providers)
}

/// 纯聚合逻辑: 把每个 provider 的 (name, has_key, 原始条目) 合成最终目录,
/// 用 has_key 覆盖每条的 available。抽成纯函数便于离线单测"available 正确反映有无 key"。
pub fn assemble(providers: Vec<(String, bool, Vec<ModelEntry>)>) -> Vec<ModelEntry> {
    let mut out: Vec<ModelEntry> = Vec::new();
    for (_name, has_key, entries) in providers {
        for mut e in entries {
            e.available = has_key;
            out.push(e);
        }
    }
    out
}

/// 把用户给的 selector(`<provider/model>` 或 `<alias>` 或裸 model_id)解析成目录里的一条。
///
/// 解析优先级(纯函数, 便于离线单测):
/// 1. alias 精确匹配(忽略大小写): 与某条 alias 相等。
/// 2. "provider/model_id": 按第一个 '/' 切分, 左为 provider、右为 model_id(model_id 自身可含 '/')。
/// 3. 裸 model_id 精确匹配(唯一时才接受, 避免歧义)。
///
/// 找不到返回 None, 由调用方给中文报错并附可选清单。
pub fn resolve_selection<'a>(entries: &'a [ModelEntry], input: &str) -> Option<&'a ModelEntry> {
    let needle = input.trim();
    if needle.is_empty() {
        return None;
    }

    // 1. alias 精确匹配(大小写不敏感)
    let lowered = needle.to_ascii_lowercase();
    for e in entries.iter() {
        if let Some(alias) = &e.alias {
            if alias.to_ascii_lowercase() == lowered {
                return Some(e);
            }
        }
    }

    // 2. "provider/model_id": 按第一个 '/' 切分
    if let Some(pos) = needle.find('/') {
        let prov = &needle[..pos];
        let model = &needle[pos + 1..];
        for e in entries.iter() {
            if e.provider == prov && e.model_id == model {
                return Some(e);
            }
        }
    }

    // 3. 裸 model_id 精确匹配(仅当全目录里唯一时接受)
    let mut matched: Option<&ModelEntry> = None;
    let mut count = 0usize;
    for e in entries.iter() {
        if e.model_id == needle {
            matched = Some(e);
            count += 1;
        }
    }
    if count == 1 {
        return matched;
    }

    None
}

/// 渲染一条目录项的人类可读标签(选择器菜单与列表共用, 纯函数便于单测)。
///
/// 形如: `agnes/agnes-image-2.1-flash  [text2image]  est $0  (alias: agnes)  可用`。
pub fn format_entry_label(e: &ModelEntry) -> String {
    let caps: Vec<&str> = e.capabilities.iter().map(|c| c.as_str()).collect();
    let alias_part = match &e.alias {
        Some(a) => format!("  (alias: {})", a),
        None => String::new(),
    };
    let avail_part = match e.available {
        true => "可用",
        false => "缺 key",
    };
    format!(
        "{}  [{}]  est ${}{}  {}",
        e.qualified(),
        caps.join(","),
        e.est_cost,
        alias_part,
        avail_part
    )
}

/// 把目录序列化成稳定的 --json 契约(纯函数, 便于单测断言)。
/// est_cost 用字符串表达(Decimal 精确, 不走浮点), 与 generate 的成本字段一致。
pub fn catalog_to_json(entries: &[ModelEntry]) -> Value {
    let arr: Vec<Value> = entries
        .iter()
        .map(|e| {
            let caps: Vec<&str> = e.capabilities.iter().map(|c| c.as_str()).collect();
            json!({
                "provider": e.provider,
                "model": e.model_id,
                "qualified": e.qualified(),
                "alias": e.alias,
                "capabilities": caps,
                "est_cost": e.est_cost.to_string(),
                "available": e.available,
            })
        })
        .collect();
    json!({ "models": arr })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一份测试目录(三个 provider, 仿真实默认 model 与 alias)。
    fn sample() -> Vec<ModelEntry> {
        assemble(vec![
            (
                "agnes".to_string(),
                true,
                vec![ModelEntry::single(
                    "agnes",
                    "agnes-image-2.1-flash",
                    Some("agnes"),
                    Capability::Text2Image,
                )],
            ),
            (
                "fal".to_string(),
                false,
                vec![ModelEntry::single(
                    "fal",
                    "fal-ai/flux/dev",
                    Some("flux"),
                    Capability::Text2Image,
                )],
            ),
            (
                "google".to_string(),
                true,
                vec![ModelEntry::single(
                    "google",
                    "gemini-2.5-flash-image",
                    Some("gemini-image"),
                    Capability::Text2Image,
                )],
            ),
        ])
    }

    #[test]
    fn assemble_sets_available_from_has_key() {
        // available 必须跟随各 provider 的 has_key: agnes/google 有 key, fal 无 key。
        let cat = sample();
        let agnes = cat.iter().find(|e| e.provider == "agnes").unwrap();
        let fal = cat.iter().find(|e| e.provider == "fal").unwrap();
        let google = cat.iter().find(|e| e.provider == "google").unwrap();
        assert!(agnes.available);
        assert!(!fal.available);
        assert!(google.available);
        // 三个 provider 的条目都在
        assert_eq!(cat.len(), 3);
    }

    #[test]
    fn resolve_by_alias_case_insensitive() {
        let cat = sample();
        let got = resolve_selection(&cat, "FLUX").unwrap();
        assert_eq!(got.provider, "fal");
        assert_eq!(got.model_id, "fal-ai/flux/dev");
    }

    #[test]
    fn resolve_by_provider_slash_model_with_slashes_in_model() {
        // model_id 自身含 '/'(fal-ai/flux/dev), 必须按第一个 '/' 切分 provider。
        let cat = sample();
        let got = resolve_selection(&cat, "fal/fal-ai/flux/dev").unwrap();
        assert_eq!(got.provider, "fal");
        assert_eq!(got.model_id, "fal-ai/flux/dev");
        // agnes 的 provider/model 形式(验收用例)
        let got2 = resolve_selection(&cat, "agnes/agnes-image-2.1-flash").unwrap();
        assert_eq!(got2.provider, "agnes");
    }

    #[test]
    fn resolve_bare_model_id_when_unique() {
        let cat = sample();
        let got = resolve_selection(&cat, "gemini-2.5-flash-image").unwrap();
        assert_eq!(got.provider, "google");
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let cat = sample();
        assert!(resolve_selection(&cat, "nope/whatever").is_none());
        assert!(resolve_selection(&cat, "").is_none());
    }

    #[test]
    fn default_registry_aggregates_all_providers() {
        // 从默认注册表聚合真实目录: 应含 13 家(fal/google/agnes + 大陆 5 家 + openai/replicate
        // + seedance/kling/jimeng)。available 跟随 has_key(测试环境无 key 时为 false),
        // 这里只断言条目齐全 + alias 命中。
        let reg = crate::core::registry::Registry::build_default();
        let cat = build_catalog(&reg);
        for prov in [
            "fal",
            "google",
            "agnes",
            "volcengine",
            "stepfun",
            "zhipu",
            "ppio",
            "siliconflow",
            // D-011 海外: OpenAI 官方 + Replicate
            "openai",
            "replicate",
            // D-014 视频: 火山方舟 Seedance + 可灵 Kling; 图像: 即梦 visual
            "seedance",
            "kling",
            "jimeng",
        ] {
            assert!(
                cat.iter().any(|e| e.provider == prov),
                "目录应包含 provider {}",
                prov
            );
        }
        // 至少 15 条(13 家各 1 个默认 model; seedance/kling 各另有 i2v 一条, 共 >= 15)。
        assert!(cat.len() >= 15, "目录条目数应 >= 15, 实得 {}", cat.len());
        // 大陆各家的语义别名应能解析命中对应 provider
        assert_eq!(resolve_selection(&cat, "seedream").unwrap().provider, "volcengine");
        assert_eq!(resolve_selection(&cat, "cogview").unwrap().provider, "zhipu");
        assert_eq!(resolve_selection(&cat, "kolors").unwrap().provider, "siliconflow");
        // 海外语义别名: OpenAI gpt-image / Replicate flux-schnell
        assert_eq!(resolve_selection(&cat, "gpt-image").unwrap().provider, "openai");
        assert_eq!(
            resolve_selection(&cat, "flux-schnell").unwrap().provider,
            "replicate"
        );
        // 视频: seedance 别名命中, 且该条目声明 text2video 能力。
        let seedance = resolve_selection(&cat, "seedance").unwrap();
        assert_eq!(seedance.provider, "seedance");
        assert!(seedance.capabilities.contains(&Capability::Text2Video));
        // 视频: kling 别名命中文生视频; 图像: jimeng 别名命中文生图。
        let kling = resolve_selection(&cat, "kling").unwrap();
        assert_eq!(kling.provider, "kling");
        assert!(kling.capabilities.contains(&Capability::Text2Video));
        let jimeng = resolve_selection(&cat, "jimeng").unwrap();
        assert_eq!(jimeng.provider, "jimeng");
        assert!(jimeng.capabilities.contains(&Capability::Text2Image));
    }

    #[test]
    fn catalog_json_has_available_and_est_cost_string() {
        let cat = sample();
        let v = catalog_to_json(&cat);
        let arr = v["models"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // est_cost 是字符串(Decimal 精确), available 是布尔
        let fal = arr.iter().find(|m| m["provider"] == "fal").unwrap();
        assert!(fal["est_cost"].is_string());
        assert_eq!(fal["available"], json!(false));
        assert_eq!(fal["qualified"], json!("fal/fal-ai/flux/dev"));
    }
}
