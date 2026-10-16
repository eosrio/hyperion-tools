#!/usr/bin/env python3
"""Optional Linux real-EIO test. Needs root, util-linux, e2fsprogs, and dmsetup.

Copies an intact blocks.log onto a small ext4 image, then maps the device sectors of one
of its 4 KiB pages to the device-mapper `error` target, so the kernel itself returns EIO
for that page through ext4 and the page cache. Checks that:

- a whole-file scan and a --damaged-block scan find exactly that page, with the record
  size taken from the filesystem (st_blksize);
- prepare builds a plan from the intact copy (--blocks-log) with the extent unreadable;
- apply fails safely: the error target also rejects writes, so the expected result is a
  failed fsync with the intent journal kept and no receipt.

A write that actually heals the damage needs ZFS or dm-dust and is not covered here; the
Rust tests cover that path with injected faults. Creates only files under --out and one
temporary loop device and device-mapper target, removed on exit.
"""
import argparse
import json
import os
import pathlib
import re
import shutil
import subprocess

DM_NAME = "antelope-log-repair-eio"


def sh(*argv, **kw):
    return subprocess.run([str(a) for a in argv], check=True, capture_output=True, text=True,
                          **kw).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tool", required=True, type=pathlib.Path)
    parser.add_argument("--blocks-log", required=True, type=pathlib.Path,
                        help="intact blocks.log with blocks.index alongside (not modified)")
    parser.add_argument("--out", required=True, type=pathlib.Path)
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise SystemExit("run as root: loop devices, mkfs, mount, and dmsetup need it")
    args.out.mkdir()  # refuse existing paths
    root = args.out.resolve()
    tool = str(args.tool.resolve())
    intact = args.blocks_log.resolve()
    size = intact.stat().st_size
    image, mnt = root / "fs.img", root / "mnt"
    mnt.mkdir()
    with open(image, "wb") as f:
        f.truncate(max(64 << 20, 2 * size + (32 << 20)))
    loop = sh("losetup", "--find", "--show", image)
    mapped = False
    mounted = False

    def tool_json(*argv, name, expect_ok=True):
        result = subprocess.run([tool, "--json", *map(str, argv)], capture_output=True,
                                text=True, timeout=120)
        (root / name).write_text(result.stdout + "\nSTDERR:\n" + result.stderr)
        report = json.loads(result.stdout)
        if expect_ok and result.returncode:
            raise RuntimeError(f"tool failed; see {name}: {report}")
        if not expect_ok and not result.returncode:
            raise RuntimeError(f"tool unexpectedly succeeded; see {name}")
        return report

    try:
        sh("mkfs.ext4", "-q", "-b", "4096", loop)
        sh("mount", loop, mnt)
        mounted = True
        (mnt / "blocks").mkdir()
        target = mnt / "blocks" / "blocks.log"
        shutil.copyfile(intact, target)
        shutil.copyfile(intact.with_suffix(".index"), target.with_suffix(".index"))
        sh("sync")

        # Damage the page holding the start of the middle block's entry.
        index = intact.with_suffix(".index").read_bytes()
        first = int.from_bytes(intact.read_bytes()[4:8], "little")
        middle = len(index) // 8 // 2
        block = first + middle
        page = int.from_bytes(index[middle * 8:middle * 8 + 8], "little") // 4096
        assert 0 < page < size // 4096 - 1, "log too small: need pages around the damage"
        extents = sh("filefrag", "-v", "-b4096", target)
        physical = None
        for m in re.finditer(r"^\s*\d+:\s+(\d+)\.\.\s*(\d+):\s+(\d+)\.\.", extents, re.M):
            start, end, phys = map(int, m.groups())
            if start <= page <= end:
                physical = phys + page - start
        assert physical is not None, f"page {page} not found in:\n{extents}"
        sh("umount", mnt)
        mounted = False

        sectors = int(sh("blockdev", "--getsz", loop))
        bad = physical * 8
        table = (f"0 {bad} linear {loop} 0\n"
                 f"{bad} 8 error\n"
                 f"{bad + 8} {sectors - bad - 8} linear {loop} {bad + 8}\n")
        subprocess.run(["dmsetup", "create", DM_NAME], input=table, text=True, check=True)
        mapped = True
        sh("mount", f"/dev/mapper/{DM_NAME}", mnt)
        mounted = True
        pathlib.Path("/proc/sys/vm/drop_caches").write_text("3\n")

        # Keep the human-readable report as evidence too.
        human = subprocess.run([tool, "scan", "--log", str(target)], capture_output=True,
                               text=True, timeout=120)
        (root / "scan-whole.txt").write_text(human.stdout + human.stderr)
        want = [{"offset": page * 4096, "len": 4096, "records": [page, page]}]
        whole = tool_json("scan", "--log", target, name="scan-whole.json")
        assert whole["record_size"] == 4096, whole["record_size"]
        assert whole["damaged"] is True
        assert [r["extents"] for r in whole["regions"]] == [want], whole["regions"]
        assert whole["regions"][0]["repairable"] is True
        narrow = tool_json("scan", "--log", target, "--damaged-block", block,
                           name="scan-block.json")
        assert [r["extents"] for r in narrow["regions"]] == [want]

        prepared = tool_json("prepare", "--log", target, "--damaged-block", block,
                             "--blocks-log", intact, "--out", root / "prepare",
                             name="prepare.json")
        assert prepared["build"]["extent_states"] == ["unreadable"]
        human = subprocess.run([tool, "prepare", "--log", str(target), "--damaged-block",
                                str(block), "--blocks-log", str(intact), "--out",
                                str(root / "prepare-human")], capture_output=True, text=True,
                               timeout=120)
        (root / "prepare.txt").write_text(human.stdout + human.stderr)
        bundle = root / "prepare" / "bundle"
        failed = tool_json("apply", "--bundle", bundle, "--approve",
                           prepared["build"]["plan_sha256"], "--node-stopped",
                           name="apply.json", expect_ok=False)
        assert "write bytes" in failed["error"], failed["error"]
        assert (bundle / "apply-intent.json").exists()
        assert not (bundle / "receipt.json").exists()

        # The failed write leaves the replacement in the page cache: reads succeed and the
        # file looks repaired although the device still fails. Only after the cache is
        # dropped does the damage show again. Operators must not trust a scan made right
        # after a failed apply.
        cached = tool_json("scan", "--log", target, name="scan-after-failed-apply.json")
        pathlib.Path("/proc/sys/vm/drop_caches").write_text("3\n")
        uncached = tool_json("scan", "--log", target, name="scan-after-drop-caches.json")
        assert uncached["damaged"] is True, "damage must reappear once the cache is dropped"
        cache_masked = cached["damaged"] is False
        assert intact.stat().st_size == size  # source untouched
    finally:
        if mounted:
            subprocess.run(["umount", mnt], check=False)
        if mapped:
            subprocess.run(["dmsetup", "remove", DM_NAME], check=False)
        subprocess.run(["losetup", "-d", loop], check=False)

    print(json.dumps({"ok": True, "evidence": str(root), "checks": [
        "kernel EIO through ext4 found by whole-file and --damaged-block scans",
        "record size taken from st_blksize (4096 on ext4)",
        "prepare from another node's blocks.log marks the extent unreadable",
        "apply against a write-failing device keeps the journal and writes no receipt",
        f"after the failed write, the page cache {'masked' if cache_masked else 'did not mask'} "
        "the damage until caches were dropped"]}, indent=2))


if __name__ == "__main__":
    main()
