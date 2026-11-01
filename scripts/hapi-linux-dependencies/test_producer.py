"""Offline source/archive controls. No npm installs or network access."""
import base64
import copy
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("dependency_producer", Path(__file__).with_name("producer.py"))
P = importlib.util.module_from_spec(spec)
spec.loader.exec_module(P)


class ProducerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="hapi-dependencies82-tests-")
        self.root = Path(self.temporary.name).resolve()
        self.parent = self.root.parent
        self.addCleanup(self.cleanup)

    def cleanup(self):
        self.assertEqual(self.root.resolve().parent, self.parent)
        self.assertTrue(self.root.name.startswith("hapi-dependencies82-tests-"))
        self.assertFalse(self.root.is_symlink())
        self.temporary.cleanup()

    def fixture(self):
        data = {"node_modules/pkg/index.js": b"export const value=1;\n"}
        rows = [{"path": root, "type": "directory"} for root in P.ROOTS]
        rows += [{"path": "node_modules/@eosrio", "type": "directory"}, {"path": "node_modules/pkg", "type": "directory"},
                 {"path": P.SOURCE_LINK, "type": "symlink", "target": P.SOURCE_TARGET}]
        rows += [{"path": name, "type": "file", "bytes": len(value), "sha256": P.digest(value), "mode": 0o644} for name, value in data.items()]
        return sorted(rows, key=lambda row: row["path"]), data

    def tar(self, rows, data, mutate=None, extra=None):
        path = self.root / ("input-" + str(len(list(self.root.glob("*.tgz")))) + ".tgz")
        with tarfile.open(path, "w:gz") as archive:
            for row in [{"path": name, "type": "directory"} for name in P.SCAFFOLD] + rows:
                member = tarfile.TarInfo(row["path"])
                if row["type"] == "directory":
                    member.type, member.mode = tarfile.DIRTYPE, 0o755
                elif row["type"] == "symlink":
                    member.type, member.mode, member.linkname = tarfile.SYMTYPE, 0o777, row["target"]
                else:
                    member.size, member.mode = row["bytes"], row["mode"]
                content = data.get(row["path"], b"")
                if mutate:
                    content = mutate(member, content)
                archive.addfile(member, io.BytesIO(content) if member.isfile() else None)
            if extra:
                archive.addfile(extra)
        return path

    def test_positive_complete_tar_and_inventory(self):
        rows, data = self.fixture()
        P.validate_inventory(rows)
        P.verify_archive(self.tar(rows, data), rows)

    def test_archive_refuses_content_size_mode_type_and_link_drift(self):
        rows, data = self.fixture()
        for field in ["content", "size", "mode", "hardlink", "device", "link"]:
            def mutate(member, content):
                if member.name == "node_modules/pkg/index.js":
                    if field == "content": content = b"x" * len(content)
                    if field == "size": member.size -= 1; content = content[:-1]
                    if field == "mode": member.mode = 0o666
                    if field == "hardlink": member.type = tarfile.LNKTYPE; member.linkname = "node_modules/pkg/index.js"
                    if field == "device": member.type = tarfile.CHRTYPE
                if member.name == P.SOURCE_LINK and field == "link": member.linkname = "../../other"
                return content
            with self.subTest(field=field), self.assertRaises(ValueError):
                P.verify_archive(self.tar(rows, data, mutate), rows)

    def test_archive_refuses_duplicates_missing_entries_and_pax_overrides(self):
        rows, data = self.fixture()
        duplicate = tarfile.TarInfo("node_modules/pkg"); duplicate.type = tarfile.DIRTYPE
        with self.assertRaises(ValueError): P.verify_archive(self.tar(rows, data, extra=duplicate), rows)
        with self.assertRaises(ValueError): P.verify_archive(self.tar(rows[:-1], data), rows)
        def mutate(member, content):
            if member.name == "node_modules/pkg/index.js": member.pax_headers = {"SCHILY.xattr.user.fake": "extra"}
            return content
        with self.assertRaises(ValueError): P.verify_archive(self.tar(rows, data, mutate), rows)

    def test_archive_refuses_concatenated_payload_and_oversized_metadata(self):
        rows, data = self.fixture(); path = self.tar(rows, data)
        with path.open("ab") as output: output.write(gzip.compress(b"hidden payload"))
        with self.assertRaises(ValueError): P.verify_archive(path, rows)
        def mutate(member, content):
            if member.name == "node_modules": member.pax_headers = {"path": "x" * (1024**2 + 1)}
            return content
        with self.assertRaises(ValueError): P.verify_archive(self.tar(rows, data, mutate), rows)

    def test_archive_whitelist_refuses_core_dist_git_credentials_and_unsafe_paths(self):
        rows, data = self.fixture()
        for name in ["build/core.js", "packages/backend-mongodb/dist/index.js", "package.json", ".git/config",
                     "node_modules/.npmrc", "node_modules/a/.git/config", "node_modules/../build/a", "/tmp/a", "C:/a", "node_modules//a", "node_modules/a\\b"]:
            member = tarfile.TarInfo(name)
            with self.subTest(name=name), self.assertRaises(ValueError):
                P.verify_archive(self.tar(rows, data, extra=member), rows)

    def test_inventory_ancestor_and_kind_mode_bounds(self):
        rows, _ = self.fixture()
        invalid = [rows + [dict(rows[-1])], [row for row in rows if row["path"] != "node_modules/pkg"],
                   [row for row in rows if row["path"] != P.ROOTS[1]]]
        for field, value in [("type", "hardlink"), ("mode", 0o2644), ("mode", 0o666), ("bytes", P.MAX_FILE + 1), ("bytes", -1)]:
            changed = copy.deepcopy(rows)
            next(row for row in changed if row["type"] == "file")[field] = value
            invalid.append(changed)
        for case in invalid:
            with self.assertRaises(ValueError): P.validate_inventory(case)

    def test_links_are_source_bound_or_complete_internal_links(self):
        rows, _ = self.fixture()
        rows += [{"path": "node_modules/.bin", "type": "directory"},
                 {"path": "node_modules/.bin/pkg", "type": "symlink", "target": "../pkg/index.js"}]
        P.validate_inventory(rows)
        for target in ["/etc/passwd", "../../../../escape", "C:/outside", "..\\outside", "../missing", "../@eosrio/hyperion-backend-mongodb/package.json"]:
            changed = copy.deepcopy(rows); changed[-1]["target"] = target
            with self.subTest(target=target), self.assertRaises(ValueError): P.validate_inventory(changed)
        cycle = copy.deepcopy(rows); cycle[-1]["target"] = "other"
        cycle.append({"path": "node_modules/.bin/other", "type": "symlink", "target": "pkg"})
        with self.assertRaises(ValueError): P.validate_inventory(cycle)
        wrong = copy.deepcopy(rows); next(row for row in wrong if row["path"] == P.SOURCE_LINK)["target"] = "../../packages/other"
        with self.assertRaises(ValueError): P.validate_inventory(wrong)

    def source_fixture(self):
        repo = self.root / "source"; repo.mkdir()
        content = b'{"name":"fixture"}\n'; (repo / "package.json").write_bytes(content)
        oid = __import__("hashlib").sha1(b"blob " + str(len(content)).encode() + b"\0" + content).hexdigest()
        row = {"path": "package.json", "bytes": len(content), "sha256": P.digest(content), "gitBlob": oid, "mode": "100644"}
        profile = {"productHead": "1" * 40, "productTree": "2" * 40, "sourceFiles": 1,
                   "sourceInventorySha256": P.digest(P.encoded([row])), "manifestLocks": {"package.json": {"bytes": len(content), "sha256": P.digest(content)}}}
        def git(_repo, *args):
            return {("rev-parse", "--show-toplevel"): str(repo).encode(), ("rev-parse", "HEAD"): b"1" * 40,
                    ("rev-parse", "HEAD^{tree}"): b"2" * 40, ("ls-tree", "-r", "-z", "HEAD"): f"100644 blob {oid}\tpackage.json\0".encode()}[args]
        return repo, profile, git

    def test_source_bytes_commit_tree_inventory_and_lock_drift(self):
        repo, profile, git = self.source_fixture()
        with mock.patch.object(P, "git", side_effect=git):
            P.verify_source(repo, profile)
            for key in ["productHead", "productTree", "sourceInventorySha256"]:
                wrong = copy.deepcopy(profile); wrong[key] = "f" * len(wrong[key])
                with self.subTest(key=key), self.assertRaises(ValueError): P.verify_source(repo, wrong)
            wrong = copy.deepcopy(profile); wrong["manifestLocks"]["package.json"]["sha256"] = "0" * 64
            with self.assertRaises(ValueError): P.verify_source(repo, wrong)
            (repo / "package.json").write_bytes(b"changed")
            with self.assertRaises(ValueError): P.verify_source(repo, profile)

    @unittest.skipUnless(os.name == "posix", "POSIX producer permission/link semantics are also checked in Linux CI")
    def test_writer_inventory_never_walks_workspace_link_or_adopts_compiled_outputs(self):
        repo = self.root / "product"; repo.mkdir()
        for name in P.ROOTS: (repo / name).mkdir(parents=True)
        (repo / "node_modules/@eosrio").mkdir()
        (repo / P.SOURCE_LINK).symlink_to(P.SOURCE_TARGET, target_is_directory=True)
        file = repo / "node_modules/a.js"; file.write_bytes(b"dependency"); file.chmod(0o644)
        for name in ["build/core.js", "packages/backend-mongodb/dist/index.js"]:
            path = repo / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(b"never adopted")
        rows = P.dependency_inventory(repo)
        self.assertNotIn("build/core.js", [row["path"] for row in rows])
        self.assertFalse(any("/dist/index.js" in row["path"] for row in rows))
        P.write_archive(repo, rows, self.root / "produced.tgz")
        (repo / "node_modules/hardlink").hardlink_to(file)
        with self.assertRaises(ValueError): P.dependency_inventory(repo)

    def test_ci_gate_refuses_local_identity_before_installs(self):
        with mock.patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError): P.ci_identity(self.root, os.environ)
        with mock.patch.object(P, "run_install") as install, mock.patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError):
            P.produce(self.root, self.root / "out")
        install.assert_not_called()

    def test_normal_wait_collects_original_closed_streams_by_value(self):
        script = self.root / "fake-install.py"
        script.write_text("import sys\nsys.stdout.buffer.write(b'original stdout\\n')\nsys.stderr.buffer.write(b'original stderr\\n')\n", encoding="utf-8")
        row = P.run_install(self.root, Path(sys.executable), script, "root")
        self.assertTrue(row["waitReaped"] and row["streamsClosed"])
        self.assertEqual(row["exitCode"], 0)
        self.assertEqual(base64.b64decode(row["stdout"]["base64"]), b"original stdout\n")
        self.assertEqual(base64.b64decode(row["stderr"]["base64"]), b"original stderr\n")
        self.assertEqual(row["argv"][2:], ["ci", "--allow-git=all", "--ignore-scripts=false"])

    def test_failed_install_never_becomes_an_eligible_terminal(self):
        script = self.root / "fail-install.py"; script.write_text("raise SystemExit(7)\n", encoding="utf-8")
        with self.assertRaises(ValueError): P.run_install(self.root, Path(sys.executable), script, "adapter")

    def receipt_fixture(self, profile=P.PROFILE, case=None):
        case = P.CASES["candidate82"] if case is None else case
        rows, data = self.fixture()
        by = {row["path"]: row for row in rows}
        def file(name, content):
            for parent in PurePosixPath(name).parents:
                parent = str(parent)
                if parent != "." and P.dependency_path(parent): by.setdefault(parent, {"path": parent, "type": "directory"})
            by[name] = {"path": name, "type": "file", "bytes": len(content), "sha256": P.digest(content), "mode": 0o644}
        provenance = {}
        for key, expected in profile["dependencySources"].items():
            name = expected["packagePath"] + "/package.json"
            content = P.encoded({field: expected[field] for field in ("name", "version")}); file(name, content)
            value = {**expected, "packageJson": {"bytes": len(content), "sha256": P.digest(content)}}
            if key != "nodeAbieos": value["sourceRevision"] = expected["resolved"].split("#")[-1]
            if key == "wire":
                value["preparedSdkRevision"] = case["preparedSdk"]; value["artifactReceipts"] = []
                for suffix in ["protocol-node-artifact.json", "protocol-rust-artifact.json", "protocol-source-receipt.json", "protocol-npm-lock.json"]:
                    name = expected["packagePath"] + "/dist/" + suffix; content = b"fixture-only"
                    file(name, content); value["artifactReceipts"].append({"path": name, "bytes": len(content), "sha256": P.digest(content)})
            if key == "nodeAbieos":
                value["nativeFiles"] = []
                for binary in ["abieos-linux-x64.node", "abieos-win-x64.node"]:
                    name = expected["packagePath"] + "/dist/" + binary; content = binary.encode(); file(name, content)
                    value["nativeFiles"].append({"path": name, "bytes": len(content), "sha256": P.digest(content), "mode": 0o644})
            provenance[key] = value
        stream = {"bytes": 0, "sha256": P.digest(b""), "base64": ""}
        installs = [{"scope": scope, "argv": ["/pinned/node", "/pinned/npm-cli.js", "ci", "--allow-git=all", "--ignore-scripts=false"] + (["--prefix", "packages/backend-mongodb"] if scope == "adapter" else []),
            "cwd": "/fixture/product", "waitReaped": True, "exitCode": 0, "deadlineExceeded": False, "streamsClosed": True, "lifecycleScriptsEnabled": True,
            "stdout": dict(stream), "stderr": dict(stream)} for scope in ["root", "adapter"]]
        return {"schemaVersion": 1, "kind": case["kind"], **{key: profile[key] for key in ["productHead", "productTree", "sourceInventorySha256", "manifestLocks"]},
            "platform": {"os": "linux", "architecture": "x64", "node": "v26.4.0", "npmVersion": "12.1.0", "bun": "1.4.4", "typescript": "7.0.2"},
            "sourceCleanAfterInstall": True, "dependencyUpdatesPerformed": False, "coreOrBackendEmissionsIncluded": False, "normalNpmCi": True,
            "normalInstalls": installs, "producer": {"kind": "github-actions-normal-npm-ci", "repository": "eosrio/hyperion-tools", "sourceHead": profile["productHead"],
                "runId": 1, "runAttempt": 1, "job": "linux-dependencies", "workflowPath": case["workflow"], "workflowRevision": "f" * 40, "artifactName": case["artifact"]},
            "sourceLinks": P.source_links(profile), "dependencyProvenance": provenance, "inventory": sorted(by.values(), key=lambda row: row["path"])}

    def test_receipt_source_lock_link_runtime_and_postupload_identity_drift(self):
        receipt = self.receipt_fixture(); P.validate_receipt(receipt)
        for mutate in [lambda r: r.update(productHead="0" * 40), lambda r: r.update(productTree="0" * 40),
                       lambda r: r.update(sourceInventorySha256="0" * 64), lambda r: r.update(manifestLocks={}),
                       lambda r: r["sourceLinks"][0].update(target="../../../outside"), lambda r: r["platform"].update(node="v26.4.1"),
                       lambda r: r["producer"].update(artifactId=1), lambda r: r["producer"].update(repository="other/repo"),
                       lambda r: r.update(coreOrBackendEmissionsIncluded=True), lambda r: r.update(exampleOnly=True)]:
            changed = copy.deepcopy(receipt); mutate(changed)
            with self.assertRaises(ValueError): P.validate_receipt(changed)

    def test_receipt_requires_original_raw_streams_normal_argv_and_actual_terminal(self):
        receipt = self.receipt_fixture()
        for mutate in [lambda r: r["normalInstalls"][0].update(exitCode=1), lambda r: r["normalInstalls"][0].update(waitReaped=False),
                       lambda r: r["normalInstalls"][0].update(streamsClosed=False), lambda r: r["normalInstalls"][0].update(deadlineExceeded=True),
                       lambda r: r["normalInstalls"][0]["argv"].append("--ignore-scripts"),
                       lambda r: r["normalInstalls"][0]["stdout"].update(base64=base64.b64encode(b"changed").decode()),
                       lambda r: r["normalInstalls"][0]["stderr"].update(sha256="0" * 64)]:
            changed = copy.deepcopy(receipt); mutate(changed)
            with self.assertRaises(ValueError): P.validate_receipt(changed)

    def test_receipt_requires_exact_dependency_sources_wire_receipts_and_every_native_binary(self):
        receipt = self.receipt_fixture()
        for mutate in [lambda r: r["dependencyProvenance"]["rootSdk"].update(sourceRevision="0" * 40),
                       lambda r: r["dependencyProvenance"]["adapterSdk"].update(integrity="changed"),
                       lambda r: r["dependencyProvenance"]["wire"].update(preparedSdkRevision="0" * 40),
                       lambda r: r["dependencyProvenance"]["wire"]["artifactReceipts"].pop(),
                       lambda r: r["dependencyProvenance"]["nodeAbieos"]["nativeFiles"].pop(),
                       lambda r: r["dependencyProvenance"]["rootSdk"]["packageJson"].update(sha256="0" * 64)]:
            changed = copy.deepcopy(receipt); mutate(changed)
            with self.assertRaises(ValueError): P.validate_receipt(changed)

    def test_fixed_case_selection_and_unresolved_case_fail_before_install(self):
        self.assertIs(P.select_case("candidate82")["profile"], P.PROFILE)
        unresolved = {**P.CASES["current88"], "profile": None, "preparedSdk": None}
        with mock.patch.dict(P.CASES, {"current88": {**unresolved, "profile": {}, "preparedSdk": "a" * 40}}):
            with self.assertRaisesRegex(ValueError, "Incomplete dependency case"):
                P.select_case("current88")
        for name in ("current88", "other", "", "__proto__"):
            with self.subTest(name=name), mock.patch.dict(P.CASES, {"current88": unresolved}), \
                    mock.patch.object(P, "run_install") as install, self.assertRaises(ValueError):
                P.produce(self.root / "absent", self.root / "out", name)
            install.assert_not_called()

    def test_cross_case_source_and_receipts_are_refused(self):
        old = self.receipt_fixture()
        fake_profile = copy.deepcopy(P.PROFILE); fake_profile["productHead"] = "a" * 40
        current = {**P.CASES["current88"], "profile": fake_profile, "preparedSdk": P.PREPARED_SDK}
        with self.assertRaisesRegex(ValueError, "Cross-case source profile"):
            P.validate_receipt(old, P.PROFILE, current)
        with self.assertRaisesRegex(ValueError, "Eligible dependency receipt"):
            P.validate_receipt(old, fake_profile, current)
        wrong_source = copy.deepcopy(old); wrong_source["kind"] = current["kind"]
        with self.assertRaisesRegex(ValueError, "Source/manifest/lock receipt drift"):
            P.validate_receipt(wrong_source, fake_profile, current)
        wrong_identity = copy.deepcopy(old); wrong_identity["kind"] = current["kind"]
        wrong_identity["productHead"] = fake_profile["productHead"]
        wrong_identity["producer"]["sourceHead"] = fake_profile["productHead"]
        with self.assertRaisesRegex(ValueError, "Actual CI producer identity"):
            P.validate_receipt(wrong_identity, fake_profile, current)

    def test_bound_current_receipt_is_valid_and_cross_case_is_rejected(self):
        case = P.select_case("current88"); profile = case["profile"]
        current = self.receipt_fixture(profile, case)
        P.validate_receipt(current, profile, case)
        with self.assertRaises(ValueError): P.validate_receipt(current)
        old = self.receipt_fixture()
        with self.assertRaises(ValueError): P.validate_receipt(old, profile, case)
        for key in ("productHead", "productTree", "sourceInventorySha256", "manifestLocks"):
            changed = copy.deepcopy(current); changed[key] = copy.deepcopy(P.PROFILE[key])
            with self.subTest(key=key), self.assertRaises(ValueError): P.validate_receipt(changed, profile, case)

    def test_current_workflow_fails_closed_and_has_distinct_outputs(self):
        workflow = Path(__file__).resolve().parents[2] / P.CASES["current88"]["workflow"]
        text = workflow.read_text()
        self.assertLess(text.index("--case current88 --check-case"), text.index("repository: eosrio/hyperion-history-api"))
        if P.CASES["current88"]["profile"] is None:
            self.assertIn("UNRESOLVED_CURRENT_HAPI_SOURCE", text)
        else:
            self.assertIn("ref: " + P.CASES["current88"]["profile"]["productHead"], text)
        for name in ("artifact", "archive", "receipt"):
            self.assertIn(P.CASES["current88"][name], text)
        self.assertIn("--ignore-scripts=false", Path(__file__).with_name("producer.py").read_text())
        self.assertNotIn("candidate82-dependencies", text)

    def test_current103_is_fixed_source_bound_and_rejects_current88_receipts(self):
        previous = P.select_case("current88"); case = P.select_case("current103")
        old, profile = previous["profile"], case["profile"]
        self.assertEqual(profile["productHead"], "bca518ddfebf04c2af957832f2ad9c0582c17932")
        self.assertEqual(profile["productTree"], "26cfd37e658d030090059096afc32a574ed787ef")
        self.assertEqual(profile["sourceInventorySha256"], "e5334bae500bc1afd70f497f69791fc1d0d8b86dc4aca8c482aaef2373359ba3")
        self.assertEqual(profile["sourceFiles"], 1066)
        for field in ("manifestLocks", "dependencySources"):
            self.assertEqual(profile[field], old[field])
        self.assertEqual(case["preparedSdk"], previous["preparedSdk"])
        for field in ("kind", "archive", "receipt", "artifact", "workflow"):
            self.assertNotEqual(case[field], previous[field])
        current = self.receipt_fixture(profile, case); P.validate_receipt(current, profile, case)
        prior = self.receipt_fixture(old, previous)
        with self.assertRaises(ValueError): P.validate_receipt(prior, profile, case)
        with self.assertRaises(ValueError): P.validate_receipt(current, old, previous)
        incomplete = {**case, "profile": {**profile, "sourceInventorySha256": None}}
        with mock.patch.dict(P.CASES, {"current103": incomplete}), mock.patch.object(P, "run_install") as install:
            with self.assertRaises(ValueError): P.produce(self.root / "absent", self.root / "out", "current103")
            install.assert_not_called()

    def test_current103_workflow_is_explicit_and_does_not_rebind_current88(self):
        base = Path(__file__).resolve().parents[2]; case = P.select_case("current103")
        text = (base / case["workflow"]).read_text(); old = (base / P.CASES["current88"]["workflow"]).read_text()
        self.assertLess(text.index("--case current103 --check-case"), text.index("repository: eosrio/hyperion-history-api"))
        self.assertIn("ref: " + case["profile"]["productHead"], text)
        for field in ("artifact", "archive", "receipt"): self.assertIn(case[field], text)
        self.assertIn("--case current103", text); self.assertNotIn("current88-dependencies", text)
        self.assertIn("--case current88", old); self.assertIn("f16351b774409802f71d08b3f49c6c5f35c9f63a", old)
        self.assertEqual(text.count("persist-credentials: false"), 2)
        self.assertIn("node-version: '26.4.0'", text); self.assertIn("bun-version: '1.4.4'", text)
        self.assertIn("cancel-in-progress: false", text)

    def test_published_workflow_is_path_scoped_pinned_and_dependency_only(self):
        workflow = Path(__file__).resolve().parents[2] / P.WORKFLOW
        text = workflow.read_text()
        self.assertIn("ref: " + P.PROFILE["productHead"], text)
        self.assertEqual(text.count("persist-credentials: false"), 2)
        self.assertIn("node-version: '26.4.0'", text); self.assertIn("bun-version: '1.4.4'", text)
        self.assertIn("'scripts/hapi-linux-dependencies/**'", text)
        self.assertIn("cancel-in-progress: false", text)
        self.assertIn(P.ARCHIVE, text); self.assertIn(P.RECEIPT, text)
        self.assertNotIn("pull_request_target", text)


if __name__ == "__main__":
    unittest.main()
