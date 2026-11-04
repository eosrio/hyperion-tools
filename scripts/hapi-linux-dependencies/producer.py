"""Tools #82 CI dependency-only producer. Import and offline verification never install.

The GitHub-only entry point performs normal locked npm installs. The tar is an
input closure, never a core/backend build or operational admission receipt.
"""
import argparse
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import posixpath
import re
import shutil
import stat
import subprocess
import tarfile
import threading
import time

ROOTS = ("node_modules", "packages/backend-mongodb/node_modules")
SCAFFOLD = ("packages", "packages/backend-mongodb")
RECEIPT = "candidate82-dependencies.receipt.json"
ARCHIVE = "candidate82-dependencies.tar.gz"
ARTIFACT = "hapi-dependencies-21ef67d84ad9-linux-x64-node26.4.0"
WORKFLOW = ".github/workflows/hapi-linux-dependencies.yml"
SDK = "dd85ee590dcd1e359509c6c41ee99b541706b1f6"
WIRE = "18d235e34a9b7ab8c3e9f9a322874444d90eed8a"
PREPARED_SDK = "58d68c5cea81490bbb1c9a7736d633d465bab3ef"
SOURCE_LINK = "node_modules/@eosrio/hyperion-backend-mongodb"
SOURCE_TARGET = "../../packages/backend-mongodb"
MAX_ENTRIES, MAX_PAYLOAD, MAX_FILE = 30000, 2 * 1024**3, 256 * 1024**2
MAX_ARCHIVE, MAX_RECEIPT, MAX_STREAM = 1024**3, 16 * 1024**2, 2 * 1024**2
FORBIDDEN = {".git", ".git-credentials", ".npmrc", ".netrc", ".ssh", ".aws"}
PROFILE = json.loads(Path(__file__).with_name("profile.json").read_bytes())

# Only fixed, source-controlled cases are accepted. Incomplete cases refuse before installs.
CASES = {
    "candidate82": {"profile": PROFILE, "receipt": RECEIPT, "archive": ARCHIVE, "artifact": ARTIFACT,
                    "workflow": WORKFLOW, "kind": "hyperion-candidate82-linux-dependencies", "preparedSdk": PREPARED_SDK},
    "current88": {"profile": json.loads(Path(__file__).with_name("profile-current88.json").read_bytes()), "receipt": "current88-dependencies.receipt.json",
                  "archive": "current88-dependencies.tar.gz", "artifact": "hapi-current88-linux-x64-node26.4.0",
                  "workflow": ".github/workflows/hapi-linux-current-dependencies.yml",
                  "kind": "hyperion-current88-linux-dependencies", "preparedSdk": "58d68c5cea81490bbb1c9a7736d633d465bab3ef"},
    "current103": {"profile": json.loads(Path(__file__).with_name("profile-current103.json").read_bytes()), "receipt": "current103-dependencies.receipt.json",
                   "archive": "current103-dependencies.tar.gz", "artifact": "hapi-current103-linux-x64-node26.4.0",
                   "workflow": ".github/workflows/hapi-linux-current103-dependencies.yml",
                   "kind": "hyperion-current103-linux-dependencies", "preparedSdk": "58d68c5cea81490bbb1c9a7736d633d465bab3ef"},
}


def select_case(name):
    need(name in CASES, "Unknown fixed dependency case")
    case = CASES[name]
    profile = case["profile"]
    need(type(profile) is dict and type(case["preparedSdk"]) is str
         and re.fullmatch("[a-f0-9]{40}", case["preparedSdk"]), "Dependency case source pins unresolved")
    need(all(type(profile.get(key)) is str and re.fullmatch("[a-f0-9]{40}", profile[key])
             for key in ("productHead", "productTree"))
         and type(profile.get("sourceFiles")) is int and profile["sourceFiles"] > 0
         and type(profile.get("sourceInventorySha256")) is str
         and re.fullmatch("[a-f0-9]{64}", profile["sourceInventorySha256"]), "Incomplete dependency case source pins")
    locks = profile.get("manifestLocks")
    need(type(locks) is dict and set(locks) == {"package.json", "package-lock.json",
         "packages/backend-mongodb/package.json", "packages/backend-mongodb/package-lock.json"}
         and all(type(pin) is dict and set(pin) == {"bytes", "sha256"} and type(pin["bytes"]) is int
                 and pin["bytes"] > 0 and type(pin["sha256"]) is str and re.fullmatch("[a-f0-9]{64}", pin["sha256"])
                 for pin in locks.values()), "Incomplete dependency case lock pins")
    sources = profile.get("dependencySources")
    need(type(sources) is dict and set(sources) == {"rootSdk", "adapterSdk", "wire", "nodeAbieos"}
         and all(type(pin) is dict and set(pin) == {"packagePath", "name", "version", "resolved", "integrity"}
                 and all(type(value) is str and value for value in pin.values()) for pin in sources.values())
         and all(re.fullmatch("[a-f0-9]{40}", sources[key]["resolved"].split("#")[-1])
                 for key in ("rootSdk", "adapterSdk", "wire")), "Incomplete dependency case provenance pins")
    return case


def need(value, message):
    if not value:
        raise ValueError(message)


def encoded(value):
    return (json.dumps(value, indent=2) + "\n").encode()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_pin(path):
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, f"Ordinary single-link file required: {path}")
    need(info.st_size <= MAX_FILE, "Dependency file exceeds bound")
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        for data in iter(lambda: stream.read(1024**2), b""):
            checksum.update(data)
    return {"bytes": info.st_size, "sha256": checksum.hexdigest()}


def safe_path(name):
    need(type(name) is str and 0 < len(name.encode()) <= 4096 and not name.startswith("/"), "Unsafe member path")
    need(not any(c in name for c in ("\\", ":", "\0")) and all(ord(c) >= 32 and ord(c) != 127 for c in name), "Unsafe member text")
    parts = name.split("/")
    need(all(part not in ("", ".", "..") for part in parts), "Noncanonical member path")
    need(not FORBIDDEN.intersection(parts), "Credential/Git payload refused")
    return name


def dependency_path(name):
    safe_path(name)
    return any(name == root or name.startswith(root + "/") for root in ROOTS)


def link_target(name, target):
    need(type(target) is str and 0 < len(target.encode()) <= 4096 and not target.startswith("/"), "Absolute/empty link refused")
    need(not any(c in target for c in ("\\", ":", "\0")) and all(ord(c) >= 32 and ord(c) != 127 for c in target), "Unsafe link text")
    result = posixpath.normpath(posixpath.join(posixpath.dirname(name), target))
    safe_path(result)
    return result


def validate_inventory(rows):
    need(type(rows) is list and 0 < len(rows) <= MAX_ENTRIES, "Dependency inventory entry bound")
    by, payload = {}, 0
    for row in rows:
        need(type(row) is dict, "Typed dependency entry required")
        name, kind = row["path"], row["type"]
        need(dependency_path(name) and name not in by, "Unknown/duplicate dependency payload")
        if kind == "file":
            need(set(row) == {"path", "type", "bytes", "sha256", "mode"}, "Exact file fields required")
            need(type(row["bytes"]) is int and 0 <= row["bytes"] <= MAX_FILE, "Dependency file byte bound")
            need(type(row["sha256"]) is str and re.fullmatch("[a-f0-9]{64}", row["sha256"]), "File digest required")
            need(type(row["mode"]) is int and 0 <= row["mode"] <= 0o777 and not row["mode"] & 0o022, "Unsafe file mode")
            payload += row["bytes"]
        elif kind == "directory":
            need(set(row) == {"path", "type"}, "Exact directory fields required")
        elif kind == "symlink":
            need(set(row) == {"path", "type", "target"}, "Exact symlink fields required")
            link_target(name, row["target"])
        else:
            raise ValueError("Hardlinks/devices/unknown dependency type refused")
        by[name] = row
    need(payload <= MAX_PAYLOAD, "Dependency payload exceeds 2 GiB")
    for root in ROOTS:
        need(root in by and by[root]["type"] == "directory", "Both complete dependency roots required")
    for name in by:
        for parent in PurePosixPath(name).parents:
            parent = parent.as_posix()
            if parent != "." and dependency_path(parent):
                need(parent in by and by[parent]["type"] == "directory", "Missing/non-directory dependency ancestor")
    for name, row in by.items():
        if row["type"] != "symlink":
            continue
        target, visited = name, set()
        while True:
            parts = target.split("/")
            prefix = next(("/".join(parts[:index]) for index in range(1, len(parts) + 1)
                           if by.get("/".join(parts[:index]), {}).get("type") == "symlink"), None)
            if prefix is None:
                break
            need(prefix not in visited, "Cyclic dependency link")
            visited.add(prefix)
            suffix = target[len(prefix):].lstrip("/")
            target = link_target(prefix, by[prefix]["target"])
            if suffix:
                target = safe_path(posixpath.normpath(posixpath.join(target, suffix)))
        if name == SOURCE_LINK:
            need(row["target"] == SOURCE_TARGET and target == "packages/backend-mongodb", "Workspace source link differs")
        else:
            need(target in by and dependency_path(target), "Dangling or non-dependency link refused")
    need(by.get(SOURCE_LINK) == {"path": SOURCE_LINK, "type": "symlink", "target": SOURCE_TARGET}, "Exact declared backend source link required")
    return by


def dependency_inventory(repo):
    rows = []

    def visit(path):
        name = path.relative_to(repo).as_posix()
        need(dependency_path(name), "Dependency whitelist violation")
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            row = {"path": name, "type": "symlink", "target": os.readlink(path)}
        elif stat.S_ISDIR(info.st_mode):
            need(not stat.S_IMODE(info.st_mode) & 0o022, "Writable dependency directory")
            rows.append({"path": name, "type": "directory"})
            need(len(rows) <= MAX_ENTRIES, "Dependency inventory entry bound")
            for child in sorted(path.iterdir()):
                visit(child)
            return
        else:
            row = {"path": name, "type": "file", **file_pin(path), "mode": stat.S_IMODE(info.st_mode)}
        rows.append(row)
        need(len(rows) <= MAX_ENTRIES, "Dependency inventory entry bound")

    for root in ROOTS:
        path = repo / root
        need(path.is_dir() and not path.is_symlink(), "Ordinary dependency root required")
        visit(path)
    rows.sort(key=lambda row: row["path"])
    validate_inventory(rows)
    for row in rows:
        if row["type"] == "symlink":
            resolved = (repo / row["path"]).resolve(strict=True)
            need(resolved.is_relative_to(repo.resolve()), "Actual dependency link escapes product")
    return rows


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.PIPE)


def source_inventory(repo):
    rows = []
    for entry in git(repo, "ls-tree", "-r", "-z", "HEAD").split(b"\0"):
        if not entry:
            continue
        metadata, raw_name = entry.split(b"\t", 1)
        mode, kind, oid = metadata.decode().split()
        name = safe_path(raw_name.decode())
        need(kind == "blob" and mode in ("100644", "100755"), "Ordinary tracked source files only")
        for parent in (repo / name).parents:
            if parent == repo:
                break
            need(parent.is_relative_to(repo) and stat.S_ISDIR(parent.lstat().st_mode), "Source directory link/type drift")
        pin = file_pin(repo / name)
        data = (repo / name).read_bytes()
        need(hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest() == oid, "Tracked source drift: " + name)
        rows.append({"path": name, **pin, "gitBlob": oid, "mode": mode})
    return sorted(rows, key=lambda row: row["path"])


def verify_source(repo, profile=PROFILE):
    need(Path(git(repo, "rev-parse", "--show-toplevel").decode().strip()).resolve() == repo.resolve(), "Exact source checkout root required")
    need(git(repo, "rev-parse", "HEAD").decode().strip() == profile["productHead"], "Product source head differs")
    need(git(repo, "rev-parse", "HEAD^{tree}").decode().strip() == profile["productTree"], "Product source tree differs")
    rows = source_inventory(repo)
    need(len(rows) == profile["sourceFiles"] and digest(encoded(rows)) == profile["sourceInventorySha256"], "Complete product source inventory differs")
    need({name: file_pin(repo / name) for name in profile["manifestLocks"]} == profile["manifestLocks"], "Manifest/lock drift")
    return rows


def write_archive(repo, rows, output):
    validate_inventory(rows)
    with output.open("xb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                all_rows = [{"path": name, "type": "directory"} for name in SCAFFOLD] + rows
                for row in sorted(all_rows, key=lambda value: (len(PurePosixPath(value["path"]).parts), value["path"])):
                    member = tarfile.TarInfo(row["path"])
                    member.uid = member.gid = member.mtime = 0
                    if row["type"] == "directory":
                        member.type, member.mode = tarfile.DIRTYPE, 0o755
                        archive.addfile(member)
                    elif row["type"] == "symlink":
                        member.type, member.mode, member.linkname = tarfile.SYMTYPE, 0o777, row["target"]
                        archive.addfile(member)
                    else:
                        path = repo / row["path"]
                        need(file_pin(path) == {key: row[key] for key in ("bytes", "sha256")}, "Dependency changed before archiving")
                        need(stat.S_IMODE(path.lstat().st_mode) == row["mode"], "Dependency mode drift")
                        member.size, member.mode = row["bytes"], row["mode"]
                        with path.open("rb") as stream:
                            archive.addfile(member, stream)
    need(output.stat().st_size <= MAX_ARCHIVE, "Dependency archive exceeds 1 GiB")
    verify_archive(output, rows)


def verify_archive(path, rows):
    by, seen = validate_inventory(rows), set()
    need(path.stat().st_size <= MAX_ARCHIVE, "Dependency archive exceeds 1 GiB")
    class BoundedRead:
        def __init__(self, stream): self.stream = stream
        def read(self, size=-1):
            need(0 <= size <= 1024**2, "Unbounded tar metadata read refused")
            return self.stream.read(size)
        def __getattr__(self, name): return getattr(self.stream, name)

    with gzip.open(path, "rb") as raw, tarfile.open(fileobj=BoundedRead(raw), mode="r:") as archive:
        for member in archive:
            name = member.name[:-1] if member.isdir() and member.name.endswith("/") else member.name
            safe_path(name)
            need(name not in seen and len(seen) < MAX_ENTRIES + 2, "Duplicate/excess archive members")
            seen.add(name)
            need(set(member.pax_headers) <= {"path", "linkpath"} and not member.sparse, "Unexpected archive metadata")
            if name in SCAFFOLD:
                need(member.isdir() and member.mode == 0o755 and member.size == 0, "Directory scaffolding only")
                continue
            need(name in by, "Uninventoried/core/backend-dist archive payload")
            row = by[name]
            if row["type"] == "directory":
                need(member.isdir() and member.mode == 0o755 and member.size == 0, "Dependency directory type/mode drift")
            elif row["type"] == "symlink":
                need(member.issym() and member.mode == 0o777 and member.size == 0 and member.linkname == row["target"], "Dependency link drift")
            else:
                need(member.type in (tarfile.REGTYPE, tarfile.AREGTYPE) and member.size == row["bytes"] and member.mode == row["mode"], "File type/size/mode drift")
                checksum, count = hashlib.sha256(), 0
                with archive.extractfile(member) as stream:
                    for data in iter(lambda: stream.read(1024**2), b""):
                        checksum.update(data)
                        count += len(data)
                need(count == row["bytes"] and checksum.hexdigest() == row["sha256"], "Archive file content drift")
        # Do not ignore a second tar/gzip member, hidden payload or oversized zero tail.
        raw.seek(archive.offset)
        tail = raw.read(2 * tarfile.RECORDSIZE + 1)
        need(1024 <= len(tail) <= 2 * tarfile.RECORDSIZE and not any(tail), "Noncanonical archive tail/payload")
    need(seen - set(SCAFFOLD) == set(by), "Complete archive/inventory equality required")


def source_links(profile=PROFILE):
    return [{"path": SOURCE_LINK, "target": SOURCE_TARGET, "resolvedPath": "packages/backend-mongodb",
             "sourceInventorySha256": profile["sourceInventorySha256"]}]


def dependency_provenance(repo, rows, profile=PROFILE, prepared_sdk=PREPARED_SDK):
    by, result = validate_inventory(rows), {}
    root_lock = json.loads((repo / "package-lock.json").read_bytes())["packages"]
    adapter_lock = json.loads((repo / "packages/backend-mongodb/package-lock.json").read_bytes())["packages"]
    for key, expected in profile["dependencySources"].items():
        package_path = expected["packagePath"]
        locked = adapter_lock["node_modules/@eosrio/hyperion-sdk"] if key == "adapterSdk" else root_lock[package_path]
        need(all(locked.get(field) == expected[field] for field in ("version", "resolved", "integrity")), "Dependency committed-lock provenance differs")
        package_file = repo / package_path / "package.json"
        package = json.loads(package_file.read_bytes())
        need(package["name"] == expected["name"] and package["version"] == expected["version"], "Installed package identity differs")
        value = {**expected, "packageJson": file_pin(package_file)}
        if key != "nodeAbieos":
            value["sourceRevision"] = expected["resolved"].split("#")[-1]
        if key == "wire":
            need(value["sourceRevision"] == profile["dependencySources"]["wire"]["resolved"].split("#")[-1], "Wire pin differs")
            value["preparedSdkRevision"] = prepared_sdk
            value["artifactReceipts"] = []
            for filename in ("protocol-node-artifact.json", "protocol-rust-artifact.json", "protocol-source-receipt.json", "protocol-npm-lock.json"):
                name = package_path + "/dist/" + filename
                value["artifactReceipts"].append({"path": name, **file_pin(repo / name)})
                receipt = json.loads((repo / name).read_bytes())
                if filename == "protocol-npm-lock.json":
                    need(receipt["packages"]["node_modules/@eosrio/hyperion-sdk"]["resolved"].endswith("#" + prepared_sdk), "Wire prepared SDK differs")
                    continue
                need(receipt["sourceRevision"] == value["sourceRevision"], "Original Wire receipt source revision differs")
                if "sourceModified" in receipt:
                    need(receipt["sourceModified"] is False, "Modified Wire source refused")
                for field in ("files", "sourceFiles", "outputFiles"):
                    for entry in receipt.get(field, []):
                        safe_path(entry["path"])
                        shipped = package_path + "/" + entry["path"]
                        if entry["path"] == "package-lock.json":
                            shipped = package_path + "/dist/protocol-npm-lock.json"
                        if shipped in by:
                            need(by[shipped]["type"] == "file" and by[shipped]["sha256"] == entry["sha256"], "Shipped Wire member hash drift")
                        else:
                            need(field == "sourceFiles", "Missing shipped Wire output/artifact")
        if key == "nodeAbieos":
            need(all(row["type"] == "file" for row in rows if row["path"].startswith(package_path + "/") and row["path"].endswith(".node")), "Native binaries must be ordinary inventoried files")
            value["nativeFiles"] = [{"path": row["path"], **{field: row[field] for field in ("bytes", "sha256", "mode")}}
                for row in rows if row["path"].startswith(package_path + "/") and row["path"].endswith(".node") and row["type"] == "file"]
            need(value["nativeFiles"], "Complete native node-abieos binaries required")
        result[key] = value
    return result


def run_install(repo, node, npm, scope):
    argv = [str(node), str(npm), "ci", "--allow-git=all", "--ignore-scripts=false"]
    if scope == "adapter":
        argv += ["--prefix", "packages/backend-mongodb"]
    print(json.dumps({"scope": scope, "argv": argv, "cwd": str(repo)}), flush=True)
    begun = time.monotonic()
    child = subprocess.Popen(argv, cwd=repo, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    streams, errors, eof = {}, [], set()

    def drain(name, pipe):
        checksum, count, captured = hashlib.sha256(), 0, bytearray()
        try:
            with pipe:
                for chunk in iter(lambda: pipe.read(65536), b""):
                    checksum.update(chunk)
                    count += len(chunk)
                    if len(captured) <= MAX_STREAM:
                        captured.extend(chunk[:MAX_STREAM + 1 - len(captured)])
                eof.add(name)
            streams[name] = {"bytes": count, "sha256": checksum.hexdigest(), "base64": base64.b64encode(captured).decode()}
        except BaseException as error:
            errors.append(error)

    readers = [threading.Thread(target=drain, args=(name, pipe)) for name, pipe in (("stdout", child.stdout), ("stderr", child.stderr))]
    for reader in readers:
        reader.start()
    code = child.wait()  # Retain the actual direct child; no cancellation, retry or PID cleanup.
    for reader in readers:
        reader.join()  # EOF proves original pipe writers closed, rather than closing only our log handles.
    expired = time.monotonic() - begun >= 1200
    need(not errors and eof == {"stdout", "stderr"}, "Original install stream EOF failed")
    need(all(row["bytes"] <= MAX_STREAM for row in streams.values()), "Install stream exceeds 2 MiB")
    row = {"scope": scope, "argv": argv, "cwd": str(repo), "waitReaped": True, "exitCode": code,
           "deadlineExceeded": expired, "streamsClosed": child.stdout.closed and child.stderr.closed, "lifecycleScriptsEnabled": True, **streams}
    print(json.dumps({"installResult": row}), flush=True)
    need(code == 0 and not expired, "Normal npm ci did not complete successfully within the CI install bound")
    return row


def validate_receipt(receipt, profile=PROFILE, case=None):
    case = CASES["candidate82"] if case is None else case
    need(profile == case["profile"], "Cross-case source profile refused")
    need(receipt.get("schemaVersion") == 1 and receipt.get("kind") == case["kind"] and "exampleOnly" not in receipt, "Eligible dependency receipt required")
    for field in ("productHead", "productTree", "sourceInventorySha256", "manifestLocks"):
        need(receipt[field] == profile[field], "Source/manifest/lock receipt drift")
    need(receipt["sourceCleanAfterInstall"] is True and receipt["dependencyUpdatesPerformed"] is False
         and receipt["coreOrBackendEmissionsIncluded"] is False and receipt["normalNpmCi"] is True, "Normal dependency-only boundary differs")
    platform_row = receipt["platform"]
    need(all(platform_row[key] == value for key, value in {"os": "linux", "architecture": "x64", "node": "v26.4.0", "bun": "1.4.4", "typescript": "7.0.2"}.items())
         and re.fullmatch(r"\d+\.\d+\.\d+(?:[-+].+)?", platform_row["npmVersion"]), "Pinned platform provenance differs")
    producer = receipt["producer"]
    need(set(producer) == {"kind", "repository", "sourceHead", "runId", "runAttempt", "job", "workflowPath", "workflowRevision", "artifactName"}, "Post-upload artifact identity must remain outside the receipt")
    need(producer["kind"] == "github-actions-normal-npm-ci" and producer["repository"] == "eosrio/hyperion-tools"
         and producer["sourceHead"] == profile["productHead"] and producer["job"] == "linux-dependencies"
         and producer["workflowPath"] == case["workflow"] and producer["artifactName"] == case["artifact"]
         and re.fullmatch("[a-f0-9]{40}", producer["workflowRevision"])
         and all(type(producer[key]) is int and producer[key] > 0 for key in ("runId", "runAttempt")), "Actual CI producer identity required")
    need(receipt["sourceLinks"] == source_links(profile), "Exact independently source-bound workspace link required")
    need([row["scope"] for row in receipt["normalInstalls"]] == ["root", "adapter"], "Both normal installs required")
    for row in receipt["normalInstalls"]:
        suffix = ["ci", "--allow-git=all", "--ignore-scripts=false"] + (["--prefix", "packages/backend-mongodb"] if row["scope"] == "adapter" else [])
        need(len(row["argv"]) == len(suffix) + 2 and row["argv"][0].endswith("/node") and row["argv"][1].endswith("/npm-cli.js")
             and row["argv"][2:] == suffix and row["cwd"].startswith("/"), "Exact native normal-install argv/cwd required")
        need(row["waitReaped"] is True and type(row["exitCode"]) is int and row["exitCode"] == 0 and row["deadlineExceeded"] is False
             and row["streamsClosed"] is True and row["lifecycleScriptsEnabled"] is True, "Normal closed terminal outcome required")
        for name in ("stdout", "stderr"):
            stream = row[name]
            data = base64.b64decode(stream["base64"], validate=True)
            need(type(stream["bytes"]) is int and 0 <= len(data) == stream["bytes"] <= MAX_STREAM and digest(data) == stream["sha256"], "Closed original stream bytes differ")
    by = validate_inventory(receipt["inventory"])
    need(set(receipt["dependencyProvenance"]) == set(profile["dependencySources"]), "Complete dependency provenance required")
    for key, expected in profile["dependencySources"].items():
        actual = receipt["dependencyProvenance"][key]
        need(all(actual[field] == value for field, value in expected.items()), "Dependency source pin drift")
        package = by[expected["packagePath"] + "/package.json"]
        need(actual["packageJson"] == {field: package[field] for field in ("bytes", "sha256")}, "Package identity byte pin drift")
        if key != "nodeAbieos":
            need(actual["sourceRevision"] == expected["resolved"].split("#")[-1], "Dependency revision drift")
        if key == "wire":
            need(actual["preparedSdkRevision"] == case["preparedSdk"], "Prepared Wire SDK drift")
            names = [expected["packagePath"] + "/dist/" + name for name in ("protocol-node-artifact.json", "protocol-rust-artifact.json", "protocol-source-receipt.json", "protocol-npm-lock.json")]
            need(actual["artifactReceipts"] == [{"path": name, **{field: by[name][field] for field in ("bytes", "sha256")}} for name in names], "Original Wire receipt byte pins differ")
        if key == "nodeAbieos":
            native = [row for name, row in sorted(by.items()) if name.startswith(expected["packagePath"] + "/") and name.endswith(".node")]
            need(native and all(row["type"] == "file" for row in native) and actual["nativeFiles"] == [{"path": row["path"], **{field: row[field] for field in ("bytes", "sha256", "mode")}} for row in native], "Every original native binary must be pinned")
    return by


def ci_identity(tools, env, case=None):
    case = CASES["candidate82"] if case is None else case
    need(env.get("GITHUB_ACTIONS") == "true" and env.get("GITHUB_REPOSITORY") == "eosrio/hyperion-tools", "Public Tools GitHub Actions producer only")
    revision = git(tools, "rev-parse", "HEAD").decode().strip()
    need(re.fullmatch("[a-f0-9]{40}", revision) and revision == env.get("GITHUB_WORKFLOW_SHA"), "Actual Tools workflow revision differs")
    need(env.get("GITHUB_JOB") == "linux-dependencies" and env.get("GITHUB_WORKFLOW_REF", "").startswith("eosrio/hyperion-tools/" + case["workflow"] + "@"), "Actual workflow/job identity differs")
    run_id, attempt = int(env["GITHUB_RUN_ID"]), int(env["GITHUB_RUN_ATTEMPT"])
    need(run_id > 0 and attempt > 0, "Actual CI run/attempt required")
    return {"kind": "github-actions-normal-npm-ci", "repository": "eosrio/hyperion-tools", "sourceHead": case["profile"]["productHead"],
            "runId": run_id, "runAttempt": attempt, "job": "linux-dependencies", "workflowPath": case["workflow"],
            "workflowRevision": revision, "artifactName": case["artifact"]}


def produce(repo, output, case_name="candidate82"):
    case = select_case(case_name)
    profile = case["profile"]
    need(platform.system() == "Linux" and platform.machine() == "x86_64", "Only the pinned Linux/x64 CI producer may install")
    tools = Path(__file__).resolve().parents[2]
    producer = ci_identity(tools, os.environ, case)
    node = Path(shutil.which("node") or "").resolve(strict=True)
    npm = Path(shutil.which("npm") or "").resolve(strict=True)
    need(node.name == "node" and npm.name == "npm-cli.js", "Actual native Node/npm entrypoints required")
    version = lambda argv: subprocess.check_output(argv, cwd=repo, text=True).strip()
    need(version([str(node), "--version"]) == "v26.4.0" and version(["bun", "--version"]) == "1.4.4", "Pinned runtime/checker required")
    need(version(["bun", "-p", "process.versions.typescript"]) == "7.0.2", "Pinned embedded TypeScript required")
    npm_version = version([str(node), str(npm), "--version"])
    verify_source(repo, profile)
    need(not output.exists() and not output.is_relative_to(repo), "New external artifact output required")
    installs = [run_install(repo, node, npm, "root"), run_install(repo, node, npm, "adapter")]
    verify_source(repo, profile)
    need(json.loads((repo / "node_modules/typescript/package.json").read_bytes())["version"] == "7.0.2", "Installed TypeScript differs")
    rows = dependency_inventory(repo)
    receipt = {"schemaVersion": 1, "kind": case["kind"], "productHead": profile["productHead"],
        "productTree": profile["productTree"], "sourceInventorySha256": profile["sourceInventorySha256"], "manifestLocks": profile["manifestLocks"],
        "platform": {"os": "linux", "architecture": "x64", "node": "v26.4.0", "npmVersion": npm_version, "bun": "1.4.4", "typescript": "7.0.2"},
        "sourceCleanAfterInstall": True, "dependencyUpdatesPerformed": False, "coreOrBackendEmissionsIncluded": False, "normalNpmCi": True,
        "normalInstalls": installs, "producer": producer, "sourceLinks": source_links(profile), "dependencyProvenance": dependency_provenance(repo, rows, profile, case["preparedSdk"]), "inventory": rows}
    data = encoded(receipt)
    validate_receipt(receipt, profile, case)
    need(len(data) <= MAX_RECEIPT, "Receipt exceeds 16 MiB")
    output.mkdir(mode=0o755)
    write_archive(repo, rows, output / case["archive"])
    need(dependency_inventory(repo) == rows, "Dependency closure changed during archive")
    verify_source(repo, profile)
    with (output / case["receipt"]).open("xb") as stream:
        stream.write(data)
    print(json.dumps({"artifactName": case["artifact"], "receipt": file_pin(output / case["receipt"]),
                      "archive": {"bytes": (output / case["archive"]).stat().st_size, "sha256": archive_digest(output / case["archive"])},
                      "entries": len(rows), "operationalAdmissionGranted": False}), flush=True)


def archive_digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024**2), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", default="candidate82")
    parser.add_argument("--check-case", action="store_true")
    parser.add_argument("--product-root", type=Path)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    select_case(arguments.case)  # Refuse unresolved/unknown cases before touching product or installing.
    if not arguments.check_case:
        parser.error("--product-root and --output required") if arguments.product_root is None or arguments.output is None else None
        produce(arguments.product_root.resolve(strict=True), arguments.output.resolve(), arguments.case)
