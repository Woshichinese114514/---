//! 基线 AI 的共享工具：合法性判定、视线（超覆盖直线）、BFS 寻路，以及
//! 「单 tick 单位动作规划器」[`UnitPlan`]。
//!
//! ## 为什么这些工具要写得这么保守
//!
//! `sim` 会对被拒绝的动作生成 `illegal_action` 事件（docs/rules.md §7.7），而
//! docs/rules.md §14 第 16 条要求基线 AI「不产生非法动作（除设计允许的少数）」。
//! 但 AI 只能看到决策前的 `Observation`，无法预知同一 tick 内其它单位的行为。
//! 所以这里统一采取**保守判定**，宁可少走一步，也不赌引擎的边界行为：
//!
//! * 移动目标格在决策时刻必须**可站立且当前为空**，并且没有被己方其它单位预定
//!   （否则会撞人：`move_conflict`，或「目标格被占用」失败）；
//! * 视线使用**超覆盖（supercover）直线**：它包含 Bresenham 直线经过的所有格子甚至更多，
//!   是 sim 判定的严格超集 → 宁可不打，也不会误报非法攻击；
//! * [`UnitPlan`] 里的「第二个动作」必须在第一个动作**成功与失败两种假设下**都合法。
//!
//! ## 一个容易踩的坑：结算顺序不是提交顺序
//!
//! docs/rules.md §7.8 规定同一 AP 池的结算顺序是「移动 → 攻击 → 放炸弹 → 拾旗」。
//! 也就是说提交 `[PickFlag, Move]` 时，引擎**先移动、后拾旗**——移动走开之后拾旗会失败。
//! 因此 `UnitPlan::try_move` 会用**移动后的新位置**重新校验所有「结算晚于移动」的已规划动作
//! （攻击的射程/视线、炸弹的阵营格限制、拾旗的旗格），保证提交的动作序列在引擎的实际
//! 结算顺序下依然全部合法。

use std::collections::{HashSet, VecDeque};

use protocol::{Coord, Direction, EntityId, TeamId};
use sim::ai::{Action, MapView, ObsUnit, Observation};
use sim::Rng;

/// 攻击射程（曼哈顿距离）。
///
/// 与 `sim::RulesConfig::attack_range` 的默认值一致（docs/rules.md §3、docs/internal-api.md §3.2）。
/// 注意：`Observation` 里**没有** `RulesConfig`，所以这里只能写常量；如果将来规则把射程做成
/// 可配置项并透传给 AI，这个常量必须跟着改（否则 AI 会误判射程，产生非法攻击）。
pub const ATTACK_RANGE: i32 = 3;

/// 格子能否被 `team` 站在上面：可通行，且不是**敌方**阵营格。
///
/// docs/rules.md §1：敌方单位不能进入己方阵营；自己的单位可以进自己阵营（用于得分/复活）。
/// `in_any_base` 返回地形编码里的阵营归属，注意入参坐标是 `Coord` 的 `x/y`。
pub fn can_stand(map: &MapView, team: TeamId, pos: Coord) -> bool {
    if !map.in_bounds(pos.x, pos.y) {
        return false;
    }
    if !map.is_walkable(pos.x, pos.y) {
        return false;
    }
    match map.in_any_base(pos.x, pos.y) {
        Some(owner) => owner == team,
        None => true,
    }
}

/// 当前存活单位占用的格子集合。
///
/// `ignore` 用于排除「自己」：一个单位当然可以「待在自己格子里」，判断移动目标时
/// 不应该把自己当成障碍。死亡单位不在地图上、不阻挡移动（docs/rules.md §2），所以只收 `alive`。
pub fn occupied_cells(obs: &Observation, ignore: Option<EntityId>) -> HashSet<Coord> {
    let mut set = HashSet::new();
    for unit in &obs.units {
        if !unit.alive {
            continue;
        }
        if Some(unit.id) == ignore {
            continue;
        }
        set.insert(unit.pos);
    }
    set
}

/// 扫描出某队 3×3 阵营区的全部格子。
///
/// 为什么不直接用 `MapView.bases[team] + 边长 3`：阵营区边长 `base_size` 属于 `RulesConfig`，
/// 而 `Observation` 不携带规则配置；用 `in_base` 扫描是对「阵营区定义」唯一不依赖硬编码常量的做法
/// （25×25 地图只有 625 格，每 tick 扫一次的开销可以忽略）。
pub fn base_cells(map: &MapView, team: TeamId) -> Vec<Coord> {
    let mut cells = Vec::new();
    for y in 0..map.height as i32 {
        for x in 0..map.width as i32 {
            if map.in_base(team, x, y) {
                cells.push(Coord::new(x, y));
            }
        }
    }
    cells
}

/// 阵营区中心（取外接矩形的中点；3×3 时正好是正中心）。
pub fn base_center(cells: &[Coord]) -> Option<Coord> {
    let min_x = cells.iter().map(|c| c.x).min()?;
    let max_x = cells.iter().map(|c| c.x).max()?;
    let min_y = cells.iter().map(|c| c.y).min()?;
    let max_y = cells.iter().map(|c| c.y).max()?;
    Some(Coord::new((min_x + max_x) / 2, (min_y + max_y) / 2))
}

/// 到一组目标格的最小曼哈顿距离；目标为空时返回 `i32::MAX`（调用方需自行判断）。
pub fn distance_to_cells(pos: Coord, cells: &[Coord]) -> i32 {
    cells
        .iter()
        .map(|c| pos.manhattan(*c))
        .min()
        .unwrap_or(i32::MAX)
}

/// 找出站在 `pos` 上的**地上**旗（`carrier == None`）。
///
/// flag 不会重叠（docs/rules.md §5），所以最多命中一面。
/// 注意协议里的 `FlagView` **没有归属队字段**：地上旗对任何队伍都是可抢目标，
/// 因此这里不能按队伍过滤（`enemy_flags_on_ground` 的名字有误导性）。
pub fn ground_flag_at(obs: &Observation, pos: Coord) -> Option<EntityId> {
    obs.flags
        .iter()
        .find(|flag| flag.carrier.is_none() && flag.pos == pos)
        .map(|flag| flag.id)
}

/// 两点之间是否有视线（docs/rules.md §3）。
///
/// 判定方式与规则描述一致：看两点连线除起点与终点外的所有格子是否 `blocks_sight()`（只有墙阻挡）。
/// 实现用**超覆盖直线**，比 Bresenham 细线更严格，因此是保守近似（见模块注释）。
pub fn has_line_of_sight(map: &MapView, from: Coord, to: Coord) -> bool {
    for cell in supercover_cells(from, to) {
        if cell == from || cell == to {
            continue;
        }
        if !map.in_bounds(cell.x, cell.y) {
            // 越界格保守视为阻挡：正常情况不会发生（两端都在图内）。
            return false;
        }
        if map.blocks_sight(cell.x, cell.y) {
            return false;
        }
    }
    true
}

/// 列出线段 `from → to` 穿过的所有格子（超覆盖 / voxel traversal）。
///
/// 做法：从两个格子的中心连一条射线，按网格边界步进，逐格收集。
/// 当射线**正好穿过格角**时，把角点四周的格子都算进来——这会让视线判定比
/// 只取一条 Bresenham 细线更严格（可能因此少打一次，但不会误报合法而吃到非法动作事件）。
fn supercover_cells(from: Coord, to: Coord) -> Vec<Coord> {
    let mut cells = Vec::with_capacity(8);
    let (mut x, mut y) = (from.x, from.y);
    let dx = (to.x - from.x).abs();
    let dy = (to.y - from.y).abs();
    let sx = (to.x - from.x).signum();
    let sy = (to.y - from.y).signum();

    // 参数 t ∈ [0,1] 表示整条线段；格子边界到中心的距离是 0.5。
    let t_delta_x = if dx == 0 {
        f64::INFINITY
    } else {
        1.0 / dx as f64
    };
    let t_delta_y = if dy == 0 {
        f64::INFINITY
    } else {
        1.0 / dy as f64
    };
    let mut t_max_x = if dx == 0 {
        f64::INFINITY
    } else {
        0.5 / dx as f64
    };
    let mut t_max_y = if dy == 0 {
        f64::INFINITY
    } else {
        0.5 / dy as f64
    };

    cells.push(Coord::new(x, y));
    // 每轮至少推进一个轴 1 格，因此最多迭代 dx+dy 次；`guard` 只是防御性上界。
    let mut guard = (dx + dy) as usize + 2;
    while (x, y) != (to.x, to.y) && guard > 0 {
        guard -= 1;
        if (t_max_x - t_max_y).abs() < 1e-9 {
            // 正好穿过格角：角点四周的格子都属于超覆盖，全部纳入（更保守）。
            if sx != 0 {
                cells.push(Coord::new(x + sx, y));
            }
            if sy != 0 {
                cells.push(Coord::new(x, y + sy));
            }
            x += sx;
            y += sy;
            cells.push(Coord::new(x, y));
            t_max_x += t_delta_x;
            t_max_y += t_delta_y;
        } else if t_max_x < t_max_y {
            x += sx;
            cells.push(Coord::new(x, y));
            t_max_x += t_delta_x;
        } else {
            y += sy;
            cells.push(Coord::new(x, y));
            t_max_y += t_delta_y;
        }
    }
    cells
}

/// 二维索引，越界返回 `None`（避免任何 panic 路径）。
fn grid_index(width: i32, height: i32, pos: Coord) -> Option<usize> {
    if pos.x < 0 || pos.y < 0 || pos.x >= width || pos.y >= height {
        return None;
    }
    Some((pos.y * width + pos.x) as usize)
}

/// 从一组源格出发的 BFS 距离场（多源最短路）。
///
/// * 只经过「本队可站立」的格子；
/// * `blocked` 里的格子不能进入（调用方用来避开当前有单位站着的格子——AI 保守策略：
///   不指望别人让路，因此不规划「踩进别人格子」的走法）；
/// * 同时记录每格的**最近源下标**，用于「选择最近的旗并认领它」这类需要知道目标身份的决策。
///
/// 用 BFS 而不是曼哈顿贪心：`greedy_flag` 要求「沿最短路径回阵营」，贪心会被墙卡住。
pub fn bfs_dist_field(
    map: &MapView,
    team: TeamId,
    sources: &[Coord],
    blocked: &HashSet<Coord>,
) -> DistField {
    let width = map.width as i32;
    let height = map.height as i32;
    let mut dist: Vec<Option<u32>> = vec![None; (width * height) as usize];
    let mut src: Vec<Option<u32>> = vec![None; (width * height) as usize];
    let mut queue: VecDeque<Coord> = VecDeque::new();

    for (i, &s) in sources.iter().enumerate() {
        if !can_stand(map, team, s) || blocked.contains(&s) {
            continue;
        }
        let Some(idx) = grid_index(width, height, s) else {
            continue;
        };
        if dist.get(idx).copied().flatten().is_none() {
            dist[idx] = Some(0);
            src[idx] = Some(i as u32);
            queue.push_back(s);
        }
    }

    while let Some(pos) = queue.pop_front() {
        let Some(idx) = grid_index(width, height, pos) else {
            continue;
        };
        let Some(d) = dist.get(idx).copied().flatten() else {
            continue;
        };
        for dir in Direction::ALL {
            let next = pos.step(dir);
            let Some(nidx) = grid_index(width, height, next) else {
                continue;
            };
            if dist.get(nidx).copied().flatten().is_some() {
                continue;
            }
            if !can_stand(map, team, next) || blocked.contains(&next) {
                continue;
            }
            dist[nidx] = Some(d + 1);
            src[nidx] = src.get(idx).copied().flatten();
            queue.push_back(next);
        }
    }

    DistField {
        width,
        height,
        dist,
        src,
    }
}

/// BFS 距离场：查询「离最近源多远」「最近源是谁」「往哪走能更近」。
pub struct DistField {
    width: i32,
    height: i32,
    dist: Vec<Option<u32>>,
    src: Vec<Option<u32>>,
}

impl DistField {
    /// 到最近源的距离；`None` 表示不可达。
    pub fn get(&self, pos: Coord) -> Option<u32> {
        let idx = grid_index(self.width, self.height, pos)?;
        self.dist.get(idx).copied().flatten()
    }

    /// 最近源的下标与距离。
    pub fn nearest_source(&self, pos: Coord) -> Option<(usize, u32)> {
        let idx = grid_index(self.width, self.height, pos)?;
        let d = self.dist.get(idx).copied().flatten()?;
        let s = self.src.get(idx).copied().flatten()? as usize;
        Some((s, d))
    }

    /// 从 `pos` 出发、离最近源更近的一步；`avoid` 里的格子不选（用于己方单位之间避让）。
    ///
    /// 平局按 `Direction::ALL` 的固定顺序取，保证同一 `Observation` 下决策确定
    /// （可复现性是本项目的硬要求）。
    pub fn step_toward(&self, pos: Coord, avoid: &HashSet<Coord>) -> Option<Direction> {
        let current = self.get(pos)?;
        if current == 0 {
            return None;
        }
        let mut best: Option<(u32, Direction)> = None;
        for dir in Direction::ALL {
            let next = pos.step(dir);
            if avoid.contains(&next) {
                continue;
            }
            let Some(d) = self.get(next) else {
                continue;
            };
            if d < current && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, dir));
            }
        }
        best.map(|(_, dir)| dir)
    }
}

/// 贪心朝一组目标靠近的一步（只接受**严格更近**的一步）。
///
/// 用于 BFS 不可达时的兜底，以及 `defender` 贴近敌人这种短距离、不需要全局最短路的行为。
pub fn greedy_step_toward(
    map: &MapView,
    team: TeamId,
    from: Coord,
    goals: &[Coord],
    occupied: &HashSet<Coord>,
    reserved: &HashSet<Coord>,
) -> Option<Direction> {
    let current = goals
        .iter()
        .map(|g| from.manhattan(*g))
        .min()
        .unwrap_or(i32::MAX);
    let mut best: Option<(i32, Direction)> = None;
    for dir in Direction::ALL {
        let next = from.step(dir);
        if !can_stand(map, team, next) || occupied.contains(&next) || reserved.contains(&next) {
            continue;
        }
        let d = goals
            .iter()
            .map(|g| next.manhattan(*g))
            .min()
            .unwrap_or(i32::MAX);
        if d < current && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, dir));
        }
    }
    best.map(|(_, dir)| dir)
}

/// 随机挑一个合法移动方向（random AI 兜底 / `greedy_flag` 被堵死时避免站着不动）。
pub fn random_legal_move(
    rng: &mut Rng,
    map: &MapView,
    team: TeamId,
    from: Coord,
    occupied: &HashSet<Coord>,
    reserved: &HashSet<Coord>,
) -> Option<Direction> {
    let mut dirs: Vec<Direction> = Direction::ALL
        .iter()
        .copied()
        .filter(|dir| {
            let next = from.step(*dir);
            can_stand(map, team, next) && !occupied.contains(&next) && !reserved.contains(&next)
        })
        .collect();
    if dirs.is_empty() {
        return None;
    }
    let i = rng.gen_index(dirs.len());
    Some(dirs.swap_remove(i))
}

/// 单 tick、单个单位的动作规划器。
///
/// 设计意图（关键，见模块头注释）：
/// * 每个单位每 tick **最多 1 个移动意图**（docs/rules.md §7.1），所以 `has_move` 一旦成立
///   就不再接受移动；
/// * 移动之后的动作（攻击/炸弹/拾旗）由引擎在移动**之后**结算，因此 `try_move` 必须用新位置
///   重新校验它们；反过来，移动之前的这些动作在「移动失败」时发生在原位置，所以 `try_attack` /
///   `can_bomb` / `try_pick` 都要求**两个位置都合法**；
/// * AP 预算取自 `ObsUnit.ap_left`，本规划器内部递减，防止提交超过 AP 的动作（那会变成「AP 不足」非法动作）。
pub struct UnitPlan<'a> {
    unit: &'a ObsUnit,
    ap: u32,
    /// 「已规划动作全部成功」时的位置；没有规划移动时等于 `unit.pos`。
    pos_now: Coord,
    has_move: bool,
    move_dir: Option<Direction>,
    has_attack: bool,
    attack_target: Option<EntityId>,
    has_bomb: bool,
    has_pick: bool,
    pick_target: Option<EntityId>,
    actions: Vec<Action>,
}

impl<'a> UnitPlan<'a> {
    /// 为 `unit` 开一个本 tick 的动作规划器（AP 预算 = `unit.ap_left`）。
    pub fn new(unit: &'a ObsUnit) -> Self {
        Self {
            unit,
            ap: unit.ap_left as u32,
            pos_now: unit.pos,
            has_move: false,
            move_dir: None,
            has_attack: false,
            attack_target: None,
            has_bomb: false,
            has_pick: false,
            pick_target: None,
            actions: Vec::new(),
        }
    }

    /// 单位原始位置（本 tick 决策时的位置）。
    pub fn original_pos(&self) -> Coord {
        self.unit.pos
    }

    /// 已规划动作全部成功后的位置（没有规划移动时即原始位置）。
    ///
    /// 拾旗/攻击的合法性都以这个位置为准——引擎的结算顺序是「移动 → 攻击 → 炸弹 → 拾旗」，
    /// 所以移动成功时后续动作发生在 `planned_pos`，移动失败时发生在 `original_pos`。
    pub fn planned_pos(&self) -> Coord {
        self.pos_now
    }

    /// 剩余 AP。
    pub fn ap_left(&self) -> u32 {
        self.ap
    }

    /// 已规划移动方向。
    pub fn planned_move_dir(&self) -> Option<Direction> {
        self.move_dir
    }

    /// 是否已规划拾旗。
    pub fn has_pick(&self) -> bool {
        self.has_pick
    }

    /// 已规划的动作序列（按规划顺序；引擎会按「移动→攻击→炸弹→拾旗」重排结算）。
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// 攻击目标 `target` 从 `from` 出发是否合法（射程 + 视线 + 目标存活 + 敌对）。
    ///
    /// 注意这里不检查 `can_attack()`/是否拿旗，由调用方在「当前状态」层面统一判断。
    fn attack_legal_at(&self, obs: &Observation, target: EntityId, from: Coord) -> bool {
        let Some(t) = obs.units.iter().find(|u| u.id == target) else {
            return false;
        };
        if !t.alive || t.team == self.unit.team {
            return false;
        }
        if from.manhattan(t.pos) > ATTACK_RANGE {
            return false;
        }
        has_line_of_sight(&obs.map, from, t.pos)
    }

    /// 本次决策里是否具备攻击资格：存活、还有 AP、本回合未攻击、且**没有拿旗**。
    ///
    /// 「拿旗不能攻击」是默认规则（`flag_carrier_can_attack = false`，docs/rules.md §5）；
    /// `Observation` 不暴露该开关，所以这里按默认规则保守处理。
    fn attack_ready(&self) -> bool {
        self.ap > 0
            && !self.has_attack
            && !self.has_pick
            && self.unit.can_attack()
            && self.unit.carrying_flag.is_none()
    }

    /// 本 tick 可以从当前位置打到、且**移动失败/成功两种情况下都合法**的所有敌人。
    pub fn legal_attack_targets(&self, obs: &Observation, _map: &MapView) -> Vec<EntityId> {
        if !self.attack_ready() {
            return Vec::new();
        }
        let mut targets = Vec::new();
        for enemy in &obs.units {
            if !enemy.alive || enemy.team == self.unit.team {
                continue;
            }
            if self.attack_legal_at(obs, enemy.id, self.unit.pos)
                && self.attack_legal_at(obs, enemy.id, self.pos_now)
            {
                targets.push(enemy.id);
            }
        }
        targets.sort_unstable();
        targets
    }

    /// 最近的合法攻击目标（先比曼哈顿距离，再比 ID，保证确定性）。
    pub fn best_attack_target(&self, obs: &Observation, map: &MapView) -> Option<EntityId> {
        let mut best: Option<(i32, EntityId)> = None;
        for id in self.legal_attack_targets(obs, map) {
            let Some(enemy) = obs.units.iter().find(|u| u.id == id) else {
                continue;
            };
            let d = self.pos_now.manhattan(enemy.pos);
            if best.is_none_or(|(bd, bid)| (d, id) < (bd, bid)) {
                best = Some((d, id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// 登记一次攻击。必须在 `unit.pos` 与 `pos_now` 两个位置都成立（见结构体注释）。
    pub fn try_attack(&mut self, obs: &Observation, _map: &MapView, target: EntityId) -> bool {
        if !self.attack_ready() {
            return false;
        }
        if !self.attack_legal_at(obs, target, self.unit.pos)
            || !self.attack_legal_at(obs, target, self.pos_now)
        {
            return false;
        }
        self.actions.push(Action::Attack(target));
        self.ap -= 1;
        self.has_attack = true;
        self.attack_target = Some(target);
        true
    }

    /// 是否能放炸弹：还有 AP、本 tick 未放过；且**当前位置与移动后位置都不是阵营格**。
    ///
    /// docs/rules.md §1：任何阵营格内禁止放炸弹。炸弹在移动之后结算，所以两个位置都要检查。
    pub fn can_bomb(&self, obs: &Observation) -> bool {
        if self.has_bomb || self.ap == 0 {
            return false;
        }
        !in_any_base(obs, self.unit.pos) && !in_any_base(obs, self.pos_now)
    }

    /// 放炸弹。
    pub fn try_bomb(&mut self, obs: &Observation) -> bool {
        if !self.can_bomb(obs) {
            return false;
        }
        self.actions.push(Action::PlaceBomb);
        self.ap -= 1;
        self.has_bomb = true;
        true
    }

    /// 是否能拾旗：还有 AP、本 tick 未拾、当前没拿旗；且**原始位置与移动后位置踩的是同一面地上旗**。
    ///
    /// 「同一面」这个约束是因为拾旗在移动之后结算：移动一旦离开旗格，拾旗就会失败并产生非法动作。
    /// 换句话说，本规划器只支持「原地拾旗」，不支持「走一步顺便拾旗」——后者需要引擎在移动后
    /// 才知道新位置有没有旗，属于过强的假设，基线 AI 不赌它。
    pub fn can_pick(&self, obs: &Observation) -> bool {
        if self.has_pick || self.ap == 0 || self.unit.carrying_flag.is_some() {
            return false;
        }
        matches!(
            (
                ground_flag_at(obs, self.unit.pos),
                ground_flag_at(obs, self.pos_now),
            ),
            (Some(a), Some(b)) if a == b
        )
    }

    /// 拾旗。
    pub fn try_pick(&mut self, obs: &Observation) -> bool {
        if !self.can_pick(obs) {
            return false;
        }
        let flag = ground_flag_at(obs, self.unit.pos).unwrap_or(0);
        self.actions.push(Action::PickFlag);
        self.ap -= 1;
        self.has_pick = true;
        self.pick_target = Some(flag);
        true
    }

    /// 「先走一步、再拾起目的地那面旗」：本 tick 已规划移动，且**移动后的格子**上正好有一面地上旗。
    ///
    /// 为什么值得单独开一个方法（而不是让 [`UnitPlan::can_pick`] 放宽）：
    /// * 引擎结算顺序是「移动 → … → 拾旗」，所以只要移动成功，拾旗就发生在**新位置**上，
    ///   `try_pick` 那种「原地拾旗」的保守要求（两个位置踩同一面旗）在这里是多余的；
    /// * 但风险确实存在：如果移动**失败**（例如目标格同时被对手请求 → `move_conflict`），
    ///   单位会留在原地，而原地没有旗 → 产生一条 `illegal_action`。
    ///   所以调用方（`greedy_flag`）只在「目的地附近没有敌人」时才使用本方法；
    ///   这里再用 `move_is_legal` 会复核的事实兜一层：`pick_target` 一旦设置，
    ///   移动的合法性判定就要求目的地仍是同一面旗。
    ///
    /// 返回 `false` 表示条件不成立（没有移动、AP 不够、目的地没旗、已经规划过拾旗等）。
    pub fn try_pick_after_move(&mut self, obs: &Observation) -> bool {
        if self.has_pick || !self.has_move || self.ap == 0 || self.unit.carrying_flag.is_some() {
            return false;
        }
        let Some(flag) = ground_flag_at(obs, self.pos_now) else {
            return false;
        };
        self.actions.push(Action::PickFlag);
        self.ap -= 1;
        self.has_pick = true;
        self.pick_target = Some(flag);
        true
    }

    /// 目标格是否允许作为移动落点（含「对已规划动作的再校验」）。
    fn move_is_legal(
        &self,
        obs: &Observation,
        dir: Direction,
        occupied: &HashSet<Coord>,
        reserved: &HashSet<Coord>,
    ) -> bool {
        if self.has_move || self.ap == 0 {
            return false;
        }
        let target = self.pos_now.step(dir);
        if !can_stand(&obs.map, obs.team, target) {
            return false;
        }
        if occupied.contains(&target) || reserved.contains(&target) {
            return false;
        }
        // 引擎结算顺序是「移动 → 攻击 → 放炸弹 → 拾旗」，所以下面三类已规划动作
        // 都会发生在新位置上，必须在此重新校验。
        if let Some(atk) = self.attack_target {
            if !self.attack_legal_at(obs, atk, target) {
                return false;
            }
        }
        if self.has_bomb && in_any_base(obs, target) {
            return false;
        }
        if let Some(flag) = self.pick_target {
            if ground_flag_at(obs, target) != Some(flag) {
                return false;
            }
        }
        true
    }

    /// 本 tick 所有合法移动方向（随机 AI 的候选集）。
    pub fn legal_moves(
        &self,
        obs: &Observation,
        occupied: &HashSet<Coord>,
        reserved: &HashSet<Coord>,
    ) -> Vec<Direction> {
        Direction::ALL
            .iter()
            .copied()
            .filter(|dir| self.move_is_legal(obs, *dir, occupied, reserved))
            .collect()
    }

    /// 登记一次移动。
    pub fn try_move(
        &mut self,
        obs: &Observation,
        dir: Direction,
        occupied: &HashSet<Coord>,
        reserved: &HashSet<Coord>,
    ) -> bool {
        if !self.move_is_legal(obs, dir, occupied, reserved) {
            return false;
        }
        let target = self.pos_now.step(dir);
        self.actions.push(Action::Move(dir));
        self.ap -= 1;
        self.has_move = true;
        self.move_dir = Some(dir);
        self.pos_now = target;
        true
    }
}

/// 该格是否属于任何队伍的阵营格（阵营内禁止放炸弹）。
fn in_any_base(obs: &Observation, pos: Coord) -> bool {
    obs.map.in_any_base(pos.x, pos.y).is_some()
}

#[cfg(test)]
mod tests {
    use super::test_support::{obs_unit, observation, test_map};
    use super::*;
    use protocol::FlagView;

    #[test]
    fn wall_blocks_sight_void_does_not() {
        let mut codes = vec![0u8; 25];
        codes[1] = 1; // (1,0) 是墙
        let map = test_map(5, 5, &codes, Vec::new());
        assert!(!has_line_of_sight(&map, Coord::new(0, 0), Coord::new(2, 0)));

        let mut codes = vec![0u8; 25];
        codes[1] = 2; // (1,0) 是虚空：不阻挡视线（docs/rules.md §1）
        let map = test_map(5, 5, &codes, Vec::new());
        assert!(has_line_of_sight(&map, Coord::new(0, 0), Coord::new(2, 0)));
    }

    #[test]
    fn supercover_los_rejects_diagonal_corner_wall() {
        // (0,0) → (1,1)：曼哈顿距离 2，超覆盖会经过 (1,0) 与 (0,1)。
        // 只要其中一个是墙，就保守地判定为无视线（比 Bresenham 细线更严格）。
        let mut codes = vec![0u8; 25];
        codes[1] = 1; // (1,0)
        let map = test_map(5, 5, &codes, Vec::new());
        assert!(!has_line_of_sight(&map, Coord::new(0, 0), Coord::new(1, 1)));

        let codes = vec![0u8; 25];
        let map = test_map(5, 5, &codes, Vec::new());
        assert!(has_line_of_sight(&map, Coord::new(0, 0), Coord::new(1, 1)));
    }

    #[test]
    fn bfs_routes_around_wall_column() {
        // 8×8，x=3 列除最底行外全是墙：从 (0,0) 到 (7,0) 必须绕到底行走。
        let (w, h) = (8u16, 8u16);
        let mut codes = vec![0u8; (w as usize) * (h as usize)];
        for y in 0..(h as i32 - 1) {
            codes[(y * w as i32 + 3) as usize] = 1;
        }
        let map = test_map(w, h, &codes, Vec::new());
        let field = bfs_dist_field(&map, 0, &[Coord::new(7, 0)], &HashSet::new());
        let from = Coord::new(0, 0);
        let Some((_, d)) = field.nearest_source(from) else {
            panic!("绕行路径存在，BFS 必须能到达");
        };
        assert!(d > 7, "应当绕墙，实际距离 {d}");
        let dir = field
            .step_toward(from, &HashSet::new())
            .expect("应能给出一步");
        assert!(
            field.get(from.step(dir)).unwrap_or(u32::MAX) < d,
            "step_toward 必须给出更近的一步"
        );
    }

    #[test]
    fn can_stand_forbids_enemy_base_only() {
        // 8×8：0 队阵营在 (0,0)-(2,2)，1 队阵营在 (5,5)-(7,7)，地形与 bases 一致。
        let (w, h) = (8u16, 8u16);
        let mut codes = vec![0u8; (w as usize) * (h as usize)];
        for y in 0..3 {
            for x in 0..3 {
                codes[(y * w as i32 + x) as usize] = 3; // TeamBase(0)
                codes[((y + 5) * w as i32 + x + 5) as usize] = 4; // TeamBase(1)
            }
        }
        let map = test_map(w, h, &codes, vec![Coord::new(0, 0), Coord::new(5, 5)]);
        assert!(can_stand(&map, 0, Coord::new(0, 0)));
        assert!(!can_stand(&map, 1, Coord::new(0, 0)));
        assert!(can_stand(&map, 0, Coord::new(3, 0)));
        assert!(can_stand(&map, 1, Coord::new(3, 0)));
    }

    #[test]
    fn unit_plan_refuses_move_that_breaks_planned_attack() {
        // (1,1) 是墙：单位 (0,0) 能打到 (0,2)（直线经过 (0,1)，通畅），
        // 但向右走到 (1,0) 后视线会被 (1,1) 挡住 → 规划器必须拒绝这步移动，
        // 否则引擎在「移动→攻击」结算时会打到一支非法攻击。
        let mut codes = vec![0u8; 25];
        codes[6] = 1; // (1,1)
        let map = test_map(5, 5, &codes, Vec::new());
        let obs = observation(
            map,
            vec![obs_unit(0, 0, 0, 0, 2), obs_unit(1, 1, 0, 2, 2)],
            Vec::new(),
        );
        let occupied = occupied_cells(&obs, Some(0));

        let mut plan = UnitPlan::new(&obs.units[0]);
        assert!(
            plan.try_attack(&obs, &obs.map, 1),
            "距离 2 且视线通畅，应可攻击"
        );
        assert_eq!(plan.ap_left(), 1);
        assert!(
            !plan.try_move(&obs, Direction::Right, &occupied, &HashSet::new()),
            "移动会破坏已规划攻击的视线，必须拒绝"
        );
        assert!(plan.actions().len() == 1, "被拒绝的移动不能进入动作序列");
    }

    #[test]
    fn unit_plan_refuses_move_away_after_planned_pick() {
        // 拾旗在结算顺序里最后执行：先规划拾旗再移动，一旦走开拾旗必然失败 → 规划器拒绝移动。
        let codes = vec![0u8; 25];
        let map = test_map(5, 5, &codes, Vec::new());
        let obs = observation(
            map,
            vec![obs_unit(0, 0, 0, 0, 2)],
            vec![FlagView {
                id: 7,
                pos: Coord::new(0, 0),
                carrier: None,
            }],
        );
        let occupied = occupied_cells(&obs, Some(0));

        let mut plan = UnitPlan::new(&obs.units[0]);
        assert!(plan.try_pick(&obs), "脚下有地上旗，应可拾旗");
        assert!(
            plan.legal_moves(&obs, &occupied, &HashSet::new())
                .is_empty(),
            "拾旗后不能再移动（走出旗格会让拾旗失败）"
        );
    }

    #[test]
    fn unit_plan_respects_ap_budget() {
        let codes = vec![0u8; 25];
        let map = test_map(5, 5, &codes, Vec::new());
        // ap_left = 1：只能做一个动作。
        let obs = observation(
            map,
            vec![obs_unit(0, 0, 0, 0, 1), obs_unit(1, 1, 0, 1, 1)],
            Vec::new(),
        );
        let occupied = occupied_cells(&obs, Some(0));
        let mut plan = UnitPlan::new(&obs.units[0]);
        assert!(plan.try_attack(&obs, &obs.map, 1));
        assert_eq!(plan.ap_left(), 0);
        assert!(
            !plan.try_move(&obs, Direction::Down, &occupied, &HashSet::new()),
            "AP 用完不能再移动"
        );
    }
}

/// 只在测试构建里存在的共享夹具。
///
/// 放在 `common` 下，是为了让各 AI 的测试模块共用同一套「整局对局 + 非法动作统计 +
/// 回放逐字节比较」逻辑，而不是各抄一份。它走 `crate::register_all()` 真实注册表，
/// 因此顺带覆盖了「CLI 能按名字从注册表造出 AI」这条路径。
#[cfg(test)]
pub mod test_support {
    use protocol::{Coord, EntityId, FlagView, GameEvent, TeamId, Terrain};
    use sim::ai::{MapView, ObsUnit, Observation};
    use sim::{MatchConfig, PlayOutcome, TeamAi};

    /// 构造测试地图。`bases` 显式传入，避免测试依赖 `in_base` 的具体实现方式
    /// （地形编码 vs 阵营区矩形）；大多数测试传空 `bases`，让判定只依赖地形。
    pub fn test_map(width: u16, height: u16, codes: &[u8], bases: Vec<Coord>) -> MapView {
        assert_eq!(codes.len(), (width as usize) * (height as usize));
        MapView {
            width,
            height,
            terrain: codes
                .iter()
                .map(|code| Terrain::from_code(*code).expect("测试地形编码必须合法"))
                .collect(),
            center: Coord::new(width as i32 / 2, height as i32 / 2),
            center_radius: 2,
            bases,
        }
    }

    /// 满血、存活、未携旗的测试单位；需要残血/携旗时由调用方改字段。
    pub fn obs_unit(id: EntityId, team: TeamId, x: i32, y: i32, ap: u8) -> ObsUnit {
        ObsUnit {
            id,
            team,
            pos: Coord::new(x, y),
            hp: 3,
            alive: true,
            respawn_timer: 0,
            carrying_flag: None,
            attacked_this_turn: false,
            ap_left: ap,
        }
    }

    /// 组装一个最小 `Observation`（只填测试需要的字段），视角为 0 队、第 1 tick。
    pub fn observation(map: MapView, units: Vec<ObsUnit>, flags: Vec<FlagView>) -> Observation {
        observation_with(0, 1, map, units, flags)
    }

    /// 同 [`observation`]，但可指定视角队伍与 tick（例如验证与 tick 相关的巡逻逻辑）。
    pub fn observation_with(
        team: TeamId,
        tick: u32,
        map: MapView,
        units: Vec<ObsUnit>,
        flags: Vec<FlagView>,
    ) -> Observation {
        let my_units = units
            .iter()
            .filter(|u| u.team == team)
            .map(|u| u.id)
            .collect();
        Observation {
            tick,
            max_ticks: 300,
            team,
            map,
            scores: vec![0; 3],
            units,
            flags,
            bombs: Vec::new(),
            my_units,
        }
    }

    /// 用注册表里的 `ai_name` 填满所有槽位，跑完整一局。
    ///
    /// * `teams` 只允许 2 或 3（由 `MatchConfig::new` 校验）；
    /// * `collect_replay` = true 时返回带完整回放的 `PlayOutcome`，便于断言逐字节可复现。
    pub fn run_match(ai_name: &str, teams: u8, seed: u64, collect_replay: bool) -> PlayOutcome {
        let registry = crate::register_all();
        let config = MatchConfig::new(teams, seed);
        let ais: Vec<Box<dyn TeamAi>> = (0..teams)
            .map(|team| {
                let ai_seed = config.ai_seed_for(team);
                registry
                    .create(ai_name, ai_seed)
                    .expect("内置 AI 名字必须已注册")
            })
            .collect();
        sim::play(config, ais, collect_replay).expect("整局模拟不应失败")
    }

    /// 统计回放里 `illegal_action` 事件数量。
    ///
    /// 为什么允许非零：AI 只能看到决策前的快照。同一 tick 内先手可能把后手的目标打死
    /// （攻击按行动顺序结算，docs/rules.md §3），或者把后手单位击杀，使后手已规划的动作
    /// 在结算时失效；三方抢同一面旗时「拾旗竞争」也会让后手扑空。这种 tick 内竞态 AI 无法
    /// 预知，属于设计允许的少数。因此这个数字只当作鲁棒性回归指标：数量级异常（例如每 tick
    /// 都有）说明合法性判定退化。
    ///
    /// 实测参考（种子 0xA000_0001..5，300 tick，2 队）：`random` 0 次、`defender` 0 次、
    /// `greedy_flag` 2..8 次；`greedy_flag` 3 队时可达 30..50 次（三方抢旗更激烈）。
    pub fn count_illegal(outcome: &PlayOutcome) -> usize {
        count_event(outcome, |event| {
            matches!(event, GameEvent::IllegalAction { .. })
        })
    }

    /// 统计回放里满足 `predicate` 的事件数量（用于「AI 真的动了/真的抢到旗了」这类行为断言）。
    pub fn count_event(outcome: &PlayOutcome, predicate: impl Fn(&GameEvent) -> bool) -> usize {
        let Some(replay) = &outcome.replay else {
            return 0;
        };
        replay
            .frames
            .iter()
            .flat_map(|frame| frame.events.iter())
            .filter(|event| predicate(event))
            .count()
    }

    /// 回放 JSONL 文本（`None` 表示本局没有收集回放）；用于逐字节可复现性断言。
    pub fn replay_jsonl(outcome: &PlayOutcome) -> Option<String> {
        outcome
            .replay
            .as_ref()
            .map(|replay| replay.to_jsonl().expect("回放序列化不应失败"))
    }
}
