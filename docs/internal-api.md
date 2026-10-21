# 内部 API 冻结契约（Rust 端）

> 本文件是三个 Rust 负责人之间的并行工作接口。**签名一旦冻结就不要单方面修改**；
> 需要改接口时：先在 `docs/` 或代码注释里说明原因，Lead 同步给其他人后再改。
> 各 crate 的落盘范围见文末「写权限分区」。

## 0. 依赖方向（不可违反）

```text
cli -> sim -> mapgen -> protocol
cli -> ai  -> sim    -> protocol
cli -> scoring -> protocol
```

* `sim` **不得**依赖 `ai`（它只认识 `sim::ai::TeamAi` trait），**不得**引入 tokio / web 框架 / 渲染库。
* `mapgen` 只依赖 `protocol`；`protocol` 不依赖任何内部 crate。
* 已冻结的 `crates/*/Cargo.toml` 已按此方向配好依赖，不要新增未列出的外部依赖；
  确需新增时先问 Lead（保持依赖面最小是本项目的硬要求）。

### 关于 `TeamAi` trait 的放置位置（与原始需求的一个技术性偏离，需知悉）

原始需求写「`ai` crate 包含 TeamAi trait」，但同时要求依赖方向 `ai -> sim` 且「sim 只能依赖 AI trait」。
若 trait 放在 `ai`，sim 就必须依赖 ai，与依赖方向矛盾。因此：

* **trait `TeamAi`、`Observation`、`TeamActions`、`UnitCommand`、`Action` 定义在 `sim` crate 的
  `sim::ai` 模块**（`crates/sim/src/ai.rs`），并由 `sim` 顶层 re-export；
* `ai` crate 负责 **注册表 `AiRegistry`、工厂 `AiFactory`、三个基线 AI 与 `raider` 的实现**；
* `cli` 从注册表创建 AI 实例后注入 `Sim::new(...)`。

这样既满足「sim 不依赖具体 AI 实现」，也满足「AI 按名字从注册表选用」。

## 1. `protocol`（**Lead 已实现并冻结，其他人只读**）

已实现完整代码，见 `crates/protocol/src/`。要点：

```rust
pub type EntityId = u32;  pub type TeamId = u8;  pub type Tick = u32;
pub const MAX_TEAMS: u8 = 4;
pub const TERRAIN_CODE_EMPTY: u8 = 0;   // 0=空 1=墙 2=虚空 3+team=阵营
pub const TERRAIN_CODE_WALL: u8 = 1;
pub const TERRAIN_CODE_VOID: u8 = 2;
pub const TERRAIN_CODE_BASE_OFFSET: u8 = 3;

pub struct Coord { pub x: i32, pub y: i32 }
impl Coord { pub const fn new(x: i32, y: i32) -> Self;
             pub const fn manhattan(self, other: Self) -> i32;
             pub const fn step(self, dir: Direction) -> Self; }

pub enum Direction { Up, Down, Left, Right }          // serde snake_case
impl Direction { pub const ALL: [Direction; 4]; pub const fn delta(self) -> (i32, i32);
                 pub const fn as_str(self) -> &'static str; }

pub enum Terrain { Empty, Wall, Void, TeamBase(TeamId) }   // JSON = u8 编码；反序列化也吃字符串
impl Terrain { pub const fn code(self) -> u8; pub const fn from_code(u8) -> Option<Self>;
               pub const fn is_walkable(self) -> bool;    // Empty | TeamBase
               pub const fn blocks_sight(self) -> bool;   // 仅 Wall
               pub const fn base_team(self) -> Option<TeamId>; }

pub struct MapInit { pub width: u16, pub height: u16, pub map_gen_version: u32,
                     pub terrain: Vec<Terrain> }           // 行优先 idx = y*width + x
impl MapInit { pub fn index(&self, x: i32, y: i32) -> Option<usize>;
               pub fn in_bounds(&self, x: i32, y: i32) -> bool;
               pub fn terrain_at(&self, x: i32, y: i32) -> Option<Terrain>;
               pub fn is_walkable(&self, x: i32, y: i32) -> bool;
               pub fn blocks_sight(&self, x: i32, y: i32) -> bool;
               pub fn validate(&self) -> Result<(), String>; }

pub struct UnitView { pub id: EntityId, pub team: TeamId, #[serde(flatten)] pub pos: Coord,
                      pub hp: u8, pub alive: bool, pub respawn_timer: u32,
                      pub carrying_flag: Option<EntityId>, pub attacked_this_turn: bool }
pub struct FlagView { pub id: EntityId, #[serde(flatten)] pub pos: Coord, pub carrier: Option<EntityId> }
pub struct BombView { pub id: EntityId, #[serde(flatten)] pub pos: Coord, pub team: TeamId,
                      pub timer: u8, pub radius: u8 }
pub struct RenderFrame { pub tick: Tick, pub scores: Vec<i32>, pub units: Vec<UnitView>,
                         pub flags: Vec<FlagView>, pub bombs: Vec<BombView>,
                         pub events: Vec<GameEvent> }

// 事件：#[serde(tag = "type", rename_all = "snake_case")]，字段见 docs/replay-format.md 第 5 节
pub enum GameEvent { UnitMoved{unit,from_x,from_y,to_x,to_y}, UnitAttacked{attacker,target,damage},
                     UnitDied{unit,team,by:Option<EntityId>}, UnitRespawned{unit,team,x,y},
                     FlagPicked{unit,flag}, FlagDropped{flag,x,y}, FlagSpawned{flag,x,y},
                     Score{team,unit,flag,new_score:i32}, BombPlaced{unit,bomb,x,y,timer},
                     BombExploded{bomb,x,y,radius,hit_units:Vec<EntityId>},
                     MoveConflict{x,y,units:Vec<EntityId>}, IllegalAction{unit,action:String,reason:String} }
impl GameEvent { pub const fn type_name(&self) -> &'static str; pub fn related_team(&self) -> Option<TeamId>; }

pub struct TeamInfo { pub team_id: TeamId, pub ai_name: String, pub base_x: i32, pub base_y: i32 }
impl TeamInfo { pub fn base_contains(&self, x: i32, y: i32) -> bool; pub fn base_center(&self) -> Coord; }
pub struct ReplayInit { pub engine_version: u32, pub rules_version: u32, pub map_gen_version: u32,
                        pub map: MapInit, pub teams: Vec<TeamInfo>, pub max_ticks: u32,
                        pub flag_spawn_interval: u32, pub center_radius: u8, pub seed: u64 }
pub enum ReplayLine { Init(ReplayInit), Frame(RenderFrame), End(MatchResult) }  // tag = "type"
pub struct MatchResult { pub match_index: u32, pub seed: u64, pub map_gen_version: u32,
                         pub ticks: u32, pub scores: Vec<i32>, pub kills: Vec<u32>,
                         pub deaths: Vec<u32>, pub winner: Option<TeamId>, pub ai_names: Vec<String> }
impl MatchResult { pub fn team_count(&self) -> usize; pub fn is_winner(&self, TeamId) -> bool;
                   pub fn is_draw(&self) -> bool; pub fn ai_name_of(&self, TeamId) -> &str; }

pub const ENGINE_VERSION: u32 = 1;  pub const RULES_VERSION: u32 = 1;
```

> `MatchResult.winner == None` 表示平局。数组字段（scores/kills/deaths/ai_names）**下标即队伍 ID**。

## 2. `mapgen`（负责人：rust-sim；写 `crates/mapgen/**`）

```rust
pub const MAP_GEN_VERSION: u32 = 1;
pub fn is_supported_version(version: u32) -> bool;                 // 目前仅 1
pub fn generate_versioned(version: u32, spec: &MapSpec) -> Result<MapData, MapGenError>;
pub fn generate(spec: &MapSpec) -> Result<MapData, MapGenError>;    // = generate_versioned(MAP_GEN_VERSION, spec)
pub fn generate_v1(spec: &MapSpec) -> Result<MapData, MapGenError>; // 版本化实现，定型后不再改

/// 确定性 RNG（SplitMix64 + 输出混淆），全 workspace 唯一随机源。
/// 必须自己实现而不用 rand crate：rand 的 StdRng 不保证跨版本输出稳定，
/// 而"同种子同版本必须产出完全相同的结果"是本项目的硬要求。
pub struct Rng { /* private state */ }
impl Rng {
    pub fn new(seed: u64) -> Self;
    pub fn next_u64(&mut self) -> u64;
    pub fn next_u32(&mut self) -> u32;
    pub fn next_f64(&mut self) -> f64;                              // [0,1)
    pub fn gen_index(&mut self, len: usize) -> usize;               // 均匀取 [0,len)
    pub fn gen_range_i32(&mut self, lo: i32, hi_exclusive: i32) -> i32;
    pub fn gen_bool(&mut self, numerator: u32, denominator: u32) -> bool;  // 概率 num/den
    pub fn pick_index<T>(&mut self, slice: &[T]) -> Option<usize>;
    pub fn shuffle<T>(&mut self, slice: &mut [T]);
}

pub struct MapSpec {
    pub seed: u64,
    pub width: u16, pub height: u16,
    pub teams: u8,                    // 2 或 3
    pub center_radius: u8,            // 默认 4
    pub wall_density_percent: u8,     // 默认 22
    pub void_density_percent: u8,     // 默认 7
    pub map_gen_version: u32,         // 默认 MAP_GEN_VERSION
}
impl Default for MapSpec { /* 25x25, 2 队, 上述默认值 */ }

pub struct MapData {
    pub width: u16, pub height: u16, pub map_gen_version: u32,
    pub terrain: Vec<Terrain>,        // 行优先
    pub bases: Vec<Coord>,            // 每队 3x3 阵营区【左上角】，下标即队伍 ID
    pub center: Coord, pub center_radius: u8,
}
impl MapData {
    pub fn to_map_init(&self) -> MapInit;
    pub fn index(&self, x: i32, y: i32) -> Option<usize>;
    pub fn in_bounds(&self, x: i32, y: i32) -> bool;
    pub fn terrain_at(&self, x: i32, y: i32) -> Option<Terrain>;
    pub fn is_walkable(&self, x: i32, y: i32) -> bool;
    pub fn blocks_sight(&self, x: i32, y: i32) -> bool;
    pub fn base_of(&self, team: TeamId) -> Option<Coord>;           // 左上角
    pub fn in_base(&self, team: TeamId, x: i32, y: i32) -> bool;
    pub fn in_any_base(&self, x: i32, y: i32) -> Option<TeamId>;
    pub fn in_center_region(&self, x: i32, y: i32) -> bool;         // manhattan(center) <= center_radius
}

#[derive(Debug, thiserror::Error)]
pub enum MapGenError { /* 尺寸过小、队伍数非法、密度非法、连通性修复失败等，消息必须人类可读 */ }
```

**阵营区左上角约定（mapgen 与 sim/UI 必须一致）**：

* 2 队：`team0 = (1,1)`，`team1 = (width-4, height-4)`
* 3 队：`team0 = (1,1)`，`team1 = (width-4, 1)`，`team2 = ((width-3)/2, height-4)`

（即 3×3 块；若地图太小放不下，`generate` 必须返回 `Err`，不要 panic。）

**RNG 使用纪律**：同一局里 `sim` 必须只用一个 RNG 实例（构造时用 `seed` 初始化），
炸弹爆炸顺序、旗刷新、掉旗位置、死亡掉落都从它取随机数，保证可复现。
`ai` 用**自己的** RNG（由 `ai_seed` 播种），不得触碰 sim 的 RNG。

## 3. `sim`（负责人：rust-sim；写 `crates/sim/**`）

```rust
pub mod ai;                       // 见 3.1
pub use ai::{Action, BaseArea?...};   // 至少 re-export：TeamAi, Observation, TeamActions, UnitCommand, Action, MapView, ObsUnit
pub use mapgen::Rng;              // 让 ai crate 只需依赖 sim 就能拿到确定性随机源

#[derive(Clone, Debug)] pub struct RulesConfig { /* 见 3.2，全部 pub 字段 + Default */ }
impl Default for RulesConfig { /* 规则默认值 */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum WinCondition { HighestScore, HighestScoreThenKills }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum WinScoreRule { /* 可用于替换胜负判定，默认 HighestScore */ }

#[derive(Clone, Debug)]
pub struct MatchConfig {
    pub match_index: u32,
    pub seed: u64,                 // 地图种子
    pub ai_seeds: Vec<u64>,        // 每队一个（下标即队伍），由 CLI 按种子计划算出
    pub ai_names: Vec<String>,     // 每队 AI 名字（写进 init/结果）
    pub teams: u8,                 // 2 或 3
    pub width: u16, pub height: u16,
    pub max_ticks: u32,
    pub rules: RulesConfig,
    pub map_gen_version: u32,
    pub win_condition: WinCondition,
    pub all_dead_loses: bool,      // 可选规则：全灭判负（默认 false）
}
impl MatchConfig {
    pub fn new(teams: u8, seed: u64) -> Self;   // 其余字段取默认值（25x25、max_ticks=300、ai_seeds 由 seed 推导）
    pub fn ai_seed_for(&self, team: TeamId) -> u64;   // 越界返回 0
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("队伍数必须是 2 或 3，得到 {0}")] BadTeamCount(u8),
    #[error("AI 实例数量 {got} 与队伍数 {expected} 不一致")] AiCountMismatch { got: usize, expected: usize },
    #[error("地图生成失败：{0}")] MapGen(#[from] mapgen::MapGenError),
    #[error("不支持的地图生成版本 {0}")] UnsupportedMapGenVersion(u32),
    #[error("地图数据非法：{0}")] BadMap(String),
}

pub struct Sim { /* private */ }
impl Sim {
    pub fn new(config: MatchConfig, ais: Vec<Box<dyn TeamAi>>) -> Result<Self, SimError>;
    pub fn config(&self) -> &MatchConfig;
    pub fn init_line(&self) -> ReplayInit;              // 回放 init 行（含地图/队伍/版本号/种子）
    pub fn current_tick(&self) -> u32;                  // 已结算的 tick 数
    pub fn is_over(&self) -> bool;
    pub fn step(&mut self) -> Option<RenderFrame>;      // 游戏结束返回 None
    pub fn outcome(&self) -> MatchResult;               // 随时可调用（未结束时给出"当前"结果）
    pub fn run_to_end(&mut self) -> MatchResult;
    pub fn run_to_end_with_replay(&mut self) -> ReplayBundle;
}

pub struct ReplayBundle { pub init: ReplayInit, pub frames: Vec<RenderFrame>, pub end: MatchResult }
impl ReplayBundle { pub fn to_jsonl(&self) -> Result<String, serde_json::Error>; }

pub struct PlayOutcome { pub result: MatchResult, pub replay: Option<ReplayBundle> }
pub fn play(config: MatchConfig, ais: Vec<Box<dyn TeamAi>>, collect_replay: bool)
    -> Result<PlayOutcome, SimError>;
```

`ReplayBundle::to_jsonl` 必须输出：`init` 行 + 每帧一行 `frame` + 最后 `end` 行，行尾 `\n`，
与 `docs/replay-format.md` 完全一致（这是 Web UI 的输入，**格式错一个字都是 bug**）。

### 3.1 `sim::ai` 模块（trait 与 AI 可见数据）

```rust
pub trait TeamAi {
    fn name(&self) -> &str;
    fn decide(&mut self, obs: &Observation) -> TeamActions;
}

pub struct Observation {
    pub tick: u32, pub max_ticks: u32, pub team: TeamId,
    pub map: MapView,
    pub scores: Vec<i32>,
    pub units: Vec<ObsUnit>,       // 默认全图可见（战争迷雾是可选扩展）
    pub flags: Vec<FlagView>,
    pub bombs: Vec<BombView>,
    pub my_units: Vec<EntityId>,   // 自己队伍的单位 ID（升序）
}
impl Observation {
    pub fn me(&self, unit: EntityId) -> Option<&ObsUnit>;
    pub fn enemies(&self) -> impl Iterator<Item = &ObsUnit>;   // 敌对且存活
    pub fn teammates(&self) -> impl Iterator<Item = &ObsUnit>;
    pub fn enemy_flags_on_ground(&self) -> impl Iterator<Item = &FlagView>;  // carrier == None
}

pub struct MapView { pub width: u16, pub height: u16, pub terrain: Vec<Terrain>,
                     pub center: Coord, pub center_radius: u8, pub bases: Vec<Coord> }
impl MapView {
    pub fn index(&self, x: i32, y: i32) -> Option<usize>;
    pub fn in_bounds(&self, x: i32, y: i32) -> bool;
    pub fn terrain_at(&self, x: i32, y: i32) -> Option<Terrain>;
    pub fn is_walkable(&self, x: i32, y: i32) -> bool;
    pub fn blocks_sight(&self, x: i32, y: i32) -> bool;
    pub fn in_base(&self, team: TeamId, x: i32, y: i32) -> bool;
    pub fn in_any_base(&self, x: i32, y: i32) -> Option<TeamId>;
    pub fn in_center_region(&self, x: i32, y: i32) -> bool;
    pub fn distance_to_center(&self, x: i32, y: i32) -> i32;
}

pub struct ObsUnit {
    pub id: EntityId, pub team: TeamId, pub pos: Coord, pub hp: u8, pub alive: bool,
    pub respawn_timer: u32, pub carrying_flag: Option<EntityId>,
    pub attacked_this_turn: bool, pub ap_left: u8,     // 本回合剩余 AP
}
impl ObsUnit { pub fn can_attack(&self) -> bool; }      // alive && ap_left>0 && !attacked_this_turn

#[derive(Clone, Debug, PartialEq, Eq)] pub struct UnitCommand { pub unit: EntityId, pub action: Action }
#[derive(Clone, Debug, PartialEq, Eq)] pub enum Action {
    Move(Direction), Attack(EntityId), PlaceBomb, PickFlag, Wait,
}
impl Action { pub fn label(&self) -> String; }          // "move(up)" / "attack(3)" / "place_bomb" ...

#[derive(Clone, Debug, Default, PartialEq, Eq)] pub struct TeamActions { pub commands: Vec<UnitCommand> }
impl TeamActions {
    pub fn new() -> Self;
    pub fn push(&mut self, unit: EntityId, action: Action) -> &mut Self;
    pub fn move_unit(&mut self, unit: EntityId, dir: Direction) -> &mut Self;
    pub fn attack(&mut self, unit: EntityId, target: EntityId) -> &mut Self;
    pub fn place_bomb(&mut self, unit: EntityId) -> &mut Self;
    pub fn pick_flag(&mut self, unit: EntityId) -> &mut Self;
    pub fn wait(&mut self, unit: EntityId) -> &mut Self;
}
```

### 3.2 `RulesConfig` 字段（数值必须与 `docs/rules.md` 一致）

```rust
pub struct RulesConfig {
    pub units_per_team: u8,          // 3
    pub unit_max_hp: u8,             // 3
    pub respawn_ticks: u32,          // 10
    pub ap_per_unit: u8,             // 2
    pub attack_range: i32,           // 3（曼哈顿）
    pub attack_damage: u8,           // 1
    pub bomb_fuse: u8,               // 2（放置当回合计时=2）
    pub bomb_radius: u8,             // 2（十字，被墙阻挡）
    pub bomb_damage: u8,             // 2（有友伤）
    pub flag_spawn_interval: u32,    // 5
    pub flag_capture_radius: u8,     // 4（中心区域半径 R）
    pub base_size: u8,               // 3（阵营区边长，固定 3）
    pub flag_carrier_can_attack: bool, // false（拿旗默认不能攻击）
    pub max_flags: u8,               // 0 表示自动 = 队伍数
}
```

## 4. `ai`（负责人：rust-ai；写 `crates/ai/**`）

```rust
pub type AiFactory = fn(u64) -> Box<dyn TeamAi>;   // 入参 = ai_seed，保证 AI 内部随机可复现

pub const AI_RANDOM: &str = "random";
pub const AI_GREEDY_FLAG: &str = "greedy_flag";
pub const AI_DEFENDER: &str = "defender";
pub const DEFAULT_AI: &str = AI_RANDOM;

#[derive(Default)] pub struct AiRegistry { /* name -> AiFactory */ }
impl AiRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, name: &str, factory: AiFactory) -> &mut Self;   // 重名覆盖
    pub fn contains(&self, name: &str) -> bool;
    pub fn create(&self, name: &str, ai_seed: u64) -> Option<Box<dyn TeamAi>>;
    pub fn names(&self) -> Vec<&str>;                                          // 已排序，便于报错提示
}

/// 注册所有内置 AI。**新增 AI 时在这里加一行**（这是唯一的扩展点）。
pub fn register_all() -> AiRegistry;

pub struct RandomAi { /* rng: sim::Rng */ }
impl RandomAi { pub fn new(ai_seed: u64) -> Self; }
pub struct GreedyFlagAi { ... }   impl GreedyFlagAi { pub fn new(ai_seed: u64) -> Self; }
pub struct DefenderAi { ... }     impl DefenderAi { pub fn new(ai_seed: u64) -> Self; }
// 三者都 impl sim::TeamAi
```

AI 必须是**无跨局状态**的（每局由工厂新建一个实例）。`ai_seed` 由 CLI 计算：
`ai_seed = ai_salt ^ ((team_id as u64) << 32) ^ 0x9E37_79B9`。

## 5. `scoring`（负责人：rust-cli；写 `crates/scoring/**`）

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoringConfig {
    pub weight_win_rate: f64,       // 默认 0.6
    pub weight_score_share: f64,    // 默认 0.25
    pub weight_kill_ratio: f64,     // 默认 0.15
    pub win_rate_prior_matches: f64,// 小样本收缩：把胜率朝 0.5 收缩，默认 5.0
    pub baseline_rating: f64,       // 默认 1000.0（哪个分数代表"平均实力"）
    pub points_per_decade: f64,     // 默认 500.0（每 500 分 ≈ 10 倍实力）
    pub min_strength: f64,          // 默认 0.02（避免 log10(0)）
    pub max_strength: f64,          // 默认 0.98（避免 log10(∞)）
    pub teams: u8,                  // 该报告所属的队伍数体系（2 或 3，分数不可跨体系比较）
}

pub fn compute_rating(strength: f64, config: &ScoringConfig) -> f64;
pub fn compute_strength(win_rate: f64, avg_score_share: f64, avg_kill_ratio: f64,
                        config: &ScoringConfig) -> f64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome { Win, Draw, Loss }
pub trait OutcomeRule { fn outcome(&self, result: &MatchResult, team: TeamId) -> Outcome; }
pub struct HighestScoreRule;      // 默认：唯一最高分胜；并列最高分 → 各算平局；否则负
impl OutcomeRule for HighestScoreRule { ... }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiAggregate { pub ai_name: String, pub matches: u32, pub wins: u32, pub draws: u32,
                         pub losses: u32, pub win_rate: f64, pub draw_rate: f64,
                         pub avg_score: f64, pub avg_kills: f64, pub avg_deaths: f64,
                         pub avg_ticks: f64, pub strength: f64, pub rating: f64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotAggregate { pub slot: u8, pub ai_name: String, pub wins: u32, pub matches: u32,
                           pub win_rate: f64, pub rating: f64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoringReport { pub total_matches: u32, pub teams: u8, pub rules_version: u32,
                           pub map_gen_version: u32, pub config: ScoringConfig,
                           pub by_ai_name: Vec<AiAggregate>, pub by_slot: Vec<SlotAggregate> }

pub fn aggregate(results: &[MatchResult], teams: u8, rules_version: u32,
                 map_gen_version: u32, config: &ScoringConfig) -> ScoringReport;
```

评分必须满足：**log scale，每 500 分 ≈ 10 倍实力**，2 队与 3 队是**两套独立体系**
（`ScoringReport.teams` 标注体系；禁止跨体系比较）。算法细节与公式推导写在
`docs/scoring.md` 与 `crates/scoring/src/lib.rs` 的注释里。

## 6. `cli`（负责人：rust-cli；写 `crates/cli/**`、`README.md`）

CLI 内部结构由负责人决定，但**对用户可见的行为必须与 `docs/rules.md` 第 7 节完全一致**：
子命令 `run`、参数名、三种种子模式的语义、输出目录四件套
（`manifest.json` / `summary.json` / `matches.jsonl` / `replays/`）。

要求（重要）：

* 所有库层错误用 `thiserror`，main 用 `anyhow` + `?`；**生产代码禁止 `unwrap()`**，
  需要 panic 的地方写 `expect("原因")`。
* 批量并行用 rayon：每局独立 `Sim` + 独立 AI；**回放先在内存收集，最后统一写盘**（不要并发写文件）。
* 种子数组在并行前一次性算好（保证并行时顺序稳定）。
* `--jobs 0` = rayon 默认线程数；`--jobs N` = `ThreadPoolBuilder::num_threads(N)`。
* 输出必须可复现：同 `base-seed` + 同 `seed-mode` 两次运行结果逐字节一致
  （`created_time` 之类的字段除外，且它必须能被 `--created-time` 之类的开关固定或省略）。

## 7. 写权限分区（避免并行写冲突）

| 区域 | 负责人 | 其他人类 |
|---|---|---|
| `crates/protocol/**` | Lead（已冻结） | 只读 |
| `crates/mapgen/**`, `crates/sim/**` | rust-sim | 只读（发现问题写注释或通知 Lead） |
| `crates/ai/**` | rust-ai | 只读 |
| `crates/scoring/**`, `crates/cli/**`, `README.md` | rust-cli | 只读 |
| `webui/**`, `webui/README.md` | webui | 只读 |
| `docs/**`, `samples/**`, `tools/**`, 根 `Cargo.toml` | Lead | 只读 |

需要改别人的文件时：**不要直接改**，把问题写进消息发给 Lead，由 Lead 决定并同步。
