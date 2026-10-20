"""Bounded I/O, provenance and evidence helpers for the real-log qualification."""
import hashlib
import json
import os
import pathlib
import stat
import uuid

CHUNK = 16 << 20


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def stamp(value):
    return {
        "device": value.st_dev, "inode": value.st_ino, "size": value.st_size,
        "modified_ns": value.st_mtime_ns, "changed_ns": value.st_ctime_ns,
    }


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def publish(path, value):
    """Publish complete JSON without replacing any existing evidence."""
    temporary = path.parent / f".{path.name}.{uuid.uuid4().hex}.pending"
    with temporary.open("xb") as output:
        output.write((json.dumps(value, indent=2) + "\n").encode())
        output.flush()
        os.fsync(output.fileno())
    os.link(temporary, path)
    sync_dir(path.parent)
    temporary.unlink()
    sync_dir(path.parent)


def event(name, **fields):
    print(json.dumps({"event": name, **fields}), flush=True)


class Pinned:
    """Read one descriptor. Append-only source growth is explicitly permitted."""
    def __init__(self, path):
        self.path = pathlib.Path(path).resolve(strict=True)
        self.file = self.path.open("rb", buffering=0)
        self.before = os.fstat(self.file.fileno())
        require(stat.S_ISREG(self.before.st_mode), f"not a regular file: {self.path}")

    def close(self):
        self.file.close()

    def check(self, minimum):
        now, named = os.fstat(self.file.fileno()), self.path.stat()
        expected = (self.before.st_dev, self.before.st_ino)
        require((now.st_dev, now.st_ino) == expected, "open source identity changed")
        require((named.st_dev, named.st_ino) == expected, f"source path replaced: {self.path}")
        require(now.st_size >= minimum, f"source shrank below copied prefix: {self.path}")
        return stamp(now)

    def read(self, offset, length):
        require(offset >= 0 and 0 <= length <= CHUNK, "unbounded fixture read")
        data = os.pread(self.file.fileno(), length, offset)
        require(len(data) == length, f"short source read at {offset}: {self.path}")
        return data

    def digest(self, size, phase="hash"):
        self.check(size)
        digest = hashlib.sha256()
        at, next_progress = 0, 1 << 30
        while at < size:
            data = self.read(at, min(CHUNK, size - at))
            digest.update(data)
            at += len(data)
            if at >= next_progress:
                event(phase, path=str(self.path), checked=at, total=size)
                next_progress = at + (4 << 30)
        self.check(size)
        return digest.hexdigest()
