import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONFIGURE = ROOT / "scripts" / "configure.py"
BIN = "/opt/coach/bin/coach"

SAFETY = {
    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "~/.claude/hooks/safety.sh", "timeout": 5}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "/opt/my-coach hook claude"}]}],
}


def run(home, *args, env=None, bin=BIN):
    e = {**os.environ, "HOME": str(home), "CODEX_HOME": str(home / ".codex"), **(env or {})}
    return subprocess.run(
        [sys.executable, "-I", str(CONFIGURE), *args, "--home", str(home), "--coach-bin", bin],
        capture_output=True, text=True, env=e,
    )


def coach_hooks(settings):
    return [
        h for groups in settings.get("hooks", {}).values() for g in groups for h in g["hooks"]
        if h["command"] == f"{BIN} hook claude" or h["command"] == f"{BIN} hook codex"
    ]


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def claude_settings(self, data=None):
        p = self.home / ".claude" / "settings.json"
        p.parent.mkdir(parents=True, exist_ok=True)
        if data is not None:
            p.write_text(json.dumps(data, indent=2))
        return p


class Claude(Base):
    def test_install_into_empty_claude_home_then_idempotent(self):
        self.claude_settings()
        r = run(self.home, "install", "--harness", "claude")
        self.assertEqual(r.returncode, 0, r.stderr)
        p = self.home / ".claude" / "settings.json"
        first = p.read_bytes()
        settings = json.loads(first)
        hooks = settings["hooks"]
        self.assertIn("PostToolUseFailure", hooks)
        stop = [h for g in hooks["Stop"] for h in g["hooks"]][0]
        self.assertNotIn("async", stop, "Stop must stay synchronous (async Stop is not delivered)")
        pre = [h for g in hooks["PreToolUse"] for h in g["hooks"]][0]
        self.assertTrue(pre["async"])
        r = run(self.home, "install", "--harness", "claude")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(p.read_bytes(), first, "second install must change nothing")

    def test_preserves_safety_hooks_and_other_settings(self):
        original = {"permissions": {"allow": ["Read"], "defaultMode": "default"}, "hooks": SAFETY}
        p = self.claude_settings(original)
        self.assertEqual(run(self.home, "install", "--harness", "claude").returncode, 0)
        after = json.loads(p.read_text())
        self.assertEqual(after["permissions"], original["permissions"])
        self.assertEqual(after["hooks"]["PreToolUse"][0], SAFETY["PreToolUse"][0])
        self.assertIn(SAFETY["Stop"][0], after["hooks"]["Stop"])

    def test_remove_restores_original_semantics_and_keeps_foreign_lookalikes(self):
        original = {"hooks": {**SAFETY, "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "coach hook claude --verbose"}]}]}}
        p = self.claude_settings(original)
        run(self.home, "install", "--harness", "claude")
        self.assertTrue(coach_hooks(json.loads(p.read_text())))
        r = run(self.home, "remove", "--harness", "claude")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(p.read_text()), original)
        self.assertTrue(list(p.parent.glob("settings.json.coach-backup-*")), "a backup is kept")

    def test_malformed_json_is_left_untouched(self):
        p = self.claude_settings()
        p.write_text("{not json")
        r = run(self.home, "install", "--harness", "claude")
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(p.read_text(), "{not json")

    def test_symlinked_settings_are_managed_elsewhere(self):
        real = self.home / "dotfiles" / "settings.json"
        real.parent.mkdir()
        real.write_text("{}")
        self.claude_settings()
        (self.home / ".claude" / "settings.json").symlink_to(real)
        r = run(self.home, "install", "--harness", "claude")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("managed", r.stderr)
        self.assertEqual(real.read_text(), "{}")

    def test_harness_dir_inside_a_git_repo_is_managed(self):
        repo = self.home / "dotfiles"
        (repo / ".git").mkdir(parents=True)
        (repo / ".claude").mkdir()
        (repo / ".claude" / "settings.json").write_text("{}")
        (self.home / ".claude").symlink_to(repo / ".claude")
        r = run(self.home, "install", "--harness", "claude")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("managed", r.stderr)
        self.assertEqual((repo / ".claude" / "settings.json").read_text(), "{}")
        r = run(self.home, "install", "--harness", "claude", "--allow-managed")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(coach_hooks(json.loads((repo / ".claude" / "settings.json").read_text())))
        self.assertFalse(list((repo / ".claude").glob("*.coach-backup-*")), "no backup litter inside a repo; Git is the backup")

    def test_home_relative_binary_for_shared_config(self):
        p = self.claude_settings({})
        r = run(self.home, "install", "--harness", "claude", bin="$HOME/.local/bin/coach")
        self.assertEqual(r.returncode, 0, r.stderr)
        commands = {h["command"] for g in json.loads(p.read_text())["hooks"]["Stop"] for h in g["hooks"]}
        self.assertEqual(commands, {"$HOME/.local/bin/coach hook claude"})
        self.assertNotEqual(run(self.home, "install", "--harness", "claude", bin="coach").returncode, 0)

    def test_dry_run_changes_nothing(self):
        p = self.claude_settings({"hooks": SAFETY})
        before = p.read_bytes()
        r = run(self.home, "install", "--harness", "claude", "--dry-run")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("PreToolUse", r.stdout)
        self.assertEqual(p.read_bytes(), before)
        self.assertFalse(list(p.parent.glob("*.coach-backup-*")))

    def test_absent_harness_is_skipped(self):
        r = run(self.home, "install", "--harness", "all")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse((self.home / ".claude").exists())
        self.assertIn("skip", r.stdout.lower())

    def test_removal_never_touches_evidence(self):
        self.claude_settings()
        state = self.home / "state"
        state.mkdir(mode=0o700)
        (state / "coach.db").write_text("evidence")
        run(self.home, "install", "--harness", "claude", env={"COACH_STATE_DIR": str(state)})
        run(self.home, "remove", "--harness", "all", env={"COACH_STATE_DIR": str(state)})
        self.assertEqual((state / "coach.db").read_text(), "evidence")

    def test_writes_are_atomic_and_mode_preserved(self):
        p = self.claude_settings({})
        os.chmod(p, 0o600)
        run(self.home, "install", "--harness", "claude")
        self.assertEqual(os.stat(p).st_mode & 0o777, 0o600)
        leftovers = [x.name for x in p.parent.iterdir() if ".tmp" in x.name]
        self.assertEqual(leftovers, [])


class Codex(Base):
    def codex(self, hooks=None, config="[features]\nhooks = true\n"):
        d = self.home / ".codex"
        d.mkdir()
        (d / "config.toml").write_text(config)
        if hooks is not None:
            (d / "hooks.json").write_text(json.dumps(hooks))
        return d

    def test_install_merges_and_explains_trust(self):
        d = self.codex({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "herdr.sh", "timeout": 5}]}]}})
        r = run(self.home, "install", "--harness", "codex")
        self.assertEqual(r.returncode, 0, r.stderr)
        hooks = json.loads((d / "hooks.json").read_text())["hooks"]
        self.assertEqual(hooks["Stop"][0]["hooks"][0]["command"], "herdr.sh")
        self.assertTrue(coach_hooks({"hooks": hooks}))
        self.assertIn("trust", r.stdout.lower())
        self.assertNotIn("trusted_hash", (d / "config.toml").read_text(), "never writes trust itself")

    def test_timeouts_fit_codex_limits(self):
        # Codex clamps Interrupt and SessionEnd hooks to 3 s and warns at startup.
        d = self.codex({})
        run(self.home, "install", "--harness", "codex")
        hooks = json.loads((d / "hooks.json").read_text())["hooks"]
        timeouts = {e: g[0]["hooks"][0]["timeout"] for e, g in hooks.items()}
        self.assertEqual({e for e, t in timeouts.items() if t > 3}, set())

    def test_reinstall_updates_an_outdated_coach_hook_in_place(self):
        old = {"hooks": [{"type": "command", "command": "/old/place/coach hook codex", "timeout": 5}]}
        d = self.codex({"hooks": {"Stop": [old, {"hooks": [{"type": "command", "command": "guard.sh"}]}]}})
        r = run(self.home, "install", "--harness", "codex")
        self.assertEqual(r.returncode, 0, r.stderr)
        stop = json.loads((d / "hooks.json").read_text())["hooks"]["Stop"]
        self.assertEqual([g["hooks"][0]["command"] for g in stop], [f"{BIN} hook codex", "guard.sh"])
        self.assertEqual(stop[0]["hooks"][0]["timeout"], 3)
        self.assertIn("re-trust", r.stdout)

    def test_remove_keeps_empty_lists_it_did_not_empty(self):
        original = {"hooks": {"Notification": [], "PreCompact": [{"hooks": []}]}}
        d = self.codex(original)
        run(self.home, "install", "--harness", "codex")
        run(self.home, "remove", "--harness", "codex")
        self.assertEqual(json.loads((d / "hooks.json").read_text()), original)

    def test_hooks_feature_disabled_is_reported_not_rewritten(self):
        d = self.codex({}, config='model = "x"\n')
        r = run(self.home, "install", "--harness", "codex")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("features.hooks", r.stdout)
        self.assertEqual((d / "config.toml").read_text(), 'model = "x"\n')

    def test_malformed_toml_stops(self):
        self.codex({}, config="[features\n")
        self.assertNotEqual(run(self.home, "install", "--harness", "codex").returncode, 0)

    def test_remove_keeps_foreign_hooks(self):
        original = {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "herdr.sh"}]}]}}
        d = self.codex(original)
        run(self.home, "install", "--harness", "codex")
        run(self.home, "remove", "--harness", "codex")
        self.assertEqual(json.loads((d / "hooks.json").read_text()), original)

    def test_remove_keeps_later_hooks_at_their_trusted_position(self):
        # Codex trust keys are positional (event:group:hook); a hook added after Coach
        # must keep its group index or Codex silently untrusts it.
        foreign = lambda c: {"hooks": [{"type": "command", "command": c}]}
        coach = {"hooks": [{"type": "command", "command": f"{BIN} hook codex", "timeout": 5}]}
        d = self.codex({"hooks": {"Stop": [foreign("a.sh"), coach, foreign("guard.sh")]}})
        r = run(self.home, "remove", "--harness", "codex")
        self.assertEqual(r.returncode, 0, r.stderr)
        stop = json.loads((d / "hooks.json").read_text())["hooks"]["Stop"]
        self.assertEqual([g["hooks"][0]["command"] if g["hooks"] else None for g in stop], ["a.sh", None, "guard.sh"])


class Pi(Base):
    def test_install_and_remove_extension(self):
        ext = self.home / ".pi" / "agent" / "extensions"
        ext.mkdir(parents=True)
        (ext / "other.ts").write_text("// foreign")
        r = run(self.home, "install", "--harness", "pi")
        self.assertEqual(r.returncode, 0, r.stderr)
        index = ext / "coach" / "index.ts"
        self.assertEqual(index.read_text(), (ROOT / "adapters" / "pi" / "index.ts").read_text())
        self.assertIn("COACH_BIN", r.stdout)
        run(self.home, "remove", "--harness", "pi")
        self.assertFalse((ext / "coach").exists())
        self.assertEqual((ext / "other.ts").read_text(), "// foreign")

    def test_foreign_coach_directory_is_not_overwritten_or_removed(self):
        ext = self.home / ".pi" / "agent" / "extensions" / "coach"
        ext.mkdir(parents=True)
        (ext / "index.ts").write_text("// someone else's")
        self.assertNotEqual(run(self.home, "install", "--harness", "pi").returncode, 0)
        run(self.home, "remove", "--harness", "pi")
        self.assertEqual((ext / "index.ts").read_text(), "// someone else's")


if __name__ == "__main__":
    unittest.main()
