# AGENTS.md

回合制抢旗 + 炸弹对战引擎：Rust workspace（确定性模拟 + 批量对局 + 回放）+ 纯前端回放查看器。
仓库文档、代码注释、提交信息、CLI 输出都用中文，新代码保持同一风格。
本文件在仓库根目录；下面的命令都从仓库根执行（当前若在 `/root`，先进入仓库目录）。

## 先读这几份（比读代码快）

- `docs/rules.md`：规则权威文档。§9 是每 tick 结算顺序，§10 是种子模式，§11 是 CLI 契约，§14 是测试清单。
  CLI 对用户可见的行为以 §11 为准；代码与文档不一致时改代码。
- `docs/internal-api.md`：crate 边界与冻结签名。§7 的「写权限分区」记录的是早期多 agent 分工，
  不要据此拒绝修改；但冻结签名本身仍要慎重，改动要同步文档。
- `docs/replay-format.md`：回放 JSONL 契约；`docs/ai-experiments.md`：已经失败过的 AI 改进，动手前必读。
- `docs/notes/`：实验心得（PGO/热点、评测口径、降频噪声），优化前先扫一眼。
- 版本号真相：`crates/protocol/src/versions.rs`、`crates/mapgen/src/lib.rs` 的 `MAP_GEN_VERSION`。

## 命令

`./scripts/cargo` 是给原始 x64 Linux 沙箱用的兼容包装：那里 AI 无权写仓库外的文件，
`$HOME/.cargo` 只读，直接 `cargo` 会报 `Read-only file system (os error 30)`。
脚本把 `CARGO_HOME` 指到仓库内 `.cargo-home/`，`CARGO_TARGET_DIR` 指向 `./target`。

**如果你有正常的文件系统权限，这一条可以无视，直接用 `cargo` 即可**；两者跑出来的
测试结果、二进制和回放一致。下面示例沿用包装脚本，只是为了在受限环境里也能直接复制执行。

```bash
./scripts/cargo test --workspace                 # 全量测试
./scripts/cargo test -p sim conflict             # 单 crate / 按测试名过滤
./scripts/cargo test -p cli --test smoke         # 单个集成测试文件
./scripts/cargo build --release -p cli           # 批量对局 / bench 前必做
./scripts/cargo run -p cli --release -- run --matches 100 --out out/foo
```

- 批量跑几千局用 `--release`；`tools/bench_ai.py` 硬编码 `target/release/qfr`。
- 没有 CI / pre-commit；`./scripts/cargo test --workspace` 和 `tools/acceptance.sh` 就是门禁。
- `tools/acceptance.sh` 是五关验收（测试 → 批量对局 → 回放校验 → 版本一致 → Web 截图）。
  `tools/acceptance.sh 3` 只重跑第 3 关并复用 `out/accept/` 产物（首次需先跑过第 2 关）。
- 第 5 关需要 `node` ≥ 22 和 chromium；找不到浏览器时用 `CHROME=/path/to/chrome`。
  环境里没装 chromium 时先只跑 1–4 关。
- 第 3 关把期望版本写死为 `--expect-engine 1 --expect-rules 2 --expect-map-gen 1`。
  改版本号时必须同步改 `tools/acceptance.sh`、`tools/make_demo_replay.py` 里的版本常量并重新生成
  `samples/`，否则验收会失败。
- Python 工具（`tools/*.py`）只用标准库，直接 `python3` 跑，无需虚拟环境。

## 架构边界（不要违反）

```text
cli -> sim -> mapgen -> protocol
cli -> ai  -> sim    -> protocol
cli -> scoring -> protocol
```

- `sim` **不得**依赖 `ai`。`TeamAi` / `Observation` / `TeamActions` / `UnitCommand` 定义在
  `sim::ai`（`crates/sim/src/ai.rs`）并由 `sim` re-export；`ai` crate 只放注册表和具体实现。
- `protocol` 是冻结契约；回放 JSONL 是 Rust 与 Web UI 之间唯一的耦合点。
- `sim` 不读文件、不做渲染、不联网。回放落盘属于 `cli`。
- 写盘顺序固定：rayon 并行跑完 → 结果与回放先在内存收集 → 单线程统一写盘。
  `matches.jsonl` 行序与 `replays/match_XXXXX.jsonl` 编号因此永远稳定。
- `frame` 是每个 tick 的**全量快照**，不是增量；Web UI 依赖这一点。

## 确定性（项目第一原则）

同配置跑两次，回放必须逐字节相同。改动以下任何一条都会让集成测试红：

- 整局只用 `MatchConfig::seed` 播种的**一个** `sim::Rng`；禁止系统时间、线程本地随机、第二个 RNG。
- 所有集合按固定顺序遍历（创建顺序 / 行优先 / ID 升序），不要依赖 `HashMap` 迭代顺序。
- `TeamAi` 实例**每局新建**，AI 自己的 RNG 用该局的 `ai_seed` 播种；状态不得跨局。
- AI 只允许看 `Observation`，不得访问 sim 内部状态，且不得 panic。
- 逐字节比对两批产物时加 `--created-time none`（抹掉 manifest 时间戳）。

## 改规则 / 改协议

三个版本号当前为 `engine 1 / rules 2 / map_gen 1`：

| 版本号 | 位置 | 何时 +1 |
|---|---|---|
| `ENGINE_VERSION` | `protocol::versions` | 模拟实现或回放格式变更 |
| `RULES_VERSION` | `protocol::versions` | 规则数值/判定变更（伤害、AP、复活、炸弹、得分、冲突、虚空…） |
| `MAP_GEN_VERSION` | `mapgen` | 地图生成算法变更（同种子出不同图） |

- **只增不改**：规则/算法定型后不原地改；加 `generate_v(N+1)` 并升版本号。
- 版本号要同时出现在 `manifest.json`、`summary.json`、每份回放 `init` 行、
  `matches.jsonl` 每行的 `map_gen_version`；`tools/check_versions.py` 会强制四处一致。
- 改规则要同步 `docs/rules.md`、§14 对应测试、版本号、样例和验收脚本。
- 改回放字段/事件标签要升 `ENGINE_VERSION`，并同步 `docs/replay-format.md` 与 Web UI 解析分支。
- 不同 `rules_version`、以及 2 队与 3 队的成绩**不可直接比较**（评分体系不同）。
- `samples/malformed.jsonl` 与 `version_mismatch.jsonl` 是故意构造的坏数据，不要「修好」。

## 加 AI 与评测

加 AI：在 `crates/ai/src/` 新建实现 → 在 `crates/ai/src/lib.rs` 的 `register_all()` 注册 →
补 `docs/rules.md` §14 测试 17–19（注册名、跑满整局不 panic、同 `ai_seed` 可复现）。
`name()` 必须稳定，`summary.json` 按名字聚合。

评测陷阱（很容易把结论做错）：

- `--seed-mode fixed` / `fixed-random` 下所有局共用一张图，几乎确定性的 AI 会把同一局重复 N 次，
  「3200 局 100% 胜率」其实只有 8 个样本。这两种模式只用于复现/回归，不用于泛化评测。
- 正确口径：`per-match`（每局新图）+ **双向换槽位**，直接用：

  ```bash
  ./scripts/cargo build --release -p cli
  python3 tools/bench_ai.py raider greedy_flag --matches 500 --base-seed 910001
  ```

  产物在 `out/bench_ab/`（已忽略）。正式对比前先跑一次 A/A 对照（把同一实现注册成两个名字），
  确认约 50/50，否则槽位/种子/统计口径有问题。
- `python3 tools/analyze_ai.py out/<run>` 可量化移动冲突、非法动作、A→B→A 横跳、掉虚空。
- `raider` 之后的 8 个改进方向都没超过基线，原因与数据见 `docs/ai-experiments.md`；不要重复试。

## TODO：AI 调度/寻路优化（下次优先）

依据：`docs/notes/pgo-2026-10-24.md` 的 profile 计数显示 AI 占 76.7%，其中
`ai::common::bfs_dist_field` 53.4%、`can_stand` 18.9%、`is_walkable` 11.3%、
`in_any_base` 6.8%。PGO 已证明没有收益，先做源码级优化：

- [ ] **复用 BFS 距离场**：同一 tick / 同一地图 / 同一目标集合下，多单位、多角色会重复 BFS。
      在 AI 实例内按 tick 缓存距离场，或改成“一次多源 BFS 服务全队”；缓存键要包含 tick +
      地图/阵营状态，跨局不得复用，保证 `ai_seed` 可复现。
- [ ] **预计算合法性网格**：每 tick 为每队生成 `walkable / standable / base_owner` 的一维数组
      （`idx = y*width + x`），BFS 内只查数组，把 `can_stand` / `is_walkable` / `in_any_base`
      从邻居内层循环里移出去。
- [ ] **替换 `HashSet<Coord>`**：占位/已访问集合改用 `Vec<bool>` 或 bitset（地图 ≤ 35×35），
      去掉 `contains_key` / `hash_one` / `insert` 的开销（当前约 3%）。
- [ ] **缓存 `base_cells`**：每 tick 每队算一次，不要每次规划都扫 3×3。
- [ ] **检查目标/角色分配**：`raider::plan_roles` 与 `greedy_flag` 的目标分配是否每 tick
      重复触发 BFS；用迟滞/认领减少同队抢同一目标，但不要引入跨局状态。
- [ ] 每项改动都要过：`./scripts/cargo test --workspace` + `python3 tools/analyze_ai.py out/<run>`
      + `python3 tools/bench_ai.py`（`per-match` + 双向换槽位，先 A/A 校准）。
      目标是同等或更好的胜率/得分下降低 CPU 时间，不是“单局更快”。

## Web UI

- 纯静态：没有构建、npm、框架或后端。双击 `webui/index.html` 即可用。
- `<script>` 必须保持经典加载顺序：`core → parser → state → render → log → controls → main`。
  **不要引入 ES module**，`file://` 下会被 CORS 拦截。
- 回放里的文本（AI 名、非法动作原因等）一律用 `textContent`，不要拼 `innerHTML`。
- 版本不匹配只告警并继续渲染；未知事件/地形也不能白屏。
- 截图取证用 `node tools/shot.js --url ... --out ...`（CDP）。
  **不要用 `chromium --headless --screenshot`**：该路径会丢掉 JS 插入的告警横幅，导致误判。

## 其它易踩坑

- `--teams` 只接受 2 或 3；`--ai` 未指定的槽位默认 `random`。
- `mapgen::splitmix64(x)` 与 `cli::seed::mix64(x)` 差一次加法，不是同一个函数；
  混用会让整批种子全变（有测试专门钉这条）。
- `out/`、`target/`、`.cargo-home/`、`.tmp/`、`*.log` 都在 `.gitignore` 里，不要提交产物；
  `out/` 里已有历史评测结果，不要把它当源码改。
- 单局复现需要 `matches.jsonl` 里的地图种子 + `ai_seeds`，以及回放 `init` 行的三件套。
