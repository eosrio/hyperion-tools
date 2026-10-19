"""Observe/advise only explicitly supplied disposable corpus descriptors."""
import ctypes
import mmap
import os

from .common import require


def residency(source):
    """Count resident pages via mincore in bounded mappings without faulting pages in."""
    libc = ctypes.CDLL(None, use_errno=True)
    mincore = libc.mincore
    mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
    mincore.restype = ctypes.c_int
    size, page = source.before.st_size, mmap.PAGESIZE
    checked, resident, offset = 0, 0, 0
    while offset < size:
        length = min(256 << 20, size - offset)
        pages = (length + page - 1) // page
        # Private writable mapping allows from_buffer to obtain the address; no bytes are written.
        with mmap.mmap(source.file.fileno(), length, flags=mmap.MAP_PRIVATE,
                       prot=mmap.PROT_READ | mmap.PROT_WRITE, offset=offset) as mapping:
            address = ctypes.addressof(ctypes.c_char.from_buffer(mapping))
            vector = (ctypes.c_ubyte * pages)()
            result = mincore(address, length, vector)
            require(result == 0, f"mincore failed with errno {ctypes.get_errno()}")
            resident += sum(value & 1 for value in vector)
            checked += pages
        offset += length
    return {"pages": checked, "resident_pages": resident,
            "resident_fraction": resident / checked if checked else 0}


def prepare(sources, mode):
    require(mode in ("ordinary", "file-dontneed"), "unknown cache mode")
    result = {}
    for source in sources:
        before = residency(source)
        if mode == "file-dontneed":
            os.posix_fadvise(source.file.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
        after = residency(source)
        result[str(source.path)] = {"before": before, "after": after}
    return {"mode": mode, "files": result, "global_cache_eviction": False,
            "note": "DONTNEED is advisory; resident fractions determine actual cache state."}
