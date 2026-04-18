#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "tomli",
# ]
# ///
"""Test harness for Lume compiler tests defined in tests/tests.toml and tests/repl.toml."""

import json
import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOML_PATH = os.path.join(REPO_ROOT, "tests", "tests.toml")
REPL_TOML_PATH = os.path.join(REPO_ROOT, "tests", "repl.toml")
LUAJIT = "luajit"

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def build_lume() -> str:
    """Build the lume binary once and return its path."""
    print("Building lume...")
    result = subprocess.run(
        ["cargo", "build", "--bin", "lume"],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )
    if result.returncode != 0:
        print("cargo build failed:")
        print(result.stderr)
        sys.exit(1)

    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )
    if meta.returncode == 0:
        target_dir = json.loads(meta.stdout)["target_directory"]
    else:
        target_dir = os.path.join(REPO_ROOT, "target")

    bin_path = os.path.join(target_dir, "debug", "lume")
    if sys.platform == "win32":
        bin_path += ".exe"

    print(f"Built: {bin_path}\n")
    return bin_path


def run_test(name: str, test: dict, lume_bin: str) -> tuple[bool, str]:
    """Run a single test. Returns (passed, message)."""
    path = os.path.join(REPO_ROOT, test["path"])
    expect = test["expect"]
    result = subprocess.run(
        [lume_bin, "lua", path],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )
    compiler_output = result.stdout + result.stderr
    if expect == "error":
        if result.returncode == 0:
            return False, "expected type error but compilation succeeded"
        error_contains = test.get("error_contains")
        if error_contains and error_contains not in compiler_output:
            return False, (
                f"expected error containing: {error_contains!r}\n"
                f"  got: {compiler_output.strip()}"
            )
        return True, "correctly rejected"
    elif expect == "typecheck":
        if result.returncode != 0:
            return (
                False,
                f"expected success but got error:\n  {compiler_output.strip()}",
            )
        lua_code = result.stdout
        expected_output = test.get("output")
        if expected_output is None:
            return True, "typechecks (no runtime check)"
        rt = subprocess.run(
            [LUAJIT, "-"],
            input=lua_code,
            capture_output=True,
            text=True,
        )
        if rt.returncode != 0:
            return False, f"runtime error:\n  {rt.stderr.strip()}"
        actual = rt.stdout
        if actual.strip() != expected_output.strip():
            return False, (
                f"output mismatch:\n"
                f"  expected: {expected_output.strip()!r}\n"
                f"  actual:   {actual.strip()!r}"
            )
        return True, "output matches"
    else:
        return False, f"unknown expect value: {expect!r}"


def run_repl_test(name: str, test: dict, lume_bin: str) -> tuple[bool, str]:
    """Run a single REPL test. Returns (passed, message).

    Each test drives `lume repl` via stdin.  Steps are joined with newlines and
    piped in; stdout is compared against `output` (ANSI codes stripped), and
    stderr is checked against `error_contains` when present.
    """
    steps: list[str] = test.get("steps", [])
    stdin_text = "\n".join(steps) + "\n"

    result = subprocess.run(
        [lume_bin, "repl"],
        input=stdin_text,
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )

    stdout = ANSI_RE.sub("", result.stdout)
    stderr = ANSI_RE.sub("", result.stderr)

    expected_output: str | None = test.get("output")
    error_contains: str | None = test.get("error_contains")

    if expected_output is not None:
        actual = stdout.strip()
        expected = expected_output.strip()
        if actual != expected:
            return False, (
                f"output mismatch:\n"
                f"  expected: {expected!r}\n"
                f"  actual:   {actual!r}"
            )

    if error_contains is not None:
        if error_contains not in stderr:
            return False, (
                f"expected stderr to contain {error_contains!r}\n"
                f"  got: {stderr.strip()!r}"
            )

    if expected_output is None and error_contains is None:
        if result.returncode != 0:
            return False, f"exited with code {result.returncode}:\n  {stderr.strip()}"

    return True, "ok"


def main():
    lume_bin = build_lume()

    with open(TOML_PATH, "rb") as f:
        config = tomllib.load(f)
    compiler_tests = config.get("tests", {})

    repl_tests: dict = {}
    if os.path.exists(REPL_TOML_PATH):
        with open(REPL_TOML_PATH, "rb") as f:
            repl_tests = tomllib.load(f).get("tests", {})

    compiler_results: dict[str, tuple[bool, str]] = {}
    repl_results: dict[str, tuple[bool, str]] = {}

    with ThreadPoolExecutor() as executor:
        compiler_futures = {
            executor.submit(run_test, name, test, lume_bin): name
            for name, test in compiler_tests.items()
        }
        repl_futures = {
            executor.submit(run_repl_test, name, test, lume_bin): name
            for name, test in repl_tests.items()
        }
        for future in as_completed(compiler_futures):
            compiler_results[compiler_futures[future]] = future.result()
        for future in as_completed(repl_futures):
            repl_results[repl_futures[future]] = future.result()

    def print_section(title: str, results: dict[str, tuple[bool, str]]) -> tuple[int, int]:
        if not results:
            return 0, 0
        print(f"\033[1m{title}\033[0m")
        p = f = 0
        for name in sorted(results):
            ok, msg = results[name]
            if ok:
                p += 1
                print(f"  \033[32m✓\033[0m {name}: {msg}")
            else:
                f += 1
                print(f"  \033[31m✗\033[0m {name}: {msg}")
        print()
        return p, f

    cp, cf = print_section("Compiler tests", compiler_results)
    rp, rf = print_section("REPL tests", repl_results)

    passed, failed = cp + rp, cf + rf
    print(f"{passed} passed, {failed} failed, {passed + failed} total")
    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
