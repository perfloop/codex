#!/usr/bin/env python3
"""Convert one Divan benchmark result into the common proof JSONL shape."""

import json
import re
import sys

BENCHMARK_NAME = "query_update_after_walk"
EXPECTED_SAMPLE_COUNT = "20"
DURATION_RE = re.compile(r"(?P<value>[0-9]+(?:\.[0-9]+)?)\s+(?P<unit>ns|µs|us|ms|s)")
NANOSECONDS_PER_UNIT = {
    "ns": 1,
    "µs": 1_000,
    "us": 1_000,
    "ms": 1_000_000,
    "s": 1_000_000_000,
}


def main() -> None:
    leaves = [line for line in sys.stdin if BENCHMARK_NAME in line]
    if len(leaves) != 1:
        raise SystemExit(f"expected one {BENCHMARK_NAME} result, got {len(leaves)}")

    columns = [column.strip() for column in leaves[0].split("│")]
    if len(columns) != 6:
        raise SystemExit(f"unexpected Divan result shape: {leaves[0]!r}")
    if columns[4] != EXPECTED_SAMPLE_COUNT or columns[5] != EXPECTED_SAMPLE_COUNT:
        raise SystemExit(
            "expected Divan to report 20 one-iteration samples, got "
            f"samples={columns[4]!r}, iters={columns[5]!r}"
        )

    median = DURATION_RE.fullmatch(columns[2])
    if median is None:
        raise SystemExit(f"unexpected Divan median: {columns[2]!r}")

    value = round(float(median["value"]) * NANOSECONDS_PER_UNIT[median["unit"]])
    print(json.dumps({"metric": "snapshot_delivery_ns", "value": value}))


if __name__ == "__main__":
    main()
