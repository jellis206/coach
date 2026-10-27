#!/usr/bin/env python3
"""Register or remove Coach's observers for Claude Code, Codex and Pi.

    python3 scripts/configure.py install|remove --harness claude|codex|pi|all
        [--home PATH] [--coach-bin PATH] [--dry-run] [--allow-managed]

Only Coach-owned entries are added or removed: hook commands exactly equal to
"<coach-bin> hook <harness>" (or any absolute ".../coach hook <harness>" on
removal) and a Pi extension directory carrying a .coach-managed marker.
Everything else in the files is preserved. Changes are written atomically
after a timestamped backup. Evidence is never touched.
"""
import argparse
import json
import os
import re
import shutil
import sys
import tempfile
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PI_SOURCE = ROOT / "adapters" / "pi" / "index.ts"
MARKER = ".coach-managed"

CLAUDE_EVENTS = [
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure",
    "PermissionRequest", "Notification", "Stop", "SubagentStart", "SubagentStop", "PreCompact",
]
# Claude 2.1.311 never delivered async Stop hooks; these stay synchronous.
CLAUDE_SYNC = {"Stop", "SubagentStop"}
CLAUDE_MATCHED = {"PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest"}
CODEX_EVENTS = [
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PermissionRequest", "PostToolUse",
    "SubagentStart", "SubagentStop", "Stop", "Interrupt", "PreCompact",
]
CODEX_MATCHED = {"PreToolUse", "PermissionRequest", "PostToolUse"}


class Refuse(Exception):
    pass


def owned(command, harness):
    return re.fullmatch(rf"(\S*/)?coach hook {harness}", command or "") is not None


def git_root(path):
    for p in [path.resolve(), *path.resolve().parents]:
        if (p / ".git").exists():
            return p
    return None


def check_unmanaged(path, allow_managed):
    """Refuse config that lives in a Git checkout (e.g. dotfiles); returns that repo when allowed."""
    if path.is_symlink():
        raise Refuse(f"{path} is a symlink (managed elsewhere, e.g. dotfiles); register Coach through that integration")
    repo = git_root(path.parent)
    if repo and not allow_managed:
        raise Refuse(
            f"{path} resolves into the Git checkout {repo} (managed config, e.g. dotfiles); "
            "register Coach through that integration, or pass --allow-managed and commit the change there"
        )
    return repo


def load_json(path):
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text() or "{}")
    except json.JSONDecodeError as e:
        raise Refuse(f"{path} is not valid JSON ({e}); fix it first, nothing was changed")
    if not isinstance(data, dict) or not isinstance(data.get("hooks", {}), dict):
        raise Refuse(f"{path} has an unexpected layout; nothing was changed")
    return data


def write_atomic(path, text, dry_run, backup=True):
    if dry_run:
        return
    mode = path.stat().st_mode & 0o777 if path.exists() else 0o600
    if backup and path.exists():
        shutil.copy2(path, path.with_name(f"{path.name}.coach-backup-{time.strftime('%Y%m%d%H%M%S')}"))
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    except BaseException:
        os.unlink(tmp)
        raise


def merge_hooks(data, harness, command, events, entry_for):
    """Add one Coach hook per event unless an owned one exists. Returns added event names."""
    hooks = data.setdefault("hooks", {})
    added = []
    for event in events:
        groups = hooks.setdefault(event, [])
        if any(owned(h.get("command"), harness) for g in groups for h in g.get("hooks", [])):
            continue
        groups.append(entry_for(event, command))
        added.append(event)
    return added


def strip_hooks(data, harness):
    """Remove owned hooks; drop groups and events that only held Coach. Returns removed count."""
    hooks = data.get("hooks", {})
    removed = 0
    for event in list(hooks):
        kept_groups = []
        for g in hooks[event]:
            kept = [h for h in g.get("hooks", []) if not owned(h.get("command"), harness)]
            removed += len(g.get("hooks", [])) - len(kept)
            if kept or not g.get("hooks"):  # groups emptied by removing Coach are dropped
                kept_groups.append({**g, "hooks": kept})
        if kept_groups:
            hooks[event] = kept_groups
        else:
            del hooks[event]
    if "hooks" in data and not data["hooks"]:
        del data["hooks"]
    return removed


def claude_entry(event, command):
    hook = {"type": "command", "command": command, "timeout": 10}
    if event not in CLAUDE_SYNC:
        hook["async"] = True
    group = {"hooks": [hook]}
    if event in CLAUDE_MATCHED:
        group = {"matcher": "*", **group}
    return group


def codex_entry(event, command):
    group = {"hooks": [{"type": "command", "command": command, "timeout": 5}]}
    if event in CODEX_MATCHED:
        group = {"matcher": "*", **group}
    return group


def json_harness(path, harness, action, bin_path, events, entry_for, dry_run, allow_managed):
    repo = check_unmanaged(path, allow_managed)
    data = load_json(path)
    original = json.dumps(data, sort_keys=True)
    if action == "install":
        changed = merge_hooks(data, harness, f"{bin_path} hook {harness}", events, entry_for)
        summary = f"add {harness} hooks: {', '.join(changed)}" if changed else f"{harness} hooks already registered"
    else:
        n = strip_hooks(data, harness)
        summary = f"remove {n} {harness} hook(s)" if n else f"no {harness} hooks to remove"
    if json.dumps(data, sort_keys=True) != original:
        write_atomic(path, json.dumps(data, indent=2) + "\n", dry_run, backup=repo is None)  # in a repo, Git is the backup
    return summary


def claude(home, action, bin_path, dry_run, allow_managed):
    base = home / ".claude"
    if not base.is_dir():
        return ["skip claude: ~/.claude not found"]
    out = [json_harness(base / "settings.json", "claude", action, bin_path, CLAUDE_EVENTS, claude_entry, dry_run, allow_managed)]
    if action == "install":
        out.append("restart Claude Code sessions to load the hooks; run `/hooks` to review them")
    return out


def codex(home, action, bin_path, dry_run, allow_managed):
    base = Path(os.environ.get("CODEX_HOME") or home / ".codex")
    if not base.is_dir():
        return ["skip codex: Codex home not found"]
    config = base / "config.toml"
    try:
        cfg = tomllib.loads(config.read_text()) if config.exists() else {}
    except tomllib.TOMLDecodeError as e:
        raise Refuse(f"{config} is not valid TOML ({e}); nothing was changed")
    out = [json_harness(base / "hooks.json", "codex", action, bin_path, CODEX_EVENTS, codex_entry, dry_run, allow_managed)]
    if action == "install":
        if cfg.get("features", {}).get("hooks") is not True:
            out.append(f"Codex hooks are off: set `features.hooks = true` in {config} yourself (not edited)")
        out.append(
            "Codex runs new hooks only after you trust them: start `codex`, review the Coach hooks when asked "
            "(or with /hooks), and approve them. Untrusted hooks are skipped silently."
        )
    return out


def pi(home, action, bin_path, dry_run, allow_managed):
    ext = home / ".pi" / "agent" / "extensions"
    if not ext.is_dir():
        return ["skip pi: ~/.pi/agent/extensions not found"]
    target = ext / "coach"
    check_unmanaged(target, allow_managed)
    ours = (target / MARKER).exists()
    if action == "remove":
        if not target.exists():
            return ["no pi extension to remove"]
        if not ours:
            raise Refuse(f"{target} exists but was not installed by Coach; leaving it")
        if not dry_run:
            shutil.rmtree(target)
        return [f"remove {target}"]
    if target.exists() and not ours:
        raise Refuse(f"{target} exists but was not installed by Coach; leaving it")
    if not dry_run:
        target.mkdir(exist_ok=True)
        write_atomic(target / "index.ts", PI_SOURCE.read_text(), dry_run)
        (target / MARKER).write_text("installed by coach scripts/configure.py\n")
    note = "" if shutil.which("coach") == bin_path else f" (coach is not first on PATH: set COACH_BIN={bin_path} for Pi)"
    return [f"install {target}/index.ts; Pi finds the binary on PATH or via COACH_BIN{note}", "run /reload in open Pi sessions"]


HARNESSES = {"claude": claude, "codex": codex, "pi": pi}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("action", choices=["install", "remove"])
    ap.add_argument("--harness", choices=[*HARNESSES, "all"], required=True)
    ap.add_argument("--home", type=Path, default=Path.home())
    ap.add_argument("--coach-bin", help="absolute path of the coach binary (default: from PATH)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--allow-managed", action="store_true", help="write config that lives in a Git checkout (e.g. dotfiles)")
    args = ap.parse_args()

    bin_path = args.coach_bin or shutil.which("coach")
    if args.action == "install" and (not bin_path or not os.path.isabs(bin_path)):
        print("coach: binary not found; install it first or pass --coach-bin /absolute/path", file=sys.stderr)
        return 2
    names = list(HARNESSES) if args.harness == "all" else [args.harness]
    status = 0
    for name in names:
        try:
            for line in HARNESSES[name](args.home.resolve(), args.action, bin_path, args.dry_run, args.allow_managed):
                print(("[dry-run] " if args.dry_run else "") + line)
        except Refuse as e:
            print(f"{name}: {e}", file=sys.stderr)
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
