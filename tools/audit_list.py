#!/usr/bin/env python3
# Modified for Distill by Samuel Fajreldines, 2026.
"""Audit list.md against the two source documents and against the tree.

Checks, in order:
  1. every catalogue function id and every numbered parte-a.md opportunity has a row;
  2. every row marked `implemented` names a flag that exists in the harness source
     and a test that exists in the tree;
  3. every row marked `deferred` names one of the allowed reasons;
  4. the counts on both sides are printed.

Exit code 0 only when all four hold.
"""

import json
import pathlib
import re
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
LIST = REPO / "list.md"
CATALOG = pathlib.Path(
    "/Users/samuelfajreldines/dev/Distill/plan-new-llm/function-catalog.json"
)

ALLOWED_REASONS = {
    "forbidden by the catalogue",
    "35B-only",
    "host-bound (macOS app)",
    "needs a local model (embeddings/NLI)",
    "host/paid authority",
    "no harness seam",
}

# Where a flag key named in a row must appear, and where a test name must appear.
FLAG_SOURCES = [
    REPO / "crates/codegen/distill-workspace/src/jev/flags.rs",
    REPO / "crates/codegen/distill-shell/src/agent/config.rs",
]
SOURCE_FILES = list((REPO / "crates/codegen").rglob("*.rs"))
SOURCE_BLOB = "\n".join(
    path.read_text(errors="replace") for path in SOURCE_FILES if path.stat().st_size < 2_000_000
)


def rows():
    """The table rows of list.md, as `(section, cells)`.

    The section matters: catalogue rows in section 1 are the implemented ones,
    section 2 lists the deferrals with their reasons, section 3 is parte-a.md.
    """
    section = ""
    for line in LIST.read_text().splitlines():
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        if line.startswith("| ") and line.count("|") >= 4 and not line.startswith("| ---"):
            if "o que faz" in line or line.startswith("| status |"):
                continue  # a table header, not a row
            cells = [cell.strip() for cell in line.strip("|").split("|")]
            yield section, cells


def main():
    failures = []
    catalogue = json.loads(CATALOG.read_text())["functions"]
    ids_in_doc = {fn["id"] for fn in catalogue}
    ids_in_list = set()
    parte_rows = []
    implemented = 0
    deferred = 0
    external = 0
    planned = 0
    mapped = 0

    catalogue_implemented = 0
    catalogue_deferred = 0
    flag_sources = [source.read_text(errors="replace") for source in FLAG_SOURCES]

    for section, cells in rows():
        first = cells[0].strip("`")
        if first in ids_in_doc:
            ids_in_list.add(first)
            if section.startswith("1."):
                # Section 1 lists what is implemented: [id, what, seam, flag, test]
                catalogue_implemented += 1
                flag_key = cells[3].strip("` ")
                if flag_key and not any(flag_key in source for source in flag_sources):
                    failures.append(f"{first}: flag `{flag_key}` not found in the flags source")
                test = cells[4] if len(cells) > 4 else ""
                test_name = test.split("`")[1] if "`" in test else test
                for part in re.findall(r"[a-z_]{8,}", test_name):
                    if part not in SOURCE_BLOB:
                        failures.append(f"{first}: test `{part}` not found in the tree")
                        break
            elif section.startswith("2."):
                catalogue_deferred += 1
                reason = cells[2] if len(cells) > 2 else ""
                if reason not in ALLOWED_REASONS:
                    failures.append(f"{first}: reason `{reason}` is not allowed")
        elif section.startswith("4.") or section.startswith("5."):
            if section.startswith("4."):
                external += 1
            for cell in cells[3:6]:
                for name in re.findall(r"[a-z_]{8,}", cell.strip("` ")):
                    if name not in SOURCE_BLOB:
                        failures.append(f"{first}: `{name}` not found in the tree")
                        break
        elif re.fullmatch(r"\d+", first):
            parte_rows.append(cells)
            status = cells[4].lower() if len(cells) > 4 else ""
            if status == "implemented":
                implemented += 1
            elif status == "planned":
                planned += 1
            elif status == "mapped":
                mapped += 1
            elif status == "deferred":
                deferred += 1

    missing = ids_in_doc - ids_in_list
    for fid in sorted(missing):
        failures.append(f"{fid}: in the catalogue, missing from list.md")

    print(f"catalogue functions: {len(ids_in_doc)}")
    print(f"rows in list.md for them: {len(ids_in_list)}")
    print(f"catalogue rows: {catalogue_implemented} implemented, {catalogue_deferred} deferred")
    if catalogue_implemented + catalogue_deferred != len(ids_in_doc):
        failures.append(
            f"catalogue rows: {catalogue_implemented} + {catalogue_deferred} != {len(ids_in_doc)}"
        )
    print(f"parte-a.md opportunities: 27, rows in list.md: {len(parte_rows)}")
    print(f"statuses: implemented {implemented}, planned {planned}, mapped {mapped}, deferred {deferred}")
    print(f"external capabilities judged: {external}")
    if external == 0:
        failures.append("section 4 (external work) is missing")
    if len(parte_rows) != 27:
        failures.append(f"parte-a rows: {len(parte_rows)} instead of 27")
    if missing:
        print(f"missing ids: {sorted(missing)}")
    for failure in failures:
        print(f"FAIL {failure}")
    print("audit:", "ok" if not failures else f"{len(failures)} failure(s)")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
