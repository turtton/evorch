#!/usr/bin/env python3
"""Exercise the harness entry point without building or running Rust tests."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


HARNESS = Path(__file__).resolve().parents[1] / "check-harness.sh"
FOCUSED_SELECTIONS = [
    [
        "-p", "runtime",
        "--test", "restore_state",
        "--test", "tools_shell",
        "--test", "delegation",
        "--test", "waits_messaging",
        "--test", "escalation_ownership",
        "--test", "compaction_budget",
        "--",
        "restore_expansion::",
        "restore_tool_intent::",
        "shell_jobs_integration::",
        "state_transitions::",
        "background::",
        "messaging_loop::",
        "wait_runs::",
        "user_questions::",
        "escalation_questions::",
        "escalation_handoff::",
        "context_observability::",
        "compaction_engine::",
        "compaction_policy::",
        "compaction_continuation::",
        "meta_ops::",
    ],
    ["-p", "runtime", "--test", "compaction_triggers", "--lib"],
    ["-p", "providers", "--lib"],
    ["-p", "tools", "-p", "sandbox"],
    [
        "-p", "storage",
        "--test", "run_data_suite",
        "--test", "platform_suite",
        "--", "user_questions::", "migration::",
    ],
    [
        "-p", "gui", "--test", "runtime_threads",
        "--", "user_questions_headless::", "goal_restore_authority::", "runtime_wiring::",
    ],
    ["-p", "gui", "--lib"],
]
WORKSPACE_SELECTION = ["--workspace", "--no-fail-fast"]
OTEL_SELECTION = ["-p", "event-bus", "--features", "otel-exporter"]


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="harness-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        scripts = self.root / "scripts"
        (scripts / "tests").mkdir(parents=True)
        shutil.copy2(HARNESS, scripts / "check-harness.sh")
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        dispatch = bin_dir / "dispatch"
        dispatch.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys, tempfile\n"
            "name = pathlib.Path(sys.argv[0]).name\n"
            "if name == 'mktemp':\n"
            "    print(tempfile.mkdtemp(dir=os.environ['FIXTURE_ROOT']))\n"
            "    sys.exit(0)\n"
            "with open(os.environ['TRACE'], 'a') as trace:\n"
            "    trace.write(json.dumps([name, *sys.argv[1:]]) + '\\n')\n"
            "failure = os.environ.get('FAIL_NEXTEST')\n"
            "if sys.argv[1:3] == ['nextest', 'run'] and (\n"
            "    failure == 'all' or (failure == 'workspace' and '--workspace' in sys.argv)\n"
            "):\n"
            "    print('fixture nextest failure', file=sys.stderr)\n"
            "    sys.exit(17)\n"
            "if os.environ.get('FAIL_WORKSPACE_DOC') and sys.argv[1:2] == ['test'] and (\n"
            "    '--workspace' in sys.argv and '--doc' in sys.argv\n"
            "):\n"
            "    print('fixture workspace doctest failure', file=sys.stderr)\n"
            "    sys.exit(19)\n"
        )
        dispatch.chmod(0o755)
        for name in ["cargo", "mktemp"]:
            (bin_dir / name).symlink_to(dispatch)
        (scripts / "check-cache-contracts.sh").symlink_to(dispatch)
        (scripts / "tests" / "test_gui_requires_cef.sh").symlink_to(dispatch)
        self.trace = self.root / "trace.jsonl"
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            "FIXTURE_ROOT": str(self.root),
            "TRACE": str(self.trace),
        }
        self.env.pop("FAIL_NEXTEST", None)
        self.env.pop("FAIL_WORKSPACE_DOC", None)

    def invoke(self, *args, fail=None, fail_docs=False):
        env = dict(self.env)
        if fail:
            env["FAIL_NEXTEST"] = fail
        if fail_docs:
            env["FAIL_WORKSPACE_DOC"] = "1"
        result = subprocess.run(
            ["bash", str(self.root / "scripts" / "check-harness.sh"), *args],
            env=env,
            capture_output=True,
            text=True,
        )
        commands = (
            [json.loads(line) for line in self.trace.read_text().splitlines()]
            if self.trace.exists()
            else []
        )
        return result, commands

    def assert_runners(self, commands, selections, docs):
        cargo = [command[1:] for command in commands if command[0] == "cargo"]
        normal = [command for command in cargo if command[:2] == ["nextest", "run"]]
        self.assertEqual(len(normal), len(selections))
        for command in normal:
            self.assertEqual(command[2:4], ["--no-tests", "fail"])
        self.assertEqual([command[4:] for command in normal], selections)
        self.assertEqual([command for command in cargo if command[0] == "test"], docs)
        other_count = sum(command[0] in ("fmt", "clippy") for command in cargo)
        self.assertEqual(len(cargo), len(selections) + len(docs) + other_count)

    def test_default_focused_uses_isolated_runner_and_retains_tools_doctests(self):
        result, commands = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_runners(
            commands,
            FOCUSED_SELECTIONS,
            [["test", "-p", "tools", "-p", "sandbox", "--doc"]],
        )
        self.assertNotIn("test_gui_requires_cef.sh", [command[0] for command in commands])

    def test_full_retains_workspace_and_optional_feature_doctests(self):
        result, commands = self.invoke("full")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_runners(
            commands,
            FOCUSED_SELECTIONS + [WORKSPACE_SELECTION, OTEL_SELECTION],
            [
                ["test", "-p", "tools", "-p", "sandbox", "--doc"],
                ["test", "--workspace", "--doc", "--no-fail-fast"],
                ["test", "-p", "event-bus", "--features", "otel-exporter", "--doc"],
            ],
        )
        self.assertIn(["test_gui_requires_cef.sh"], commands)

    def test_runner_failure_stops_later_groups_and_reports_its_log(self):
        result, commands = self.invoke("focused", fail="all")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture nextest failure", result.stderr)
        self.assertIn("Full log:", result.stderr)
        self.assertEqual(commands[-1][1:3], ["nextest", "run"])
        runner_count = sum(command[1:3] == ["nextest", "run"] for command in commands)
        self.assertEqual(runner_count, 1)

    def test_workspace_failure_still_runs_doctests_before_returning_failure(self):
        result, commands = self.invoke("full", fail="workspace")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture nextest failure", result.stderr)
        self.assertEqual(
            commands[-1], ["cargo", "test", "--workspace", "--doc", "--no-fail-fast"]
        )

    def test_workspace_doctest_failure_is_reported_without_success_summary(self):
        result, commands = self.invoke("full", fail_docs=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture workspace doctest failure", result.stderr)
        self.assertNotIn("fixture nextest failure", result.stderr)
        self.assertNotIn("Passed.", result.stdout)
        self.assert_runners(
            commands,
            FOCUSED_SELECTIONS + [WORKSPACE_SELECTION],
            [
                ["test", "-p", "tools", "-p", "sandbox", "--doc"],
                ["test", "--workspace", "--doc", "--no-fail-fast"],
            ],
        )

    def test_invalid_mode_does_not_launch_any_checks(self):
        result, commands = self.invoke("invalid")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Usage:", result.stderr)
        self.assertEqual(commands, [])


if __name__ == "__main__":
    unittest.main()
