#!/usr/bin/env python3
"""Guard security-sensitive GitHub Actions invariants."""

from __future__ import annotations

import re
import sys
from pathlib import Path

WORKFLOW_DIR = Path(".github/workflows")
SHA_RE = re.compile(r"[0-9a-f]{40}")
USES_RE = re.compile(r"\buses:\s*([^\s#]+)")


def workflow_text(name: str) -> str:
    return (WORKFLOW_DIR / name).read_text()


def check_action_refs(errors: list[str]) -> None:
    workflows = [*WORKFLOW_DIR.glob("*.yml"), *WORKFLOW_DIR.glob("*.yaml")]
    for path in sorted(workflows):
        for line_no, line in enumerate(path.read_text().splitlines(), start=1):
            if "\t" in line:
                errors.append(f"{path}:{line_no}: tab character")

            match = USES_RE.search(line)
            if match is None:
                continue

            ref = match.group(1)
            parts = ref.rsplit("@", maxsplit=1)
            if len(parts) != 2:
                errors.append(f"{path}:{line_no}: action ref is missing @: {ref}")
                continue

            if SHA_RE.fullmatch(parts[1]) is None:
                errors.append(
                    f"{path}:{line_no}: action ref is not pinned to a full commit SHA: {ref}"
                )


def check_docker_publish(errors: list[str]) -> None:
    text = workflow_text("docker.yml")
    if "workflow_dispatch:" in text:
        errors.append("docker.yml: publishing workflow must not accept arbitrary manual refs")
    if "packages: write" not in text:
        errors.append("docker.yml: publishing workflow must keep explicit packages: write permission")
    if "push: true" not in text:
        errors.append("docker.yml: publishing workflow must still publish images")


def check_docker_test(errors: list[str]) -> None:
    text = workflow_text("docker-test.yml")
    if "workflow_dispatch:" not in text:
        errors.append("docker-test.yml: manual test build must keep workflow_dispatch")
    if "packages: write" in text:
        errors.append("docker-test.yml: manual test build must not have package write permission")
    if "push: false" not in text:
        errors.append("docker-test.yml: manual test build must not push images")
    if "cache-to:" in text:
        errors.append("docker-test.yml: manual test build must not write shared caches")


def check_release(errors: list[str]) -> None:
    text = workflow_text("release.yml")
    before_jobs = text.split("jobs:", maxsplit=1)[0]
    if "permissions:\n  contents: read\n" not in before_jobs:
        errors.append("release.yml: missing top-level read-only token permissions")
    if "cargo build --locked --release --target ${{ matrix.target }}" not in text:
        errors.append("release.yml: release builds must use cargo build --locked")


def check_coverage_comment(errors: list[str]) -> None:
    text = workflow_text("coverage-comment.yml")
    for needle in ("Validate coverage comment artifact", "gt 60000", "@<!-- -->"):
        if needle not in text:
            errors.append(f"coverage-comment.yml: missing artifact hardening marker: {needle}")


def main() -> int:
    errors: list[str] = []
    check_action_refs(errors)
    check_docker_publish(errors)
    check_docker_test(errors)
    check_release(errors)
    check_coverage_comment(errors)

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("Workflow hardening checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
