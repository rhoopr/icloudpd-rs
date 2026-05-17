#!/usr/bin/env python3
"""Re-parse phase log files for an existing run record and merge any
newly-supported metrics into the JSON record. One-off helper used when
the parser in time_phase.sh gains a new field and we want today's run
to show it without re-running the whole suite.

Only fills fields that are missing from the record AND the log file's
mtime is after the run's started_at (so we don't pick up logs from a
later run that overwrote the same path).

Usage: backfill_metrics.py <run-record.json>
"""
from __future__ import annotations

import json
import os
import re
import sys
from datetime import datetime
from pathlib import Path

CARGO_TESTS = re.compile(r"test result: ok\. (\d+) passed")
SHELL_TESTS = re.compile(r"RESULTS: (\d+) pass")
TRANSFERRED = re.compile(r"Transferred ([\d.]+)\s*MiB")
SUMMARY = re.compile(r"(\d+)\s+downloaded,\s+(\d+)\s+failed\s+\((\d+)\s+total\)")
ICLOUD = re.compile(r"kei::icloud")


def parse_log(path: Path) -> dict:
    tests = bytes_mib = downloaded = processed = icloud_lines = 0
    tests_seen = bytes_seen = summary_seen = False
    with path.open(errors="replace") as f:
        for line in f:
            m = CARGO_TESTS.search(line)
            if m:
                tests += int(m.group(1)); tests_seen = True
            m = SHELL_TESTS.search(line)
            if m:
                tests += int(m.group(1)); tests_seen = True
            m = TRANSFERRED.search(line)
            if m:
                bytes_mib += float(m.group(1)); bytes_seen = True
            m = SUMMARY.search(line)
            if m:
                downloaded += int(m.group(1))
                processed += int(m.group(3))
                summary_seen = True
            if ICLOUD.search(line):
                icloud_lines += 1
    out = {}
    if tests_seen:
        out["tests"] = tests
    if bytes_seen:
        out["bytes_transferred_mib"] = round(bytes_mib, 2)
    if summary_seen:
        out["assets_downloaded"] = downloaded
        out["assets_processed"] = processed
    if icloud_lines:
        out["icloud_log_lines"] = icloud_lines
    return out


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <run-record.json>", file=sys.stderr)
        return 64
    rec_path = Path(sys.argv[1])
    rec = json.loads(rec_path.read_text())
    started_at = rec.get("started_at", "")
    started_dt = None
    if started_at:
        try:
            # Records use naive ISO-8601 in local time; comparing to file
            # mtime which is UTC epoch is good enough for a guard rail.
            started_dt = datetime.fromisoformat(started_at)
        except ValueError:
            pass

    new_fields = {"assets_downloaded", "assets_processed"}
    changed = 0
    for phase_name, phase in rec.get("phases", {}).items():
        log_path = phase.get("log_path")
        if not log_path or not os.path.exists(log_path):
            continue
        if started_dt is not None:
            log_mtime = datetime.fromtimestamp(os.path.getmtime(log_path))
            # started_at is set at finalize-run time, so the log mtime
            # should be earlier (during the run). Accept anything within
            # 6h before finalize; reject logs newer than finalize (those
            # were overwritten by a later run that reused the path).
            delta = (started_dt - log_mtime).total_seconds()
            if delta < 0 or delta > 6 * 3600:
                continue
        parsed = parse_log(Path(log_path))
        for field in new_fields:
            if field in parsed and field not in phase:
                phase[field] = parsed[field]
                changed += 1

    if changed:
        rec_path.write_text(json.dumps(rec, indent=2))
        print(f"backfilled {changed} field(s) into {rec_path}")
    else:
        print(f"no fields to backfill in {rec_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
