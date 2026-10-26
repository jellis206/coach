import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "support"))

import probe_check  # noqa: E402

HARNESS = {"pid": 500, "ppid": 1, "start": "Mon Oct 26 10:00:00 2026", "comm": "/opt/homebrew/bin/codex"}
SHELL = {"pid": 600, "ppid": 500, "start": "Mon Oct 26 10:00:05 2026", "comm": "/bin/sh"}
HOOK = {"pid": 601, "ppid": 600, "start": "Mon Oct 26 10:00:05 2026", "comm": "python3"}


def record(event, chain, **payload):
    return {"event": event, "pid": chain[0]["pid"], "chain": chain, "keys": sorted(payload)}


class DiscoverOwner(unittest.TestCase):
    def test_skips_hook_and_shell_wrappers(self):
        owner = probe_check.discover_owner([HOOK, SHELL, HARNESS])
        self.assertEqual(owner["pid"], 500)

    def test_missing_harness_is_none_not_a_guess(self):
        self.assertIsNone(probe_check.discover_owner([HOOK, SHELL]))

    def test_launcher_shell_above_harness_is_not_owner(self):
        login = {"pid": 400, "ppid": 1, "start": "x", "comm": "-zsh"}
        owner = probe_check.discover_owner([HOOK, SHELL, HARNESS, login])
        self.assertEqual(owner["pid"], 500)


class Check(unittest.TestCase):
    def test_owner_matching_launched_pid_passes(self):
        recs = [record("SessionStart", [HOOK, SHELL, HARNESS], session_id=1),
                record("Stop", [HOOK, SHELL, HARNESS], session_id=1),
                record("SessionEnd", [HOOK, SHELL, HARNESS], session_id=1)]
        self.assertEqual(probe_check.check(recs, 500, HARNESS["start"]), [])

    def test_owner_mismatch_fails(self):
        recs = [record("SessionStart", [HOOK, SHELL, HARNESS])]
        self.assertTrue(probe_check.check(recs, 999, HARNESS["start"]))

    def test_ps_padding_in_start_time_is_ignored(self):
        recs = [record("SessionStart", [HOOK, SHELL, HARNESS])]
        self.assertEqual(probe_check.check(recs, 500, HARNESS["start"] + "    "), [])

    def test_start_time_mismatch_fails_pid_reuse(self):
        recs = [record("SessionStart", [HOOK, SHELL, HARNESS])]
        self.assertTrue(probe_check.check(recs, 500, "Sun Oct 25 09:00:00 2026"))

    def test_missing_owner_fails(self):
        recs = [record("SessionStart", [HOOK, SHELL])]
        self.assertTrue(probe_check.check(recs, 500, HARNESS["start"]))

    def test_owner_equal_to_hook_pid_fails(self):
        recs = [record("SessionStart", [HARNESS])]
        recs[0]["pid"] = 500
        self.assertTrue(probe_check.check(recs, 500, HARNESS["start"]))

    def test_in_process_extension_owns_itself(self):
        pi = {"pid": 700, "ppid": 1, "start": "s", "comm": "node"}
        recs = [record("session_start", [pi])]
        recs[0]["in_process"] = True
        self.assertEqual(probe_check.check(recs, 700, "s"), [])

    def test_node_hosted_harness_is_owner(self):
        node = {"pid": 800, "ppid": 1, "start": "s", "comm": "node"}
        self.assertEqual(probe_check.discover_owner([HOOK, SHELL, node])["pid"], 800)

    def test_no_records_fails(self):
        self.assertTrue(probe_check.check([], 500, HARNESS["start"]))


class Lifecycle(unittest.TestCase):
    def test_stop_is_idle_and_session_end_is_shutdown(self):
        self.assertEqual(probe_check.lifecycle("Stop"), "idle")
        self.assertEqual(probe_check.lifecycle("SessionEnd"), "shutdown")
        self.assertEqual(probe_check.lifecycle("session_shutdown"), "shutdown")
        self.assertEqual(probe_check.lifecycle("agent_settled"), "idle")
        self.assertIsNone(probe_check.lifecycle("PreToolUse"))


class Matrix(unittest.TestCase):
    def test_absent_identity_is_marked_unavailable(self):
        recs = [record("PermissionRequest", [HOOK, SHELL, HARNESS], session_id=1, turn_id=2)]
        row = probe_check.matrix(recs)["PermissionRequest"]
        self.assertEqual(row["session_id"], "present")
        self.assertEqual(row["tool_use_id"], "unavailable")

    def test_identity_present_in_any_delivery_counts(self):
        recs = [record("PostToolUse", [HOOK, SHELL, HARNESS]),
                record("PostToolUse", [HOOK, SHELL, HARNESS], tool_use_id=1)]
        self.assertEqual(probe_check.matrix(recs)["PostToolUse"]["tool_use_id"], "sometimes")


if __name__ == "__main__":
    unittest.main()
