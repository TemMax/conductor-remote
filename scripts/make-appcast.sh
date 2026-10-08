#!/usr/bin/env bash
# Writes a one-item Sparkle appcast to stdout.
set -euo pipefail

if [ $# -ne 5 ]; then
	echo "usage: make-appcast.sh <version> <build> <download-url> <sig-attrs> <notes.md>" >&2
	exit 2
fi
version=$1
build=$2
url=$3
sig_attrs=$4
notes=$5

if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
	echo "error: the version '$version' is not of the form x.y.z" >&2
	exit 1
fi
if ! [[ "$build" =~ ^[1-9][0-9]*$ ]]; then
	echo "error: the build '$build' is not a positive integer" >&2
	exit 1
fi
if [ -z "$url" ] || [[ "$url" =~ [[:space:]] ]]; then
	echo "error: the download URL is empty or contains whitespace" >&2
	exit 1
fi
if ! [[ "$sig_attrs" =~ ^sparkle:edSignature=\"[A-Za-z0-9+/=]+\"\ length=\"[0-9]+\"$ ]]; then
	echo "error: the signature attributes must look like: sparkle:edSignature=\"…\" length=\"<digits>\"" >&2
	exit 1
fi
if [ ! -f "$notes" ]; then
	echo "error: $notes is not a file" >&2
	exit 1
fi

here=$(cd "$(dirname "$0")" && pwd)
body=$(bash "$here/notes-to-html.sh" "$notes")
pub_date=$(date -u +'%a, %d %b %Y %H:%M:%S +0000')

xml_url=${url//&/&amp;}
xml_url=${xml_url//</&lt;}
xml_url=${xml_url//\"/&quot;}

cat <<XML
<?xml version="1.0" standalone="yes"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Conductor Remote</title>
    <item>
      <title>${version}</title>
      <pubDate>${pub_date}</pubDate>
      <sparkle:version>${build}</sparkle:version>
      <sparkle:shortVersionString>${version}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
      <description><![CDATA[<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
body { font: 13px -apple-system, BlinkMacSystemFont, "Helvetica Neue", sans-serif; color-scheme: light dark; margin: 0; }
h1, h2, h3 { margin: 12px 0 4px; line-height: 1.25; }
h1 { font-size: 16px; }
h2 { font-size: 14px; }
h3 { font-size: 13px; }
p { margin: 4px 0; }
ul { margin: 4px 0; padding-left: 20px; }
li { margin: 2px 0; }
code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
</style>
</head>
<body>
${body}
</body>
</html>
]]></description>
      <enclosure url="${xml_url}" ${sig_attrs} type="application/octet-stream" />
    </item>
  </channel>
</rss>
XML
