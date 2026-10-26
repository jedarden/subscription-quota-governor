import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "install-adapter-scripts.sh"
SOURCE_ARGS = [] if (REPO / ".git").exists() else ["--source-root", str(REPO)]
MANIFEST = {
    "subgov-codex": [
        "codex-productive.py",
        "subgov_adapter_common.py",
        "subgov_ready_beads.py",
        "subgov_host_capacity.py",
        "zai-source-refresh.py",
    ],
    "subgov-zai": [
        "subgov_adapter_common.py",
        "subgov_ready_beads.py",
        "subgov_host_capacity.py",
        "zai-collector.py",
        "zai-fleet.py",
        "zai-source-refresh.py",
    ],
    "subgov-anthropic": [
        "anthropic-productive.py",
        "codex-productive.py",
        "subgov_adapter_common.py",
        "subgov_ready_beads.py",
        "subgov_host_capacity.py",
        "zai-source-refresh.py",
    ],
    "subgov-source-watch": [
        "subgov_source_watch.py",
        "subgov_adapter_common.py",
    ],
}


class AdapterScriptInstallerTests(unittest.TestCase):
    def run_installer(self, root, *args):
        return subprocess.run(
            ["bash", str(SCRIPT), "--root", str(root), *SOURCE_ARGS, *args],
            cwd=REPO,
            text=True,
            capture_output=True,
            env={**os.environ, "HOME": str(root / "home")},
        )

    def test_install_uses_head_and_populates_all_adapter_manifests(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "lib"
            result = self.run_installer(root)
            self.assertEqual(result.returncode, 0, result.stderr)
            for adapter, files in MANIFEST.items():
                for name in files:
                    deployed = root / adapter / name
                    self.assertEqual(deployed.read_bytes(), (REPO / "scripts" / name).read_bytes())

    def test_verify_fails_when_any_deployed_copy_diverges(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "lib"
            installed = self.run_installer(root)
            self.assertEqual(installed.returncode, 0, installed.stderr)
            target = root / "subgov-anthropic" / "codex-productive.py"
            target.write_bytes(target.read_bytes() + b"\n# local drift\n")

            result = self.run_installer(root, "--verify")

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("differs from HEAD", result.stderr)

    def test_verify_does_not_create_missing_destinations(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "lib"
            result = self.run_installer(root, "--verify")
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(root.exists())


if __name__ == "__main__":
    unittest.main()
