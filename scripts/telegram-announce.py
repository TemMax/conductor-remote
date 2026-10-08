#!/usr/bin/env python3
"""Announce the latest stable GitHub release; never log Telegram diagnostics."""

import argparse
import html
import http.client
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

REPO = "TemMax/conductor-remote"


class AnnouncementError(Exception):
    """Contains only a fixed, safe-to-log explanation."""


def github_json(*args):
    try:
        result = subprocess.run(["gh", *args], check=True, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, text=True, timeout=30)
        return json.loads(result.stdout)
    except (OSError, subprocess.SubprocessError, ValueError):
        raise AnnouncementError("Cannot read the published GitHub release.") from None


def release_notes(tag):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise AnnouncementError("Use a stable release tag of the form vX.Y.Z.")
    release = github_json("release", "view", tag, "--repo", REPO,
                          "--json", "body,isDraft,isPrerelease,assets")
    if release.get("isDraft") is not False or release.get("isPrerelease") is not False:
        raise AnnouncementError("Only a published stable release can be announced.")
    latest = github_json("api", f"repos/{REPO}/releases/latest")
    if latest.get("tag_name") != tag:
        raise AnnouncementError("Only the latest stable release can be announced.")
    dmg = f"Conductor-Remote-{tag[1:]}.dmg"
    if not any(asset.get("name") == dmg for asset in release.get("assets", [])):
        raise AnnouncementError("The release is missing its downloadable DMG.")
    notes = release.get("body")
    if not isinstance(notes, str) or not notes.strip():
        raise AnnouncementError("The release has no release notes.")
    return notes.strip()


def inline_html(text):
    # Escape source before adding supported Telegram tags. Code remains literal.
    pattern = r"(`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\(https?://[^\s)]+\))"
    parts = re.split(pattern, text)
    result = []
    for index, part in enumerate(parts):
        if index % 2 == 0:
            result.append(html.escape(part))
        elif part.startswith("`"):
            result.append("<code>" + html.escape(part[1:-1]) + "</code>")
        elif part.startswith("**"):
            result.append("<b>" + html.escape(part[2:-2]) + "</b>")
        else:
            label, url = part[1:-1].split("](", 1)
            result.append(f'<a href="{html.escape(url, quote=True)}">{html.escape(label)}</a>')
    return "".join(result)


def messages(tag, notes):
    release_url = f"https://github.com/{REPO}/releases/tag/{tag}"
    dmg_url = f"https://github.com/{REPO}/releases/download/{tag}/Conductor-Remote-{tag[1:]}.dmg"
    # Raw HTML from notes is text, never executable rich-message markup.
    rich_parts = re.split(r"(```[\s\S]*?```|`[^`\n]+`)", notes)
    rich_notes = "".join(part if i % 2 else html.escape(part, quote=False)
                         for i, part in enumerate(rich_parts))
    rich = (f"# Conductor Remote {tag[1:]}\n\n{rich_notes}\n\n"
            '<tg-button-row align="left">\n'
            f'<tg-button type="url" style="success" url="{dmg_url}">Download</tg-button>\n'
            f'<tg-button type="url" url="{release_url}">Release notes</tg-button>\n'
            '</tg-button-row>')

    # Whole rendered lines keep tags balanced when truncating. Count UTF-16 units
    # of the markup conservatively: entities and tags only reduce the actual text.
    lines = []
    used = 0
    in_code = False
    for line in notes.splitlines():
        if line.startswith("```"):
            in_code = not in_code
            continue
        heading = re.match(r"^#{1,6}\s+(.+)", line) if not in_code else None
        bullet = re.match(r"^\s*[-*]\s+(.+)", line) if not in_code else None
        if in_code:
            rendered = "<code>" + html.escape(line) + "</code>"
        elif heading:
            rendered = "<b>" + html.escape(heading[1]) + "</b>"
        elif bullet:
            rendered = "• " + inline_html(bullet[1])
        else:
            rendered = inline_html(line)
        size = len(rendered.encode("utf-16-le")) // 2 + 1
        if used + size > 3500:
            lines.append("…")
            break
        lines.append(rendered)
        used += size
    fallback = (f"<b>Conductor Remote {tag[1:]} is out</b>\n\n" + "\n".join(lines) +
                f'\n\n<a href="{release_url}">Release notes</a> · '
                f'<a href="{dmg_url}">Download</a>')
    return rich, fallback


def telegram_request(method, token, payload):
    # The secret-bearing URL stays inside this process, never in argv or logs.
    try:
        request = urllib.request.Request(f"https://api.telegram.org/bot{token}/{method}",
                                         data=json.dumps(payload).encode(),
                                         headers={"Content-Type": "application/json"})
        try:
            response = urllib.request.urlopen(request, timeout=30)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            body = json.load(response)
        if isinstance(body, dict) and type(body.get("ok")) is bool:
            return body["ok"]
    except (OSError, ValueError, urllib.error.URLError, http.client.HTTPException):
        pass
    # Delivery is uncertain: don't send again and risk a duplicate announcement.
    raise AnnouncementError("Telegram delivery could not be confirmed; check the channel before retrying.")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    preview = parser.add_mutually_exclusive_group()
    preview.add_argument("--dry-run", action="store_true", help="Preview rich Markdown without posting")
    preview.add_argument("--dry-run-fallback", action="store_true", help="Preview HTML without posting")
    parser.add_argument("tag")
    args = parser.parse_args(argv)
    try:
        rich, fallback = messages(args.tag, release_notes(args.tag))
        if args.dry_run or args.dry_run_fallback:
            print(fallback if args.dry_run_fallback else rich)
            return 0
        token = os.environ.get("TELEGRAM_BOT_TOKEN", "")
        chat = os.environ.get("TELEGRAM_CHAT_ID", "")
        if not token or not chat:
            raise AnnouncementError("Configure TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID repository secrets first.")
        if telegram_request("sendRichMessage", token, {"chat_id": chat, "rich_message": {"markdown": rich}}):
            print("Release announced on Telegram.")
            return 0
        print("Telegram rejected the rich message; trying HTML.")
        if not telegram_request("sendMessage", token, {"chat_id": chat, "text": fallback,
                                "parse_mode": "HTML", "link_preview_options": {"is_disabled": True}}):
            raise AnnouncementError("Telegram rejected the announcement. Check the bot's channel permissions.")
        print("Release announced on Telegram (HTML).")
        return 0
    except AnnouncementError as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
