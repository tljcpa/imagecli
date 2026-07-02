//! 成本预估单价表(Decimal)与预算估算。
//!
//! 用途(对应 D-006 的"成本预检与预算护栏"工程项): generate 在真正提交前,
//! 用本表对"本次将花多少钱"做一个量级估算, 支撑两个护栏:
//!   - `--dry-run`: 只估不跑, 给用户一个心理价位;
//!   - `--max-cost`: 估算超过上限就拒绝执行, 防止批量误操作烧钱。
//!
//! 金额一律用 `rust_decimal::Decimal`(全局规则: 金额绝不用 f64, 避免二进制浮点
//! 累加误差让预算判断在边界上抖动)。
//!
//! 重要诚实声明(单价来源):
//!   - 本表的非零单价全部是**粗估占位**, 来源不确定、未与各厂商实时计费对齐,
//!     仅用于"量级护栏", 不可当账单依据。各家按 model/分辨率/步数/时长阶梯计费,
//!     这里只取一个保守量级常数。
//!   - agnes = 0: D-009 实测免费层 cost=0(2026-06-19)。
//!   - google(Gemini)= 0: 免费额度内按 0 计; 本表不建模"超出免费额度后的阶梯",
//!     超额计费需接各厂商真实价目表(后续补充)。
//!   - fal = 粗估占位: 例如 flux/dev 文生图量级约 0.025 USD/张, 视频量级更高。
//!
//! 结构允许后续补充: 新增 provider/model 只需在 match 里加分支;
//! 把粗估替换为真实价目表时, 单测的"Decimal 精确乘法"契约不变。

use rust_decimal::Decimal;

use crate::core::provider::Capability;

/// 单条任务的预估单价(USD, Decimal)。
///
/// 入参 provider/model/capability 共同决定单价。当前 model 维度仅 fal 用到
/// (按能力区分量级), 免费 provider 直接返回 0, 与 model 无关。
pub fn unit_price(provider: &str, model: &str, capability: Capability) -> Decimal {
    match provider {
        // agnes: D-009 免费层, 实测 cost=0。
        "agnes" => Decimal::ZERO,
        // google: 免费额度内按 0 计(本表不建模超额阶梯)。
        "google" => Decimal::ZERO,
        // fal: 粗估占位, 按能力给量级常数(来源不确定, 仅护栏用)。
        "fal" => fal_unit_price(model, capability),
        // seedance(火山方舟视频): 视频量级显著高于图像, 复用 fal 同款按能力占位。
        // 粗估, 非真实价目(Ark 按 model/分辨率/时长阶梯计费), 仅护栏用。
        "seedance" => fal_unit_price(model, capability),
        // kling(可灵视频): 同为视频量级, 复用按能力占位(粗估, 非真实价目, 仅护栏用)。
        "kling" => fal_unit_price(model, capability),
        // jimeng(即梦 visual 图像): 图像量级, 复用按能力占位(粗估, 非真实价目, 仅护栏用)。
        "jimeng" => fal_unit_price(model, capability),
        // 未知 provider: 给一个保守占位量级, 避免把"未知成本"当成 0 而绕过护栏。
        _ => placeholder_by_capability(capability),
    }
}

/// fal 的占位单价: 按能力区分量级。**粗估, 非真实价目**。
///
/// 这里不细分到具体 model(flux/dev vs flux/pro vs seedance 各不同),
/// 只给一个按能力的保守量级常数; 需要精确计费时在此按 model 细化。
fn fal_unit_price(_model: &str, capability: Capability) -> Decimal {
    match capability {
        // 文生图/图生图: 量级约 0.025 USD/张(占位)。Decimal::new(25, 3) = 0.025。
        Capability::Text2Image => Decimal::new(25, 3),
        Capability::Image2Image => Decimal::new(25, 3),
        // 视频: 量级显著更高, 占位 0.50 USD/条。Decimal::new(50, 2) = 0.50。
        Capability::Text2Video => Decimal::new(50, 2),
        Capability::Image2Video => Decimal::new(50, 2),
        Capability::FramesToVideo => Decimal::new(50, 2),
        // 超分: 量级较低, 占位 0.01 USD/次。Decimal::new(1, 2) = 0.01。
        Capability::Upscale => Decimal::new(1, 2),
    }
}

/// 未知 provider 的占位单价(按能力)。与 fal 同量级, 仅为不让护栏漏判。
fn placeholder_by_capability(capability: Capability) -> Decimal {
    fal_unit_price("", capability)
}

/// 估算一批任务的总成本: 单价 × 任务数。Decimal 精确乘法, 无浮点误差。
pub fn estimate_total(
    provider: &str,
    model: &str,
    capability: Capability,
    task_count: usize,
) -> Decimal {
    let unit = unit_price(provider, model, capability);
    // Decimal::from(u64): 任务数转 Decimal 参与精确乘法。
    unit * Decimal::from(task_count as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn free_providers_are_zero() {
        // agnes / google 免费 provider 单价恒为 0, 与 model/能力无关。
        assert_eq!(
            unit_price("agnes", "agnes-image-2.1-flash", Capability::Text2Image),
            Decimal::ZERO
        );
        assert_eq!(
            unit_price("google", "gemini-2.5-flash-image", Capability::Text2Image),
            Decimal::ZERO
        );
        // 免费 provider 任意任务数总成本仍为 0。
        assert_eq!(
            estimate_total("agnes", "m", Capability::Text2Image, 100),
            Decimal::ZERO
        );
    }

    #[test]
    fn fal_image_unit_price_is_placeholder_value() {
        // fal 文生图占位单价 0.025; 视频占位 0.50。
        assert_eq!(
            unit_price("fal", "fal-ai/flux/dev", Capability::Text2Image),
            Decimal::from_str("0.025").unwrap()
        );
        assert_eq!(
            unit_price("fal", "fal-ai/any-video", Capability::Text2Video),
            Decimal::from_str("0.50").unwrap()
        );
    }

    #[test]
    fn estimate_total_is_exact_decimal_multiplication() {
        // 0.025 × 4 = 0.100, Decimal 精确, 不是 0.09999999...。
        let total = estimate_total("fal", "fal-ai/flux/dev", Capability::Text2Image, 4);
        assert_eq!(total, Decimal::from_str("0.100").unwrap());
        // 数值相等(忽略 scale 差异)。
        assert_eq!(total, Decimal::from_str("0.1").unwrap());
    }

    #[test]
    fn max_cost_boundary_equal_over_under() {
        // 4 个 fal 文生图任务, 预估 0.10。验证 --max-cost 边界比较语义(total > max 才拒绝)。
        let total = estimate_total("fal", "fal-ai/flux/dev", Capability::Text2Image, 4);
        let equal = Decimal::from_str("0.10").unwrap();
        let slightly_under = Decimal::from_str("0.09").unwrap();
        let slightly_over = Decimal::from_str("0.11").unwrap();
        // 等于上限: 不拒绝(total > max 为假, 即 total <= max)。
        assert!(total <= equal);
        // 略低于上限(上限 0.09 < total): 拒绝。
        assert!(total > slightly_under);
        // 略高于上限(上限 0.11 > total): 不拒绝。
        assert!(total <= slightly_over);
    }

    #[test]
    fn unknown_provider_not_treated_as_free() {
        // 未知 provider 不能被当成 0 成本而绕过护栏。
        assert!(unit_price("mystery", "x", Capability::Text2Image) > Decimal::ZERO);
    }
}
