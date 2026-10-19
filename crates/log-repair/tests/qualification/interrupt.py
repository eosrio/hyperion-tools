"""Abrupt process termination and syscall fault tests on disposable real-entry copies.

Linux strace injection is confined to the owned tracee. These tests do not emulate
hardware cache loss, a filesystem crash, or physical storage ENOSPC.
"""
import hashlib
import json
import os
import pathlib
import platform
import signal
import shutil
import subprocess

from .common import Pinned, event, publish, require, stamp
from .damage import groups, reference
from .process import run


def stopped_tracee(tracer, tool):
    """Identify a stopped direct child before opening a race-safe signal handle."""
    children = pathlib.Path(f"/proc/{tracer}/task/{tracer}/children")
    try:
        pids = children.read_text().split()
    except FileNotFoundError:
        return None
    for value in pids:
        pid = int(value)
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            continue
        try:
            root = pathlib.Path(f"/proc/{pid}")
            fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
            if fields[0] not in ("T", "t") or int(fields[1]) != tracer:
                continue
            if (root / "exe").resolve() != tool:
                continue
            return pid, fields[19], os.dup(fd)
        except FileNotFoundError:
            continue
        finally:
            os.close(fd)
    return None


def interruption(args):
    require(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"),
            "Linux pidfds are required for owned process termination")
    root = args.out.resolve()
    root.mkdir(mode=0o700)
    tool = Pinned(args.tool)
    fixture = Pinned(args.fixture)
    tracer = None
    try:
        executable = shutil.which("strace")
        require(executable is not None, "strace is required for syscall fault injection")
        tracer = Pinned(executable)
        version = subprocess.run([tracer.path, "--version"], capture_output=True, text=True,
                                 check=True, timeout=5).stdout
        require(fixture.before.st_size <= 16 << 20, "fixture exceeds bound")
        data = json.loads(fixture.read(0, fixture.before.st_size))
        require(data.get("schema") == 1 and 0 < len(data.get("records", [])) <= 10000,
                "unknown or empty real-entry fixture")
        require(all(a["number"] < b["number"] for a, b in
                    zip(data["records"], data["records"][1:])), "fixture is not ordered")
        records = groups(data["records"])[0]
        require(len(records) >= 3, "need three canonical real entries")
        gold = reference(root / "reference", records, data["chain_id"])["blocks"]
        damaged = gold["bytes"][:-4]
        prefix, prefix_index = gold["bytes"][:gold["positions"][-1]], gold["index"][:-8]
        plan = {
            "tool_sha256": tool.digest(tool.before.st_size),
            "strace_sha256": tracer.digest(tracer.before.st_size), "strace_version": version,
            "host": platform.uname()._asdict(),
            "fixture_sha256": fixture.digest(fixture.before.st_size),
            "original_log_sha256": hashlib.sha256(damaged).hexdigest(),
            "original_index_sha256": hashlib.sha256(gold["index"]).hexdigest(),
            "mutation": "Remove four bytes from the real final entry's position trailer.",
            "scope": "Process SIGKILL and injected syscall errors; no storage power-cut claim.",
        }
        cases = [(f"kill-truncate-{n}-{action}", "ftruncate", n, action, True)
                 for n in (1, 2) for action in ("resume", "rollback")]
        cases += [("enospc-before-intent", "fsync", 1, "refuse", False)]
        cases += [(f"enospc-truncate-{n}", "ftruncate", n, "resume", False) for n in (1, 2)]
        cases += [(f"enospc-rollback-write-{n}", "pwrite64", n, "rollback", False) for n in (1, 2)]
        plan["cases"] = [case[0] for case in cases]
        publish(root / "plan.json", plan)
        results = []
        for name, syscall, when, action, abrupt in cases:
            directory = root / name
            directory.mkdir(mode=0o700)
            log = directory / "blocks.log"
            index = log.with_suffix(".index")
            for path, raw in ((log, damaged), (index, gold["index"])):
                with path.open("xb") as output:
                    output.write(raw)
                    output.flush()
                    os.fsync(output.fileno())
            identities = [(p.stat().st_dev, p.stat().st_ino) for p in (log, index)]
            journal = directory / "journal"

            def command(argv, label, success=True):
                result = run([tool.path, "--json", *argv], directory / label,
                             interval=0.05, timeout=20, announce=False)
                require((result["returncode"] == 0) is success and not result["timed_out"],
                        f"unexpected {label} outcome in {name}")
                return result

            trim = ["trim-tail", "--log", log, "--journal", journal, "--node-stopped"]
            resume = ["resume-tail-trim", "--journal", journal, "--node-stopped"]
            operation = trim
            if syscall == "pwrite64":
                command(trim, "initial-trim")
                operation = [*resume, "--rollback"]
            trace = directory / "strace.txt"
            fault = f"{syscall}:{'signal=SIGSTOP' if abrupt else 'error=ENOSPC'}:when={when}"
            killed = []

            def observer(tracer):
                if killed or not trace.exists():
                    return
                require(trace.stat().st_size <= 1 << 20, "trace exceeds observation bound")
                if "SIGSTOP" not in trace.read_text():
                    return
                target = stopped_tracee(tracer, tool.path)
                if target is None:
                    return
                pid, start_ticks, fd = target
                try:
                    publish(directory / "termination.json", {
                        "pid": pid, "start_ticks": start_ticks, "tracer": tracer,
                        "signal": "SIGKILL", "syscall_stop": fault,
                        "log_bytes_at_stop": log.stat().st_size,
                        "index_bytes_at_stop": index.stat().st_size,
                    })
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                    killed.append(pid)
                finally:
                    os.close(fd)

            result = run([tracer.path, "-f", "-o", trace, "-e",
                          "trace=ftruncate,pwrite64,fsync,link,linkat", "-e", "inject=" + fault,
                          tool.path, "--json", *operation], directory / "interrupted",
                         interval=0.05, timeout=20, announce=False,
                         observer=observer if abrupt else None)
            require(result["returncode"] != 0 and not result["timed_out"],
                    f"fault was not observed in {name}")
            require(bool(killed) if abrupt else "INJECTED" in trace.read_text(),
                    f"missing fault evidence in {name}")
            if action == "refuse":
                require(log.read_bytes() == damaged and index.read_bytes() == gold["index"],
                        "pre-intent failure changed originals")
                require(not (journal / "intent.json").exists(), "intent published too early")
                command(resume, "refused-resume", success=False)
            else:
                command([*resume, *(["--rollback"] if action == "rollback" else [])], "resume")
                if action == "resume":
                    require(log.read_bytes() == prefix and index.read_bytes() == prefix_index,
                            "resumed trim differs from exact prefix")
                    command([*resume, "--rollback"], "rollback")
                require(log.read_bytes() == damaged and index.read_bytes() == gold["index"],
                        "rollback differs from damaged originals")
            require([(p.stat().st_dev, p.stat().st_ino) for p in (log, index)] == identities,
                    "target identities changed")
            publish(directory / "oracle.json", {"ok": True, "inodes_preserved": True,
                                                "original_bytes_restored": True, "fault": fault})
            results.append({"case": name, "ok": True, "actual_process_kill": abrupt})
            event("interruption-case", case=name, passed=True)
        require(tool.check(tool.before.st_size) == stamp(tool.before), "tool changed")
        require(tracer.check(tracer.before.st_size) == stamp(tracer.before), "strace changed")
        require(fixture.check(fixture.before.st_size) == stamp(fixture.before), "fixture changed")
        publish(root / "complete.json", {"ok": True, "results": results, **plan})
    except BaseException as error:
        publish(root / "failure.json", {"ok": False, "error": str(error)})
        raise
    finally:
        if tracer is not None:
            tracer.close()
        fixture.close()
        tool.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("interrupt", help="Linux real-entry process kills and syscall faults")
    parser.add_argument("--tool", type=pathlib.Path, required=True)
    parser.add_argument("--fixture", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.set_defaults(run=interruption)
