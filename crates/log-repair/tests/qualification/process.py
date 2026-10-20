"""Linux per-child measurements and immutable terminal evidence."""
import json
import os
import pathlib
import re
import signal
import shutil
import subprocess
import time

from .common import event, publish, require


def sample(pid):
    root = pathlib.Path("/proc") / str(pid)
    values = {}

    def read_part(name, read):
        try:
            values[name] = read()
        except (FileNotFoundError, ProcessLookupError, PermissionError) as error:
            values.setdefault("unavailable", {})[name] = type(error).__name__

    def memory():
        found = {}
        for line in (root / "status").read_text().splitlines():
            key, _, value = line.partition(":")
            if key in ("VmRSS", "VmHWM", "VmData", "VmSwap"):
                found[key] = int(value.split()[0])
        return found

    def counters():
        fields = (root / "stat").read_text().rsplit(") ", 1)[1].split()
        return {"state": fields[0], "minor_faults": int(fields[7]), "major_faults": int(fields[9]),
                "user_ticks": int(fields[11]), "system_ticks": int(fields[12]),
                "block_io_delay_ticks": int(fields[39])}

    # Availability differs by kernel. One missing metric must not hide every other metric.
    read_part("exe", lambda: str((root / "exe").resolve(strict=True)))
    read_part("io", lambda: {key: int(value) for key, value in
                            (line.split(":", 1) for line in (root / "io").read_text().splitlines())})
    read_part("memory_kib", memory)
    read_part("stat", counters)
    return values


def run(argv, out, metadata=None, timeout=3600, interval=0.5, announce=True, observer=None):
    """Measure exactly one child; wait4 gives that child's RSS/CPU, not prior maxima."""
    require(hasattr(os, "wait4"), "process measurement requires Linux wait4")
    require(0 < timeout <= 86400 and 0.05 <= interval <= 5, "invalid measurement bounds")
    out.mkdir(mode=0o700)
    argv = list(map(str, argv))
    if announce:
        event("command-start", executable=argv[0], evidence=str(out))
    started, timed_out, child, usage = time.monotonic(), False, None, None
    last_sample = {}
    io_max, sampled_hwm = {}, 0
    expected_exe = str(pathlib.Path(shutil.which(argv[0]) or argv[0]).resolve())
    try:
        with (out / "stdout.json").open("xb") as stdout, \
                (out / "stderr.txt").open("xb") as stderr, \
                (out / "samples.jsonl").open("x") as samples, \
                (out / "phases.jsonl").open("x") as phases:
            child = subprocess.Popen(argv, stdout=stdout, stderr=stderr, start_new_session=True)
            publish(out / "started.json", {"argv": argv, "pid": child.pid,
                                            "created_unix": time.time(), **(metadata or {})})
            with (out / "stderr.txt").open("rb") as phase_input:
                pending = b""

                def harvest():
                    nonlocal pending
                    pending += phase_input.read(1 << 20)
                    lines = pending.split(b"\n")
                    pending = lines.pop()
                    for line in lines:
                        text = line.decode("utf-8", "replace")
                        match = re.match(r"recover-tail \[(.+?)\]:", text)
                        if match or text.startswith("trim-tail:"):
                            phases.write(json.dumps({"observed_seconds": time.monotonic() - started,
                                                     "phase": match[1] if match else "trim-journal",
                                                     "line": text}) + "\n")
                    phases.flush()

                while True:
                    observation = {"seconds": time.monotonic() - started, **sample(child.pid)}
                    if observation.get("io"):
                        for key, value in observation["io"].items():
                            io_max[key] = max(io_max.get(key, 0), value)
                    if observation.get("memory_kib") or observation.get("io"):
                        last_sample = observation
                    if observation.get("exe") == expected_exe:
                        sampled_hwm = max(sampled_hwm, observation.get("memory_kib", {}).get("VmHWM", 0))
                    samples.write(json.dumps(observation) + "\n")
                    samples.flush()
                    harvest()
                    if observer is not None:
                        observer(child.pid)
                    pid, status, used = os.wait4(child.pid, os.WNOHANG)
                    if pid:
                        usage = used
                        child.returncode = os.waitstatus_to_exitcode(status)
                        break
                    if time.monotonic() - started >= timeout:
                        timed_out = True
                        os.killpg(child.pid, signal.SIGKILL)
                        _, status, usage = os.wait4(child.pid, 0)
                        child.returncode = os.waitstatus_to_exitcode(status)
                        break
                    time.sleep(interval)
                elapsed = time.monotonic() - started
                harvest()
    finally:
        if child is not None and child.returncode is None:
            # Never leave an unobserved benchmark or destructive test running on a harness error.
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, _ = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
    require(usage is not None, "child usage was not collected")
    result = {
        "returncode": child.returncode, "timed_out": timed_out, "elapsed_seconds": elapsed,
        "user_seconds": usage.ru_utime, "system_seconds": usage.ru_stime,
        "max_rss_kib": usage.ru_maxrss, "minor_faults": usage.ru_minflt,
        "sampled_program_peak_rss_kib": sampled_hwm or None,
        "major_faults": usage.ru_majflt, "input_block_ops": usage.ru_inblock,
        "output_block_ops": usage.ru_oublock, "last_sample": last_sample, "sampled_io_max": io_max,
        "sampling_interval_seconds": interval,
        "phase_timing": "Polling observations, not exact internal stage boundaries.",
        "io_counters": "Last /proc sample is a lower bound; wait4 CPU/RSS belongs to this child.",
        "rss_caveat": "wait4 RSS may include fork-before-exec launcher memory; sampled VmHWM is exe-matched.",
    }
    publish(out / "result.json", result)
    if announce:
        event("command-complete", evidence=str(out), returncode=child.returncode,
              elapsed_seconds=elapsed, timed_out=timed_out)
    return result
