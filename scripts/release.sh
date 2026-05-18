#!/usr/bin/env bash
# Cut a new release end-to-end: bump Cargo.toml, push a release PR, wait
# for CI, merge, tag, and push the tag (which triggers the Release
# workflow that builds the multi-platform zips).
#
# Usage:
#   ./scripts/release.sh <new-version>
#
# Examples:
#   ./scripts/release.sh 0.1.1     # patch
#   ./scripts/release.sh 0.2.0     # minor
#   ./scripts/release.sh 1.0.0     # major
#
# Requires:
#   * `gh` CLI authenticated as a repo admin (needed for `--admin` merge
#     and to push the protected `v*` tag).
#   * Clean working tree on `main`, up to date with origin.
#
# What it does, in order:
#   1. Sanity checks (clean tree, on main, current version differs).
#   2. Bumps the [package] version in Cargo.toml.
#   3. Refreshes Cargo.lock for the streaming-simulator entry only.
#   4. Builds + tests locally to make sure the bump compiles cleanly.
#   5. Pushes a chore/release-vX.Y.Z branch and opens a PR.
#   6. Waits for CI (`Test (Linux)`) to go green.
#   7. Squash-merges the PR with admin bypass.
#   8. Pulls the squashed main, creates an annotated tag vX.Y.Z, pushes it.
#   9. Prints the URL of the in-flight Release workflow run.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ "$#" -ne 1 ]]; then
    echo "usage: $0 <new-version>   (e.g. 0.1.1)" >&2
    exit 2
fi

NEW_VERSION="$1"
TAG="v${NEW_VERSION}"
BRANCH="chore/release-${TAG}"

# Match X.Y.Z with optional pre-release / build metadata, per SemVer.
if ! [[ "${NEW_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
    echo "error: '${NEW_VERSION}' doesn't look like a SemVer version" >&2
    exit 2
fi

# ── 1. Sanity checks ────────────────────────────────────────────────
if ! git diff-index --quiet HEAD --; then
    echo "error: working tree is dirty — commit or stash first" >&2
    exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
    echo "error: untracked changes present — clean up first" >&2
    exit 1
fi
if [[ "$(git rev-parse --abbrev-ref HEAD)" != "main" ]]; then
    echo "error: not on 'main'" >&2
    exit 1
fi
git fetch --quiet origin main
if [[ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]]; then
    echo "error: local 'main' is not in sync with origin/main" >&2
    exit 1
fi

CURRENT_VERSION="$(grep -E '^version = "' Cargo.toml | head -1 | sed -E 's/^version = "(.+)"$/\1/')"
if [[ "${CURRENT_VERSION}" == "${NEW_VERSION}" ]]; then
    echo "error: Cargo.toml already at ${NEW_VERSION}" >&2
    exit 1
fi

if git rev-parse "${TAG}" >/dev/null 2>&1; then
    echo "error: tag ${TAG} already exists locally" >&2
    exit 1
fi
if git ls-remote --exit-code --tags origin "${TAG}" >/dev/null 2>&1; then
    echo "error: tag ${TAG} already exists on origin" >&2
    exit 1
fi

echo "Bumping ${CURRENT_VERSION} → ${NEW_VERSION}"
echo

# ── 2. Bump Cargo.toml ──────────────────────────────────────────────
sed -i.bak -E "0,/^version = \"${CURRENT_VERSION}\"$/{s//version = \"${NEW_VERSION}\"/}" Cargo.toml
rm -f Cargo.toml.bak

# ── 3. Refresh Cargo.lock for just our package ──────────────────────
cargo update -p streaming-simulator --quiet

# ── 4. Local build + test (CI will repeat, but failing fast saves a PR) ──
echo "Running cargo test (this is what CI will re-run on the PR)..."
cargo test --features fdk-aac --lib --quiet

# ── 5. Branch, commit, push, open PR ────────────────────────────────
git checkout -b "${BRANCH}"
git add Cargo.toml Cargo.lock
git commit -m "chore(release): ${TAG}"
git push -u origin "${BRANCH}"

PR_URL="$(gh pr create --base main --fill --head "${BRANCH}")"
PR_NUM="${PR_URL##*/}"
echo "Opened PR ${PR_URL}"

# ── 6. Wait for CI green ────────────────────────────────────────────
echo "Waiting for CI on PR #${PR_NUM} (~3 min)..."
for _ in $(seq 1 24); do
    status="$(gh pr view "${PR_NUM}" --json statusCheckRollup --jq '.statusCheckRollup[0] | "\(.status) \(.conclusion)"')"
    case "${status}" in
        "COMPLETED SUCCESS")
            echo "CI green."
            break
            ;;
        "COMPLETED FAILURE"|"COMPLETED CANCELLED"|"COMPLETED TIMED_OUT")
            echo "error: CI failed on PR #${PR_NUM} (${status})" >&2
            echo "       Inspect: ${PR_URL}" >&2
            exit 1
            ;;
    esac
    sleep 15
done

# ── 7. Merge ────────────────────────────────────────────────────────
gh pr merge "${PR_NUM}" --squash --delete-branch --admin

# ── 8. Tag from the squashed main ───────────────────────────────────
git checkout main
git pull --ff-only origin main
git tag -a "${TAG}" -m "${TAG}"
git push origin "${TAG}"

# ── 9. Surface the Release workflow run ─────────────────────────────
sleep 3
RUN_URL="$(gh run list --workflow=release.yml --limit 1 --json url --jq '.[0].url')"
echo
echo "Release tag pushed. Workflow building binaries:"
echo "  ${RUN_URL}"
echo
echo "Watch live:"
echo "  gh run watch \$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status"
