"""Focused helper behavior checks; invented commands only, never invokes Cargo."""

import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
BASH = shutil.which("bash")
PYTHON = sys.executable


class BuildHelpers(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="kaban-build-helper-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.python_path = self.root / "python-path"
        self.python_path.mkdir()
        (self.python_path / "python3").symlink_to(PYTHON)
        self.env = {
            **os.environ,
            # The sandbox's interactive-tool setup must not replace the fixture PATH.
            "BASH_ENV": os.devnull,
            "KABAN_CARGO_SLOTS": "1",
            "KABAN_CARGO_SLOT_WAIT": "0",
            "KABAN_CARGO_SLOT_DIR": str(self.root / "slots"),
        }

    def slot(self, command, *, python=False, env=None):
        environ = {**self.env, **(env or {})}
        if python:
            environ["PATH"] = str(self.python_path)
        return subprocess.run(
            [BASH, str(ROOT / "scripts/dev/cargo-slot.sh"), *command],
            env=environ,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def hold_slot(self, *, python):
        environ = self.env.copy()
        if python:
            environ["PATH"] = str(self.python_path)
        child = subprocess.Popen(
            [BASH, str(ROOT / "scripts/dev/cargo-slot.sh"), PYTHON, "-c",
             "import sys; print('held', flush=True); sys.stdin.read()"],
            env=environ,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        def stop():
            if child.poll() is None:
                child.communicate(timeout=10)
            for stream in (child.stdin, child.stdout, child.stderr):
                if stream and not stream.closed:
                    stream.close()

        self.addCleanup(stop)
        ready, _, _ = select.select([child.stdout], [], [], 10)
        self.assertTrue(ready, "admitted command failed to start")
        self.assertEqual(child.stdout.readline(), "held\n")
        return child

    def test_python_backend_executes_and_preserves_failure(self):
        success = self.slot([PYTHON, "-c", "print('ran')"], python=True)
        self.assertEqual((success.returncode, success.stdout), (0, "ran\n"))
        failure = self.slot([PYTHON, "-c", "raise SystemExit(23)"], python=True)
        self.assertEqual(failure.returncode, 23)

    def test_python_backend_holds_slot_across_exec_then_releases(self):
        holder = self.hold_slot(python=True)
        blocked = self.slot([PYTHON, "-c", "print('must not run')"], python=True)
        self.assertEqual((blocked.returncode, blocked.stdout), (75, ""))
        holder.communicate(timeout=10)
        admitted = self.slot([PYTHON, "-c", "print('after release')"], python=True)
        self.assertEqual((admitted.returncode, admitted.stdout), (0, "after release\n"))

    @unittest.skipUnless(shutil.which("flock"), "flock(1) backend unavailable")
    def test_flock_and_python_contend_for_the_same_lock(self):
        for python_holder in (False, True):
            with self.subTest(python_holder=python_holder):
                holder = self.hold_slot(python=python_holder)
                blocked = self.slot(
                    [PYTHON, "-c", "print('must not run')"], python=not python_holder
                )
                self.assertEqual((blocked.returncode, blocked.stdout), (75, ""))
                holder.communicate(timeout=10)
        failure = self.slot([PYTHON, "-c", "raise SystemExit(24)"])
        self.assertEqual(failure.returncode, 24)

    def test_missing_backend_and_invalid_settings_do_not_run_command(self):
        empty = self.root / "empty-path"
        empty.mkdir()
        missing = self.slot([PYTHON, "-c", "print('must not run')"], env={"PATH": str(empty)})
        self.assertEqual((missing.returncode, missing.stdout), (69, ""))
        for settings in ({"KABAN_CARGO_SLOTS": "0"}, {"KABAN_CARGO_SLOT_WAIT": "-1"}):
            invalid = self.slot([PYTHON, "-c", "print('must not run')"], env=settings)
            self.assertEqual((invalid.returncode, invalid.stdout), (64, ""))

    def test_flock_error_is_not_treated_as_contention_or_admission(self):
        backend = self.root / "broken-backend"
        backend.mkdir()
        fake = backend / "flock"
        fake.write_text(f"#!{BASH}\nexit 70\n")
        fake.chmod(0o700)
        failed = self.slot(
            [PYTHON, "-c", "print('must not run')"],
            env={"PATH": str(backend) + os.pathsep + os.environ["PATH"]},
        )
        self.assertEqual((failed.returncode, failed.stdout), (70, ""))

    def gate(self, mode="pass"):
        (self.root / "scripts" / "dev").mkdir(parents=True)
        (self.root / "crates").mkdir()
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        log = self.root / "calls"
        cargo = bin_dir / "cargo"
        cargo.write_text(
            f"#!{PYTHON}\n"
            "import os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['CALLS'], 'a') as log: log.write('cargo ' + ' '.join(args) + '\\n')\n"
            "mode = os.environ['MODE']\n"
            "if args[:2] == ['nextest', 'list']:\n"
            "    print('crate::tests:\\n   \\n    positive')\n"
            "    sys.exit(17 if mode == 'list-fail' else 0)\n"
            "if args[:2] == ['nextest', 'run']:\n"
            "    print('Summary: 1 passed')\n"
            "    if mode == 'skip': print('NOT RUN: native_check: unavailable')\n"
            "    sys.exit(19 if mode == 'run-fail' else 0)\n"
        )
        cargo.chmod(0o700)
        slot = self.root / "scripts/dev/cargo-slot.sh"
        slot.write_text(f'#!{BASH}\nprintf "slot %s\\n" "$*" >> "$CALLS"\nexec "$@"\n')
        slot.chmod(0o700)
        base = self.root / "base-list"
        base.write_text("    positive\ncrate::tests:\n")
        prefix = self.root / "gate"
        outcome = subprocess.run(
            [BASH, str(ROOT / "scripts/dev/lane-gate.sh"), str(prefix), str(base), "-p", "invented"],
            cwd=self.root,
            env={**self.env, "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
                 "CALLS": str(log), "MODE": mode, "LC_ALL": "C"},
            capture_output=True,
            text=True,
            timeout=10,
        )
        return outcome, log.read_text(), (self.root / "gate-gate-summary.log").read_text()

    def test_gate_pass_includes_slot_protected_list(self):
        result, calls, summary = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("slot cargo nextest list -p invented --offline", calls)
        self.assertIn("--success-output immediate", calls)
        self.assertIn("added 0 removed 0", summary)
        self.assertIn("result PASS", summary)

    def test_gate_required_skip_is_incomplete_not_pass(self):
        result, _, summary = self.gate("skip")
        self.assertEqual(result.returncode, 3)
        self.assertIn("mandatory_skip_markers 1", summary)
        self.assertIn("result INCOMPLETE", summary)
        self.assertNotIn("result PASS", summary)

    def test_gate_failed_list_does_not_pass_despite_nonempty_output(self):
        result, _, summary = self.gate("list-fail")
        self.assertEqual(result.returncode, 1)
        self.assertIn("list exit non-zero", summary)
        self.assertIn("result FAIL", summary)

    def test_gate_failed_run_does_not_pass_despite_summary(self):
        result, _, summary = self.gate("run-fail")
        self.assertEqual(result.returncode, 1)
        self.assertIn("nextest exit 19", summary)
        self.assertIn("result FAIL", summary)


if __name__ == "__main__":
    unittest.main()
