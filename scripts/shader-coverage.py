#!/usr/bin/env python3
"""Per-stage and per-pipeline translation coverage of the title's shader corpus.

    scripts/shader-coverage.py <corpus-dir>

The corpus directory holds `<hash>.<stage>.dxbc` blobs and a `pipelines.txt` whose
lines name the shaders of one pipeline (`pipeline 12 vertex=<hash> pixel=<hash>`).
Prints how many blobs of each stage translate and how many pipelines become fully
translatable; writes the failing hashes to `/tmp/cov-failed.json`.
"""
import collections
import json
import os
import subprocess
import sys

STAGES = ("compute", "pixel", "vertex", "mesh")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 64
    root = sys.argv[1]
    tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "nativ64-shader")
    files = collections.defaultdict(list)
    for f in os.listdir(root):
        if f.endswith(".dxbc"):
            files[f.rsplit(".", 2)[-2]].append(f)
    failed = {}
    for stage in STAGES:
        items = sorted(files.get(stage, []))
        bad = set()
        for f in items:
            r = subprocess.run(
                [tool, "--spirv", os.path.join(root, f), "-o", "/tmp/coverage.spv"],
                capture_output=True,
            )
            if r.returncode != 0:
                bad.add(f.split(".")[0])
        failed[stage] = bad
        print(f"{stage:8} {len(items) - len(bad):5} of {len(items):5} translate", flush=True)
    total = sum(len(files.get(s, [])) for s in STAGES)
    done = sum(len(files.get(s, [])) - len(failed[s]) for s in STAGES)
    print(f"{'all':8} {done:5} of {total:5} ({100.0 * done / max(total, 1):.1f}%)")
    with open("/tmp/cov-failed.json", "w") as f:
        json.dump({k: sorted(v) for k, v in failed.items()}, f)

    path = os.path.join(root, "pipelines.txt")
    if not os.path.exists(path):
        return 0
    kinds = collections.Counter()
    covered = collections.Counter()
    total_pipes = 0
    for line in open(path):
        total_pipes += 1
        kind = "mesh" if "mesh=" in line else ("compute" if "compute=" in line else "graphics")
        kinds[kind] += 1
        ok = True
        for part in line.split():
            if "=" not in part:
                continue
            stage, h = part.split("=", 1)
            if stage in STAGES and h in failed.get(stage, ()):
                ok = False
                break
        if ok:
            covered[kind] += 1
    all_cov = sum(covered.values())
    print(f"pipelines {all_cov} of {total_pipes} fully translatable ({100.0 * all_cov / max(total_pipes, 1):.1f}%)")
    for kind in kinds:
        print(f"  {kind:8} {covered[kind]} of {kinds[kind]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
