#!/usr/bin/env python3
"""
resolve_version.py (desktop)

Deterministically resolves version names, tags, and artifact names for ASLC-desktop
releases (nightly and stable). Mirrors the Android repo's counterpart.

Guarantees:
1. Nightly tags follow 'nightly-YYYYMMDD-bVERSION-SHORT_SHA' and are immutable.
2. Stable tags follow 'vX.Y.Z' and must point to a commit on 'main'.
3. The installer is named 'ASLC-Node-Setup-<versionName>.exe'.
4. Historical versions and releases are preserved and never overwritten.
"""

import argparse
import datetime
import json
import os
import re
import subprocess
import sys
import urllib.request

REPO = "KollTHOR/simple-audio-stream-desktop"
BASELINE_VERSION_CODE = 0
DEFAULT_BASE_VERSION = "0.1.0"


def run_cmd(cmd, check=True):
    res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if check and res.returncode != 0:
        raise RuntimeError(f"Command failed ({res.returncode}): {' '.join(cmd)}\n{res.stderr.strip()}")
    return res.stdout.strip()


def get_base_version_from_cargo():
    cargo = os.path.join(os.path.dirname(__file__), "../../Cargo.toml")
    if os.path.exists(cargo):
        with open(cargo, "r", encoding="utf-8") as f:
            for line in f:
                m = re.match(r'\s*version\s*=\s*"([^"]+)"', line)
                if m:
                    return m.group(1).strip()
    return DEFAULT_BASE_VERSION


def get_git_commit_sha():
    env_sha = os.environ.get("GITHUB_SHA") or os.environ.get("GIT_COMMIT_SHA")
    if env_sha and len(env_sha) >= 7:
        return env_sha
    try:
        return run_cmd(["git", "rev-parse", "HEAD"])
    except Exception:
        return "unknown"


def get_git_commit_epoch():
    """Commit time (Unix seconds), used as SOURCE_DATE_EPOCH for a reproducible build stamp."""
    try:
        return run_cmd(["git", "log", "-1", "--format=%ct"]).strip()
    except Exception:
        return str(int(datetime.datetime.now(datetime.timezone.utc).timestamp()))


def is_commit_on_main(commit_sha):
    for ref in ["main", "origin/main", "refs/heads/main", "refs/remotes/origin/main"]:
        try:
            res = subprocess.run(["git", "merge-base", "--is-ancestor", commit_sha, ref],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            if res.returncode == 0:
                return True
        except Exception:
            continue
    return False


def fetch_published_version_codes_and_tags():
    version_codes = {BASELINE_VERSION_CODE}
    published_tags = set()

    try:
        url = f"https://api.github.com/repos/{REPO}/releases?per_page=100"
        req = urllib.request.Request(url, headers={"User-Agent": "ASLC-ReleaseScript"})
        token = os.environ.get("GITHUB_TOKEN")
        if token:
            req.add_header("Authorization", f"Bearer {token}")
        with urllib.request.urlopen(req, timeout=8) as resp:
            for r in json.loads(resp.read().decode("utf-8")):
                tag = (r.get("tag_name") or "").strip()
                if tag:
                    published_tags.add(tag)
                    m = re.search(r"-b(\d+)-", tag)
                    if m:
                        version_codes.add(int(m.group(1)))
    except Exception:
        pass

    try:
        for t in run_cmd(["git", "tag", "-l"], check=False).splitlines():
            t = t.strip()
            if t:
                published_tags.add(t)
                m = re.search(r"-b(\d+)-", t)
                if m:
                    version_codes.add(int(m.group(1)))
    except Exception:
        pass

    return version_codes, published_tags


def main():
    parser = argparse.ArgumentParser(description="Resolve release version metadata.")
    parser.add_argument("--channel", choices=["nightly", "stable"], required=True)
    parser.add_argument("--tag", default="", help="Tag name (required for stable)")
    parser.add_argument("--run-number", default="0")
    parser.add_argument("--output-file", default="")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()

    base_version = get_base_version_from_cargo()
    commit_sha = get_git_commit_sha()
    commit_short = commit_sha[:7] if commit_sha != "unknown" else "unknown"
    commit_epoch = get_git_commit_epoch()
    build_date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d")

    if args.offline:
        published_codes, published_tags = {BASELINE_VERSION_CODE}, set()
    else:
        published_codes, published_tags = fetch_published_version_codes_and_tags()

    max_code = max(published_codes) if published_codes else BASELINE_VERSION_CODE
    version_code = max_code + 1

    if args.channel == "nightly":
        version_name = f"{base_version}-nightly.{build_date}.{commit_short}"
        tag_name = f"nightly-{build_date}-b{version_code}-{commit_short}"
        is_prerelease = "true"
        release_title = f"Nightly Build: {tag_name}"
        if not args.dry_run and tag_name in published_tags:
            sys.stderr.write(f"ERROR: Tag '{tag_name}' already exists! Cannot overwrite an existing nightly.\n")
            sys.exit(1)
    else:
        raw_tag = args.tag.strip()
        if not raw_tag:
            sys.stderr.write("ERROR: --tag is required for stable releases.\n")
            sys.exit(1)
        if not re.match(r"^v[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?$", raw_tag):
            sys.stderr.write(f"ERROR: Tag '{raw_tag}' is not a valid vX.Y.Z tag.\n")
            sys.exit(1)
        if not args.dry_run and raw_tag in published_tags:
            sys.stderr.write(f"ERROR: Release for tag '{raw_tag}' already exists! Cannot overwrite it.\n")
            sys.exit(1)
        if not args.dry_run and not is_commit_on_main(commit_sha):
            sys.stderr.write(f"ERROR: Commit {commit_sha} for stable release {raw_tag} is NOT on main!\n")
            sys.exit(1)
        tag_name = raw_tag
        version_name = raw_tag.lstrip("v")
        is_prerelease = "false"
        release_title = f"Release {tag_name}"

    installer_name = f"ASLC-Node-Setup-{version_name}.exe"
    sha_name = f"{installer_name}.sha256"

    metadata = {
        "version_code": str(version_code),
        "version_name": version_name,
        "tag_name": tag_name,
        "installer_name": installer_name,
        "sha_name": sha_name,
        "commit_sha": commit_sha,
        "commit_short": commit_short,
        "commit_epoch": commit_epoch,
        "build_date": build_date,
        "base_version": base_version,
        "is_prerelease": is_prerelease,
        "channel": args.channel,
        "release_title": release_title,
    }

    print(json.dumps(metadata, indent=2))

    out_file = args.output_file or os.environ.get("GITHUB_OUTPUT")
    if out_file:
        with open(out_file, "a", encoding="utf-8") as f:
            for k, v in metadata.items():
                f.write(f"{k}={v}\n")


if __name__ == "__main__":
    main()
