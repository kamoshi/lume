#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "tomli",
# ]
# ///
"""Test harness for Lume compiler tests defined in tests/tests.toml."""

import subprocess
import sys
import os

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOML_PATH = os.path.join(REPO_ROOT, "tests", "tests.toml")
LUME_BIN = ["cargo", "run", "--quiet", "--bin", "lume"]
LUAJIT = "luajit"


def run_test(name: str, test: dict) -> tuple[bool, str]:
    """Run a single test. Returns (passed, message)."""
    path = os.path.join(REPO_ROOT, test["path"])
    expect = test["expect"]

    result = subprocess.run(
        LUME_BIN + ["lua", path],
        capture_output=True, text=True, cwd=REPO_ROOT,
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
            return False, f"expected success but got error:\n  {compiler_output.strip()}"

        lua_code = result.stdout
        expected_output = test.get("output")
        if expected_output is None:
            return True, "typechecks (no runtime check)"

        rt = subprocess.run(
            [LUAJIT, "-"],
            input=lua_code, capture_output=True, text=True,
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


def main():
    with open(TOML_PATH, "rb") as f:
        config = tomllib.load(f)

    tests = config.get("tests", {})
    passed = 0
    failed = 0

    for name, test in tests.items():
        ok, msg = run_test(name, test)
        if ok:
            passed += 1
            print(f"  \033[32m✓\033[0m {name}: {msg}")
        else:
            failed += 1
            print(f"  \033[31m✗\033[0m {name}: {msg}")

    print()
    print(f"{passed} passed, {failed} failed, {passed + failed} total")

    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
