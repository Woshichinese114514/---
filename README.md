# 抢旗人（Capture the Flag）——批量对局与评测工具链

一个**确定性**的回合制抢旗对战引擎：给定种子与 AI 名单，任何一次评测都能被逐字节复算。
仓库里没有网络、没有 UI 依赖（Web UI 是纯前端读回放），核心是一条 Rust workspace + 一组 Python 校验脚本。

```text
固定种子 ─→ mapgen 生成地图 ─→ sim 按固定结算顺序推进 ─→ ReplayBundle(JSONL) ─→ scoring 评分 ─→ 榜单
                                      ↑
                                 ai（TeamAi 实现，只用 Observation）
```

## 目录结构

```text
crates/
  protocol/   冻结的数据契约：Coord/Terrain/MapInit、事件与 RenderFrame、ReplayInit/ReplayLine、
              MatchResult、版本号三件套（protocol::versions）
  mapgen/     确定性地图生成（版本化 generate_vN），MAP_GEN_VERSION 在此
  sim/        规则实现：回合结算、移动冲突、攻击、炸弹、旗、复活、得分（无 AI、无 UI）
  ai/         TeamAi 特质、三个基线 AI（random / greedy_flag / defender）
              与得分参照 AI raider + AiRegistry
  scoring/    把 MatchResult 聚合成评分报告（胜率/分数占比/击杀比，默认权重见 docs/scoring.md）
  cli/        `qfr` 命令：种子模式、rayon 并行批量跑、回放导出、四件套写盘
docs/
  rules.md          规则权威文档（含 §9 每 tick 结算顺序、§10 种子模式、§11 CLI 规范、§14 测试清单）
  internal-api.md   各 crate 的冻结接口签名
  ai-experiments.md 历次 AI 对抗实验、评测口径与失败原因
  replay-format.md  回放 JSONL 的行结构与 12 种事件字段
  scoring.md        评分公式与归一化
  webui-spec.md     Web UI 规格
tools/
  validate_replay.py  回放契约校验器（0 error 才算过）
  check_versions.py   manifest / summary / matches / 回放 init 四处版本号一致性
  analyze_ai.py       把一批 replays 汇总成 AI 行为指标（冲突/非法动作/横跳/停顿/掉虚空，按 AI 归因）
  bench_ai.py         A/B 评测：per-match 多地图 + 双向换槽位（胜率/平局/场均分）
  acceptance.sh       五关验收脚本（全量测试 → 批量对局 → 回放校验 → 版本一致性 → Web 无头取证）
  shot.js             无头取证工具（CDP：等 __QFR_READY__ → 断言 __QFR_STATUS__ → 截图）
  make_demo_replay.py 生成 samples/ 下的示例回放
samples/      demo_2p.jsonl / demo_3p.jsonl / malformed.jsonl / version_mismatch.jsonl
webui/        纯前端回放播放器（读 out/<run>/replays/*.jsonl）
scripts/cargo  cargo 包装脚本（把 CARGO_HOME 指到仓库内可写目录，见下文）
```

## 快速开始

> **所有 cargo 命令都走 `./scripts/cargo`**。默认 `$HOME/.cargo` 可能是只读的，
> 直接 `cargo` 会报 `Read-only file system (os error 30)`；包装脚本把 `CARGO_HOME`
> 指向 `./.cargo-home`、`CARGO_TARGET_DIR` 指向 `./target`。

```bash
# 1) 每局随机地图，跑 1000 局，3 队分别用不同 AI
./scripts/cargo run -p cli --release -- run --matches 1000 --teams 3 \
    --ai 0=greedy_flag --ai 1=defender --ai 2=random --out out/3p_1000 --jobs 0

# 2) 固定随机：随机一张图，跑 500 局，随机序列可复现（同 base-seed 重跑结果一致）
./scripts/cargo run -p cli --release -- run --matches 500 --seed-mode fixed-random \
    --base-seed 20240501 --out out/fixedrandom_500

# 3) 固定种子：指定种子跑 100 局
./scripts/cargo run -p cli --release -- run --matches 100 --seed-mode fixed \
    --seed 12345 --out out/fixed_100
```

`--teams` 只接受 2 或 3；`--ai` 重复给出即可（裸名字占用最小空闲槽）；未指定的槽位用 `random`。
完整的参数表见 [docs/rules.md](docs/rules.md) §11。

## 产物（四件套）

每次 `qfr run` 在 `--out` 目录写下：

```text
<out>/
  manifest.json   配置 + 版本号三件套 + 种子信息（模式、base_seed、前几个种子样本）+ 创建时间
  summary.json    评分汇总（按 AI 名字聚合 + 按槽位聚合），同样带版本号三件套
  matches.jsonl   每局一行：MatchResult + 该局的 ai_seeds（按局索引升序）
  replays/match_00000.jsonl ...   回放（受 --replay-sample all|none|N 控制）
```

- 写入顺序是**先并行跑完、结果与回留在内存收集，再单线程统一写盘**，因此
  `matches.jsonl` 的行序与回放编号永远稳定。
- 想要逐字节比对两批产物：加 `--created-time none`（抹掉 manifest 里的时间戳）。
- 同 `--seed-mode` + 同 `--base-seed` + 同局数 → `matches.jsonl` 逐行相同（有集成测试守着）。

## 怎么读回放

回放是 JSONL，一行一个 JSON 对象，靠 `"type"` 区分：

```text
{"type":"init",  ...}   第 1 行且只出现一次：三件套版本号、地图（terrain 一维数组）、队伍、tick 上限、种子
{"type":"frame", ...}   每个全局 tick 一行：单位/炸弹/旗/阵营/本 tick 事件（移动、攻击、爆炸、得分…）
{"type":"end",   ...}   最后 1 行：MatchResult（比分、击杀、死亡、胜者）
```

字段逐项说明见 [docs/replay-format.md](docs/replay-format.md)。手动检查一份回放：

```bash
python3 tools/validate_replay.py out/fixed_100/replays/match_00000.jsonl   # 0 error 才算通过
python3 tools/check_versions.py out/fixed_100                               # manifest/summary/matches/init 版本一致性
```

`samples/demo_2p.jsonl`、`samples/demo_3p.jsonl` 是可直接喂给 Web UI 的小回放；
`malformed.jsonl` 与 `version_mismatch.jsonl` 是**故意坏掉**的样本，用来验证校验器真的会报错。

用 Web UI 播放：`webui/index.html`，选择本地产出的 `replays/*.jsonl` 即可（`map_gen_version`
不匹配时界面会告警）。

## 怎么加一个新 AI

1. 在 `crates/ai/src/` 新建 `my_ai.rs`，实现 `sim::TeamAi`（**只看 `Observation`，不得访问 sim 内部状态**）：

   ```rust
   use sim::{Action, Observation, TeamActions, TeamAi, UnitCommand};

   #[derive(Debug)]
   pub struct MyAi { /* 自己的 RNG 等状态 */ }

   impl MyAi {
       pub fn new(seed: u64) -> Self { /* 用 ai_seed 播种自己的 RNG */ }
   }

   impl TeamAi for MyAi {
       fn name(&self) -> &str { "my_ai" }
       fn decide(&mut self, obs: &Observation) -> TeamActions { /* 返回合法动作 */ }
   }
   ```

2. 在 `crates/ai/src/lib.rs` 里注册工厂（`AiFactory = fn(u64) -> Box<dyn TeamAi>`）：

   ```rust
   pub const AI_MY_AI: &str = "my_ai";
   // register_all() 里：
   registry.register(AI_MY_AI, |seed| Box::new(MyAi::new(seed)));
   ```

3. 加测试（docs/rules.md §14 的 15/16）：注册名可查、`create("my_ai", seed)` 有实例、
   跑满一整局不 panic 且**同 `ai_seed` 两次运行结果一致**。

4. 用名字跑：`./scripts/cargo run -p cli --release -- run --ai 0=my_ai --ai 1=random --matches 200 -o out/my_ai`

硬性纪律：`TeamAi` 实例**每局新建**（AI 不允许把状态跨局带到下一局，否则并行结果不可复现）；
实现**不得 panic**（`Observation` 是唯一输入，任何越界都要自己兜住）；名字必须稳定，
因为 `summary.json` 按名字聚合。

## 版本号三件套与升级约定

| 版本号 | 位置 | 什么时候 +1 |
|---|---|---|
| `ENGINE_VERSION` | `protocol::versions` | 模拟核心实现变更（结算顺序不改语义、数据结构替换、回放格式变更） |
| `RULES_VERSION` | `protocol::versions` | **规则**数值/判定变更（伤害、复活时长、AP、炸弹、得分、冲突规则） |
| `MAP_GEN_VERSION` | `mapgen::MAP_GEN_VERSION` | 地图生成算法变更（同种子产出不同地图） |

纪律（详见 `crates/protocol/src/versions.rs` 的模块文档）：

1. **只增不改**：规则/算法定型后不原地修改；要改就加 `generate_v(N+1)` 并升版本号。
2. 删字段 / 改字段含义 / 改事件枚举标签 = 协议变更 → 必须升 `engine_version`，
   并同步更新 `docs/replay-format.md` 与 Web UI 的解析分支。
3. `manifest.json`、`summary.json`、`matches.jsonl`（每行 `map_gen_version`）、回放 `init` 行
   **四处都必须写版本号且一致**；`tools/check_versions.py` 会强制比对，缺失/为 0 直接判失败。
4. 不同 `rules_version` 的两批成绩不可放进同一张榜比较；`summary.json` 里带着该批次所属的
   `teams` 与 `rules_version`，就是为了让下游拒绝跨版本比较。
5. 当前 `rules_version = 2`：虚空从「不可通行」改为「**可进入、进入即死**」（环境致死、不计击杀），
   墙仍是「不可进入 + 挡视线 + 挡爆炸」。这是**规则**变化而非格式变化，所以只升 `rules_version`
   （回放 JSON 不变，Web UI 无需改解析），但两版的成绩不可直接比较。历史见 `docs/replay-format.md` 的版本历史段。

## 测试

```bash
./scripts/cargo test --workspace        # 单元测试 + 集成测试（mapgen/sim/ai/scoring/cli）
tools/acceptance.sh                     # 五关验收（含真实批量对局、回放校验、版本一致性、Web 截图）
tools/acceptance.sh 3                   # 只重跑某一关（复用 ARTIFACT_DIR 里的产物）
```

cli 侧的集成测试在 `crates/cli/tests/`：`seed_modes.rs`（三种种子模式，规则 §14-20）、
`reproducibility.rs`（批量逐字节可复现，§14-21）、`smoke.rs`（100 局不 panic + 回放结构 +
`tools/validate_replay.py` 交叉校验，§14-22）。

## AI 基线与已知限制

三个基线 AI 的设计目标不同，**不是三个强度档位**，而是三种可对比的行为策略；
`raider` 是基线之外的得分参照实现（用来量化"引擎还能榨出多少分"）：

| AI | 策略 | 备注 |
|---|---|---|
| `random` | 随机移动/攻击/放炸弹 | 强度下界；也是未指定槽位的默认值 |
| `greedy_flag` | 抢最近的旗 → 回己方阵营得分；没有旗时在中心区待命（三单位分散占位） | 主基线 |
| `defender` | 守在己方阵营 3×3 内（空闲时每 25 tick 在 9 格里轮换站位），只打进入射程的敌人 | 反抢旗基线 |
| `raider` | 流水线抢旗：互斥分配地上旗、携带者避开火力、拦截敌方携带者、窄走廊主动换位 | 非基线，得分参照（见下） |

已记录在代码注释里的取舍（改动前请先读 `crates/ai/src/greedy_flag.rs` 与 `defender.rs` 的模块文档）：

- `greedy_flag` 用「目标迟滞 + 无进展重规划 + 队友占位排除」压制 A→B→A 打转：
  2 队 60 局批量里冲突次数从 366 降到 **110**（−70%），横跳相对重写前降约 84%。
  代价是 3 队场景横跳略升（+16%）、场均得分略降；这是**刻意选择的权衡**（用户需求是减少冲突）。
- 仍存在的残留问题：同队三单位挤在同一条窄路时，会互相把对方当障碍而「抱团横跳」
  （典型样本：`match_00015`，单局横跳 1100 次、冲突 0、全场不开火）。修它需要重做目标分配，
  属于已知限制，已在 `crates/ai/src/greedy_flag.rs` 的模块文档里写明证据与假设。
- `defender` 在本游戏里**天然很少开火**：得分要把旗送回**自己**阵营，对手没有理由走进你的基地，
  所以 `THREAT_RADIUS` 常常整局不触发——这是地图/规则决定的，不是 AI 失效。
- 想量化上述指标：`python3 tools/analyze_ai.py out/<run>`（可按 AI 名字拆分）。

### 评测口径（先读）

内置 AI 几乎是确定性的：`--seed-mode fixed` 下，同一张图 + 同一批 AI 会把**同一局重复 N 次**，
把 8 张图的胜负放大成"3200 局 100% 胜率"。泛化测试必须用 `per-match`（每局新地图种子）
+ **双向换槽位**（抵消先手与 AI 种子偏差）。复现：

```bash
./scripts/cargo build --release -p cli
python3 tools/bench_ai.py raider greedy_flag --matches 500 --base-seed 910001
```

| 对局 | 胜 / 平 / 负 | 场均得分 | 对手场均 |
|---|---|---|---|
| `raider` vs `greedy_flag`（2 队，1000 局 / 500 图） | 908 / 37 / 55（**90.8%**） | **11.19** | 4.69 |
| `raider` vs `random`（2 队，1000 局 / 500 图） | 932 / 68 / 0（93.2%） | 14.89 | 0.00 |
| `raider` + `greedy_flag` + `defender`（3 队，750 局，轮换槽位） | 700 / 15 / 35（**93.3%**） | **16.24** | 7.54 / 0.00 |

> 本表旧版写的是 fixed 模式下的"100% / 3200 局"，属于重复计数，已按 per-match 口径更正。

### 迭代记录：窄走廊死锁

第一版 `raider` 没有换位逻辑，在固定的 8 张图上做消融时，2 队成绩是 91.0% / 10.54 分
（3 队 66.4% / 12.01 分）。差距几乎全来自**窄走廊死锁**：三个单位在 1 格宽的通道里互相挡路，
`--seed 4` 整局只拿 1 分。修法不是"卡住就随机走一步"（那会在死胡同里 A→B→A 循环），
而是利用规则 §7.5 的 2-环换位（`A→B, B→A` 双方都成功），见 `raider.rs` 的 `plan_swaps`。

`--seed 5` 的场均 8.1 分不是缺陷：该图阵营到中心的最短路是 22 格（单程 22 tick），
300 tick 内 3 个单位的理论上限约 9–10 分，实测 9 分。

### 尝试做出更强的 AI（未成功）

以 `raider` 为基线，后续试了 8 个方向（预测设伏 + 拦截半径、带权绕路、射程内停火、
护送优先、集火、终局控旗、拦截"最接近回城"的携带者、完全关闭拦截），
在 800–1000 张随机地图上**全部不超过基线**；
纯克隆的 A/A 对照是精确的 50/50。实验数据、原因分析与后续方向见
[docs/ai-experiments.md](docs/ai-experiments.md)。没有把同水平的 900 行副本合入仓库。

## 贡献者## 贡献者

- 项目发起、规则设计与验收：[Woshichinese114514](https://github.com/Woshichinese114514)
- Rust 引擎 / 地图生成 / AI / 评分 / CLI / Web UI 回放查看器：**DeepSeek**（AI 结对实现，
  按 `docs/` 里的契约分工完成：`crates/*`、`webui/*`、`tools/*` 与全部验收脚本）
- 回放格式契约（`docs/replay-format.md`）与规则文档（`docs/rules.md`）由双方共同冻结，
  任何格式变更都要升版本号并同步两端。

## 作者的话
- 该项目全权由deepseek-v4.1-flash自主规划建设，作者并未进行代码审查与修改
- v4.1-flash太快了...
- 还有不少bug,有待打磨
- 似乎很适合作为AI的benchmark（指编写这个游戏队伍的AI）
- 目前DS的AI还有不少bug，诸如停在原地不动、webui的显示也有少许问题
