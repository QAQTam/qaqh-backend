"""Counterexamples for the structural gate; no Rust or network dependencies."""

import copy
import importlib.util
import tempfile
import unittest
from pathlib import Path


spec = importlib.util.spec_from_file_location("architecture_audit", Path(__file__).parents[1] / "audit.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


def matrix():
    return {
        "schema_version": 1,
        "crates": {
            "consumer": {
                "owner": "consumer", "modules": ["entry"],
                "allowed_local_dependencies": ["contract"],
                "forbidden_external_dependencies": ["tauri"],
            },
            "contract": {
                "owner": "contract", "modules": ["ids"],
                "allowed_local_dependencies": [], "forbidden_external_dependencies": [],
            },
        },
        "current_to_target": {"old": ["consumer", "contract"]},
    }


class GateCounterexamples(unittest.TestCase):
    def test_cycle_unknown_destination_and_pending_overlap_are_rejected(self):
        data = matrix()
        self.assertEqual(audit.validate_matrix(data), [])
        data["crates"]["contract"]["allowed_local_dependencies"] = ["consumer"]
        data["current_to_target"]["old"].append("missing")
        data["pending_integrations"] = {"consumer": {"included_in_target_graph": False}}
        errors = "\n".join(audit.validate_matrix(data))
        self.assertIn("dependency cycle", errors)
        self.assertIn("unknown target missing", errors)
        self.assertIn("also present", errors)

    def test_duplicate_json_keys_cannot_silently_override_rules(self):
        with tempfile.TemporaryDirectory(prefix="qaqh-audit-") as directory:
            path = Path(directory) / "matrix.json"
            path.write_text('{"crates": {}, "crates": {"overridden": {}}}', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate JSON key"):
                audit.load_matrix(path)

    def test_build_dev_optional_target_and_renamed_dependencies_are_checked(self):
        metadata = {
            "workspace_members": ["consumer-id", "contract-id"],
            "packages": [
                {"id": "consumer-id", "name": "consumer", "dependencies": [
                    {"name": "contract", "rename": "public_api", "kind": None, "path": "/contract"},
                    {"name": "forbidden-local", "kind": "dev", "path": "/other", "optional": True},
                    {"name": "tauri", "rename": "ui_alias", "kind": "build", "target": "cfg(windows)"},
                ]},
                {"id": "contract-id", "name": "contract", "dependencies": []},
                {"id": "external-id", "name": "not-a-member", "dependencies": []},
            ],
        }
        errors, count = audit.compare_workspace(matrix(), metadata)
        self.assertEqual(count, 2)
        self.assertEqual(len(errors), 2, errors)
        self.assertTrue(any("forbidden-local [dev;" in error for error in errors))
        self.assertTrue(any("tauri [build; cfg(windows)]" in error for error in errors))
        changed = copy.deepcopy(metadata)
        changed["workspace_members"].remove("contract-id")
        errors, _ = audit.compare_workspace(matrix(), changed)
        self.assertIn("missing target crate: contract", errors)

    def test_file_links_check_missing_and_escape_but_skip_code_and_urls(self):
        with tempfile.TemporaryDirectory(prefix="qaqh-audit-") as directory:
            root = Path(directory).resolve()
            docs = root / "new-tech/docs"
            docs.mkdir(parents=True)
            (docs / "space name.md").write_text("# Present", encoding="utf-8")
            (docs / "README.md").write_text(
                "[ok](space%20name.md#not-checked)\n[missing](gone.md)\n"
                "[escape](../../../outside.md)\n[web](https://example.invalid/x)\n"
                "[anchor](#local)\n```md\n[example](not-a-real-file.md)\n```\n",
                encoding="utf-8",
            )
            errors, count = audit.check_links(root)
            self.assertEqual(count, 3)
            self.assertEqual(len(errors), 2, errors)
            self.assertTrue(any("missing link target" in error for error in errors))
            self.assertTrue(any("outside repository" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
