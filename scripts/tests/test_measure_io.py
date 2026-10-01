"""Counter attribution, PID/TID identity, and Linux child-reaping contracts."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "measure_io", Path(__file__).resolve().parents[1] / "measure-io.py"
)
measure_io = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(measure_io)


def stat_text(identifier, start=100, utime=30, stime=20):
    fields = ["S"] + ["0"] * 19
    fields[11:15] = [str(utime), str(stime), "9000", "8000"]
    fields[19] = str(start)
    return f"{identifier} (worker (nested) name)) " + " ".join(fields)


def sample(time=10.0, cpu_ticks=50, process_start=100, start=101, counters=None):
    return {
        "time": time,
        "process_start_ticks": process_start,
        "start_ticks": start,
        "cpu_ticks": cpu_ticks,
        "io": counters or {},
        "io_error": None,
    }


class ParserAndCounterTests(unittest.TestCase):
    def test_stat_comm_with_parentheses_and_child_cpu_exclusion(self):
        self.assertEqual(measure_io.parse_stat(stat_text(42)), {
            "id": 42, "start_ticks": 100, "cpu_ticks": 50,
        })

    def test_malformed_stat_and_negative_io_are_rejected(self):
        for text in ["invalid", "42 (truncated) S 0"]:
            with self.subTest(text=text), self.assertRaises((ValueError, IndexError)):
                measure_io.parse_stat(text)
        with self.assertRaises(ValueError):
            measure_io.parse_io("write_bytes: -1")

    def test_io_parser_ignores_future_fields_and_keeps_missing_unavailable(self):
        self.assertEqual(measure_io.parse_io("wchar: 10\nfuture_field: unknown"), {"wchar": 10})
        first = sample(counters={"wchar": 100})
        last = sample(time=12.0, cpu_ticks=100, counters={"wchar": 2100})
        result = measure_io.report(42, first, last, 100, "summary")
        self.assertEqual(result["cpu_seconds"], 0.5)
        self.assertEqual(result["cpu_percent_one_core"], 25)
        self.assertEqual(result["io_delta"]["wchar"], 2000)
        self.assertEqual(result["io_per_second"]["wchar"], 1000)
        self.assertIsNone(result["io_delta"]["write_bytes"])
        self.assertIn("write_bytes", result["unavailable_fields"])

    def test_counter_regression_and_zero_elapsed_are_not_reported_as_rates(self):
        first = sample(counters={"wchar": 100})
        last = sample(cpu_ticks=1, counters={"wchar": 1})
        result = measure_io.report(42, first, last, 100, "summary", tid=43)
        self.assertIsNone(result["cpu_seconds"])
        self.assertIsNone(result["cpu_percent_one_core"])
        self.assertIsNone(result["io_delta"]["wchar"])
        self.assertIsNone(result["io_per_second"]["wchar"])

    def test_scope_distinguishes_process_io_from_cpu_and_thread_io(self):
        first = sample()
        process = measure_io.report(42, first, first, 100, "summary")
        thread = measure_io.report(42, first, first, 100, "summary", tid=43)
        self.assertEqual(process["scope"], "process")
        self.assertIsNone(process["tid"])
        self.assertIn("excludes child CPU", process["cpu_scope"])
        self.assertIn("reaped children", process["io_scope"])
        self.assertIn("excludes still-running children", process["io_scope"])
        self.assertEqual(thread["scope"], "thread")
        self.assertEqual(thread["tid"], 43)
        self.assertIn("selected thread only", thread["cpu_scope"])
        self.assertIn("excludes other threads and child processes", thread["io_scope"])


class SnapshotIdentityTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.process = self.root / "42"
        self.thread = self.process / "task" / "43"
        self.thread.mkdir(parents=True)
        (self.process / "stat").write_text(stat_text(42, start=100, utime=100))
        (self.thread / "stat").write_text(stat_text(43, start=101))
        (self.process / "io").write_text("wchar: 1000\nwrite_bytes: 4096\n")
        (self.thread / "io").write_text("wchar: 20\nwrite_bytes: 0\n")

    def test_thread_uses_task_directory_and_tracks_process_identity(self):
        process = measure_io.snapshot(42, proc_root=self.root)
        thread = measure_io.snapshot(42, 43, proc_root=self.root)
        self.assertEqual(process["io"]["wchar"], 1000)
        self.assertEqual(thread["io"]["wchar"], 20)
        self.assertEqual(thread["cpu_ticks"], 50)
        self.assertEqual(thread["start_ticks"], 101)
        self.assertEqual(thread["process_start_ticks"], 100)

    def test_io_missing_is_unavailable_but_missing_thread_fails(self):
        (self.thread / "io").unlink()
        result = measure_io.snapshot(42, 43, proc_root=self.root)
        self.assertEqual(result["io"], {})
        self.assertIsNotNone(result["io_error"])
        with self.assertRaises(FileNotFoundError):
            measure_io.snapshot(42, 44, proc_root=self.root)

    def test_wrong_tid_in_stat_is_rejected(self):
        (self.thread / "stat").write_text(stat_text(44))
        with self.assertRaisesRegex(ValueError, "identifier"):
            measure_io.snapshot(42, 43, proc_root=self.root)

    def test_reuse_during_snapshot_discards_mixed_sample(self):
        for reuse in ["pid", "tid"]:
            process = measure_io.parse_stat(stat_text(42, start=100))
            thread = measure_io.parse_stat(stat_text(43, start=101))
            process_after = dict(process, start_ticks=999) if reuse == "pid" else process
            thread_after = dict(thread, start_ticks=999) if reuse == "tid" else thread
            with self.subTest(reuse=reuse), mock.patch.object(
                measure_io, "parse_stat", side_effect=[process, thread, thread_after, process_after]
            ), self.assertRaisesRegex(measure_io.IdentityChangedError, f"{reuse}_reused_during_sample"):
                measure_io.snapshot(42, 43, proc_root=self.root)

    def test_cli_stops_before_aggregating_a_reused_pid_or_tid(self):
        for change, last in [
            ("pid_reused", sample(time=11.0, process_start=999)),
            ("tid_reused", sample(time=11.0, start=999)),
        ]:
            output = io.StringIO()
            with self.subTest(change=change), mock.patch.object(
                sys, "argv", ["measure-io.py", "--pid", "42", "--tid", "43", "--duration", "1"]
            ), mock.patch.object(measure_io, "snapshot", side_effect=[sample(), last]), \
                    mock.patch.object(measure_io.Path, "exists", return_value=True), \
                    mock.patch.object(measure_io.os, "sysconf", return_value=100), \
                    mock.patch.object(measure_io.time, "sleep"), redirect_stdout(output):
                measure_io.main()
            summary = json.loads(output.getvalue())
            self.assertEqual(summary["stop_reason"], change)
            self.assertEqual(summary["elapsed_seconds"], 0)

    def test_cli_reports_disappeared_thread_and_invalid_tid(self):
        output = io.StringIO()
        with mock.patch.object(
            sys, "argv", ["measure-io.py", "--pid", "42", "--tid", "43", "--duration", "1"]
        ), mock.patch.object(measure_io, "snapshot", side_effect=[sample(), FileNotFoundError("gone")]), \
                mock.patch.object(measure_io.Path, "exists", return_value=True), \
                mock.patch.object(measure_io.os, "sysconf", return_value=100), \
                mock.patch.object(measure_io.time, "sleep"), redirect_stdout(output):
            measure_io.main()
        self.assertTrue(json.loads(output.getvalue())["stop_reason"].startswith("thread_unavailable:"))
        with mock.patch.object(sys, "argv", ["measure-io.py", "--pid", "42", "--tid", "0"]), \
                redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as failure:
            measure_io.main()
        self.assertEqual(failure.exception.code, 2)


@unittest.skipUnless(sys.platform == "linux", "requires Linux /proc accounting")
class LinuxAttributionTests(unittest.TestCase):
    def test_reaped_child_io_is_in_process_counters_but_not_parent_thread(self):
        pid = os.getpid()
        tid = threading.get_native_id()
        before_process = measure_io.snapshot(pid)
        before_thread = measure_io.snapshot(pid, tid)
        if any("wchar" not in value["io"] for value in [before_process, before_thread]):
            self.skipTest("/proc I/O counters unavailable")
        size = 1024 * 1024
        with tempfile.TemporaryDirectory() as directory:
            child = subprocess.Popen(
                [sys.executable, "-c", """
import os, sys
with open(sys.argv[1], 'wb') as output:
    output.write(b'x' * 1048576)
    output.flush()
    os.fsync(output.fileno())
print('ready', flush=True)
sys.stdin.readline()
""", str(Path(directory) / "child-write.bin")],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            )
            try:
                self.assertEqual(child.stdout.readline(), "ready\n")
                # The child is still alive after completing the write.
                running = measure_io.snapshot(pid)
                self.assertLess(running["io"]["wchar"] - before_process["io"]["wchar"], size)
                child.communicate("finish\n", timeout=10)
                self.assertEqual(child.returncode, 0)
                after_process = measure_io.snapshot(pid)
                after_thread = measure_io.snapshot(pid, tid)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                child.stdin.close()
                child.stdout.close()
        process_bytes = after_process["io"]["wchar"] - before_process["io"]["wchar"]
        thread_bytes = after_thread["io"]["wchar"] - before_thread["io"]["wchar"]
        self.assertGreaterEqual(process_bytes, size)
        self.assertLess(thread_bytes, size)
        self.assertGreaterEqual(process_bytes - thread_bytes, size)
        # Physical write_bytes may be zero on tmpfs; wchar proves attribution
        # without imposing a particular filesystem's physical-I/O behavior.


if __name__ == "__main__":
    unittest.main()
