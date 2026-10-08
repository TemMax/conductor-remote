#!/usr/bin/env python3
"""Exercise the real zsh prompts with a fake GitHub CLI and synthetic secrets."""

import base64
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import tempfile
import time
import unittest


SCRIPT = Path(__file__).with_name("setup-release-secrets.zsh")
NAMES = ["BUILD_CERTIFICATE_BASE64", "P12_PASSWORD", "NOTARY_KEY_BASE64",
         "NOTARY_KEY_ID", "NOTARY_ISSUER_ID", "SPARKLE_PRIVATE_KEY"]
VALUES = [base64.b64encode(b"synthetic certificate").decode(), "  test $()`\\ password  ",
          base64.b64encode(b"synthetic notary key").decode(), "TESTKEY123",
          "11111111-2222-3333-4444-555555555555", base64.b64encode(b"s" * 32).decode()]
FAKE_GH = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
args = sys.argv[1:]
if args[:2] in (["auth", "status"], ["repo", "view"]):
    sys.exit(1 if os.environ.get("FAKE_AUTH_FAIL") else 0)
if args[:2] != ["secret", "set"]:
    sys.exit(2)
value = sys.stdin.buffer.read()
assert value.decode() not in args
assert value.decode() not in os.environ.values()
with open(os.environ["FAKE_RECORD"], "a") as out:
    out.write(json.dumps({"args": args, "digest": hashlib.sha256(value).hexdigest()}) + "\n")
if args[2] == os.environ.get("FAKE_SET_FAIL"):
    print("private diagnostic " + value.decode())
    print("private diagnostic " + value.decode(), file=sys.stderr)
    sys.exit(1)
'''


class PromptProcess:
    def __init__(self, env, repo=None, traced=False, clipboard=False, telegram=False):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            args = ["zsh", "-f"] + (["-x"] if traced else []) + [str(SCRIPT)]
            if clipboard:
                args.append("--clipboard")
            if telegram:
                args.append("--telegram")
            if repo:
                args.append(repo)
            os.execve("/bin/zsh", args, env)
        self.output = b""
        self.cursor = 0
        self.status = None

    def expect(self, text):
        expected = text.encode()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            index = self.output.find(expected, self.cursor)
            if index >= 0:
                self.cursor = index + len(expected)
                return
            if select.select([self.fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.output += chunk
        raise AssertionError(f"Missing prompt {text!r}; output: {self.output!r}")

    def send(self, value):
        os.write(self.fd, value.encode() + b"\n")

    def finish(self):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if select.select([self.fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    chunk = b""
                self.output += chunk
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)
                os.close(self.fd)
                return self.status
        self.close()
        raise AssertionError("Prompt process did not exit")

    def close(self):
        if self.status is None:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            os.close(self.fd)
            self.status = -9


class SetupSecretsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        gh = self.root / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(0o700)
        self.record = self.root / "calls.jsonl"
        self.env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ["PATH"],
                        FAKE_RECORD=str(self.record))

    def clipboard_backend(self):
        self.clipboard = self.root / "synthetic-clipboard"
        self.clipboard_failure = self.root / "clipboard-failure"
        self.env.update(FAKE_CLIPBOARD=str(self.clipboard), FAKE_CLIPBOARD_FAILURE=str(self.clipboard_failure))
        tool = self.root / "pbpaste"
        tool.write_text('''#!/usr/bin/env python3
import os, pathlib, sys
if pathlib.Path(os.environ["FAKE_CLIPBOARD_FAILURE"]).exists():
    print("private clipboard diagnostic", file=sys.stderr)
    sys.exit(1)
sys.stdout.buffer.write(pathlib.Path(os.environ["FAKE_CLIPBOARD"]).read_bytes())
''')
        tool.chmod(0o700)

    def process(self, **kwargs):
        process = PromptProcess(self.env, **kwargs)
        self.addCleanup(process.close)
        return process

    def records(self):
        return [json.loads(line) for line in self.record.read_text().splitlines()] if self.record.exists() else []

    def fill(self, process, values=VALUES):
        for name, value in zip(NAMES, values):
            process.expect(name)
            process.expect("Value (hidden): ")
            process.send(value)

    def test_sequential_hidden_input_and_stdin_preserve_password(self):
        process = self.process(traced=True)
        self.fill(process)
        self.assertEqual(process.finish(), 0)
        records = self.records()
        self.assertEqual([r["args"][2] for r in records], NAMES)
        for record, value in zip(records, VALUES):
            self.assertEqual(record["digest"], hashlib.sha256(value.encode()).hexdigest())
            self.assertEqual(record["args"][3:], ["--app", "actions", "--repo", "TemMax/conductor-remote"])
            self.assertNotIn(value.encode(), process.output)

    def test_file_inputs_custom_repo_and_retry(self):
        certificate = self.root / "certificate with spaces.p12"
        key = self.root / "notary key.p8"
        certificate.write_bytes(b"synthetic certificate\x00\xff")
        key.write_bytes(b"synthetic notary key\n")
        values = list(VALUES)
        values[0] = "@" + str(certificate)
        values[2] = "@" + str(key)
        process = self.process(repo="example/other")
        process.expect(NAMES[0])
        process.expect("Value (hidden): ")
        process.send("@/nonexistent/test-certificate.p12")
        process.expect("Value (hidden): ")
        process.send("not base64!")
        process.expect("Value (hidden): ")
        process.send(values[0])
        for name, value in zip(NAMES[1:], values[1:]):
            process.expect(name)
            process.expect("Value (hidden): ")
            if name == "P12_PASSWORD":
                process.send("")
                process.expect("Value (hidden): ")
            process.send(value)
        self.assertEqual(process.finish(), 0)
        records = self.records()
        self.assertEqual(len(records), 6)
        for record in records:
            self.assertEqual(record["args"][-1], "example/other")
        for index, file in [(0, certificate), (2, key)]:
            self.assertEqual(records[index]["digest"], hashlib.sha256(base64.b64encode(file.read_bytes())).hexdigest())

    def test_upload_failure_is_private_and_stops(self):
        self.env["FAKE_SET_FAIL"] = NAMES[1]
        process = self.process()
        for name, value in zip(NAMES[:2], VALUES[:2]):
            process.expect(name)
            process.expect("Value (hidden): ")
            process.send(value)
        self.assertNotEqual(process.finish(), 0)
        self.assertEqual(len(self.records()), 2)
        self.assertNotIn(b"private diagnostic", process.output)
        self.assertNotIn(VALUES[1].encode(), process.output)

    def test_clipboard_upload_preserves_large_multiline_base64_and_password(self):
        self.clipboard_backend()
        values = list(VALUES)
        values[0] = base64.b64encode(b"synthetic certificate " * 10000).decode()
        process = self.process(clipboard=True, traced=True)
        for name, value in zip(NAMES, values):
            process.expect(name)
            process.expect("Copy the value, then press Enter: ")
            pasted = "\n".join(value[i:i + 76] for i in range(0, len(value), 76)) if name == NAMES[0] else value
            self.clipboard.write_text(pasted)
            process.send("")
        self.assertEqual(process.finish(), 0)
        self.assertEqual(len(self.records()), 6)
        for record, value in zip(self.records(), values):
            self.assertEqual(record["digest"], hashlib.sha256(value.encode()).hexdigest())
            self.assertNotIn(value.encode(), process.output)

    def test_clipboard_errors_and_empty_values_retry_without_uploading(self):
        self.clipboard_backend()
        self.clipboard_failure.touch()
        process = self.process(clipboard=True)
        process.expect("Copy the value, then press Enter: ")
        process.send("")
        process.expect("Cannot read the clipboard; try again.")
        self.assertEqual(self.records(), [])
        self.assertNotIn(b"private clipboard diagnostic", process.output)
        self.clipboard_failure.unlink()
        self.clipboard.write_text("")
        process.expect("Copy the value, then press Enter: ")
        process.send("")
        process.expect("Empty value; try again.")
        self.assertEqual(self.records(), [])
        for name, value in zip(NAMES, VALUES):
            process.expect("Copy the value, then press Enter: ")
            self.clipboard.write_text(value)
            process.send("")
        self.assertEqual(process.finish(), 0)
        self.assertEqual(len(self.records()), 6)

    def test_auth_failure_precedes_input(self):
        self.env["FAKE_AUTH_FAIL"] = "1"
        process = self.process()
        self.assertNotEqual(process.finish(), 0)
        self.assertIn(b"GitHub authentication failed", process.output)
        self.assertNotIn(b"Value (hidden):", process.output)
        self.assertEqual(self.records(), [])

    def test_telegram_mode_only_uploads_two_secrets_from_clipboard(self):
        self.clipboard_backend()
        values = ["synthetic-bot-value", "@example_channel"]
        names = ["TELEGRAM_BOT_TOKEN", "TELEGRAM_CHAT_ID"]
        process = self.process(telegram=True, clipboard=True, traced=True)
        for name, value in zip(names, values):
            process.expect(name)
            process.expect("Copy the value, then press Enter: ")
            self.clipboard.write_text(value)
            process.send("")
        self.assertEqual(process.finish(), 0)
        records = self.records()
        self.assertEqual([r["args"][2] for r in records], names)
        for record, value in zip(records, values):
            self.assertEqual(record["digest"], hashlib.sha256(value.encode()).hexdigest())
            self.assertNotIn(value.encode(), process.output)

    def test_noninteractive_input_rejected(self):
        result = subprocess.run(["zsh", "-f", str(SCRIPT)], input=b"synthetic\n", env=self.env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"interactive terminal", result.stderr)
        self.assertEqual(self.records(), [])

    def test_interrupt_restores_terminal_echo(self):
        import termios
        process = self.process()
        process.expect("Value (hidden): ")
        os.kill(process.pid, signal.SIGINT)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            pid, status = os.waitpid(process.pid, os.WNOHANG)
            if pid:
                process.status = os.waitstatus_to_exitcode(status)
                self.assertTrue(termios.tcgetattr(process.fd)[3] & termios.ECHO)
                os.close(process.fd)
                self.assertNotEqual(process.status, 0)
                return
            time.sleep(0.05)
        self.fail("Interrupted script did not exit")


if __name__ == "__main__":
    unittest.main()
