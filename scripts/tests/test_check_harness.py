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

    def invoke(self, *args, fail=None):
        result = subprocess.run(
            ["bash", str(self.root / "scripts" / "check-harness.sh"), *args],
            env={**self.env, **({"FAIL_NEXTEST": fail} if fail else {})},
            capture_output=True,
            text=True,
        )
        commands = (
            [json.loads(line) for line in self.trace.read_text().splitlines()]
            if self.trace.exists()
            else []
        )
        return result, commands

    def assert_runners(self, commands, normal_count, docs):
        cargo = [command[1:] for command in commands if command[0] == "cargo"]
        normal = [command for command in cargo if command[:2] == ["nextest", "run"]]
        self.assertEqual(len(normal), normal_count)
        for command in normal:
            self.assertEqual(command[2:4], ["--no-tests", "fail"])
        self.assertEqual([command for command in cargo if command[0] == "test"], docs)
        other_count = sum(command[0] in ("fmt", "clippy") for command in cargo)
        self.assertEqual(len(cargo), normal_count + len(docs) + other_count)

    def test_default_focused_uses_isolated_runner_and_retains_tools_doctests(self):
        result, commands = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_runners(commands, 7, [["test", "-p", "tools", "-p", "sandbox", "--doc"]])
        self.assertNotIn("test_gui_requires_cef.sh", [command[0] for command in commands])

    def test_full_retains_workspace_and_optional_feature_doctests(self):
        result, commands = self.invoke("full")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_runners(
            commands,
            9,
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

    def test_invalid_mode_does_not_launch_any_checks(self):
        result, commands = self.invoke("invalid")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Usage:", result.stderr)
        self.assertEqual(commands, [])


if __name__ == "__main__":
    unittest.main()
