//! # ai —— AI 注册表与三个内置基线 AI
//!
//! 本 crate 把「AI 怎么做决策」与「引擎怎么跑对局」彻底分开：`sim` 只认 [`sim::ai::TeamAi`]
//! 这个 trait，AI 只能通过 [`sim::ai::Observation`]（一份只读快照）观察战场，通过
//! [`sim::ai::TeamActions`] 提交意图。这样 AI 无法偷看引擎内部状态，也无法改变规则。
//!
//! ## 四个内置 AI 的定位
//!
//! 它们不是「强 AI」，而是**行为可预期、可复现的对照基线**，用来验证引擎、做后续 AI 的 A/B 基准：
//!
//! | 名字 | 常量 | 行为 |
//! |---|---|---|
//! | `random` | [`AI_RANDOM`] | 纯随机（但仍只提交合法动作）——下界基线 |
//! | `greedy_flag` | [`AI_GREEDY_FLAG`] | 抢最近的地上旗、沿最短路送回己方阵营 |
//! | `defender` | [`AI_DEFENDER`] | 守家，打靠近阵营的敌人 |
//! | `raider` | [`AI_RAIDER`] | 流水线抢旗（互斥分旗 / 携带者避险 / 拦截 / 换位），得分参照 |
//!
//! ## 扩展点只有一个
//!
//! 新增 AI = 写一个新模块实现 `TeamAi`，然后在 [`register_all`] 里加一行 `register`。
//! CLI 用 `--ai <名字>` 选择；名字不存在时报错信息由 [`AiRegistry::names`] 提供（已排序）。
//!
//! ## 可复现性（本 crate 的硬约束）
//!
//! AI 内部**禁止**使用系统时间、线程本地随机、全局可变状态；随机数只来自 `sim::Rng`，
//! 并用工厂入参 `ai_seed` 播种（每个队伍一个种子）。因此同一局配置跑两次，动作序列完全相同，
//! 回放逐字节一致（docs/rules.md §14.14）。AI 也不允许跨局保存状态：每局由工厂新建实例。

mod defender;
mod greedy_flag;
mod raider;
mod random;

pub mod common;

pub use defender::DefenderAi;
pub use greedy_flag::GreedyFlagAi;
pub use raider::RaiderAi;
pub use random::RandomAi;
pub use sim::ai::TeamAi;

use std::collections::BTreeMap;

/// AI 工厂：入参是 `ai_seed`，保证 AI 内部随机可复现。
///
/// 用函数指针而不是 `Box<dyn Fn>`：内置 AI 的构造都是无捕获的，函数指针更轻，
/// 也让 [`AiRegistry`] 天然是 `Copy` 友好的（不需要 `Arc`）。
pub type AiFactory = fn(u64) -> Box<dyn TeamAi>;

/// 纯随机基线 AI 的名字。
pub const AI_RANDOM: &str = "random";
/// 抢旗基线 AI 的名字。
pub const AI_GREEDY_FLAG: &str = "greedy_flag";
/// 守家基线 AI 的名字。
pub const AI_DEFENDER: &str = "defender";
/// 流水线抢旗 AI 的名字（基线之外的第四个 AI，用于和基线做 A/B）。
pub const AI_RAIDER: &str = "raider";
/// CLI 未指定 `--ai` 时使用的默认 AI。
pub const DEFAULT_AI: &str = AI_RANDOM;

/// 名字 → 工厂的注册表。
///
/// 用 `BTreeMap` 而不是 `HashMap`：`names()` 必须有序（CLI 报错时提示可用名字要稳定可读），
/// 有序表顺便让测试断言不需要排序，也不引入任何随机性（AI 侧的可复现性连注册顺序都不该受影响）。
#[derive(Default)]
pub struct AiRegistry {
    factories: BTreeMap<String, AiFactory>,
}

impl AiRegistry {
    /// 空注册表。CLI 若要注册自定义 AI，可以从这里起步再 `register`。
    pub fn new() -> Self {
        Self {
            factories: BTreeMap::new(),
        }
    }

    /// 注册一个 AI 工厂；同名覆盖（后注册的生效），便于测试替换内置实现。
    pub fn register(&mut self, name: &str, factory: AiFactory) -> &mut Self {
        self.factories.insert(name.to_string(), factory);
        self
    }

    /// 名字是否已注册（CLI 校验参数时用）。
    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }

    /// 按名字造一个 AI 实例；名字未知返回 `None`（**不 panic**，由调用方决定报错文案）。
    pub fn create(&self, name: &str, ai_seed: u64) -> Option<Box<dyn TeamAi>> {
        self.factories.get(name).map(|factory| factory(ai_seed))
    }

    /// 所有已注册名字，**已排序**（错误提示里直接可用）。
    pub fn names(&self) -> Vec<&str> {
        self.factories.keys().map(String::as_str).collect()
    }
}

/// 注册所有内置 AI。**新增 AI 时在这里加一行**（这是唯一的扩展点）。
pub fn register_all() -> AiRegistry {
    let mut registry = AiRegistry::new();
    registry
        .register(AI_RANDOM, |ai_seed| Box::new(RandomAi::new(ai_seed)))
        .register(AI_GREEDY_FLAG, |ai_seed| {
            Box::new(GreedyFlagAi::new(ai_seed))
        })
        .register(AI_DEFENDER, |ai_seed| Box::new(DefenderAi::new(ai_seed)))
        .register(AI_RAIDER, |ai_seed| Box::new(RaiderAi::new(ai_seed)));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim::ai::{Observation, TeamActions};

    #[test]
    fn register_all_registers_all_builtins() {
        let registry = register_all();
        for name in [AI_RANDOM, AI_GREEDY_FLAG, AI_DEFENDER, AI_RAIDER] {
            assert!(registry.contains(name), "{name} 必须在注册表里");
        }
        // names() 必须已排序（docs/internal-api.md §4）：BTreeMap 保证字典序。
        assert_eq!(
            registry.names(),
            vec![AI_DEFENDER, AI_GREEDY_FLAG, AI_RAIDER, AI_RANDOM]
        );
        assert!(registry.contains(DEFAULT_AI), "默认 AI 必须可用");
        assert_eq!(DEFAULT_AI, AI_RANDOM);
    }

    #[test]
    fn create_unknown_name_returns_none() {
        let registry = register_all();
        assert!(
            registry.create("no-such-ai", 7).is_none(),
            "未知名字必须返回 None，不能 panic"
        );
    }

    #[test]
    fn created_ai_reports_the_registered_name() {
        let registry = register_all();
        for (name, seed) in [
            (AI_RANDOM, 1u64),
            (AI_GREEDY_FLAG, 2),
            (AI_DEFENDER, 3),
            (AI_RAIDER, 4),
        ] {
            let ai = registry.create(name, seed).expect("内置 AI 必须可创建");
            assert_eq!(ai.name(), name, "create 出来的实例名字必须与注册名一致");
        }
    }

    #[test]
    fn duplicate_registration_overwrites_without_adding_entries() {
        /// 名字可变的假 AI：用来区分「第一次注册」和「第二次注册」的工厂。
        struct TaggedAi(&'static str);
        impl TeamAi for TaggedAi {
            fn name(&self) -> &str {
                self.0
            }
            fn decide(&mut self, _obs: &Observation) -> TeamActions {
                TeamActions::new()
            }
        }

        let mut registry = AiRegistry::new();
        registry.register("dup", |_seed| Box::new(TaggedAi("first")));
        assert_eq!(
            registry.create("dup", 0).expect("第一次注册的工厂").name(),
            "first"
        );
        registry.register("dup", |_seed| Box::new(TaggedAi("second")));
        assert_eq!(registry.names().len(), 1, "重名覆盖不能增加条目数");
        assert_eq!(
            registry.create("dup", 0).expect("第二次注册的工厂").name(),
            "second",
            "后注册的工厂必须生效"
        );
    }

    #[test]
    fn empty_registry_is_usable() {
        // CLI 从 AiRegistry::new() / default() 起步时不能有任何隐藏的内置项。
        for registry in [AiRegistry::new(), AiRegistry::default()] {
            assert!(registry.names().is_empty());
            assert!(!registry.contains(AI_RANDOM));
            assert!(registry.create(AI_RANDOM, 0).is_none());
        }
    }
}
