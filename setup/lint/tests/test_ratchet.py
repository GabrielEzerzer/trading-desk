# Tests for the lint ratchet: baseline checks, the base-ref comparison, which files get scanned, and tool errors.
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from test_rules import setUpModule

PACK = Path(__file__).resolve().parent.parent
RATCHET = PACK / "ratchet"
CONFIG = PACK / "sgconfig.yml"
GIT_IDENTITY = {
    "GIT_AUTHOR_NAME": "lint test",
    "GIT_AUTHOR_EMAIL": "lint@example.invalid",
    "GIT_COMMITTER_NAME": "lint test",
    "GIT_COMMITTER_EMAIL": "lint@example.invalid",
}

CLEAN = "def add(a, b):\n    return a + b\n"
ONE = "def add(a, b):\n    return a + b  # one\n"
TWO = "def add(a, b):\n    x = a  # one\n    return x + b  # two\n"
THREE = "def add(a, b):\n    x = a  # one\n    y = b  # two\n    return x + y  # three\n"
BODY = "".join(f"def step_{i}(value):\n    return value + {i}\n\n\n" for i in range(12))
LONG_TWO = BODY + TWO
LONG_THREE = BODY + THREE
SWAPPED = "def add(a, b):\n    x = a  # one\n    return x + b  # other\n"
NC_PY = "no-comments-python"


def make_repo(test):
    repo = Path(tempfile.mkdtemp())
    test.addCleanup(shutil.rmtree, repo, ignore_errors=True)
    git(repo, "init", "-q")
    return repo


def git(repo, *args):
    proc = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, env=dict(os.environ, **GIT_IDENTITY))
    if proc.returncode != 0:
        raise AssertionError(proc.stderr)
    return proc.stdout


def commit(repo):
    git(repo, "add", "-A")
    git(repo, "-c", "core.hooksPath=.no-hooks", "commit", "-q", "--no-gpg-sign", "-m", "base")
    return git(repo, "rev-parse", "HEAD").strip()


def run_ratchet(repo, *args, env=None, ratchet=RATCHET):
    return subprocess.run(
        [sys.executable, str(ratchet), *args],
        cwd=repo,
        capture_output=True,
        text=True,
        encoding="utf-8",
        env=env,
    )


def fake_ast_grep(directory, behaviours):
    script = directory / "fake_ast_grep.py"
    script.write_text(
        "import sys\n"
        f"behaviours = {behaviours!r}\n"
        "mode = 'files' if '--files-with-matches' in sys.argv else sys.argv[1]\n"
        "code, out, err = behaviours[mode]\n"
        "sys.stdout.write(out)\n"
        "sys.stderr.write(err)\n"
        "sys.exit(code)\n",
        encoding="utf-8",
    )
    if os.name == "nt":
        Path(directory, "ast-grep.bat").write_text(f'@"{sys.executable}" "{script}" %*\n', encoding="utf-8")
    else:
        wrapper = Path(directory, "ast-grep")
        wrapper.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n', encoding="utf-8")
        wrapper.chmod(0o755)
    return dict(os.environ, PATH=os.pathsep.join([str(directory), str(Path(shutil.which("git")).parent)]))


class RepoTest(unittest.TestCase):
    def setUp(self):
        self.repo = make_repo(self)

    def write(self, name, text):
        path = Path(self.repo, name)
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(text, bytes):
            path.write_bytes(text)
        else:
            path.write_text(text, encoding="utf-8", newline="\n")

    def ratchet(self, *args, env=None):
        return run_ratchet(self.repo, *args, env=env)

    def baseline(self):
        return json.loads(Path(self.repo, ".lint-baseline.json").read_text(encoding="utf-8"))

    def set_baseline(self, text):
        Path(self.repo, ".lint-baseline.json").write_text(text, encoding="utf-8")

    def init(self):
        proc = self.ratchet("--init")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return proc

    def reinit(self):
        Path(self.repo, ".lint-baseline.json").unlink()
        return self.init()

    def entries(self, path):
        return self.baseline()[path][NC_PY]

    def counts(self):
        return {path: {rule: len(keys) for rule, keys in rules.items()} for path, rules in self.baseline().items()}

    def assertExit(self, proc, code):
        self.assertEqual(proc.returncode, code, proc.stdout + proc.stderr)


class Ratchet(RepoTest):
    def test_baseline_records_each_violation(self):
        self.write("a.py", TWO)
        self.init()
        self.assertEqual(list(self.baseline()), ["a.py"])
        keys = self.entries("a.py")
        self.assertEqual(len(set(keys)), 2)
        self.assertTrue(all(len(key) == 16 and set(key) <= set("0123456789abcdef") for key in keys))
        self.assertExit(self.ratchet(), 0)

    def test_new_violation_fails_and_prints_only_it(self):
        self.write("a.py", ONE)
        self.init()
        self.write("a.py", TWO)
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertIn("a.py: 1 no-comments-python violations are not in the baseline", proc.stdout)
        self.assertIn("a.py:3: no-comments-python: Comments are not allowed.", proc.stdout)
        self.assertNotIn("a.py:2:", proc.stdout)

    def test_swapped_violation_fails_and_prints_only_the_new_one(self):
        self.write("a.py", TWO)
        self.init()
        self.write("a.py", SWAPPED)
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertIn("a.py:3: no-comments-python:", proc.stdout)
        self.assertNotIn("a.py:2:", proc.stdout)

    def test_fixed_violation_frees_no_slack(self):
        self.write("a.py", TWO)
        self.init()
        self.write("a.py", ONE)
        proc = self.ratchet()
        self.assertExit(proc, 0)
        self.assertIn("run with --update to remove their entries", proc.stdout)
        self.write("a.py", SWAPPED)
        self.assertExit(self.ratchet(), 1)
        self.write("a.py", ONE)
        self.assertExit(self.ratchet("--update"), 0)
        self.write("a.py", SWAPPED)
        self.assertExit(self.ratchet(), 1)

    def test_moved_or_reindented_violation_passes(self):
        self.write("a.py", TWO)
        self.init()
        self.write("a.py", "def add(a, b):\n    x = a  # two\n\n    return x + b  #   one\n")
        self.assertExit(self.ratchet(), 0)

    def test_duplicate_text_is_counted_per_occurrence(self):
        self.write("a.py", "x = 1  # same\ny = 2  # same\n")
        self.init()
        self.assertEqual(len(self.entries("a.py")), 2)
        self.write("a.py", "x = 1  # same\ny = 2  # same\nz = 3  # same\n")
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertIn("a.py:3: no-comments-python:", proc.stdout)
        self.assertNotIn("a.py:1:", proc.stdout)
        self.assertNotIn("a.py:2:", proc.stdout)

    def test_new_file_fails(self):
        self.write("a.py", ONE)
        self.init()
        self.write("b.py", ONE)
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertIn("b.py:2: no-comments-python:", proc.stdout)

    def test_update_removes_fixed_entries_and_drops_clean_files(self):
        self.write("a.py", TWO)
        self.write("b.py", ONE)
        self.init()
        before = self.entries("a.py")
        self.write("a.py", ONE)
        self.write("b.py", CLEAN)
        self.assertExit(self.ratchet(), 0)
        self.assertExit(self.ratchet("--update"), 0)
        self.assertEqual(list(self.baseline()), ["a.py"])
        self.assertEqual(len(self.entries("a.py")), 1)
        self.assertIn(self.entries("a.py")[0], before)

    def test_update_refuses_to_add(self):
        self.write("a.py", ONE)
        self.init()
        self.write("a.py", TWO)
        self.write("b.py", CLEAN)
        before = self.baseline()
        proc = self.ratchet("--update")
        self.assertExit(proc, 1)
        self.assertIn("refusing to update", proc.stdout)
        self.assertEqual(self.baseline(), before)

    def test_init_refuses_existing_baseline(self):
        self.write("a.py", ONE)
        self.init()
        before = self.baseline()
        self.write("a.py", TWO)
        self.assertExit(self.ratchet("--init"), 2)
        self.assertEqual(self.baseline(), before)

    def test_missing_baseline_is_tool_error(self):
        self.write("a.py", CLEAN)
        self.assertExit(self.ratchet(), 2)

    def test_missing_ast_grep_is_tool_error(self):
        self.write("a.py", CLEAN)
        self.init()
        env = dict(os.environ, PATH=str(Path(shutil.which("git")).parent))
        proc = self.ratchet(env=env)
        self.assertExit(proc, 2)
        self.assertIn("ast-grep is not on PATH", proc.stderr)
        self.assertIn("uv tool install ast-grep-cli==0.51.1", proc.stderr)

    def test_outside_a_git_repo_is_tool_error(self):
        plain = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, plain, ignore_errors=True)
        Path(plain, "a.py").write_text(ONE, encoding="utf-8")
        env = dict(os.environ, GIT_CEILING_DIRECTORIES=str(plain.parent))
        self.assertExit(run_ratchet(plain, "--init", env=env), 2)

    def test_baseline_values_must_be_hash_lists(self):
        self.write("a.py", ONE)
        for value in ["Infinity", "NaN", "1.5", "true", "null", '"0123456789abcdef"', "[1]", '["0123456789ABCDEF"]',
                      '["0123456789abcde"]', '["0123456789abcdef0"]', '{"0123456789abcdef": 1}']:
            with self.subTest(value):
                self.set_baseline('{"a.py": {"no-comments-python": %s}}' % value)
                proc = self.ratchet()
                self.assertExit(proc, 2)
                self.assertIn("must be a list of 16-character violation hashes", proc.stderr)
        for text in ["[]", '{"a.py": 1}']:
            with self.subTest(text):
                self.set_baseline(text)
                self.assertExit(self.ratchet(), 2)

    def test_count_format_baseline_asks_for_init(self):
        self.write("a.py", ONE)
        for value in ["1", "0", "-1"]:
            with self.subTest(value):
                self.set_baseline('{"a.py": {"no-comments-python": %s}}' % value)
                for args in [(), ("--update",)]:
                    proc = self.ratchet(*args)
                    self.assertExit(proc, 2)
                    self.assertIn("older format", proc.stderr)
                    self.assertIn("Delete it and run with --init", proc.stderr)


class FileSelection(RepoTest):
    def test_non_utf8_file_is_tool_error(self):
        self.write("clean.py", CLEAN)
        self.init()
        self.write("b.py", b'y = "\xe9"  # hidden\n')
        proc = self.ratchet()
        self.assertExit(proc, 2)
        self.assertIn("b.py (byte 5)", proc.stderr)

    def test_dot_folders_and_tracked_ignored_files_are_scanned(self):
        self.write(".gitignore", "generated/\n")
        for name in [".github/scripts/a.js", ".storybook/main.ts", ".husky/check.mjs", "generated/kept.py"]:
            self.write(name, ONE if name.endswith(".py") else "export const x = 1; // c\n")
        git(self.repo, "add", "-f", "generated/kept.py")
        self.write("generated/untracked.py", ONE)
        self.init()
        self.assertEqual(sorted(self.baseline()), [".github/scripts/a.js", ".husky/check.mjs", ".storybook/main.ts", "generated/kept.py"])

    def test_each_extension_is_scanned(self):
        rules = {
            "a.mjs": "no-comments-js", "a.cjs": "no-comments-js", "a.jsx": "no-comments-js",
            "a.mts": "no-comments-ts", "a.cts": "no-comments-ts", "a.pyi": "no-comments-python",
        }
        for name in rules:
            self.write(name, ONE if name.endswith(".pyi") else "export const x = 1; // c\n")
        self.write("notes.txt", "x # c\n")
        self.init()
        self.assertEqual(self.counts(), {name: {rule: 1} for name, rule in rules.items()})

    def test_empty_file_is_not_a_parse_error(self):
        self.write("empty.py", "")
        self.write("empty.ts", "")
        self.init()
        self.assertEqual(self.baseline(), {})

    def test_extensionless_scripts_are_scanned_by_shebang(self):
        scripts = {
            "bin/tool": ("#!/usr/bin/env python3\nx = 1  # c\n", "no-comments-python"),
            "bin/ratchet": ("#!/usr/bin/python3 -u\nx = 1  # c\n", "no-comments-python"),
            "bin/cli": ("#!/usr/bin/env node\nconst x = 1; // c\n", "no-comments-js"),
            "scripts/cli": ("#! /usr/bin/env python3.12\nx = 1  # c\n", "no-comments-python"),
            "bin/task": ("#!/usr/bin/env -S deno run -A\nconst x: number = 1; // c\n", "no-comments-ts"),
            "bin/serve": ("#!/usr/bin/env bun\nconst x = 1; // c\n", "no-comments-ts"),
            "bin/odd[1]{2}": ("#!/usr/bin/env tsx\nconst x = 1; // c\n", "no-comments-ts"),
            "bin/uvtool": ("#!/usr/bin/env -S uv run --script\nx = 1  # c\n", "no-comments-python"),
            "bin/envtool": ("#!/usr/bin/env -u NAME python3\nx = 1  # c\n", "no-comments-python"),
            "bin/npxtool": ("#!/usr/bin/env -S npx tsx\nconst x = 1; // c\n", "no-comments-ts"),
            "a.pyw": (ONE, "no-comments-python"),
        }
        for name, (text, _) in scripts.items():
            self.write(name, text)
        self.write("bin/build.sh", "#!/usr/bin/env python3\nx = 1  # c\n")
        self.write("bin/deploy", "#!/bin/sh\n# c\n")
        self.write("Makefile", "# c\nall:\n")
        self.init()
        self.assertEqual(self.counts(), {name: {rule: 1} for name, (_, rule) in scripts.items()})

    def test_more_paths_than_one_command_line_holds(self):
        names = [f"src/{'x' * 100}{i:03}.py" for i in range(320)]
        for name in names:
            self.write(name, ONE)
        self.init()
        self.assertEqual(len(self.baseline()), len(names))


class AstGrepFailures(RepoTest):
    def run_with(self, behaviours):
        self.write("a.py", ONE)
        self.set_baseline("{}")
        fake = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, fake, ignore_errors=True)
        defaults = {"scan": (0, "[]", ""), "run": (1, "[]", ""), "files": (0, "a.py\n", "")}
        return self.ratchet(env=fake_ast_grep(fake, {**defaults, **behaviours}))

    def test_error_text_with_exit_zero_is_tool_error(self):
        proc = self.run_with({"scan": (0, "[]", "ERROR: a.py: The system cannot find the file specified.\n")})
        self.assertExit(proc, 2)
        self.assertIn("cannot find the file", proc.stderr)

    def test_error_exit_with_output_is_tool_error(self):
        match = json.dumps([{"file": "a.py", "range": {"start": {"line": 1}}, "ruleId": "no-comments-python", "message": "m",
                             "severity": "error"}])
        for code, out, err in [(2, "[]", "Error: bad rule\n"), (1, match, "Error: 1 error(s) found in code.\nHelp: x\npanicked\n"),
                               (1, match, "Error: 3 error(s) found in code.\nHelp: x\n"), (1, "[]", ""), (0, "not json", "")]:
            with self.subTest(code=code, err=err):
                self.assertExit(self.run_with({"scan": (code, out, err)}), 2)

    def test_file_ast_grep_did_not_parse_is_tool_error(self):
        proc = self.run_with({"scan": (0, "[]", ""), "files": (1, "", ""), "run": (1, "[]", "")})
        self.assertExit(proc, 2)
        self.assertIn("did not parse these python files", proc.stderr)
        self.assertIn("a.py", proc.stderr)


class Base(RepoTest):
    def start(self, files, baseline=True):
        for name, text in files.items():
            self.write(name, text)
        if baseline:
            self.init()
        return commit(self.repo)

    def test_new_entry_fails_against_base(self):
        ref = self.start({"a.py": ONE})
        self.write("a.py", TWO)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 1)
        self.assertIn("a.py adds 1 no-comments-python violations", proc.stdout)
        self.assertIn("  a.py:3: no-comments-python:", proc.stdout)
        self.assertNotIn("a.py:2:", proc.stdout)

    def test_swapped_entry_fails_against_base(self):
        ref = self.start({"a.py": TWO})
        self.write("a.py", SWAPPED)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 1)
        self.assertIn("a.py adds 1 no-comments-python violations", proc.stdout)

    def test_new_key_fails_against_base(self):
        ref = self.start({"a.py": ONE})
        self.write("b.py", ONE)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_new_key_for_missing_file_fails_against_base(self):
        ref = self.start({"a.py": ONE})
        baseline = self.baseline()
        baseline["ghost.py"] = baseline["a.py"]
        self.set_baseline(json.dumps(baseline))
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_deleted_baseline_rewritten_with_init_fails_against_base(self):
        ref = self.start({"a.py": ONE})
        self.write("a.py", TWO)
        self.reinit()
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_invalid_head_baseline_fails_against_base(self):
        ref = self.start({"a.py": ONE})
        self.set_baseline('{"a.py": {"no-comments-python": ["not a hash"]}}')
        self.assertExit(self.ratchet("--base", ref), 2)

    def test_invalid_base_baseline_is_tool_error(self):
        self.write("a.py", ONE)
        self.set_baseline('{"a.py": {"no-comments-python": ["x"]}}')
        ref = commit(self.repo)
        self.reinit()
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 2)
        self.assertIn(f"{ref}:.lint-baseline.json", proc.stderr)

    def test_count_format_base_has_nothing_to_compare(self):
        self.write("a.py", ONE)
        self.set_baseline('{"a.py": {"no-comments-python": 1}}')
        ref = commit(self.repo)
        self.reinit()
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 0)
        self.assertIn("older count format, so there is nothing to compare against", proc.stdout)

    def test_rename_passes_after_update(self):
        ref = self.start({"a.py": TWO})
        before = self.entries("a.py")
        git(self.repo, "mv", "a.py", "b.py")
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertIn("moved with files git sees as renamed; run with --update", proc.stdout)
        self.assertExit(self.ratchet("--update"), 0)
        self.assertEqual(self.baseline(), {"b.py": {NC_PY: before}})
        self.assertExit(self.ratchet("--base", ref), 0)
        commit(self.repo)
        self.assertExit(self.ratchet("--base", ref), 0)

    def test_committed_rename_passes_after_update(self):
        ref = self.start({"a.py": TWO})
        before = self.entries("a.py")
        git(self.repo, "mv", "a.py", "b.py")
        commit(self.repo)
        self.assertExit(self.ratchet(), 1)
        self.assertExit(self.ratchet("--update"), 0)
        self.assertEqual(self.baseline(), {"b.py": {NC_PY: before}})
        self.assertExit(self.ratchet("--base", ref), 0)

    def test_rename_must_be_staged(self):
        self.start({"a.py": TWO})
        before = self.entries("a.py")
        Path(self.repo, "a.py").rename(Path(self.repo, "b.py"))
        proc = self.ratchet("--update")
        self.assertExit(proc, 1)
        self.assertNotIn("renamed", proc.stdout)
        git(self.repo, "add", "-A")
        self.assertExit(self.ratchet("--update"), 0)
        self.assertEqual(self.baseline(), {"b.py": {NC_PY: before}})

    def test_deleted_file_frees_nothing(self):
        ref = self.start({"dead.py": TWO, "live.py": CLEAN})
        git(self.repo, "rm", "-q", "dead.py")
        self.write("live.py", TWO)
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertNotIn("renamed", proc.stdout)
        self.assertExit(self.ratchet("--update"), 1)
        self.assertExit(self.ratchet("--base", ref), 1)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 1)
        self.assertIn("live.py adds 2 no-comments-python violations", proc.stdout)

    def test_rename_to_unscanned_extension_frees_nothing(self):
        ref = self.start({"dead.py": TWO, "live.py": CLEAN})
        git(self.repo, "mv", "dead.py", "dead.txt")
        self.write("live.py", TWO)
        proc = self.ratchet()
        self.assertExit(proc, 1)
        self.assertNotIn("renamed", proc.stdout)
        self.assertExit(self.ratchet("--update"), 1)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_rename_carries_entries_only_to_its_target(self):
        ref = self.start({"a.py": LONG_TWO, "c.py": CLEAN})
        git(self.repo, "mv", "a.py", "b.py")
        self.write("c.py", ONE)
        self.assertExit(self.ratchet("--update"), 1)
        self.write("b.py", BODY + ONE)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 1)
        self.assertIn("c.py adds 1 no-comments-python violations", proc.stdout)
        self.assertNotIn("b.py adds", proc.stdout)

    def test_rename_with_new_violation_fails_against_base(self):
        ref = self.start({"a.py": LONG_TWO})
        git(self.repo, "mv", "a.py", "b.py")
        self.write("b.py", LONG_THREE)
        self.assertExit(self.ratchet("--update"), 1)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 1)
        self.assertIn("b.py was renamed from a.py", proc.stdout)

    def test_trading_a_fix_for_a_new_violation_fails_against_base(self):
        ref = self.start({"a.py": TWO, "b.py": CLEAN})
        self.write("a.py", ONE)
        self.write("b.py", ONE)
        self.reinit()
        self.assertExit(self.ratchet(), 0)
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_stale_entry_beside_new_file_fails_against_base(self):
        ref = self.start({"a.py": TWO})
        stale = self.baseline()
        Path(self.repo, "a.py").unlink()
        self.write("c.py", TWO)
        self.reinit()
        self.set_baseline(json.dumps({**stale, **self.baseline()}))
        self.assertExit(self.ratchet(), 0)
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_removed_entries_pass_against_base(self):
        ref = self.start({"a.py": TWO})
        self.write("a.py", ONE)
        self.assertExit(self.ratchet("--update"), 0)
        self.assertExit(self.ratchet("--base", ref), 0)

    def test_base_without_baseline_passes(self):
        ref = self.start({"a.py": ONE}, baseline=False)
        self.init()
        proc = self.ratchet("--base", ref)
        self.assertExit(proc, 0)
        self.assertIn("nothing to compare against", proc.stdout)

    def test_base_still_runs_the_plain_check(self):
        ref = self.start({"a.py": ONE})
        self.write("a.py", TWO)
        self.assertExit(self.ratchet("--base", ref), 1)

    def test_unknown_base_is_tool_error(self):
        self.start({"a.py": ONE})
        for ref in ["no-such-branch", "--help"]:
            with self.subTest(ref):
                self.assertExit(self.ratchet(f"--base={ref}"), 2)


class AstGrepIgnore(unittest.TestCase):
    CASES = {
        "same line": ("a.py", "x = 1\ny = 2  # ast-grep-ignore\n", 2),
        "line above": ("a.ts", "export const x = 1;\n// ast-grep-ignore\nexport const y = 2; // hidden\n", 2),
        "named rule": ("a.rs", "fn f() {}\n// ast-grep-ignore: no-inline-suppressions-rust\n#[allow(dead_code)]\nfn g() {}\n", 2),
        "whole file": ("a.js", "// ast-grep-ignore\n\nmodule.exports = 1; // hidden\n", 1),
        "header": ("a.tsx", "// ast-grep-ignore\nexport const x = <div>{/* hidden */}</div>;\n", 1),
        "html comment": ("b.js", "module.exports = 1;\n<!-- ast-grep-ignore\n", 2),
    }

    def test_ignore_comment_cannot_hide_a_violation(self):
        for label, (name, text, line) in self.CASES.items():
            with self.subTest(label):
                repo = make_repo(self)
                Path(repo, "clean.py").write_text(CLEAN, encoding="utf-8")
                init = run_ratchet(repo, "--init")
                self.assertEqual(init.returncode, 0, init.stderr)
                Path(repo, name).write_text(text, encoding="utf-8", newline="\n")
                proc = run_ratchet(repo)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn(f"{name}:{line}: no-ast-grep-ignore:", proc.stdout)

    def test_scan_alone_is_blind_to_same_line_ignore(self):
        repo = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, repo)
        Path(repo, "a.py").write_text("y = 2  # ast-grep-ignore\n", encoding="utf-8")
        proc = subprocess.run(
            [shutil.which("ast-grep"), "scan", "-c", str(CONFIG), "--json=compact", "."],
            cwd=repo, capture_output=True, text=True, encoding="utf-8",
        )
        self.assertEqual(json.loads(proc.stdout), [])


class SelfCheck(unittest.TestCase):
    def test_pack_passes_its_own_ratchet(self):
        repo = make_repo(self)
        copy = Path(repo, "setup", "lint")
        shutil.copytree(PACK, copy, ignore=shutil.ignore_patterns("__pycache__"))
        proc = run_ratchet(repo, "--init", ratchet=copy / "ratchet")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("with 0 violations", proc.stdout)
        self.assertEqual(json.loads(Path(repo, ".lint-baseline.json").read_text(encoding="utf-8")), {})


if __name__ == "__main__":
    unittest.main()
