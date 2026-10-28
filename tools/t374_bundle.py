#!/usr/bin/env python3
"""Prepare the dedicated pi/Codex producer bundle for the t374 acceptance.

No agent is started, no live store is touched, and no owner file is edited.
The bundle contains:

  pi/extensions/dunkingsheep.ts   installed extension + prior patch reversed +
                                  PR72 turn-generation patch applied, with log
  codex/                          PR72 helper/wrapper/hooks + owner-default
                                  config (auth is linked at run time, not copied)
  opencode/dunkingsheep-turn.js   PR72 producer for coverage

The bundle manifest records the source PR72 head and SHA-256 of every producer
file, so the release verifier can pin the exact producers.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def run(argv, *, cwd=None, input_text=None):
    return subprocess.run([str(a) for a in argv], cwd=cwd, input=input_text,
                          capture_output=True, text=True, timeout=120)


def must(argv, **kw):
    result = run(argv, **kw)
    if result.returncode != 0:
        raise SystemExit(f"{' '.join(map(str, argv))} failed:\n"
                         f"{result.stdout}\n{result.stderr}")
    return result


def git_show(ref, path):
    return must(["git", "-C", str(ROOT), "show", f"{ref}:{path}"]).stdout


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def prepare_pi(out, installed, source_ref):
    source = Path(installed).expanduser()
    if not source.is_file():
        raise SystemExit(f"installed pi extension not found: {source}")
    target = out / "pi" / "extensions"
    target.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="t374-pi-patch-"))
    try:
        test = work / "dunkingsheep.ts"
        shutil.copyfile(source, test)
        prior = git_show("HEAD", "integrations/pi/turn-generation.patch")
        pr72 = git_show(source_ref, "integrations/pi/turn-generation.patch")
        (work / "prior.patch").write_text(prior)
        (work / "pr72.patch").write_text(pr72)
        reverse = run(["patch", "-R", "-p1"], cwd=work, input_text=prior)
        if reverse.returncode != 0:
            raise SystemExit(f"prior pi patch would not reverse:\n{reverse.stdout}\n{reverse.stderr}")
        apply = run(["patch", "-p1"], cwd=work, input_text=pr72)
        if apply.returncode != 0:
            raise SystemExit(f"PR72 pi patch would not apply:\n{apply.stdout}\n{apply.stderr}")
        text = test.read_text()
        checks = {
            "before_agent_start_user_prompt": 'pi.on("before_agent_start", async (event, ctx) =>' in text,
            "prompt_argument": 'function turn(kind: "start" | "end", cwd: string, prompt = "")' in text,
            "handoff_id_echo": 'args.push("--handoff-id", handoff[1])' in text,
            "handoff_regex": r"\bHandoff (nj-[0-9a-f]{32})\." in text,
        }
        if not all(checks.values()):
            raise SystemExit(f"patched pi extension failed checks: {checks}")
        shutil.copyfile(test, target / "dunkingsheep.ts")
        log = [
            f"installed source: {source}",
            f"installed sha256: {sha(source)}",
            f"prior patch sha256: {hashlib.sha256(prior.encode()).hexdigest()}",
            f"PR72 patch sha256: {hashlib.sha256(pr72.encode()).hexdigest()}",
            f"source ref: {source_ref}",
            f"source head: {must(['git', '-C', str(ROOT), 'rev-parse', source_ref]).stdout.strip()}",
            "== reverse prior ==", reverse.stdout.strip(), reverse.stderr.strip(),
            "== apply PR72 ==", apply.stdout.strip(), apply.stderr.strip(),
            f"result sha256: {sha(target / 'dunkingsheep.ts')}",
            f"checks: {json.dumps(checks, sort_keys=True)}",
        ]
        (out / "pi" / "PATCH-APPLICATION.txt").write_text("\n".join(log) + "\n")
        return target / "dunkingsheep.ts", checks, log
    finally:
        shutil.rmtree(work, ignore_errors=True)


def prepare_codex(out, owner_config, source_ref):
    directory = out / "codex"
    directory.mkdir(parents=True, exist_ok=True)
    helper = directory / "dunkingsheep-turn.py"
    wrapper = directory / "dunkingsheep-turn.sh"
    helper.write_text(git_show(source_ref, "integrations/codex/turn-generation.py"))
    wrapper.write_text(git_show(source_ref, "integrations/codex/dunkingsheep-turn.sh"))
    os.chmod(wrapper, 0o700)
    hooks = {
        "hooks": {
            "UserPromptSubmit": [{"hooks": [{
                "command": f"bash '{wrapper}' start", "timeout": 30,
                "type": "command"}]}],
            "Stop": [{"hooks": [{
                "command": f"bash '{wrapper}' end", "timeout": 30,
                "type": "command"}]}],
        }
    }
    (directory / "hooks.json").write_text(json.dumps(hooks, indent=2) + "\n")
    config = Path(owner_config).expanduser()
    if not config.is_file():
        raise SystemExit(f"owner Codex config not found: {config}")
    shutil.copyfile(config, directory / "config.toml")
    (directory / "AUTH.md").write_text(
        "At run time only, link the existing owner auth; never copy it into the bundle:\n"
        f"  ln -sf {Path.home() / '.codex' / 'auth.json'} {directory / 'auth.json'}\n"
        "The dedicated CODEX_HOME uses the owner-default model configuration from "
        "config.toml; no --model override is used.\n")
    return directory


def prepare_opencode(out, source_ref):
    directory = out / "opencode"
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / "dunkingsheep-turn.js"
    target.write_text(git_show(source_ref, "integrations/opencode/dunkingsheep-turn.js"))
    return target


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True)
    parser.add_argument("--source-ref", default="c4042b5d8c36eb7426fd2a04653d13751fbecc4b",
                        help="PR72 head containing the handoff echo producers")
    parser.add_argument("--installed-extension",
                        default=os.path.expanduser("~/.pi/agent/extensions/dunkingsheep.ts"))
    parser.add_argument("--owner-codex-config",
                        default=os.path.expanduser("~/.codex/config.toml"))
    parser.add_argument("--run", default="", help="optional run dir for the launch note")
    args = parser.parse_args(argv)
    out = Path(args.out).expanduser().resolve()
    if out.exists() and any(out.iterdir()):
        raise SystemExit(f"refusing to overwrite non-empty {out}")
    out.mkdir(parents=True, exist_ok=True)
    source_head = must(["git", "-C", str(ROOT), "rev-parse", args.source_ref]).stdout.strip()
    pi_file, checks, patch_log = prepare_pi(out, args.installed_extension, args.source_ref)
    codex_dir = prepare_codex(out, args.owner_codex_config, args.source_ref)
    opencode_file = prepare_opencode(out, args.source_ref)
    files = {}
    for path in sorted(out.rglob("*")):
        if path.is_file():
            files[str(path.relative_to(out))] = sha(path)
    manifest = {
        "created_at": time.time(),
        "source_ref": args.source_ref,
        "source_head": source_head,
        "pi_extension": str(pi_file),
        "pi_patch_checks": checks,
        "pi_patch_log": "pi/PATCH-APPLICATION.txt",
        "codex_home": str(codex_dir),
        "codex_auth": "link at run time with codex/AUTH.md; not copied",
        "opencode_plugin": str(opencode_file),
        "files": files,
        "run": args.run or None,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    launch = [
        "# t374 dedicated producer bundle",
        "",
        "No agent was started and no live store was touched.",
        "",
        "1. Link auth for the dedicated Codex home (runtime only; no secret in the bundle):",
        f"   ln -sf {Path.home() / '.codex' / 'auth.json'} {codex_dir / 'auth.json'}",
        "2. Launch the dedicated lanes with the exact commands in the run's LAUNCH.md.",
        "   They sleep 30 seconds; run `t374_acceptance.py prepare` within that window.",
        "3. Do not send any further prompt or key. A is the only initial prompt; B must",
        "   arrive from the automatic handoff.",
        "",
        f"source head: {source_head}",
        f"pi extension: {pi_file}",
        f"codex home: {codex_dir}",
        f"opencode plugin: {opencode_file}",
        "",
        "See pi/PATCH-APPLICATION.txt for the validated patch application and checksums.",
    ]
    (out / "LAUNCH.md").write_text("\n".join(launch) + "\n")
    print(json.dumps({"bundle": str(out), "manifest": str(out / "manifest.json"),
                      "source_head": source_head, "files": files}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
