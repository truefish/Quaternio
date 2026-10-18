#!/usr/bin/env python3
"""Preview a release, or publish it with --publish. Requires Python 3.11+ and Rust.

Stage the changes to include first. Unstaged files are never added automatically.
Beta numbers advance automatically; --version selects a new series or stable release.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
VERSION = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-beta\.(0|[1-9]\d*))?")


def run(*args):
    result = subprocess.run(args, cwd=ROOT, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip() or f"Failed: {' '.join(args)}")
    return result.stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--publish", action="store_true", help="Commit staged changes, tag, and push atomically")
    parser.add_argument("--version", help="Explicit version, e.g. 0.2.0-beta.1 or 0.1.0")
    args = parser.parse_args()
    if run("git", "branch", "--show-current") != "main":
        raise RuntimeError("Switch to main before preparing a release")
    if run("git", "diff", "--name-only", "--diff-filter=U"):
        raise RuntimeError("Resolve merge conflicts first")
    if run("git", "diff", "--name-only", "--", "Cargo.toml", "Cargo.lock"):
        raise RuntimeError("Stage or resolve your version-file edits first")

    manifest = (ROOT / "Cargo.toml").read_bytes().decode("utf-8")
    lock = (ROOT / "Cargo.lock").read_bytes().decode("utf-8")
    current = tomllib.loads(manifest)["package"]["version"]
    parsed = VERSION.fullmatch(current)
    if not parsed:
        raise RuntimeError(f"Unsupported current version: {current}")
    refs = run("git", "ls-remote", "--refs", "origin", "refs/heads/main", "refs/tags/*")
    remote = {ref.removeprefix("refs/tags/"): sha for sha, ref in (line.split() for line in refs.splitlines()) if ref.startswith("refs/tags/")}
    local = set(run("git", "tag", "--list").splitlines())
    staged = run("git", "diff", "--cached", "--name-only")
    pending = "v" + current in local and "v" + current not in remote
    if pending:
        if run("git", "rev-parse", f"v{current}^{{}}") != run("git", "rev-parse", "HEAD") or staged:
            raise RuntimeError(f"Unpublished v{current} exists. Push or resolve it before making another release")
        if args.version and args.version != current:
            raise RuntimeError(f"Finish publishing v{current} before selecting another version")
        version = current
    elif args.version:
        version = args.version
    else:
        if parsed[4] is None:
            raise RuntimeError("After a stable release, choose a new series with --version X.Y.Z-beta.1")
        base = ".".join(parsed.groups()[:3])
        if "v" + base in local | remote.keys():
            raise RuntimeError(f"{base} is already stable; choose a new series with --version")
        numbers = [int(m[4]) for tag in local | remote.keys()
                   if (m := VERSION.fullmatch(tag.removeprefix("v")))
                   and ".".join(m.groups()[:3]) == base and m[4] is not None]
        version = f"{base}-beta.{max(int(parsed[4]), max(numbers, default=0) + 1)}"
    if not VERSION.fullmatch(version):
        raise RuntimeError("Use X.Y.Z-beta.N or X.Y.Z for the version")
    tag = "v" + version
    if tag in remote or tag in local and not pending:
        raise RuntimeError(f"{tag} already exists; release tags are never moved")

    print(f"Release: {tag}\nBranch: main\nRemote: {run('git', 'remote', 'get-url', 'origin')}")
    print("Staged changes:\n" + (staged or "(version files only, or resume an unpublished tag)"))
    print("Unstaged changes and untracked files will be left out.")
    if not args.publish:
        print("Preview only. Run again with --publish to commit, tag, and push.")
        return

    run("git", "fetch", "--no-tags", "origin", "main")
    run("git", "merge-base", "--is-ancestor", "FETCH_HEAD", "HEAD")
    if version != current:
        package = re.search(r"(?ms)^\[package\]\s*\n.*?(?=^\[|\Z)", manifest)
        if not package:
            raise RuntimeError("Cannot locate Cargo.toml package section")
        updated, count = re.subn(r'(?m)^(version\s*=\s*")[^"]+(".*)$', lambda m: m[1] + version + m[2], package[0])
        lock_pattern = r'(?m)(^name = "quaternio"\r?\nversion = ")' + re.escape(current) + r'(")'
        updated_lock, lock_count = re.subn(lock_pattern, lambda m: m[1] + version + m[2], lock)
        if count != 1 or lock_count != 1:
            raise RuntimeError("Expected one matching application version in each Cargo file")
        (ROOT / "Cargo.toml").write_bytes((manifest[:package.start()] + updated + manifest[package.end():]).encode("utf-8"))
        (ROOT / "Cargo.lock").write_bytes(updated_lock.encode("utf-8"))
        run("git", "add", "--", "Cargo.toml", "Cargo.lock")
    run("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1")
    run("git", "diff", "--cached", "--check")
    if run("git", "diff", "--cached", "--name-only"):
        run("git", "commit", "-m", f"Release {version}")
    if not pending:
        run("git", "tag", "-a", tag, "-m", f"Quaternio {version}")
    try:
        run("git", "push", "--atomic", "origin", "main", f"refs/tags/{tag}")
    except RuntimeError as error:
        raise RuntimeError(f"{error}\nCommit and tag remain local. Retry: git push --atomic origin main {tag}") from error
    print(f"Published {tag}. GitHub Actions will build and attach the release packages.")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        print(f"Release stopped: {error}", file=sys.stderr)
        sys.exit(1)
