"""Fast refusal/oracle tests for the optional destructive real-log harness."""
import argparse
import hashlib
import os
import pathlib
import struct
import tempfile
import unittest
import sys
import json
import zlib
from unittest.mock import patch

from qualification.common import Pinned, publish, require, stamp
from qualification.corpus import SHIP_MAGIC, copy_prefix, snapshot
from qualification.bench import Corpus, oracle
from qualification.process import run
from qualification import cache
from qualification.formats import block_header, ship_payload


class RealFormatOracles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data = json.loads((pathlib.Path(__file__).parent / "fixtures/libre-real-entries.json").read_text())

    def test_header_ids_match_independent_next_previous_and_snapshot_anchor(self):
        for record in self.data["records"]:
            body = bytes.fromhex(record["blocks"]["body_hex"])
            actual = block_header(body)
            self.assertEqual(actual["number"], record["number"])
            self.assertEqual(actual["id"], record["blocks"]["id"])
            if record["number"] == 209394282:
                self.assertEqual(actual["id"], "0c7b1a6a5ad53b3a96f7a219a175e14c90e4394a752eef3f4cad8b119c6be78d")

    def test_real_ship_checksums_ids_and_single_byte_crc_damage(self):
        for record in self.data["records"]:
            for kind in ("trace_history", "chain_state_history"):
                body = bytes.fromhex(record[kind]["body_hex"])
                actual = ship_payload(body)
                self.assertEqual(actual["block_id"], record["blocks"]["id"])
                self.assertEqual(actual["uncompressed_sha256"], record[kind]["uncompressed_sha256"])
                damaged = body[:-1] + bytes([body[-1] ^ 1])
                with self.assertRaises(zlib.error):
                    ship_payload(damaged)

    def test_size_changing_damage_has_a_reconstructable_splice_manifest(self):
        from qualification.damage import mutation
        for original, actual in [(b"abcdefgh", b"abcdXYZefgh"),
                                 (b"abcdefgh", b"abXefgh"),
                                 (b"abcdefgh", b"abXXXXXXXX")]:
            value = mutation(original, actual)
            self.assertEqual(value["operation"], "splice")
            at = value["offset"]
            removed = original[at:at + value["removed_bytes"]]
            inserted = actual[at:at + value["inserted_bytes"]]
            self.assertEqual(value["removed_sha256"], sha(removed))
            self.assertEqual(value["inserted_sha256"], sha(inserted))
            self.assertEqual(original[:at] + inserted + original[at + len(removed):], actual)


def sha(data):
    return hashlib.sha256(data).hexdigest()


@unittest.skipUnless(hasattr(os, "pread") and hasattr(os, "copy_file_range"), "Linux fixture I/O")
class QualificationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()

    def fixture(self, kind="blocks"):
        data = bytearray(struct.pack("<II", 3, 10) + bytes(32) + bytes([255]) * 8
                         if kind == "blocks" else b"")
        positions = []
        for number in range(10, 13):
            positions.append(len(data))
            payload = bytes([number]) * (80 + number)
            if kind != "blocks":
                payload = (struct.pack("<Q", SHIP_MAGIC) + number.to_bytes(4, "big")
                           + bytes(28) + struct.pack("<Q", len(payload)) + payload)
            data.extend(payload)
            data.extend(struct.pack("<Q", positions[-1]))
        log = self.source / f"{kind}.log"
        log.write_bytes(data)
        log.with_suffix(".index").write_bytes(b"".join(struct.pack("<Q", n) for n in positions))
        return log, bytes(data), positions

    def args(self, kinds):
        return argparse.Namespace(blocks=self.source, ship=self.source, kinds=kinds,
                                  through_block=11, source_revision="test",
                                  out=self.root / "copy")

    def test_snapshot_copies_only_selected_prefix_and_preserves_sources(self):
        expected = []
        for kind in ("blocks", "trace_history", "chain_state_history"):
            log, original, positions = self.fixture(kind)
            expected.append((kind, log, original, positions[2]))
        args = self.args([item[0] for item in expected])
        snapshot(args)
        self.assertTrue((args.out / "ready.json").is_file())
        for kind, source, original, end in expected:
            directory = "blocks" if kind == "blocks" else "state-history"
            copied = args.out / directory / source.name
            self.assertEqual(copied.read_bytes(), original[:end])
            self.assertEqual(copied.with_suffix(".index").stat().st_size, 16)
            self.assertEqual(source.read_bytes(), original)
            self.assertFalse(os.path.samefile(source, copied))

    def test_live_directory_missing_head_and_unsupported_header_refuse_before_output(self):
        log, original, _ = self.fixture()
        for case in ("live-directory", "missing-head", "unsupported-header", "duplicate"):
            args = self.args(["blocks"])
            if case == "live-directory":
                args.out = self.source / "must-not-exist"
            elif case == "missing-head":
                args.through_block = 100
            elif case == "duplicate":
                args.kinds *= 2
            else:
                log.write_bytes(struct.pack("<I", 0x80000003) + original[4:])
            with self.assertRaises(RuntimeError, msg=case):
                snapshot(args)
            self.assertFalse(args.out.exists(), case)
            log.write_bytes(original)

    def test_source_growth_is_allowed_but_replacement_is_refused(self):
        log, original, _ = self.fixture()
        source = Pinned(log)
        self.addCleanup(source.close)
        with log.open("ab") as output:
            output.write(b"next append")
        self.assertEqual(source.digest(len(original)), sha(original))
        log.rename(log.with_suffix(".old"))
        log.write_bytes(original)
        with self.assertRaisesRegex(RuntimeError, "path replaced"):
            source.digest(len(original))

    def test_short_kernel_copy_never_becomes_a_verified_copy(self):
        log, original, _ = self.fixture()
        source = Pinned(log)
        self.addCleanup(source.close)
        with patch("os.copy_file_range", return_value=0):
            with self.assertRaisesRegex(RuntimeError, "ended during bounded copy"):
                copy_prefix(source, self.root / "partial", len(original))

    def test_changed_source_hash_is_refused(self):
        log, original, _ = self.fixture()
        source = Pinned(log)
        self.addCleanup(source.close)
        with patch.object(source, "digest", return_value="0" * 64):
            with self.assertRaisesRegex(RuntimeError, "differ from pinned source"):
                copy_prefix(source, self.root / "bad-copy", len(original))

    def test_evidence_publication_never_overwrites_existing_result(self):
        result = self.root / "ready.json"
        publish(result, {"first": True})
        original = result.read_bytes()
        with self.assertRaises(FileExistsError):
            publish(result, {"first": False})
        self.assertEqual(result.read_bytes(), original)

    def test_guard_does_not_depend_on_python_asserts(self):
        with self.assertRaisesRegex(RuntimeError, "explicit refusal"):
            require(False, "explicit refusal")

    def test_corpus_manifest_cannot_escape_to_live_inputs(self):
        import json
        self.fixture()
        args = self.args(["blocks"])
        snapshot(args)
        manifest_path = args.out / "ready.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["pairs"][0]["log"]["path"] = "../source/blocks.log"
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(RuntimeError, "unexpected corpus path"):
            Corpus(args.out)

    def test_corpus_checksum_changes_are_refused(self):
        self.fixture()
        args = self.args(["blocks"])
        snapshot(args)
        copied = args.out / "blocks/blocks.log"
        os.chmod(copied, 0o600)
        raw = copied.read_bytes()
        copied.write_bytes(raw[:-1] + bytes([raw[-1] ^ 1]))
        with self.assertRaisesRegex(RuntimeError, "fingerprint mismatch"):
            Corpus(args.out)

    def test_per_child_usage_is_not_a_previous_childs_peak(self):
        first = run([sys.executable, "-c", "import time; large=bytearray(32<<20); time.sleep(.1); print('{}')"],
                    self.root / "large", interval=0.05)
        second = run([sys.executable, "-c", "import time; time.sleep(.1); print('{}')"],
                     self.root / "small", interval=0.05)
        self.assertEqual(first["returncode"], 0)
        self.assertEqual(second["returncode"], 0)
        self.assertLess(second["max_rss_kib"], first["max_rss_kib"] * 0.75)
        self.assertTrue(second["sampled_io_max"] or "io" in second["last_sample"].get("unavailable", {}))
        self.assertLess(second["sampled_program_peak_rss_kib"], first["sampled_program_peak_rss_kib"] * 0.75)

    def test_timed_out_child_is_reaped_and_has_terminal_evidence(self):
        import json
        result = run([sys.executable, "-c", "import time; time.sleep(10)"],
                     self.root / "timeout", timeout=0.15, interval=0.05)
        self.assertTrue(result["timed_out"])
        self.assertNotEqual(result["returncode"], 0)
        pid = json.loads((self.root / "timeout/started.json").read_text())["pid"]
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        self.assertTrue((self.root / "timeout/result.json").is_file())

    def test_cache_observation_and_advice_preserve_bytes_and_identity(self):
        log, original, _ = self.fixture()
        source = Pinned(log)
        self.addCleanup(source.close)
        cache.prepare([source], "file-dontneed")
        source.check(len(original))
        self.assertEqual(log.read_bytes(), original)

    def test_observer_failure_cannot_leave_a_child_running(self):
        import json

        def fail(_):
            raise RuntimeError("observer failure")

        with self.assertRaisesRegex(RuntimeError, "observer failure"):
            run([sys.executable, "-c", "import time; time.sleep(10)"],
                self.root / "observer-failure", interval=0.05, observer=fail)
        pid = json.loads((self.root / "observer-failure/started.json").read_text())["pid"]
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_verification_cannot_pass_by_skipping_declared_entries(self):
        pair = {"manifest": {"first_block": 10, "last_block": 12}}
        with self.assertRaisesRegex(RuntimeError, "cover every declared entry"):
            oracle("verify", {"ok": True, "corrupt": False, "entries": 2,
                              "blocks": [10, 12], "skipped_entries": 1, "truncated": False}, pair)

    def test_verify_range_oracle_checks_bytes_as_well_as_entry_counts(self):
        pair = {"manifest": {"first_block": 10, "last_block": 20}}
        value = {"ok": True, "corrupt": False, "entries": 3, "blocks": [12, 14],
                 "skipped_entries": 0, "truncated": False, "bytes": 512}
        oracle("verify", value, pair, (12, 14), 512)
        with self.assertRaisesRegex(RuntimeError, "byte coverage"):
            oracle("verify", value, pair, (12, 14), 1024)

    def test_retirement_refuses_external_or_changed_files_and_preserves_other_work(self):
        from qualification.mutate import fingerprint, retire
        log, original, _ = self.fixture()
        output = self.root / "owned-output"
        output.mkdir()
        with self.assertRaisesRegex(RuntimeError, "escapes"):
            retire(output, [fingerprint(log)], "outside")
        self.assertEqual(log.read_bytes(), original)
        owned = output / "blocks.log"
        owned.write_bytes(original)
        proof = fingerprint(owned)
        owned.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
        # Model a coarse clock or restored metadata: the old byte proof must
        # still reject a same-length edit even if every observed stamp matches.
        proof["stamp"] = stamp(owned.stat())
        with self.assertRaisesRegex(RuntimeError, "changed"):
            retire(output, [proof], "changed")
        self.assertTrue(owned.exists())
        notes = output / "operator-notes.txt"
        notes.write_text("keep this")
        retire(output, [fingerprint(owned)], "verified")
        self.assertFalse(owned.exists())
        self.assertEqual(notes.read_text(), "keep this")

    def test_block_only_references_do_not_require_unrelated_ship_payloads(self):
        import json
        from qualification.damage import reference
        fixture = json.loads((pathlib.Path(__file__).parent / "fixtures/libre-real-entries.json").read_text())
        records = [{"number": p["number"], "blocks": p["blocks"]}
                   for p in fixture["records"][:3]]
        result = reference(self.root / "block-only", records, fixture["chain_id"], ("blocks",))
        self.assertEqual(set(result), {"blocks"})
        self.assertEqual(len(result["blocks"]["positions"]), 3)


if __name__ == "__main__":
    unittest.main()
