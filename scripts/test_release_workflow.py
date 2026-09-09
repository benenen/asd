"""Offline release workflow safety contracts (requires PyYAML)."""
import json
import os
import pathlib
import subprocess
import tempfile
import unittest

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]


class ReleaseWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = yaml.load(
            (ROOT / ".github/workflows/release.yml").read_text(), Loader=yaml.BaseLoader
        )
        cls.jobs = cls.workflow["jobs"]

    def test_build_policy_gates_draft_creation_without_blocking_publication(self):
        self.assertIn("build-policy", self.jobs.keys())
        policy = self.jobs["build-policy"]
        self.assertEqual(policy["if"], "github.event_name != 'release'")
        self.assertEqual(self.jobs["prepare"].get("needs"), "build-policy")
        commands = "\n".join(step.get("run", "") for step in policy["steps"])
        self.assertIn("python3 scripts/test_ghostty_cpu_baseline.py", commands)
        self.assertIn("python3 scripts/test_release_workflow.py", commands)
        self.assertIn("PyYAML==6.0.2", commands)
        self.assertNotIn("needs", self.jobs["npm"])

    def test_published_release_is_the_only_npm_trigger(self):
        self.assertEqual(self.workflow["on"].get("release", {}).get("types", []), ["published"])
        job = self.jobs["npm"]
        self.assertNotIn("needs", job, "skipped build jobs must not suppress publication")
        self.assertIn("github.event_name == 'release'", job["if"])
        self.assertIn("github.event.action == 'published'", job["if"])
        checkout = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["ref"], "${{ github.event.release.tag_name }}")

    def test_published_event_cannot_rebuild_or_redraft(self):
        for name in ["build-policy", "prepare", "build", "windows", "macos", "notes"]:
            self.assertIn("github.event_name != 'release'", self.jobs.get(name, {}).get("if", ""), name)
        preparation = self.jobs["prepare"]["steps"][0]["run"]
        self.assertIn("--draft", preparation)
        self.assertIn("--verify-tag", preparation)
        self.assertIn("isDraft", preparation)
        for name in ["build", "windows", "macos"]:
            upload = next(step for step in self.jobs[name]["steps"] if step.get("name") == "Attach to release")
            self.assertNotIn("uses", upload, "upload must not create or redraft releases")
            self.assertIn("isDraft", upload["run"])
            self.assertIn("gh release upload", upload["run"])
        notes = self.jobs["notes"]["steps"][0]["run"]
        edit = next(line for line in notes.splitlines() if "gh release edit" in line)
        self.assertNotIn("--draft", edit)
        self.assertNotIn("--latest", edit)

    def test_prepare_refuses_public_release_and_only_creates_drafts(self):
        script = self.jobs["prepare"]["steps"][0]["run"]
        fake = """#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
if args[:2] == ['release', 'view']:
    state = os.environ['RELEASE_STATE']
    if state == 'missing': sys.exit(1)
    print(state)
elif args[:2] == ['release', 'create']:
    with open(os.environ['CALL_LOG'], 'w') as out: json.dump(args, out)
elif args[0] == 'api':
    sys.exit(1)
else:
    sys.exit(2)
"""
        for state, expected in [("missing", 0), ("true", 0), ("false", 1)]:
            with self.subTest(state=state), tempfile.TemporaryDirectory() as temporary:
                directory = pathlib.Path(temporary)
                tool = directory / "gh"
                tool.write_text(fake)
                tool.chmod(0o755)
                log = directory / "calls.json"
                env = dict(os.environ, PATH=temporary + os.pathsep + os.environ["PATH"],
                           RELEASE_STATE=state, CALL_LOG=str(log), REF_NAME="v0.2.1", REPO="example/asd")
                result = subprocess.run(["bash", "-e", "-c", script], cwd=directory, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, expected, result.stderr)
                if state == "missing":
                    arguments = json.loads(log.read_text())
                    self.assertIn("--draft", arguments)
                    self.assertIn("--verify-tag", arguments)
                else:
                    self.assertFalse(log.exists(), "existing releases must not be recreated")

    def test_each_upload_rechecks_draft_without_creating_or_editing_release(self):
        fake = """#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
if args[:2] == ['release', 'view']:
    state = os.environ['RELEASE_STATE']
    if state == 'missing': sys.exit(1)
    print(state)
elif args[:2] == ['release', 'upload']:
    with open(os.environ['CALL_LOG'], 'w') as out: json.dump(args, out)
else:
    sys.exit(9)
"""
        for job in ["build", "windows", "macos"]:
            upload = next(step for step in self.jobs[job]["steps"] if step.get("name") == "Attach to release")
            self.assertIn("run", upload)
            self.assertEqual(upload.get("shell"), "bash")
            self.assertEqual(upload["env"]["ARTIFACT_PATH"], "${{ steps.pkg.outputs.archive }}")
            for state in ["true", "false", "missing"]:
                with self.subTest(job=job, state=state), tempfile.TemporaryDirectory() as temporary:
                    directory = pathlib.Path(temporary)
                    tool = directory / "gh"
                    tool.write_text(fake)
                    tool.chmod(0o755)
                    artifact = directory / "artifact with spaces.zip"
                    artifact.write_bytes(b"archive")
                    log = directory / "calls.json"
                    env = dict(os.environ, PATH=temporary + os.pathsep + os.environ["PATH"],
                               RELEASE_STATE=state, CALL_LOG=str(log), REF_NAME="v0.2.1",
                               REPO="example/asd", ARTIFACT_PATH=str(artifact))
                    result = subprocess.run(["bash", "-e", "-c", upload["run"]], cwd=directory, env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, state == "true", result.stderr)
                    if state == "true":
                        arguments = json.loads(log.read_text())
                        self.assertIn(str(artifact), arguments)
                        self.assertIn("--clobber", arguments)
                        self.assertEqual(arguments[:2], ["release", "upload"])
                    else:
                        self.assertFalse(log.exists(), "public or absent releases must not receive uploads")

    def test_release_shell_steps_parse(self):
        for job in ["prepare", "notes", "npm"]:
            for step in self.jobs[job]["steps"]:
                if "run" in step:
                    result = subprocess.run(["bash", "-n"], input=step["run"], capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, f"{job}: {result.stderr}")

    def test_asset_guard_rejects_drafts_missing_and_empty_archives(self):
        steps = self.jobs["npm"]["steps"]
        check = next(step for step in steps if step.get("name") == "Verify the published release assets and package version")
        program = check["run"].split("<<'PYCODE'\n", 1)[1].split("\nPYCODE", 1)[0]
        targets = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"]
        archives = [{"name": f"asd-0.2.1-{target}." + ("zip" if "windows" in target else "tar.gz"), "size": 1024} for target in targets]
        valid = dict(tagName="v0.2.1", isDraft=False, isPrerelease=False, assets=archives)
        cases = [(valid, True), (dict(valid, isDraft=True), False),
                 (dict(valid, assets=archives[:-1]), False),
                 (dict(valid, assets=[dict(asset, size=0) for asset in archives]), False),
                 (dict(valid, tagName="v0.2.0"), False)]
        for release, success in cases:
            with self.subTest(release=release), tempfile.TemporaryDirectory() as temporary:
                pathlib.Path(temporary, "release.json").write_text(json.dumps(release))
                result = subprocess.run(["python3", "-", "v0.2.1"], input=program, cwd=temporary, capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, success, result.stderr)

    def test_deprecation_is_after_publish_and_only_for_the_bad_version(self):
        steps = self.jobs["npm"]["steps"]
        publish = next(i for i, step in enumerate(steps) if step.get("name") == "Publish to npm")
        self.assertTrue(any(step.get("name") == "Deprecate the withdrawn 0.2.0 installer" for step in steps))
        deprecate = next(i for i, step in enumerate(steps) if step.get("name") == "Deprecate the withdrawn 0.2.0 installer")
        self.assertGreater(deprecate, publish)
        step = steps[deprecate]
        self.assertEqual(step["if"], "github.event.release.tag_name == 'v0.2.1'")
        self.assertIn("@shibenenen/asd@0.2.0", step["run"])
        self.assertIn("deprecated", step["run"])
        self.assertIn("${{ secrets.NPM_TOKEN }}", step["env"]["NODE_AUTH_TOKEN"])


if __name__ == "__main__":
    unittest.main()
