#!/usr/bin/env python3
"""A/B 评测两个 AI：per-match 多地图 + 双向换槽位，输出胜率/平局/场均分。

## 为什么必须用 `per-match`

内置 AI 几乎是确定性的（只有破僵局时用 `ai_seed` 的 RNG）。在
`--seed-mode fixed` 下，同一张地图 + 同一批 AI 会把**同一局重复 N 次**：

    $ target/release/qfr run --matches 200 --seed-mode fixed --seed 4 ...
    # 200 局全部 1:0（或全部 0:1）——这不是 200 个样本，是 1 个样本重复 200 次

这会让人把"某张图赢了"误读成"胜率 100%"。正确口径是 `per-match`：每局一个新地图种子，
`--base-seed` 只保证整批可复现（docs/rules.md §10）。

## 为什么要双向换槽位

槽位会影响 AI 种子与每 tick 的行动顺序。把两个 AI 各放一次 slot 0 / slot 1，
再合并统计，可以抵消先手与种子的系统偏差。

## 用法

    ./scripts/cargo build --release -p cli
    python3 tools/bench_ai.py raider greedy_flag --matches 500 --base-seed 910001
    python3 tools/bench_ai.py raider random --matches 200 --teams 2
    python3 tools/bench_ai.py raider greedy_flag defender --matches 200 --teams 3

产物写在 `out/bench_ab/`（已被 .gitignore 忽略），可加 `--keep` 保留供分析。
"""

from __future__ import annotations

import argparse
import json
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
QFR = ROOT / "target" / "release" / "qfr"
OUT = ROOT / "out" / "bench_ab"


def ensure_binary() -> None:
    if QFR.exists():
        return
    sys.exit(
        "找不到 target/release/qfr。先运行：\n"
        "    ./scripts/cargo build --release -p cli"
    )


def run_batch(names: list[str], matches: int, base_seed: int, jobs: int, tag: str) -> pathlib.Path:
    """跑一批对局，返回产物目录。`names` 的顺序就是槽位顺序。"""
    out = OUT / tag
    if out.exists():
        shutil.rmtree(out)
    cmd = [str(QFR), "run", "--matches", str(matches), "--teams", str(len(names))]
    for slot, name in enumerate(names):
        cmd += ["--ai", f"{slot}={name}"]
    cmd += [
        "--out", str(out),
        "--seed-mode", "per-match",
        "--base-seed", str(base_seed),
        "--replay-sample", "none",
        "--jobs", str(jobs),
    ]
    print("  $", " ".join(cmd[1:]), flush=True)
    subprocess.run(cmd, check=True)
    return out


def read_matches(out: pathlib.Path) -> list[dict]:
    path = out / "matches.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def main() -> None:
    parser = argparse.ArgumentParser(description="两个（或多个）AI 的 A/B 评测")
    parser.add_argument("names", nargs="+", help="AI 名字，按槽位顺序")
    parser.add_argument("--matches", type=int, default=500, help="每个槽位顺序的对局数")
    parser.add_argument("--base-seed", type=int, default=910001, help="种子生成器基准种子（可复现）")
    parser.add_argument("--jobs", type=int, default=0, help="并行线程数，0 = 自动")
    parser.add_argument("--keep", action="store_true", help="保留 out/bench_ab 下的产物")
    args = parser.parse_args()

    if len(args.names) not in (2, 3):
        sys.exit("只支持 2 或 3 个 AI（与引擎的 --teams 限制一致）")

    ensure_binary()
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True, exist_ok=True)

    # 生成所有轮换：每个 AI 至少坐一次 slot 0/1(/2)。
    orders = [list(args.names)]
    if len(args.names) >= 2:
        rotated = list(args.names[1:]) + [args.names[0]]
        orders.append(rotated)
    if len(args.names) == 3:
        orders.append(list(args.names[2:]) + list(args.names[:2]))

    stats = {name: {"n": 0, "win": 0, "draw": 0, "loss": 0, "score": 0} for name in args.names}
    total = 0
    for index, order in enumerate(orders):
        tag = f"{index}_" + "_".join(order)
        print(f"[{index + 1}/{len(orders)}] 槽位顺序：{' / '.join(order)}", flush=True)
        out = run_batch(order, args.matches, args.base_seed + index, args.jobs, tag)
        for match in read_matches(out):
            total += 1
            winner = match["winner"]
            for slot, name in enumerate(match["ai_names"]):
                entry = stats[name]
                entry["n"] += 1
                entry["score"] += match["scores"][slot]
                if winner is None:
                    entry["draw"] += 1
                elif winner == slot:
                    entry["win"] += 1
                else:
                    entry["loss"] += 1

    print(f"\n总局数：{total}")
    print(f"{'AI':<16}{'n':>6}{'win':>7}{'draw':>7}{'loss':>7}{'winrate':>10}{'avg_score':>11}")
    for name, entry in stats.items():
        n = entry["n"] or 1
        print(
            f"{name:<16}{entry['n']:>6}{entry['win']:>7}{entry['draw']:>7}{entry['loss']:>7}"
            f"{entry['win'] / n:>10.3f}{entry['score'] / n:>11.2f}"
        )

    if len(args.names) == 2:
        a, b = args.names
        print(f"\n两两对比（{a} 视角）：")
        print(f"  胜 {stats[a]['win']} / 平 {stats[a]['draw']} / 负 {stats[a]['loss']}")
        print(f"  场均分 {stats[a]['score'] / max(stats[a]['n'], 1):.2f} vs {stats[b]['score'] / max(stats[b]['n'], 1):.2f}")
        print(
            "\n注意：胜率差距 < 2% 时不要下结论——1000 局的标准误约 1.6 个百分点。"
            "\n建议先跑一次 A/A 对照（同一实现注册两个名字），应得到 50/50。"
        )

    if not args.keep:
        shutil.rmtree(OUT, ignore_errors=True)


if __name__ == "__main__":
    main()
