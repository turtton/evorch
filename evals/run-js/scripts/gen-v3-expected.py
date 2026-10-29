#!/usr/bin/env python3
"""v3 課題（t1-survey-v3 / t2-aggregate-v3）の期待値を、固定コミットの checkout から再計算する。

使い方: scripts/gen-v3-expected.py <base-checkout> <out-dir>
  <out-dir>/t1-survey-v3.json と t2-aggregate-v3.json を書く（通常は verify/expected/）。

- t1: `impl Tool for X` のブロックをソースから切り出して、name / execution_mode / permissions / overrides を求める。
  allowed_roles は `Role::capabilities()` を実際に呼んで確認した行列（ROLE_MATRIX）。参照数は git ls-files と本文の走査。
- t2: Rust の集計（v2 と同じ 6 指標）、Cargo.toml の内部依存グラフ、git log の履歴、ADR の状態と被参照、
  拡張子 × 最上位ディレクトリの表、それらを合成した hotspots。
Python 3.11+（tomllib）。実行時（Worker・検証）には使わない。
"""
import json
import re
import subprocess
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

root = Path(sys.argv[1])
out_dir = Path(sys.argv[2])
if len(sys.argv) < 3:
    sys.exit("usage: gen-v3-expected.py <base-checkout> <out-dir>")
BASE = "4391a7e5"


def git(*args):
    return subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True).stdout


tracked = [f for f in git("ls-files", "-z").decode().split("\0") if f]
_text = {}


def text(path):
    if path not in _text:
        _text[path] = (root / path).read_bytes().decode("utf-8", "surrogateescape")
    return _text[path]


def newlines(path):
    return (root / path).read_bytes().count(b"\n")


# ---------------------------------------------------------------- t1-survey-v3
ROLES = ["Orchestrator", "Explorer", "Worker", "Reviewer", "WebResearcher", "Planner", "Advisor", "MultimodalLooker", "Evaluator"]
# `Role::capabilities().allowed_tools` に各ツールを含むロール（enum の宣言順）。cargo test で出力して確認した値。
ROLE_MATRIX = {
    "apply_patch": ["Worker"],
    "edit": ["Worker"],
    "git_diff": ["Orchestrator", "Worker", "Reviewer", "Planner", "Advisor"],
    "grep": ["Orchestrator", "Explorer", "Worker", "Reviewer", "WebResearcher", "Planner", "Advisor", "Evaluator"],
    "read": ROLES,
    "shell": ["Worker"],
    "web_fetch": ["Orchestrator", "WebResearcher", "Planner"],
    "web_search": ["WebResearcher"],
    "write": ["Worker"],
    "lsp_diagnostics": [],
    "eval_list_threads": ["Evaluator"],
    "eval_inspect_thread": ["Evaluator"],
    "eval_save_sample": ["Evaluator"],
    "eval_remove_sample": ["Evaluator"],
    "eval_start": ["Evaluator"],
    "eval_report": ["Evaluator"],
    "eval_apply_config": ["Evaluator"],
}
PERMISSION_CONSTRUCTORS = {
    "read_only": (True, False, False, False),
    "read_write": (True, True, False, False),
    "process": (True, True, True, False),
    "network": (False, False, False, True),
}
SOURCES = [
    "crates/tools/src/tools/apply_patch.rs",
    "crates/tools/src/tools/edit.rs",
    "crates/tools/src/tools/git_diff.rs",
    "crates/tools/src/tools/grep.rs",
    "crates/tools/src/tools/read.rs",
    "crates/tools/src/tools/shell.rs",
    "crates/tools/src/tools/web_fetch/mod.rs",
    "crates/tools/src/tools/web_search.rs",
    "crates/tools/src/tools/write.rs",
    "crates/tools/src/lsp/mod.rs",
    "crates/eval/src/evaluator.rs",
    "crates/eval/src/evaluator/apply.rs",
    "crates/eval/src/evaluator/start.rs",
]
REQUIRED = {"name", "schema", "permissions", "execute"}


def impl_blocks(path):
    """`impl Tool for X {` ... 対応する閉じ括弧（行頭の `}`）までを切り出す。"""
    src = text(path)
    for m in re.finditer(r"^impl (?:crate::)?Tool for (\w+) \{\n", src, re.M):
        end = src.index("\n}\n", m.end())
        yield m.group(1), src[m.end() : end]


tools = []
for path in SOURCES:
    for type_name, block in impl_blocks(path):
        name_fn = re.search(r"fn name\(&self\)[^{]*\{\s*(.*?)\s*\n    \}", block, re.S).group(1)
        literal = re.fullmatch(r'"([a-z_]+)"', name_fn)
        if literal:
            name = literal.group(1)
        else:
            assert "tool_contract::NAME" in name_fn, name_fn
            name = "apply_patch"
        mode_fn = re.search(r"fn execution_mode\(&self\)[^{]*\{\s*ToolExecutionMode::(\w+)", block)
        mode = mode_fn.group(1) if mode_fn else "Exclusive"  # トレイトの既定値（未分類は排他）
        perm_fn = re.search(r"fn permissions\(&self\)[^{]*\{(.*?)\n    \}", block, re.S).group(1)
        ctor = re.search(r"Permissions::(\w+)\(\)", perm_fn)
        if ctor:
            flags = PERMISSION_CONSTRUCTORS[ctor.group(1)]
        else:
            flags = tuple(re.search(rf"{f}: (true|false)", perm_fn).group(1) == "true" for f in ("fs_read", "fs_write", "process_spawn", "network"))
        methods = re.findall(r"^    (?:async )?fn (\w+)", block, re.M)
        overrides = sorted(set(methods) - REQUIRED)
        tools.append(
            {
                "name": name,
                "type": type_name,
                "source": path,
                "execution_mode": mode,
                "fs_read": flags[0],
                "fs_write": flags[1],
                "process_spawn": flags[2],
                "network": flags[3],
                "overrides": overrides,
            }
        )
tools.sort(key=lambda t: t["name"])
assert len(tools) == 17, [t["name"] for t in tools]
assert [t["name"] for t in tools if t["name"] in ROLE_MATRIX] == [t["name"] for t in tools]

REF_DIRS = {
    "tools_tests": "crates/tools/tests/",
    "runtime_src": "crates/runtime/src/",
    "runtime_tests": "crates/runtime/tests/",
    "eval_src": "crates/eval/src/",
    "gui_src": "crates/gui/src/",
    "evorch_tests": "crates/evorch/tests/",
}
rs_files = [f for f in tracked if f.endswith(".rs")]
for t in tools:
    t["allowed_roles"] = ROLE_MATRIX[t["name"]]
    t["refs"] = {key: sum(1 for f in rs_files if f.startswith(d) and f'"{t["name"]}"' in text(f)) for key, d in REF_DIRS.items()}
    del t["type"]
by_role = {r: sorted(t["name"] for t in tools if r in t["allowed_roles"]) for r in ROLES}
ref_ranking = [t["name"] for t in sorted(tools, key=lambda t: (-sum(t["refs"].values()), t["name"]))]
(out_dir / "t1-survey-v3.json").write_text(json.dumps({"tools": tools, "by_role": by_role, "ref_ranking": ref_ranking}, ensure_ascii=False, indent=1) + "\n")

# ---------------------------------------------------------------- t2-aggregate-v3
# A. Rust の crate 別集計（v2 と同じ 6 指標）
crates = {}
for f in rs_files:
    if not f.startswith("crates/"):
        continue
    parts = f.split("/")
    cat = "src" if parts[2] == "src" else "tests" if parts[2] == "tests" else "other"
    body = text(f)
    lines = newlines(f)
    attrs = sum(1 for line in body.split("\n") if re.match(r"\s*#\[(test\]|tokio::test)", line))
    c = crates.setdefault(parts[1], dict(rs_files=0, rs_lines=0, test_attrs=0, src_lines=0, tests_lines=0, other_lines=0))
    c["rs_files"] += 1
    c["rs_lines"] += lines
    c["test_attrs"] += attrs
    c[f"{cat}_lines"] += lines
crates = dict(sorted(crates.items()))

# B. Cargo.toml の内部依存グラフ（[dependencies] と [build-dependencies] を normal、[dev-dependencies] を dev）
names = set(crates)
deps, dev_deps = {}, {}
for name in crates:
    data = tomllib.loads(text(f"crates/{name}/Cargo.toml"))

    def internal(section):
        found = set()
        for key, value in data.get(section, {}).items():
            if isinstance(value, dict) and "path" in value and value["path"].startswith("../"):
                found.add(Path(value["path"]).name)
            elif isinstance(value, dict) and value.get("workspace") and key in names:
                found.add(key)
        return found

    deps[name] = internal("dependencies") | internal("build-dependencies")
    dev_deps[name] = internal("dev-dependencies") - deps[name]
assert all(d in names for s in deps.values() for d in s), "unknown internal dep"


def closure(name, seen=None):
    seen = set() if seen is None else seen
    for d in deps[name]:
        if d not in seen:
            seen.add(d)
            closure(d, seen)
    return seen


def layer(name):
    return 0 if not deps[name] else 1 + max(layer(d) for d in deps[name])


graph = {}
for name in crates:
    graph[name] = {
        "deps": sorted(deps[name]),
        "dev_deps": sorted(dev_deps[name]),
        "dependents": sum(1 for other in crates if name in deps[other]),
        "transitive_deps": len(closure(name)),
        "layer": layer(name),
    }

# C. git 履歴: 4391a7e5 から遡った最新 200 コミットのうち、各 crate のディレクトリに触れたもの
log = git("log", "-200", "--no-renames", "--format=@@%H %cs", "--numstat", BASE).decode()
history = {name: dict(commits_200=0, churn_200=0, last_date=None) for name in crates}
current_date, touched = None, set()


def flush(touched):
    for name in touched:
        history[name]["commits_200"] += 1


for line in log.split("\n"):
    if line.startswith("@@"):
        flush(touched)
        touched = set()
        current_date = line.split()[1]
        continue
    if not line.strip():
        continue
    added, deleted, path = line.split("\t", 2)
    parts = path.split("/")
    if parts[0] != "crates" or parts[1] not in history:
        continue
    name = parts[1]
    touched.add(name)
    if added != "-":
        history[name]["churn_200"] += int(added) + int(deleted)
    if history[name]["last_date"] is None:
        history[name]["last_date"] = current_date
flush(touched)

# D. ADR（intents/evorch/decisions/*.md）
adr_files = sorted(f for f in tracked if re.fullmatch(r"intents/evorch/decisions/\d{4}-[^/]+\.md", f))
adr_status = defaultdict(int)
mentions = defaultdict(set)  # ADR 番号 -> それを本文で参照する他の ADR ファイル
for f in adr_files:
    body = text(f)
    number = int(re.match(r"(\d+)-", Path(f).name).group(1))
    status = re.search(r"^## Status\s*\n+\s*([A-Za-z]+)", body, re.M)
    adr_status[status.group(1).lower() if status else "none"] += 1
    for m in re.finditer(r"ADR[ -]?0*(\d+)", body):
        target = int(m.group(1))
        if target != number:
            mentions[target].add(f)
status_counts = dict(sorted(adr_status.items()))
most_referenced = [
    {"adr": n, "referenced_by": len(fs)}
    for n, fs in sorted(mentions.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    if any(Path(f).name.startswith(f"{n:04d}-") for f in adr_files)
][:5]

# F. 最上位ディレクトリ × 拡張子の表（ファイル数と改行数）
TOPS = ["crates", ".intent-cli", "intents", "docs", "vendor", "scripts", "examples"]
EXTS = ["rs", "md", "yaml", "json", "toml", "sql"]
matrix = {top: {} for top in TOPS}
for f in tracked:
    top = f.split("/")[0]
    if top not in matrix:
        continue
    ext = Path(f).suffix.lstrip(".")
    ext = ext if ext in EXTS else "other"
    cell = matrix[top].setdefault(ext, {"files": 0, "lines": 0})
    cell["files"] += 1
    cell["lines"] += newlines(f)
matrix = {top: dict(sorted(cells.items())) for top, cells in matrix.items()}

# E. hotspots: rs_lines >= 3000 の crate を score = commits_200 × (1 + dependents) ÷ (rs_lines ÷ 1000) の降順で上位 5
hot = []
for name, c in crates.items():
    if c["rs_lines"] >= 3000:
        score = history[name]["commits_200"] * (1 + graph[name]["dependents"]) / (c["rs_lines"] / 1000)
        hot.append({"crate": name, "score": round(score, 2), "commits_200": history[name]["commits_200"], "dependents": graph[name]["dependents"], "rs_lines": c["rs_lines"]})
hot.sort(key=lambda h: (-h["score"], h["crate"]))
hotspots = hot[:5]

result = {
    "crates": crates,
    "graph": graph,
    "history": history,
    "adr": {"files": len(adr_files), "status_counts": status_counts, "most_referenced": most_referenced},
    "ext_matrix": matrix,
    "hotspots": hotspots,
}
(out_dir / "t2-aggregate-v3.json").write_text(json.dumps(result, ensure_ascii=False, indent=1) + "\n")
