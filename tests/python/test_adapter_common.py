"""live_children: a zombie child must not block retiring an idle supervisor."""
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))

from subgov_adapter_common import live_children  # noqa: E402


class LiveChildrenTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.task = self.root / "task"
        self.task.mkdir()
        self.proc = self.root / "proc"

    def child(self, pid, state):
        directory = self.proc / str(pid)
        directory.mkdir(parents=True)
        # The command name may itself contain spaces and parentheses.
        (directory / "stat").write_text(f"{pid} (git (x) y) {state} 1 1 1 0 -1\n")

    def children(self, *pids):
        (self.task / "children").write_text(" ".join(str(pid) for pid in pids) + "\n")

    def test_zombie_child_is_ignored(self):
        self.child(10, "Z")
        self.children(10)
        self.assertEqual(live_children(self.task, self.proc), [])

    def test_dead_child_is_ignored(self):
        self.child(10, "X")
        self.children(10)
        self.assertEqual(live_children(self.task, self.proc), [])

    def test_running_and_sleeping_children_still_block(self):
        self.child(10, "R")
        self.child(11, "S")
        self.children(10, 11)
        self.assertEqual(live_children(self.task, self.proc), ["10", "11"])

    def test_zombie_does_not_hide_a_live_sibling(self):
        self.child(10, "Z")
        self.child(11, "S")
        self.children(10, 11)
        self.assertEqual(live_children(self.task, self.proc), ["11"])

    def test_unreadable_child_state_is_treated_as_live(self):
        self.children(99)
        self.assertEqual(live_children(self.task, self.proc), ["99"])

    def test_no_children_is_empty(self):
        self.children()
        self.assertEqual(live_children(self.task, self.proc), [])


if __name__ == "__main__":
    unittest.main()
