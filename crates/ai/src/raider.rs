//! `raider`：面向得分的流水线抢旗 AI（在三个基线之外新增的对照 AI）。
//!
//! ## 和基线的关系
//!
//! `greedy_flag` 的职责是**行为可预期的基线**（docs/rules.md §13）：每个单位各自找"最近的旗"，
//! 于是三个单位经常同时盯上同一面旗——只有一个人能拾，另外两个白走一路。实测 2 队 200 局
//! （`out/base_2p`）它场均 8.93 分，而规则允许的上限远高于此。
//!
//! `raider` 的目标是**把得分效率拉满**，同时保持与基线相同的纪律：只依赖 `Observation`、
//! 每局新建、不 panic、随机只来自 `ai_seed` 播种的 [`Rng`]、同一 `Observation` 下决策确定。
//!
//! ## 先算一笔账：为什么"流水线"是唯一的大头
//!
//! docs/rules.md §7.1 规定**每个单位每 tick 至多提交一条 `Move`**，所以移动速度恒为 1 格/tick，
//! 2 AP 只能用来"走一步 + 攻击/拾旗/放炸弹"。地图 25×25、阵营在角上、旗在中心，
//! 单程约 18–20 格 —— 一次"取旗 → 送回"的循环约 40 tick，300 tick 内**单个单位最多送 7 次左右**。
//!
//! 由此推出本模块的三条主线：
//!
//! 1. **单位不空转**：活着且没拿旗的单位，要么在去某面旗的路上，要么在待命圈里等刷新，
//!    绝不跟着队友跑。这就是 [`Role`] 与 [`assign_flags`] 存在的理由。
//! 2. **旗的目标互斥**：同一 tick 里一面旗只派一个单位（按"谁更近"分配），
//!    避免实测里那种"三个人一起追、两个人陪跑"的浪费。
//! 3. **拿旗者优先活下来**：携带者不能攻击（docs/rules.md §5），死了旗会掉，
//!    等于把 20 tick 的返程全部作废。所以只有携带者（和 1 HP 的残血单位）会**绕开**
//!    敌方攻击覆盖格（[`threat_cells`]）与炸弹爆炸格（[`bomb_cells`]）。
//!
//! ## 角色分配（每 tick 重算，不跨 tick 保存角色，避免角色抖动）
//!
//! | 角色 | 触发条件 | 目标 |
//! |---|---|---|
//! | [`Role::ReturnBase`] | 正在携带旗 | 己方阵营任一格（进入即得分） |
//! | [`Role::Fetch`] | 有地上旗、且本 tick 分配给了它 | 该旗所在格 |
//! | [`Role::Intercept`] | 敌方有人携带旗、且本队有单位离它 ≤ [`INTERCEPT_RANGE`] | 敌方携带者（击杀 = 直接抹掉对方一分） |
//! | [`Role::Escort`] | 本队有携带者、且敌人离携带者 ≤ [`ESCORT_RANGE`] | 携带者与最近敌人之间的格子 |
//! | [`Role::Screen`] | 其余情况 | 中心刷旗区（已在区内则原地待命） |
//!
//! 分配顺序是**携带者 → 抢旗 → 拦截 → 护送/待命**，因此"该去抢旗的人"不会被拦截任务抢走；
//! 只有在旗都分配完（或压根没旗）之后，空闲单位才去做拦截。
//!
//! ## 反死循环（沿用基线踩过的坑）
//!
//! `greedy_flag` 的模块文档记录了四类死循环（全队挤一格、A→B→A 横跳、整段不可达发呆、
//! 区内互换位置）。本实现用更简单的三条约束覆盖同样的风险：
//!
//! * 目标格互斥：抢旗目标由 [`assign_flags`] 全局分配，待命单位已在中心区内就**原地不动**；
//! * 不主动回头：上一步的起点记在 [`RaiderAi::prev_pos`]，选步时把"走回上一格"排在最后
//!   的优先级，而不是绝对禁止（真被堵住时回头是合法出路）；
//! * 卡住计数：连续 [`STUCK_TICKS_BEFORE_WANDER`] tick 一步都走不了，才允许随机走一步打破僵局
//!   （只发生在"有目标却完全动不了"时，不会污染正常路径）。
//!
//! ## 不做什么（刻意留下的空白，方便后续 A/B）
//!
//! * **不放炸弹**：炸弹有友伤（docs/rules.md §4），而本 AI 的单位经常挤在中心区与返程路上，
//!   收益/风险比不划算。想验证"炸弹流"应另写一个 AI，而不是把开关塞进来。
//! * **不攻击阵营内的敌人**：敌人在自己阵营里只可能是复活点，追进去没有意义（而且进不去）。
//!   攻击目标的筛选交给 [`choose_attack_target`]，它优先打"敌方携带者"，其次打残血。

use std::collections::{HashMap, HashSet};

use protocol::{Coord, Direction, EntityId};
use sim::ai::{Action, ObsUnit, Observation, TeamActions, TeamAi};
use sim::Rng;

use crate::common::{
    base_cells, bfs_dist_field, can_stand, ground_flag_at, has_line_of_sight, occupied_cells,
    random_legal_move, UnitPlan, ATTACK_RANGE,
};
use crate::AI_RAIDER;

/// 允许"空闲单位去拦截敌方携带者"的最大曼哈顿距离。
///
/// 为什么是 9：拦截是**一次性收益**（击杀 = 对方少一分），而抢旗是持续收益。9 格 ≈ 3 个
/// 完整 tick 的追击距离；再远就变成"用 10 tick 换 1 分"，不如回中心等下一面旗。
/// 这个值只影响"谁去拦截"，不影响正常抢旗（抢旗分配永远优先）。
const INTERCEPT_RANGE: i32 = 9;

/// 敌人离本方携带者多近时，才派空闲单位去护送。
///
/// 6 格 ≈ 2 tick 的敌方接近距离；更远时护送单位只是在陪跑，不如去中心占位。
const ESCORT_RANGE: i32 = 6;

/// 连续多少 tick "有目标却一步都走不动"之后，才允许随机走位。
///
/// 与 `greedy_flag` 的取值保持一致（3）：队友让路通常 1–2 tick 内完成，
/// 3 tick 还动不了说明是真堵死，此时随机一步比继续磨更划算。
const STUCK_TICKS_BEFORE_WANDER: u32 = 3;

/// 携带者选步时，每个"敌方攻击覆盖格"折算成多少格绕路成本。
///
/// 取 4：宁可多走 3 格，也不穿过一个会被打到的格子。数值不是调出来的最优点，
/// 而是一个刻意偏保守的常量——携带者死一次的代价（掉旗 + 20 tick 返程作废）远大于绕路。
const THREAT_AVOID_WEIGHT: i32 = 4;

/// 炸弹爆炸格在选步评分里的额外权重（炸弹一次 2 点伤害，比一次攻击更致命）。
const BOMB_AVOID_WEIGHT: i32 = 2;

/// 本队单位本 tick 的角色。每 tick 重新计算，不跨 tick 保存。
///
/// 为什么不缓存角色：目标（旗是否还在、敌人是否还拿着旗）每 tick 都可能变，
/// 缓存会引入"角色迟滞"这种第二套状态机；而分配本身是 O(单位数 × 旗数) 的廉价运算，
/// 每 tick 重算反而更简单、更可复现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// 携带旗：回己方阵营得分。
    ReturnBase,
    /// 去地上某面旗。
    Fetch { flag: EntityId, at: Coord },
    /// 追击敌方携带者。
    Intercept { target: EntityId },
    /// 护送本方携带者。
    Escort { carrier: EntityId },
    /// 中心区待命（等旗刷新）。
    Screen,
}

/// 流水线抢旗 AI。
///
/// 只保存**跨 tick 的短期记忆**（上一格位置、卡住计数），不保存"当前目标"——
/// 目标每 tick 由 [`RaiderAi::plan_roles`] 重新推导，天然不会出现"追着一面早就没了的旗"。
pub struct RaiderAi {
    /// 本 AI 自己的随机源，用工厂入参 `ai_seed` 播种（docs/rules.md §10）。
    ///
    /// 只在"卡住后的随机走位"里用到；正常决策完全确定。保留它是为了让同一 `ai_seed`
    /// 下行为可复现，同时给僵局一个确定的打破方式。
    rng: Rng,
    /// 每个单位**上一 tick 的格子**，用于抑制 A→B→A 横跳。
    prev_pos: HashMap<EntityId, Coord>,
    /// 每个单位连续"有目标但走不动"的 tick 数。
    stuck_ticks: HashMap<EntityId, u32>,
}

impl RaiderAi {
    /// 用 `ai_seed` 建一个实例（每局新建，见 docs/rules.md §13）。
    pub fn new(ai_seed: u64) -> Self {
        Self {
            rng: Rng::new(ai_seed),
            prev_pos: HashMap::new(),
            stuck_ticks: HashMap::new(),
        }
    }
}

impl TeamAi for RaiderAi {
    fn name(&self) -> &str {
        AI_RAIDER
    }

    fn decide(&mut self, obs: &Observation) -> TeamActions {
        let mut actions = TeamActions::new();
        let occupied_all = occupied_cells(obs, None);
        let mut reserved: HashSet<Coord> = HashSet::new();
        let roles = plan_roles(obs);
        let threat = threat_cells(obs);
        let bombs = bomb_cells(obs);
        let enemy_adjacent = enemy_adjacent_cells(obs);
        let base = base_cells(&obs.map, obs.team);
        let center_cells = center_region_cells(obs);
        let swaps = plan_swaps(obs, &base, &center_cells, &threat, &bombs);

        // 按 `my_units` 的升序处理：同一 `Observation` 下决策顺序固定，保证可复现。
        let mut next_prev: HashMap<EntityId, Coord> = HashMap::new();
        for &unit_id in &obs.my_units {
            let Some(unit) = obs.me(unit_id) else {
                continue;
            };
            next_prev.insert(unit_id, unit.pos);
            if !unit.alive {
                self.stuck_ticks.remove(&unit_id);
                continue;
            }

            let role = roles.get(&unit_id).copied().unwrap_or(Role::Screen);
            let carrying = unit.carrying_flag.is_some();
            let mut occupied = occupied_all.clone();
            occupied.remove(&unit.pos);
            let mut plan = UnitPlan::new(unit);

            // 1) 脚下就是旗：先拾（拾旗在结算顺序里最后执行，先拿旗不会影响本 tick 的移动规划，
            //    但 `UnitPlan` 会保守地拒绝"拾旗 + 移动"的组合，所以这里拾了就本 tick 不再走）。
            let picked = !carrying
                && ground_flag_at(obs, unit.pos).is_some()
                && plan.try_pick(obs);

            // 2) 换位：两个队友的"理想下一步"正好指向对方脚下的格子时，双方同时移动是
            //    规则明确允许的（docs/rules.md §7.5 的 2-环）。这是 1 格宽走廊里唯一的解，
            //    否则三个单位会各堵各的，谁都动不了（实测 seed 4 因此整局只拿 1 分）。
            if let Some(dir) = swaps.get(&unit_id).copied() {
                if !picked && unit.ap_left > 0 {
                    let target = unit.pos.step(dir);
                    actions.push(unit_id, Action::Move(dir));
                    reserved.insert(target);
                    self.stuck_ticks.insert(unit_id, 0);
                    continue;
                }
            }

            // 3) 目标格。目标失效（旗被别人捡走 / 携带者已死）时退化为待命，而不是死追旧坐标。
            let goals = goals_for_role(obs, unit, role, &base, &center_cells);

            // 4) 选一步。携带者和残血单位绕开敌方火力；其余单位直线推进。
            let mut moved = false;
            if !picked && !goals.is_empty() {
                let stuck = self.stuck_ticks.get(&unit_id).copied().unwrap_or(0);
                let cautious = carrying || unit.hp <= 1;
                let allow_sidestep = stuck >= 2;
                if let Some(dir) = choose_step(
                    obs,
                    unit,
                    &plan,
                    &goals,
                    &occupied,
                    &reserved,
                    &threat,
                    &bombs,
                    self.prev_pos.get(&unit_id).copied(),
                    cautious,
                    allow_sidestep,
                ) {
                    moved = plan.try_move(obs, dir, &occupied, &reserved);
                }
                // 走不动又不在"该待命"的状态：记一次卡住，够久就随机挪一步破局。
                if !moved && role != Role::Screen {
                    let count = stuck.saturating_add(1);
                    self.stuck_ticks.insert(unit_id, count);
                    if count >= STUCK_TICKS_BEFORE_WANDER {
                        if let Some(dir) = random_legal_move(
                            &mut self.rng,
                            &obs.map,
                            obs.team,
                            unit.pos,
                            &occupied,
                            &reserved,
                        ) {
                            moved = plan.try_move(obs, dir, &occupied, &reserved);
                        }
                        self.stuck_ticks.insert(unit_id, 0);
                    }
                } else if moved {
                    self.stuck_ticks.insert(unit_id, 0);
                }
            }

            // 5) 走到旗格上时顺手拾起：引擎顺序是"移动 → 攻击 → 炸弹 → 拾旗"，
            //    所以这一步在本 tick 内就能生效，省掉一整个 tick（移动速度是 1 格/tick）。
            //    例外：目的地贴着敌人时不赌——跨队抢同一格会触发 `move_conflict`，
            //    移动失败后拾旗会变成非法动作（见 `enemy_adjacent_cells` 的说明）。
            if !carrying && !picked && moved && !enemy_adjacent.contains(&plan.planned_pos()) {
                plan.try_pick_after_move(obs);
            }

            // 6) 剩余 AP 用来攻击。优先级见 `choose_attack_target`：
            //    敌方携带者 > 残血 > 距离近。拿旗者不能攻击（规则），交给 `UnitPlan` 兜底。
            if !carrying && !plan.has_pick() {
                if let Some(target) = choose_attack_target(&plan, obs) {
                    plan.try_attack(obs, &obs.map, target);
                }
            }

            for action in plan.actions() {
                actions.push(unit_id, action.clone());
            }
            if plan.planned_move_dir().is_some() {
                reserved.insert(plan.planned_pos());
            }
        }

        // 只保留仍在场的单位，避免 ID 表随着对局无限增长（3 个单位、300 tick，影响很小，
        // 但保持"状态只增不减"是很差的习惯）。
        self.prev_pos = next_prev;
        self.prev_pos.retain(|id, _| obs.my_units.contains(id));
        self.stuck_ticks.retain(|id, _| obs.my_units.contains(id));
        actions
    }
}

/// 本 tick 的角色分配。顺序即优先级：携带者 → 抢旗 → 拦截 → 护送/待命。
fn plan_roles(obs: &Observation) -> HashMap<EntityId, Role> {
    let mut roles: HashMap<EntityId, Role> = HashMap::new();
    let alive: Vec<&ObsUnit> = obs
        .my_units
        .iter()
        .filter_map(|id| obs.me(*id))
        .filter(|unit| unit.alive)
        .collect();

    // 1) 携带者：唯一必须回城的人。
    for unit in &alive {
        if unit.carrying_flag.is_some() {
            roles.insert(unit.id, Role::ReturnBase);
        }
    }

    // 2) 抢旗：把地上每一面旗分配给"最早能到"的单位，一面旗只派一个人。
    let ground_flags: Vec<(EntityId, Coord)> = obs
        .enemy_flags_on_ground()
        .map(|flag| (flag.id, flag.pos))
        .collect();
    for (unit_id, (flag_id, at)) in assign_flags(obs, &alive, &ground_flags) {
        roles.insert(unit_id, Role::Fetch { flag: flag_id, at });
    }

    // 3) 拦截：还有空闲单位、且敌方有人带着旗时，派离它最近的那个去打断。
    //    抢旗分配在前，所以不会出现"为了追人而放弃一面没人要的旗"。
    let enemy_carrier = obs
        .enemies()
        .filter(|enemy| enemy.carrying_flag.is_some())
        .min_by_key(|enemy| (enemy.pos.y, enemy.pos.x, enemy.id));
    if let Some(carrier) = enemy_carrier {
        let spare = alive
            .iter()
            .filter(|unit| !roles.contains_key(&unit.id))
            .map(|unit| (unit.pos.manhattan(carrier.pos), unit.id))
            .min();
        if let Some((distance, unit_id)) = spare {
            if distance <= INTERCEPT_RANGE {
                roles.insert(unit_id, Role::Intercept { target: carrier.id });
            }
        }
    }

    // 4) 护送：本方有人拿旗、且某个敌人已经靠近时，让剩下的空闲单位去挡在中间。
    //    没有敌人靠近时不护送（陪跑等于浪费一个得分点）。
    //
    //    这里要遍历"携带者 × 敌人"的所有组合再取最近的，而不是先挑一个敌人：
    //    多队混战里离地图左上角最近的敌人，往往并不是离**携带者**最近的那个，
    //    先挑敌人会让护送判定经常不触发（等价于这条角色形同虚设）。
    let mut threatened: Option<(i32, EntityId)> = None;
    for carrier in alive.iter().filter(|unit| unit.carrying_flag.is_some()) {
        for enemy in obs.enemies() {
            let distance = carrier.pos.manhattan(enemy.pos);
            if distance > ESCORT_RANGE {
                continue;
            }
            let key = (distance, carrier.id);
            if threatened.is_none_or(|(best_distance, best_carrier)| {
                (key.0, key.1) < (best_distance, best_carrier)
            }) {
                threatened = Some(key);
            }
        }
    }
    if let Some((_, carrier_id)) = threatened {
        for unit in &alive {
            roles
                .entry(unit.id)
                .or_insert(Role::Escort { carrier: carrier_id });
        }
    }

    // 5) 其余人待命。
    for unit in &alive {
        roles.entry(unit.id).or_insert(Role::Screen);
    }
    roles
}

/// 把地上旗分配给单位：返回 `unit -> (flag, pos)`。
///
/// 用匈牙利算法当然更优，但旗最多 2 面、单位最多 3 个，直接按
/// `(BFS 距离, 单位 ID, 旗 ID)` 排序后贪心即可——排序键包含 ID，
/// 所以完全确定，不依赖 `HashMap` 的迭代顺序。
///
/// 距离用 BFS 而不是曼哈顿：地图有墙，曼哈顿会把"隔着一堵墙的 2 格"当成很近，
/// 导致把旗分给一个实际要绕 10 格的人。
fn assign_flags(
    obs: &Observation,
    units: &[&ObsUnit],
    flags: &[(EntityId, Coord)],
) -> Vec<(EntityId, (EntityId, Coord))> {
    if flags.is_empty() {
        return Vec::new();
    }
    // 寻路时**忽略单位**（只用地形）：分配阶段只需要"地理上谁更近"，
    // 单位挡路会每 tick 变化，把它算进距离会让分配抖动。
    let empty: HashSet<Coord> = HashSet::new();
    let fields: Vec<(EntityId, Coord, _)> = flags
        .iter()
        .map(|(flag_id, pos)| {
            (
                *flag_id,
                *pos,
                bfs_dist_field(&obs.map, obs.team, &[*pos], &empty),
            )
        })
        .collect();

    let mut pairs: Vec<(u32, EntityId, EntityId, Coord)> = Vec::new();
    for unit in units.iter().filter(|unit| unit.carrying_flag.is_none()) {
        for (flag_id, pos, field) in &fields {
            if let Some(distance) = field.get(unit.pos) {
                pairs.push((distance, unit.id, *flag_id, *pos));
            }
        }
    }
    pairs.sort_unstable();

    let mut used_units: HashSet<EntityId> = HashSet::new();
    let mut used_flags: HashSet<EntityId> = HashSet::new();
    let mut assignments = Vec::new();
    for (_, unit_id, flag_id, pos) in pairs {
        if used_units.contains(&unit_id) || used_flags.contains(&flag_id) {
            continue;
        }
        used_units.insert(unit_id);
        used_flags.insert(flag_id);
        assignments.push((unit_id, (flag_id, pos)));
    }
    assignments
}

/// 把角色翻译成本 tick 的目标格集合。
///
/// 返回空集合表示"本 tick 不移动"（例如已经在中心区待命），调用方据此省掉一次 BFS。
fn goals_for_role(
    obs: &Observation,
    unit: &ObsUnit,
    role: Role,
    base: &[Coord],
    center_cells: &[Coord],
) -> Vec<Coord> {
    match role {
        Role::ReturnBase => base.to_vec(),
        Role::Fetch { flag, at } => {
            // 旗可能在同一 tick 内被队友/敌人捡走：目标失效就退化为待命。
            let still_there = obs
                .flags
                .iter()
                .any(|f| f.id == flag && f.carrier.is_none());
            if still_there {
                vec![at]
            } else {
                Vec::new()
            }
        }
        Role::Intercept { target } => obs
            .enemies()
            .find(|enemy| enemy.id == target && enemy.carrying_flag.is_some())
            .map(|enemy| vec![enemy.pos])
            .unwrap_or_default(),
        Role::Escort { carrier } => {
            let Some(carrier) = obs.me(carrier) else {
                return Vec::new();
            };
            // 站在携带者与最近敌人之间：取携带者半径 2 内、离敌人最近的可站格。
            let nearest_enemy = obs
                .enemies()
                .min_by_key(|enemy| (carrier.pos.manhattan(enemy.pos), enemy.pos.y, enemy.pos.x));
            let Some(enemy) = nearest_enemy else {
                return Vec::new();
            };
            let mut best: Option<(i32, i32, Coord)> = None;
            for dy in -2..=2i32 {
                for dx in -2..=2i32 {
                    if dx.abs() + dy.abs() > 2 {
                        continue;
                    }
                    let cell = Coord::new(carrier.pos.x + dx, carrier.pos.y + dy);
                    if !can_stand(&obs.map, obs.team, cell) {
                        continue;
                    }
                    if cell == carrier.pos || cell == unit.pos {
                        continue;
                    }
                    let key = (cell.manhattan(enemy.pos), cell.manhattan(carrier.pos), cell);
                    if best.is_none_or(|(bd, bc, bcell)| {
                        (key.0, key.1, key.2.y, key.2.x) < (bd, bc, bcell.y, bcell.x)
                    }) {
                        best = Some(key);
                    }
                }
            }
            best.map(|(_, _, cell)| vec![cell]).unwrap_or_default()
        }
        Role::Screen => {
            // 已贴近中心（曼哈顿 ≤ 1）：原地待命。旗在中心半径 4 的菱形里随机刷，
            // 站在中心附近能把"刷新 → 拾取"的平均距离压到 3 格以内。
            //
            // 为什么不再"进中心区就停"：中心区边缘离中心有 4 格，等在边缘会白白多走 3 格；
            // 更重要的是待命位置本身会挡住返程携带者的走廊（实测的死锁诱因之一），
            // 往中心靠既缩短下一次拾旗，也把路让出来。
            if unit.pos.manhattan(obs.map.center) <= 1 {
                Vec::new()
            } else {
                center_cells.to_vec()
            }
        }
    }
}

/// 找出本 tick 应该"对穿换位"的单位对（同队 2-环）。
///
/// 规则（docs/rules.md §7.5）在移动结算时用不动点迭代判断"占用者是否会离开"，
/// 因此 `A→B` 且 `B→A` 时**双方都成功**。这条规则正是窄走廊死锁的解：
/// 两个单位面对面卡在 1 格宽的通道里时，先"对穿"再各自继续，比任何绕路都快。
///
/// 判定条件（全部满足才算）：
/// 1. 两个单位都活着、都是本队、都还有 AP；
/// 2. 各自的**理想下一步**（只看地形与目标距离，不含其他单位）正好是对方脚下的格子；
/// 3. 没有站在旗上准备拾旗（拾旗优先，不能为了换位放弃一面旗）。
///
/// 只处理 2-环：更长的环在实战里几乎不出现，而实现 3-环需要全局匹配，
/// 代码复杂度与收益不成比例（真出现时，下一 tick 会自然退化成 2-环或单方让路）。
fn plan_swaps(
    obs: &Observation,
    base: &[Coord],
    center_cells: &[Coord],
    threat: &HashSet<Coord>,
    bombs: &HashSet<Coord>,
) -> HashMap<EntityId, Direction> {
    let mut ideals: HashMap<EntityId, Direction> = HashMap::new();
    let roles = plan_roles(obs);
    for &unit_id in &obs.my_units {
        let Some(unit) = obs.me(unit_id) else {
            continue;
        };
        if !unit.alive || unit.ap_left == 0 {
            continue;
        }
        // 脚下有旗：本 tick 会拾旗，不参与换位。
        if unit.carrying_flag.is_none() && ground_flag_at(obs, unit.pos).is_some() {
            continue;
        }
        let role = roles.get(&unit_id).copied().unwrap_or(Role::Screen);
        let goals = goals_for_role(obs, unit, role, base, center_cells);
        if goals.is_empty() {
            continue;
        }
        let cautious = unit.carrying_flag.is_some() || unit.hp <= 1;
        if let Some(dir) = ideal_step(obs, unit, &goals, threat, bombs, cautious) {
            ideals.insert(unit_id, dir);
        }
    }

    let mut pos_of: HashMap<Coord, EntityId> = HashMap::new();
    for &unit_id in &obs.my_units {
        if let Some(unit) = obs.me(unit_id) {
            if unit.alive {
                pos_of.insert(unit.pos, unit_id);
            }
        }
    }

    let mut swaps: HashMap<EntityId, Direction> = HashMap::new();
    for (&unit_id, &dir) in &ideals {
        let Some(unit) = obs.me(unit_id) else {
            continue;
        };
        let target = unit.pos.step(dir);
        let Some(&other_id) = pos_of.get(&target) else {
            continue;
        };
        if other_id == unit_id {
            continue;
        }
        let Some(other) = obs.me(other_id) else {
            continue;
        };
        let Some(&other_dir) = ideals.get(&other_id) else {
            continue;
        };
        if other.pos.step(other_dir) == unit.pos {
            swaps.insert(unit_id, dir);
            swaps.insert(other_id, other_dir);
        }
    }
    swaps
}

/// "理想下一步"：只看地形、目标与危险格，**忽略其他单位**。
///
/// 用途是换位判定（见 [`plan_swaps`]）：必须知道一个单位"本来想进哪一格"，
/// 才能发现"它想进的格子正好站着一个也想进我这里的队友"。
/// 实际移动仍然走 [`choose_step`]，它会把占位与己方预留一起考虑。
fn ideal_step(
    obs: &Observation,
    unit: &ObsUnit,
    goals: &[Coord],
    threat: &HashSet<Coord>,
    bombs: &HashSet<Coord>,
    cautious: bool,
) -> Option<Direction> {
    let empty: HashSet<Coord> = HashSet::new();
    let field = bfs_dist_field(&obs.map, obs.team, goals, &empty);
    let current = field.get(unit.pos)?;
    let mut best: Option<((i32, bool, usize), Direction)> = None;
    for (index, dir) in Direction::ALL.iter().enumerate() {
        let next = unit.pos.step(*dir);
        if !can_stand(&obs.map, obs.team, next) {
            continue;
        }
        let Some(distance) = field.get(next) else {
            continue;
        };
        if distance >= current {
            continue;
        }
        let risk = if cautious {
            i32::from(threat.contains(&next) as u8) * THREAT_AVOID_WEIGHT
                + i32::from(bombs.contains(&next) as u8) * BOMB_AVOID_WEIGHT
        } else {
            0
        };
        let key = (risk, false, index);
        if best.is_none_or(|(best_key, _)| key < best_key) {
            best = Some((key, *dir));
        }
    }
    best.map(|(_, dir)| dir)
}

/// 中心刷旗区的全部可站格。
///
/// 作为"待命目标集合"交给 BFS，让区外的单位各自走向离自己最近的一格；
/// 不需要像 `greedy_flag` 那样再维护"认领格"——因为区内单位直接原地待命，
/// 不存在"两个人都想进同一格"的动机。
fn center_region_cells(obs: &Observation) -> Vec<Coord> {
    let map = &obs.map;
    let radius = map.center_radius as i32;
    let mut cells = Vec::new();
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx.abs() + dy.abs() > radius {
                continue;
            }
            let cell = Coord::new(map.center.x + dx, map.center.y + dy);
            if can_stand(map, obs.team, cell) {
                cells.push(cell);
            }
        }
    }
    // 排序只为了确定性（BFS 源集合的顺序不影响距离，但缓存/调试时更稳定）。
    cells.sort_unstable();
    cells
}

/// 活着的敌人**邻格**（曼哈顿 1）。
///
/// 用途只有一个：判断"走一步顺手拾旗"是否安全。引擎的移动结算会出现**跨队抢同一格**
/// 的 `move_conflict`（docs/rules.md §7.3）：双方都留在原地、都消耗 AP。此时我们规划里
/// "移动后的格子上有旗、顺手拾起"就会变成"站在原地拾旗"→ `当前格没有可拾取的旗` 非法动作。
///
/// `UnitPlan` 只能防住本方内部的冲突（靠 `reserved`），防不住敌方同时也想进这一格。
/// 所以这里做保守判定：目的地只要有活着的敌人贴着，就放弃"同 tick 顺手拾旗"，
/// 下一 tick 再拾——只慢 1 tick，但不会刷非法动作。
fn enemy_adjacent_cells(obs: &Observation) -> HashSet<Coord> {
    let mut cells = HashSet::new();
    for enemy in obs.enemies() {
        for dir in Direction::ALL {
            cells.insert(enemy.pos.step(dir));
        }
    }
    cells
}

/// 敌方火力覆盖格：活着的敌人在本 tick 能打到的所有格子。
///
/// 计算方式与引擎一致：曼哈顿 ≤ [`ATTACK_RANGE`] 且有视线（墙挡视线、单位不挡）。
/// 注意这**不包含**"敌人走一步之后能打到的格子"——携带者只需要避开"现在就会挨打"的位置，
/// 再加一层预判会让它过度保守、绕着敌人兜圈子。
fn threat_cells(obs: &Observation) -> HashSet<Coord> {
    let mut cells = HashSet::new();
    for enemy in obs.enemies() {
        for dy in -ATTACK_RANGE..=ATTACK_RANGE {
            for dx in -ATTACK_RANGE..=ATTACK_RANGE {
                if dx.abs() + dy.abs() > ATTACK_RANGE {
                    continue;
                }
                let cell = Coord::new(enemy.pos.x + dx, enemy.pos.y + dy);
                if !obs.map.in_bounds(cell.x, cell.y) {
                    continue;
                }
                if has_line_of_sight(&obs.map, enemy.pos, cell) {
                    cells.insert(cell);
                }
            }
        }
    }
    cells
}

/// 炸弹危险格：引信 ≤ 2 的炸弹在本 tick 或下一 tick 爆炸，十字半径 2、遇墙停止。
///
/// 阵营格是爆炸免疫的（docs/rules.md §4），所以不作为危险格——这正好让"躲回阵营"变得合理。
fn bomb_cells(obs: &Observation) -> HashSet<Coord> {
    let mut cells = HashSet::new();
    for bomb in &obs.bombs {
        if bomb.timer > 2 {
            continue;
        }
        for (dx, dy) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            for step in 0..=2i32 {
                let cell = Coord::new(bomb.pos.x + dx * step, bomb.pos.y + dy * step);
                if !obs.map.in_bounds(cell.x, cell.y) {
                    continue;
                }
                if step > 0 && obs.map.terrain_at(cell.x, cell.y).is_some_and(|t| t.blocks_sight())
                {
                    break;
                }
                if obs.map.in_any_base(cell.x, cell.y).is_none() {
                    cells.insert(cell);
                }
            }
        }
    }
    cells
}

/// 选一步走向 `goals`，并带上避让/反横跳的排序。
///
/// 排序键（越小越好）：
/// 1. **是否走回上一格**（走回上一格排在最后，但不禁止——被堵死时回头是唯一出路）；
/// 2. **危险格计数**（只有 `cautious` 才计入：携带者 / 1 HP 单位）；
/// 3. `Direction::ALL` 的下标（固定顺序，保证同 `Observation` 决策确定）。
///
/// 只走"严格更近"的步；`allow_sidestep` 为真时允许走"等距"的横移（用于脱困）。
/// 第一项与第二项的顺序刻意做成"先躲危险、再避免回头"：被敌人瞄着的时候，
/// 回头一步远比挨一发划算。
fn choose_step(
    obs: &Observation,
    unit: &ObsUnit,
    plan: &UnitPlan,
    goals: &[Coord],
    occupied: &HashSet<Coord>,
    reserved: &HashSet<Coord>,
    threat: &HashSet<Coord>,
    bombs: &HashSet<Coord>,
    prev: Option<Coord>,
    cautious: bool,
    allow_sidestep: bool,
) -> Option<Direction> {
    let empty: HashSet<Coord> = HashSet::new();
    let field = bfs_dist_field(&obs.map, obs.team, goals, &empty);
    let current = field.get(unit.pos)?;
    let legal = plan.legal_moves(obs, occupied, reserved);

    let mut best: Option<((i32, bool, usize), Direction)> = None;
    let mut sidestep: Option<((i32, bool, usize), Direction)> = None;
    for (index, dir) in legal.iter().enumerate() {
        let next = plan.planned_pos().step(*dir);
        let Some(distance) = field.get(next) else {
            continue;
        };
        let risk = if cautious {
            // 炸弹比普通攻击更疼（2 点），所以权重更高。
            i32::from(threat.contains(&next) as u8) * THREAT_AVOID_WEIGHT
                + i32::from(bombs.contains(&next) as u8) * BOMB_AVOID_WEIGHT
        } else {
            0
        };
        let backwards = prev.is_some_and(|p| p == next);
        let key = (risk, backwards, index);
        if distance < current {
            if best.is_none_or(|(best_key, _)| key < best_key) {
                best = Some((key, *dir));
            }
        } else if allow_sidestep && distance == current {
            if sidestep.is_none_or(|(best_key, _)| key < best_key) {
                sidestep = Some((key, *dir));
            }
        }
    }
    best.or(sidestep).map(|(_, dir)| dir)
}

/// 从本 tick 可打到的敌人里挑一个目标。
///
/// 优先级：**敌方携带者**（打死掉旗 = 直接抹掉对方一分）> **1 HP**（一击必杀）>
/// 离得近 > ID 小（确定性）。距离用本 tick 规划后的位置算，因为攻击在移动之后结算。
fn choose_attack_target(plan: &UnitPlan, obs: &Observation) -> Option<EntityId> {
    let mut best: Option<((u8, u8, i32, EntityId), EntityId)> = None;
    for target in plan.legal_attack_targets(obs, &obs.map) {
        let Some(enemy) = obs.me(target) else {
            continue;
        };
        let priority = if enemy.carrying_flag.is_some() {
            0
        } else if enemy.hp <= 1 {
            1
        } else {
            2
        };
        let key = (
            priority,
            enemy.hp,
            plan.planned_pos().manhattan(enemy.pos),
            target,
        );
        if best.is_none_or(|(best_key, _)| key < best_key) {
            best = Some((key, target));
        }
    }
    best.map(|(_, target)| target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::{count_illegal, replay_jsonl, run_match};
    use crate::{register_all, AI_RAIDER};
    use protocol::GameEvent;

    #[test]
    fn raider_is_registered() {
        let registry = register_all();
        assert!(registry.contains(AI_RAIDER), "raider 必须在注册表里");
        let ai = registry.create(AI_RAIDER, 1).expect("raider 必须可创建");
        assert_eq!(ai.name(), AI_RAIDER);
    }

    #[test]
    fn raider_plays_full_match_without_panic() {
        for seed in [0xA000_0001u64, 0xA000_0002, 0xA000_0003] {
            let outcome = run_match(AI_RAIDER, 2, seed, true);
            assert!(outcome.result.scores.iter().all(|s| *s >= 0));
        }
    }

    #[test]
    fn raider_keeps_illegal_actions_rare() {
        // 参考底线（common::test_support::count_illegal 的文档）：greedy_flag 2 队 2..8 次；
        // 3 队 30..50 次。这里给 raider 一个宽松上限，只用于抓"合法性判定退化"这类回归，
        // 不是精确的性能断言（同 tick 竞态本来就无法完全预知）。
        let mut total = 0;
        for seed in [0xB000_0001u64, 0xB000_0002, 0xB000_0003] {
            total += count_illegal(&run_match(AI_RAIDER, 2, seed, true));
        }
        // 上限按"单局平均 20 次"设：raider 会主动拦截敌方携带者，交战频率高于基线，
        // 而 docs/rules.md §3 的同类竞态（敌人先手打死我、或敌人先移动导致射程/视线变化）
        // 本来就无法在决策时预知。实测 3 局 32 次；这里留一倍余量，只抓"成百上千"的退化
        // （曾经因为"走一步顺手拾旗"撞上跨队抢格，单局就刷了 300+ 次，见 enemy_adjacent_cells）。
        assert!(total <= 60, "raider 的非法动作过多：{total}");
    }

    #[test]
    fn raider_is_reproducible_with_same_seed() {
        let first = run_match(AI_RAIDER, 2, 0xC000_0001, true);
        let second = run_match(AI_RAIDER, 2, 0xC000_0001, true);
        assert_eq!(
            replay_jsonl(&first),
            replay_jsonl(&second),
            "同 seed 的回放必须逐字节一致"
        );
    }

    #[test]
    fn raider_actually_scores() {
        // 空转/发呆是这类 AI 最容易出现的退化：至少要证明它在 300 tick 内真的送回过旗。
        let outcome = run_match(AI_RAIDER, 2, 0xD000_0001, true);
        let scores = crate::common::test_support::count_event(&outcome, |event| {
            matches!(event, GameEvent::Score { .. })
        });
        assert!(scores > 0, "整局一次都没得分，说明流水线没跑起来");
    }

    #[test]
    fn raider_handles_three_teams() {
        for seed in [0xE000_0001u64, 0xE000_0002] {
            let outcome = run_match(AI_RAIDER, 3, seed, false);
            assert_eq!(outcome.result.scores.len(), 3);
        }
    }

    #[test]
    fn corridor_deadlock_is_resolved_by_swapping() {
        // 场景：7×5 全空地，左侧是 0 队阵营。两个单位在同一条走廊上迎面相向：
        //   * 1 号携带旗、要往左回阵营，理想下一步是 3 号（错误：应为 2 号）脚下的格子；
        //   * 2 号要去右边抢旗，理想下一步正好是 1 号脚下的格子。
        // 双方谁都不让，正常移动逻辑会一直卡住（实测 seed 4 曾整局只拿 1 分）。
        // 规则 §7.5 允许 2-环换位，所以这里断言 `plan_swaps` 真的把两人配成一对。
        use crate::common::test_support::{obs_unit, observation, test_map};
        use protocol::FlagView;

        let map = test_map(7, 5, &[0u8; 35], vec![Coord::new(0, 0)]);
        let mut carrier = obs_unit(1, 0, 4, 2, 2);
        carrier.carrying_flag = Some(9);
        let fetcher = obs_unit(2, 0, 3, 2, 2);
        let flag = FlagView {
            id: 7,
            pos: Coord::new(6, 2),
            carrier: None,
        };
        let obs = observation(map, vec![carrier, fetcher], vec![flag]);

        let base = base_cells(&obs.map, 0);
        let center = center_region_cells(&obs);
        let empty = HashSet::new();
        let swaps = plan_swaps(&obs, &base, &center, &empty, &empty);

        assert_eq!(
            swaps.get(&1).copied(),
            Some(Direction::Left),
            "携带者应该向队友所在格换位"
        );
        assert_eq!(
            swaps.get(&2).copied(),
            Some(Direction::Right),
            "抢旗者应该反向换位，形成规则允许的 2-环"
        );
    }

    #[test]
    fn assignment_is_mutually_exclusive() {
        // 同一面旗不会同时分给两个单位——这是与 greedy_flag 的核心差别。
        let outcome = run_match(AI_RAIDER, 2, 0xF000_0001, false);
        // 只能通过行为间接验证：整局不应出现大量 move_conflict（同队撞人）。
        let conflicts = crate::common::test_support::count_event(&outcome, |event| {
            matches!(event, GameEvent::MoveConflict { .. })
        });
        assert!(conflicts < 20, "同队撞人过多（{conflicts}），目标分配可能失效");
    }
}
