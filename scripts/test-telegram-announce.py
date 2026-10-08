#!/usr/bin/env python3
"""Test announcement guards and payloads without contacting Telegram."""

import contextlib
import importlib.util
import io
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError
from html.parser import HTMLParser

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).with_name("telegram-announce.py")
TAG = "v1.2.3"
TOKEN = "synthetic-bot-value"
CHAT = "@example_channel"
NOTES = "## Changes\n\n- **Faster** updates with `a < b`.\n- [Details](https://example.com/?a=1&b=2)\n"


class HtmlCheck(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.stack = []
        self.text = ""

    def handle_starttag(self, tag, attrs):
        assert tag in {"b", "code", "a", "pre"}, tag
        self.stack.append(tag)

    def handle_endtag(self, tag):
        assert self.stack.pop() == tag

    def handle_data(self, data):
        self.text += data


class AnnouncementTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.exists(), "Telegram announcement implementation is missing")
        spec = importlib.util.spec_from_file_location("telegram_announce", SCRIPT)
        self.app = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.app)
        self.release = {"body": NOTES, "isDraft": False, "isPrerelease": False,
                        "assets": [{"name": "Conductor-Remote-1.2.3.dmg"}]}
        self.commands = []
        self.requests = []
        self.responses = [{"ok": True}]
        self.output = io.StringIO()
        self.addCleanup(self.output.close)

    def gh(self, args, **kwargs):
        self.commands.append(args)
        data = {"tag_name": TAG} if args[1] == "api" else self.release
        return subprocess.CompletedProcess(args, 0, json.dumps(data))

    def http(self, request, **kwargs):
        self.requests.append((request.full_url, json.loads(request.data)))
        response = self.responses.pop(0)
        if isinstance(response, Exception):
            raise response
        return io.BytesIO(json.dumps(response).encode())

    def run_app(self, args=None, secrets=True, gh=None):
        env = {"TELEGRAM_BOT_TOKEN": TOKEN, "TELEGRAM_CHAT_ID": CHAT} if secrets else {}
        with patch.dict(os.environ, env, clear=True), \
             patch.object(self.app.subprocess, "run", side_effect=gh or self.gh), \
             patch.object(self.app.urllib.request, "urlopen", side_effect=self.http), \
             contextlib.redirect_stdout(self.output), contextlib.redirect_stderr(self.output):
            result = self.app.main(args or [TAG])
        self.assertNotIn(TOKEN, self.output.getvalue())
        self.assertNotIn(CHAT, self.output.getvalue())
        return result

    def test_latest_stable_release_sends_notes_with_native_buttons(self):
        self.assertEqual(self.run_app(), 0)
        self.assertEqual(len(self.requests), 1)
        url, body = self.requests[0]
        self.assertTrue(url.endswith("/sendRichMessage"))
        self.assertEqual(body["chat_id"], CHAT)
        rich = body["rich_message"]["markdown"]
        self.assertIn("# Conductor Remote 1.2.3", rich)
        self.assertIn("**Faster**", rich)
        self.assertIn("Conductor-Remote-1.2.3.dmg", rich)
        self.assertIn('<tg-button type="url" style="success"', rich)
        self.assertIn('/releases/tag/v1.2.3', rich)
        self.assertIn("TemMax/conductor-remote", " ".join(self.commands[0]))

    def test_rejection_uses_valid_html_fallback_without_raw_errors(self):
        self.responses = [{"ok": False, "description": TOKEN + CHAT}, {"ok": True}]
        self.assertEqual(self.run_app(), 0)
        self.assertEqual(len(self.requests), 2)
        url, body = self.requests[1]
        self.assertTrue(url.endswith("/sendMessage"))
        self.assertEqual(body["parse_mode"], "HTML")
        self.assertEqual(body["link_preview_options"], {"is_disabled": True})
        self.assertIn("<b>Faster</b>", body["text"])
        self.assertIn("<code>a &lt; b</code>", body["text"])
        parser = HtmlCheck()
        parser.feed(body["text"])
        self.assertEqual(parser.stack, [])

    def test_http_rejection_can_fall_back(self):
        rejection = HTTPError("https://example.com/" + TOKEN, 400, CHAT, {},
                              io.BytesIO(json.dumps({"ok": False, "description": TOKEN}).encode()))
        self.responses = [rejection, {"ok": True}]
        self.assertEqual(self.run_app(), 0)
        self.assertEqual(len(self.requests), 2)

    def test_ambiguous_transport_or_response_failure_does_not_resend(self):
        for response in [URLError(TOKEN + CHAT), TimeoutError(TOKEN),
                         http.client.InvalidURL(TOKEN + CHAT), {}, {"ok": "true"}]:
            with self.subTest(response=type(response).__name__):
                self.requests.clear()
                self.responses = [response]
                self.assertEqual(self.run_app(), 1)
                self.assertEqual(len(self.requests), 1)

    def test_fallback_rejection_fails_without_sensitive_description(self):
        self.responses = [{"ok": False}, {"ok": False, "description": TOKEN + CHAT}]
        self.assertEqual(self.run_app(), 1)
        self.assertEqual(len(self.requests), 2)

    def test_invalid_tags_never_contact_github_or_telegram(self):
        for tag in ["v1.2.3-rc.1", "main", "v1.2.3\n", "$(example)", "v１.2.3"]:
            with self.subTest(tag=tag):
                self.assertEqual(self.run_app([tag]), 1)
        self.assertEqual(self.commands, [])
        self.assertEqual(self.requests, [])

    def test_unpublished_or_incomplete_releases_never_send(self):
        for field, value in [("isDraft", True), ("isPrerelease", True), ("assets", []), ("body", "")]:
            with self.subTest(field=field):
                original = self.release[field]
                self.release[field] = value
                self.assertEqual(self.run_app(), 1)
                self.release[field] = original
        self.assertEqual(self.requests, [])

    def test_old_release_never_sends(self):
        def old(args, **kwargs):
            if args[1] == "api":
                return subprocess.CompletedProcess(args, 0, '{"tag_name":"v9.0.0"}')
            return self.gh(args, **kwargs)
        self.assertEqual(self.run_app(gh=old), 1)
        self.assertEqual(self.requests, [])

    def test_github_failure_and_missing_secrets_fail_without_sending(self):
        def fail(args, **kwargs):
            raise subprocess.CalledProcessError(1, args, output=TOKEN, stderr=CHAT)
        self.assertEqual(self.run_app(gh=fail), 1)
        self.assertEqual(self.run_app(secrets=False), 1)
        self.assertEqual(self.requests, [])

    def test_dry_runs_render_without_secrets_or_network_posts(self):
        for option in ["--dry-run", "--dry-run-fallback"]:
            self.assertEqual(self.run_app([option, TAG], secrets=False), 0)
        self.assertIn("# Conductor Remote 1.2.3", self.output.getvalue())
        self.assertIn("<b>Conductor Remote 1.2.3 is out</b>", self.output.getvalue())
        self.assertEqual(self.requests, [])

    def test_html_in_notes_is_escaped_and_long_unicode_fallback_stays_valid(self):
        self.release["body"] = "<tg-button url=\"https://example.com\">Injected</tg-button>\n" + (
            "- **" + "😀" * 150 + "**\n") * 40
        self.responses = [{"ok": False}, {"ok": True}]
        self.assertEqual(self.run_app(), 0)
        rich = self.requests[0][1]["rich_message"]["markdown"]
        self.assertIn("&lt;tg-button", rich)
        parser = HtmlCheck()
        parser.feed(self.requests[1][1]["text"])
        self.assertEqual(parser.stack, [])
        self.assertLessEqual(len(parser.text.encode("utf-16-le")) // 2, 4096)
        self.assertIn("…", parser.text)
        self.assertIn("Release notes", parser.text)


if __name__ == "__main__":
    unittest.main()
