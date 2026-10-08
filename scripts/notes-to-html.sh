#!/usr/bin/env bash
# Converts Markdown release notes to an HTML fragment on stdout.
# Supports #/##/### headings, -/* bullets, paragraphs, `code`, **bold** and [text](https://…) links.
set -euo pipefail

if [ $# -ne 1 ]; then
	echo "usage: notes-to-html.sh <notes.md>" >&2
	exit 2
fi
if [ ! -f "$1" ]; then
	echo "error: $1 is not a file" >&2
	exit 2
fi

awk '
function escape(s) {
	gsub(/&/, "\\&amp;", s)
	gsub(/</, "\\&lt;", s)
	gsub(/>/, "\\&gt;", s)
	return s
}
function links(s,    out, m, idx, text, url) {
	out = ""
	while (match(s, /\[[^]]+\]\(https:\/\/[^) ]+\)/)) {
		m = substr(s, RSTART, RLENGTH)
		idx = index(m, "](")
		text = substr(m, 2, idx - 2)
		url = substr(m, idx + 2, length(m) - idx - 2)
		gsub(/"/, "%22", url)
		out = out substr(s, 1, RSTART - 1) "<a href=\"" url "\">" text "</a>"
		s = substr(s, RSTART + RLENGTH)
	}
	return out s
}
function bold(s,    out, m) {
	out = ""
	while (match(s, /\*\*[^*]+\*\*/)) {
		m = substr(s, RSTART + 2, RLENGTH - 4)
		out = out substr(s, 1, RSTART - 1) "<strong>" m "</strong>"
		s = substr(s, RSTART + RLENGTH)
	}
	return out s
}
function inline(s,    n, parts, i, out) {
	s = escape(s)
	n = split(s, parts, "`")
	if (n % 2 == 0) return bold(links(s))
	out = ""
	for (i = 1; i <= n; i++) {
		if (i % 2 == 1) out = out bold(links(parts[i]))
		else out = out "<code>" parts[i] "</code>"
	}
	return out
}
function trim(s) {
	sub(/^[[:space:]]+/, "", s)
	sub(/[[:space:]]+$/, "", s)
	return s
}
function flush_paragraph() {
	if (para != "") print "<p>" inline(para) "</p>"
	para = ""
}
function close_list() {
	if (in_list) print "</ul>"
	in_list = 0
}
{ sub(/\r$/, "") }
/^[[:space:]]*$/ { flush_paragraph(); close_list(); next }
/^###[[:space:]]/ { flush_paragraph(); close_list(); print "<h3>" inline(trim(substr($0, 4))) "</h3>"; next }
/^##[[:space:]]/ { flush_paragraph(); close_list(); print "<h2>" inline(trim(substr($0, 3))) "</h2>"; next }
/^#[[:space:]]/ { flush_paragraph(); close_list(); print "<h1>" inline(trim(substr($0, 2))) "</h1>"; next }
/^[-*][[:space:]]+/ {
	flush_paragraph()
	if (!in_list) { print "<ul>"; in_list = 1 }
	item = $0
	sub(/^[-*][[:space:]]+/, "", item)
	print "<li>" inline(trim(item)) "</li>"
	next
}
{
	close_list()
	line = trim($0)
	para = (para == "") ? line : para " " line
}
END { flush_paragraph(); close_list() }
' "$1"
