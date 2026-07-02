//! Provider 注册表: 按名字取出一个已构造的 Provider。
//!
//! 当前 MVP 只有 fal 一个 provider。注册表的意义是为"下一棒并行加 provider"
//! 留好扩展点: 新 provider 只需在 build_default 里多注册一行, CLI 与 runner 不动。

use std::collections::HashMap;
use std::sync::Arc;

use crate::core::provider::Provider;
use crate::providers::agnes;
use crate::providers::fal::FalProvider;
use crate::providers::google::GoogleProvider;
use crate::providers::replicate::ReplicateProvider;
use crate::providers::jimeng::JimengProvider;
use crate::providers::kling::KlingProvider;
use crate::providers::seedance::SeedanceProvider;
use crate::providers::{openai, ppio, siliconflow, stepfun, volcengine, zhipu};

/// provider 注册表。内部用 name -> Provider 的映射。
pub struct Registry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl Registry {
    /// 构造空注册表。
    pub fn new() -> Registry {
        Registry {
            providers: HashMap::new(),
        }
    }

    /// 注册一个 provider。若重名则覆盖(MVP 不做冲突告警)。
    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        let key = provider.name().to_string();
        self.providers.insert(key, provider);
    }

    /// 按名字取 provider。取不到返回 None, 由调用方给中文报错。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        // cloned: Arc 是引用计数, clone 只增计数, 不复制 provider 本体
        self.providers.get(name).cloned()
    }

    /// 列出所有已注册 provider 名(排序后, 输出稳定)。
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.keys().cloned().collect();
        names.sort();
        names
    }

    /// 构造内置默认注册表。新增 provider 在这里加注册。
    pub fn build_default() -> Registry {
        let mut reg = Registry::new();
        // fal 是 D-004 指定的首个落地 provider
        reg.register(Arc::new(FalProvider::new()));
        // google(Gemini)是 D-008 指定的首个真实端到端 provider, 走 http-sync
        reg.register(Arc::new(GoogleProvider::new()));
        // agnes 是 D-009 的 OpenAI 兼容模板首个实例, 走 http-sync
        reg.register(Arc::new(agnes::provider()));
        // D-010/D-012 大陆 5 家: 火山 Seedream / StepFun / 智谱 CogView / PPIO(A 类 drop-in)
        // + SiliconFlow(B 类方言)。全部复用同一 OpenAI 兼容模板, 仅 config 不同。
        reg.register(Arc::new(volcengine::provider()));
        reg.register(Arc::new(stepfun::provider()));
        reg.register(Arc::new(zhipu::provider()));
        reg.register(Arc::new(ppio::provider()));
        reg.register(Arc::new(siliconflow::provider()));
        // D-011 海外: OpenAI 官方(gpt-image-1, drop-in 模板, 走 http-sync)
        reg.register(Arc::new(openai::provider()));
        // D-011 海外: Replicate(flux-schnell, C 类异步 prediction 提交+轮询)
        reg.register(Arc::new(ReplicateProvider::new()));
        // D-014 视频: 火山方舟 Ark Seedance(首个视频 provider, 走通用 async-task 骨架)
        reg.register(Arc::new(SeedanceProvider::new()));
        // D-014 视频: 可灵 Kling(同骨架, 鉴权换 HS256 JWT)
        reg.register(Arc::new(KlingProvider::new()));
        // D-014 图像: 即梦 visual(同骨架, 鉴权换火山 AK/SK V4 签名)
        reg.register(Arc::new(JimengProvider::new()));
        reg
    }
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}
