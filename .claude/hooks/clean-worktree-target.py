#!/usr/bin/env python3
"""SessionEnd hook: `cargo clean` the checkout this session ran in.

Scope is one checkout, never the whole machine. `cargo clean` is already
per-workspace, so the only real job here is refusing to run when the target
dir is shared (CARGO_TARGET_DIR) — otherwise cleaning one worktree would
throw away every other worktree's build cache too.

Main checkout with no worktree is a normal target: it gets cleaned like any
other. Skipped when the session is resumable (reason "clear"), since a
resumed session usually wants its build cache intact.
"""
import json
import os
import subprocess
import sys

EXPECTED_COMMON_DIR = "/Users/michaelmcclelland/Documents/Projects/borg/.git"
# SessionEnd reasons that mean "this checkout is done with"; anything else
# (notably "clear") keeps the cache.
CLEAN_ON = {"logout", "prompt_input_exit", "other"}


def log(msg):
    sys.stderr.write(f"[clean-worktree-target] {msg}\n")


def git(cwd, *args):
    r = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=False
    )
    return r.stdout.strip() if r.returncode == 0 else ""


def main():
    # No usable payload means no confirmed cwd/reason. Deleting a multi-GB
    # build cache is not a safe default, so do nothing.
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        log("unreadable hook payload; leaving target/ alone")
        return 0
    if not isinstance(payload, dict) or not payload.get("cwd"):
        log("hook payload has no cwd; leaving target/ alone")
        return 0

    cwd = payload["cwd"]
    reason = payload.get("reason", "other")
    if reason not in CLEAN_ON:
        log(f"reason={reason}; leaving target/ alone")
        return 0

    # Only ever act inside this repo (main checkout or one of its worktrees).
    common = git(cwd, "rev-parse", "--path-format=absolute", "--git-common-dir")
    if common != EXPECTED_COMMON_DIR:
        return 0

    # A shared target dir means "clean" is not worktree-scoped. Bail out.
    if os.environ.get("CARGO_TARGET_DIR"):
        log("CARGO_TARGET_DIR is set; refusing to clean a shared target dir")
        return 0

    root = git(cwd, "rev-parse", "--show-toplevel")
    if not root or not os.path.isfile(os.path.join(root, "Cargo.toml")):
        return 0

    target = os.path.join(root, "target")
    if not os.path.isdir(target):
        log(f"no target/ in {root}")
        return 0

    r = subprocess.run(
        ["cargo", "clean"], cwd=root, capture_output=True, text=True, check=False
    )
    if r.returncode != 0:
        log(f"cargo clean failed in {root}: {r.stderr.strip()}")
        return 0

    log(f"cleaned {target}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
