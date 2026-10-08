#!/usr/bin/env python3
"""Compare the read responses of two relays without revealing their contents.

Prints JSON paths, types, string lengths, counts, status codes and exception
class names only. Never prints a value taken from a response, a token, a byte
of a token file or the text of an exception. See docs/development.md
("Comparing two relays").
"""
import argparse
import contextlib
import io
import json
import os
import re
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request

MAX_PATHS = 40
TIMEOUT = 30
MAX_BODY = 64 * 1024 * 1024
MAX_TOKEN = 512
ENV_NAMES = ("PARITY_A_URL", "PARITY_B_URL", "PARITY_A_TOKEN_FILE", "PARITY_B_TOKEN_FILE")

PLAIN_KEY = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,39}")
TOKEN_RE = re.compile(rb"[\x21-\x7e]{1,512}")
IGNORE_TOKEN = re.compile(r"([^.\[\]]+)|\[(\d*)\]")

ANY_INDEX = "[]"


class UsageError(Exception):
    """Carries a message written by this script, never text from elsewhere."""


class NotJson:
    """Marker for a body that did not parse as JSON."""


NOT_JSON = NotJson()


# ---------------------------------------------------------------- comparison

def type_name(v):
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, (int, float)):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    if isinstance(v, dict):
        return "object"
    return "unknown"


def render_path(parts):
    """Render a path. Object keys that are not plain names become <key>."""
    if not parts:
        return "$"
    out = []
    for p in parts:
        if isinstance(p, int):
            out.append("[%d]" % p)
        else:
            name = p if PLAIN_KEY.fullmatch(p) else "<key>"
            out.append(name if not out else "." + name)
    return "".join(out)


def parse_ignore(pattern):
    """Turn 'a[].b' into ('a', '[]', 'b'); None when the pattern is malformed."""
    parts = []
    pos = 0
    while pos < len(pattern):
        if parts and pattern[pos] == ".":
            pos += 1
        m = IGNORE_TOKEN.match(pattern, pos)
        if not m:
            return None
        if m.group(1) is not None:
            parts.append(m.group(1))
        elif m.group(2) == "":
            parts.append(ANY_INDEX)
        else:
            parts.append(int(m.group(2)))
        pos = m.end()
    return tuple(parts) if parts else None


def is_ignored(parts, ignores):
    for pat in ignores:
        if len(parts) < len(pat):
            continue
        for want, got in zip(pat, parts):
            if want == ANY_INDEX:
                if not isinstance(got, int):
                    break
            elif want != got:
                break
        else:
            return True
    return False


def diff(a, b, parts, ignores, out):
    """Append (path, message) pairs to out. Messages carry no values."""
    if parts and is_ignored(parts, ignores):
        return
    here = render_path(parts)
    ta, tb = type_name(a), type_name(b)
    if ta != tb:
        out.append((here, "type %s != %s" % (ta, tb)))
        return
    if ta == "object":
        for k in sorted(set(a) | set(b)):
            sub = parts + (k,)
            if k not in b:
                if not is_ignored(sub, ignores):
                    out.append((render_path(sub), "only in A"))
            elif k not in a:
                if not is_ignored(sub, ignores):
                    out.append((render_path(sub), "only in B"))
            else:
                diff(a[k], b[k], sub, ignores, out)
    elif ta == "array":
        if len(a) != len(b):
            out.append((here, "length %d != %d" % (len(a), len(b))))
        for i in range(min(len(a), len(b))):
            diff(a[i], b[i], parts + (i,), ignores, out)
    elif a != b:
        if ta == "string":
            out.append((here, "string(%d) != string(%d)" % (len(a), len(b))))
        else:
            out.append((here, "%s != %s" % (ta, tb)))


def classify(status_a, doc_a, status_b, doc_b, ignores):
    """Differences for one pair of answers. Only identical when both sides
    answered 200 with a JSON body and no path differs."""
    out = []
    if status_a != 200 or status_b != 200:
        out.append(("<status>", "A=%d B=%d" % (status_a, status_b)))
    if isinstance(doc_a, NotJson):
        out.append(("<body>", "not JSON on A"))
    if isinstance(doc_b, NotJson):
        out.append(("<body>", "not JSON on B"))
    if (status_a == 200 and status_b == 200
            and not isinstance(doc_a, NotJson) and not isinstance(doc_b, NotJson)):
        diff(doc_a, doc_b, (), ignores, out)
    return out


def is_good(status, doc):
    return status == 200 and not isinstance(doc, NotJson)


# ------------------------------------------------------------ input checking

def printable(s):
    """A string safe to print: ASCII only, no control characters."""
    return "".join(c if " " <= c <= "~" else "?" for c in s)


def load_token(path):
    """Read a token file as bytes and validate it. Never echoes its content."""
    try:
        with open(path, "rb") as f:
            data = f.read(MAX_TOKEN + 8)
    except (OSError, ValueError):
        raise UsageError("cannot read token file %s" % printable(path))
    if data.endswith(b"\r\n"):
        data = data[:-2]
    elif data.endswith(b"\n"):
        data = data[:-1]
    if not TOKEN_RE.fullmatch(data):
        raise UsageError(
            "token file %s must hold a single line of printable ASCII "
            "without spaces" % printable(path))
    return data.decode("ascii")


def check_base_url(name, value):
    """Return a normalised base URL; the error names the variable only."""
    bad = UsageError("%s is not an http(s) URL" % name)
    if not value or not all("\x21" <= c <= "\x7e" for c in value):
        raise bad
    try:
        p = urllib.parse.urlsplit(value)
        host = p.hostname
        p.port  # raises ValueError for an invalid port
    except ValueError:
        raise bad
    if p.scheme not in ("http", "https") or not host or "@" in p.netloc:
        raise bad
    return "%s://%s%s" % (p.scheme, p.netloc, p.path.rstrip("/"))


# ----------------------------------------------------------------- transport

class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never follow a redirect: the token must not go to another host."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def build_opener():
    return urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


class ConnectError(Exception):
    def __init__(self, side, cls):
        super().__init__("cannot reach %s: %s" % (side, cls))


def exc_class(e):
    reason = getattr(e, "reason", None)
    return type(reason if isinstance(reason, BaseException) else e).__name__


_opener = None


def fetch(side, base, token, path):
    """Return (status, parsed JSON or NOT_JSON). Bodies are never printed."""
    global _opener
    if _opener is None:
        _opener = build_opener()
    req = urllib.request.Request(
        base + path, headers={"Authorization": "Bearer " + token})
    try:
        try:
            r = _opener.open(req, timeout=TIMEOUT)
        except urllib.error.HTTPError as e:
            r = e
        with r:
            status = r.status if hasattr(r, "status") else r.code
            body = r.read(MAX_BODY + 1)
    except Exception as e:  # refused, timeout, malformed answer, TLS, ...
        raise ConnectError(side, exc_class(e))
    if len(body) > MAX_BODY:
        return status, NOT_JSON
    try:
        return status, json.loads(body)
    except (ValueError, RecursionError):
        return status, NOT_JSON


# ------------------------------------------------------------------- the run

def sub_ids(doc, key, n):
    """Identifiers of the first n entries of doc[key]."""
    if not isinstance(doc, dict) or not isinstance(doc.get(key), list):
        return []
    res = []
    for it in doc[key][:n]:
        if isinstance(it, dict):
            i = it.get("id")
            if isinstance(i, int) and not isinstance(i, bool):
                i = str(i)
            if isinstance(i, str) and i:
                res.append(i)
    return res


def quote(s):
    return urllib.parse.quote(s, safe="", errors="replace")


class Report:
    def __init__(self):
        self.requests = 0
        self.identical = 0
        self.different = 0
        self.paths = {}

    def add(self, tmpl, sa, sb, diffs):
        self.requests += 1
        if diffs:
            self.different += 1
        else:
            self.identical += 1
        print("%s A=%d B=%d differences=%d" % (tmpl, sa, sb, len(diffs)))
        for key in diffs:
            self.paths[key] = self.paths.get(key, 0) + 1

    def finish(self):
        keys = list(self.paths)
        for k in keys[:MAX_PATHS]:
            print("  %s: %s (x%d)" % (k[0], k[1], self.paths[k]))
        if len(keys) > MAX_PATHS:
            print("  ... %d more paths not shown" % (len(keys) - MAX_PATHS))
        print("requests: %d, identical: %d, different: %d"
              % (self.requests, self.identical, self.different))
        return 0 if self.different == 0 else 1


def run(get_a, get_b, n_ws, n_sess, ignores):
    rep = Report()

    def both(tmpl, path):
        sa, da = get_a(path)
        sb, db = get_b(path)
        rep.add(tmpl, sa, sb, classify(sa, da, sb, db, ignores))
        return sa, da, sb, db

    sa, state, sb, state_b = both("/api/state", "/api/state")
    if not (is_good(sa, state) and is_good(sb, state_b)):
        return rep.finish()
    for wid in sub_ids(state, "workspaces", n_ws):
        sa, sess, _, _ = both("/api/workspaces/<id>/sessions",
                              "/api/workspaces/%s/sessions" % quote(wid))
        if not is_good(sa, sess):
            continue
        for sid in sub_ids(sess, "sessions", n_sess):
            both("/api/sessions/<id>/messages?after=0",
                 "/api/sessions/%s/messages?after=0" % quote(sid))
    return rep.finish()


# ----------------------------------------------------------------------- CLI

class Parser(argparse.ArgumentParser):
    def error(self, message):
        # argparse messages quote the offending argument; say nothing of it.
        raise UsageError("invalid command line, see --help")


def make_parser():
    ap = Parser(
        allow_abbrev=False,
        description="Compare two relays' read responses; prints paths, types, "
                    "lengths and counts only.",
        epilog="Environment: PARITY_A_URL, PARITY_B_URL, PARITY_A_TOKEN_FILE, "
               "PARITY_B_TOKEN_FILE.")
    ap.add_argument("--workspaces", type=int, default=5)
    ap.add_argument("--sessions", type=int, default=2)
    ap.add_argument("--ignore", action="append", default=[], metavar="PATH")
    ap.add_argument("--self-test", action="store_true")
    return ap


def real_fetch(side, base, token, path):
    return fetch(side, base, token, path)


def run_cli(argv, env, fetcher):
    try:
        args = make_parser().parse_args(argv)
    except SystemExit as e:  # --help
        return 0 if not e.code else 2
    if args.self_test:
        return self_test()
    if args.workspaces < 0 or args.sessions < 0:
        raise UsageError("--workspaces and --sessions must be 0 or more")
    ignores = []
    for pat in args.ignore:
        parts = parse_ignore(pat)
        if parts is None:
            raise UsageError("an --ignore pattern is malformed")
        ignores.append(parts)
    # Validate everything before any request is built.
    for name in ENV_NAMES:
        if env.get(name) is None:
            raise UsageError("%s is not set" % name)
    base_a = check_base_url("PARITY_A_URL", env["PARITY_A_URL"])
    base_b = check_base_url("PARITY_B_URL", env["PARITY_B_URL"])
    tok_a = load_token(env["PARITY_A_TOKEN_FILE"])
    tok_b = load_token(env["PARITY_B_TOKEN_FILE"])
    return run(lambda p: fetcher("A", base_a, tok_a, p),
               lambda p: fetcher("B", base_b, tok_b, p),
               args.workspaces, args.sessions, ignores)


def main(argv=None, env=None, fetcher=real_fetch):
    """Always returns an exit status; never lets an exception escape."""
    if argv is None:
        argv = sys.argv[1:]
    if env is None:
        env = os.environ
    try:
        return run_cli(argv, env, fetcher)
    except (UsageError, ConnectError) as e:
        msg = "error: %s" % e
    except (Exception, KeyboardInterrupt) as e:
        # Required behaviour: class name only, never str(e), never a traceback.
        msg = "error: unexpected %s" % type(e).__name__
    try:
        print(msg, file=sys.stderr)
    except Exception:
        pass
    return 2


# ------------------------------------------------------------------ self-test

class SelfTestFailure(Exception):
    pass


def check(name, cond):
    if not cond:
        raise SelfTestFailure(name)


def capture(fn):
    """Run fn, return (result, everything it wrote to stdout and stderr)."""
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        result = fn()
    return result, buf.getvalue()


def self_test_body():
    def d(x, y, ign=()):
        out = []
        diff(x, y, (), [parse_ignore(i) for i in ign], out)
        return out

    # Sample values and identifiers that must never reach the output.
    S1, S2 = "SAMPLEVALUE-alpha", "SAMPLEVALUE-be"
    ID_WS, ID_SESS = "9wsid-0001/x y", "9sessid-0002"
    MAPKEY = "7f3a-MAPKEYSAMPLE"
    state = {"n": 1, "b": True, "list": [1, 2], "o": {"k": S1}, MAPKEY: {"v": S1},
             "workspaces": [{"id": ID_WS, "branch": S1, "change_stats": {"n": 1}}]}
    sessions = {"sessions": [{"id": ID_SESS, "t": S1}, {"id": "s-2", "t": S1}]}
    messages = {"messages": [{"text": S1}]}

    def clone(x):
        return json.loads(json.dumps(x))

    # Structural differences and their paths.
    check("identical documents give no differences", d(state, clone(state)) == [])
    other = clone(state)
    other["workspaces"][0]["branch"] = S2
    check("different string reports path and lengths",
          d(state, other) == [("workspaces[0].branch", "string(17) != string(14)")])
    check("key only in B", d({"x": 1}, {"x": 1, "y": 2}) == [("y", "only in B")])
    check("key only in A", d({"x": 1, "y": 2}, {"x": 1}) == [("y", "only in A")])
    check("type mismatch", d({"x": 1}, {"x": S1}) == [("x", "type number != string")])
    check("null against object is a type mismatch",
          d({"x": None}, {"x": {}}) == [("x", "type null != object")])
    check("array length", d({"x": [1, 2]}, {"x": [1]}) == [("x", "length 2 != 1")])
    check("array elements matched by index and numbers print no value",
          d({"x": [1, 2]}, {"x": [1, 3]}) == [("x[1]", "number != number")])
    check("booleans print only the type",
          d({"x": True}, {"x": False}) == [("x", "boolean != boolean")])
    check("root type mismatch uses $", d([], {}) == [("$", "type array != object")])
    check("number 1 against true is a type mismatch",
          d({"x": 1}, {"x": True}) == [("x", "type number != boolean")])

    # --ignore.
    ch = clone(state)
    ch["workspaces"][0]["change_stats"] = {"n": 2, "extra": 1}
    check("change is seen without --ignore", d(state, ch) != [])
    check("--ignore suppresses a path and what is under it",
          d(state, ch, ["workspaces[].change_stats"]) == [])
    check("--ignore of a missing key",
          d({"a": {"b": 1}}, {"a": {}}, ["a.b"]) == [])
    check("--ignore does not suppress a sibling",
          d({"a": 1, "b": 1}, {"a": 2, "b": 2}, ["a"]) == [("b", "number != number")])
    check("--ignore with an index matches that index only",
          d({"l": [1, 1]}, {"l": [2, 2]}, ["l[0]"]) == [("l[1]", "number != number")])
    check("malformed --ignore pattern is rejected",
          parse_ignore("a..b") is None and parse_ignore("") is None
          and parse_ignore("a[x]") is None)

    # Keys in paths.
    out = d({MAPKEY: S1, "ok_name": 1}, {MAPKEY: S2, "ok_name": 2})
    check("map key that is not a plain name is printed as <key>",
          ("<key>", "string(17) != string(14)") in out)
    check("plain key is printed as it is", ("ok_name", "number != number") in out)
    check("the map key is absent from the paths",
          all("MAPKEY" not in p and MAPKEY not in p for p, _ in out))
    check("key with a trailing newline is not plain",
          render_path(("abc\n",)) == "<key>")
    check("key of 41 characters is not plain", render_path(("a" * 41,)) == "<key>"
          and render_path(("a" * 40,)) == "a" * 40)
    check("key starting with a digit is not plain", render_path(("1a",)) == "<key>")
    check("nested path rendering",
          render_path(("a", 3, "b", MAPKEY)) == "a[3].b.<key>")

    # Classification of a pair of answers.
    ok = {"a": 1}
    check("200 and identical JSON is identical",
          classify(200, ok, 200, clone(ok), []) == [])
    check("a non-200 pair is different even when the bodies match",
          classify(404, ok, 404, ok, []) == [("<status>", "A=404 B=404")])
    check("a redirect status is reported as a status",
          classify(302, NOT_JSON, 200, ok, [])[0] == ("<status>", "A=302 B=200"))
    check("non-JSON on A", ("<body>", "not JSON on A") in classify(200, NOT_JSON, 200, ok, []))
    check("non-JSON on B", ("<body>", "not JSON on B") in classify(200, ok, 200, NOT_JSON, []))
    both = classify(200, NOT_JSON, 200, NOT_JSON, [])
    check("non-JSON on both sides is different",
          ("<body>", "not JSON on A") in both and ("<body>", "not JSON on B") in both)
    check("status and body problems both count",
          len(classify(500, NOT_JSON, 200, ok, [])) == 2)
    check("is_good wants 200 and JSON",
          is_good(200, ok) and not is_good(200, NOT_JSON) and not is_good(404, ok))

    # End to end with fake fetchers; capture everything printed.
    docs = {"/api/state": state,
            "/api/workspaces/9wsid-0001%2Fx%20y/sessions": sessions,
            "/api/sessions/9sessid-0002/messages?after=0": messages,
            "/api/sessions/s-2/messages?after=0": messages}
    docs_b = clone(docs)
    docs_b["/api/state"]["workspaces"][0]["branch"] = S2
    docs_b["/api/state"][MAPKEY]["v"] = S2
    docs_b["/api/sessions/s-2/messages?after=0"]["messages"][0]["text"] = S2

    def fake(table, log):
        def f(side, base, token, path):
            log.append((side, base, token, path))
            return (200, table[path]) if path in table else (404, NOT_JSON)
        return f

    def drive(table_a, table_b, a_args, log):
        fa, fb = fake(table_a, log), fake(table_b, log)
        return capture(lambda: run(lambda p: fa("A", "", "", p),
                                   lambda p: fb("B", "", "", p), *a_args))

    log = []
    code, text = drive(docs, docs_b, (5, 2, []), log)
    check("differing relays exit 1", code == 1)
    check("summary line", "requests: 4, identical: 2, different: 2" in text)
    check("request lines carry templates and statuses",
          "/api/workspaces/<id>/sessions A=200 B=200 differences=0" in text
          and "/api/sessions/<id>/messages?after=0 A=200 B=200 differences=1" in text)
    check("the paths are listed with counts",
          "workspaces[0].branch: string(17) != string(14) (x1)" in text)
    for sample in (S1, S2, "SAMPLEVALUE", ID_WS, ID_SESS, "9wsid", "9sessid", MAPKEY,
                   "MAPKEY", "s-2", "%2F"):
        check("no sample value or identifier in the output", sample not in text)
    check("identifiers are percent-encoded in the path",
          any(p == "/api/workspaces/9wsid-0001%2Fx%20y/sessions" for _, _, _, p in log))
    code, text = drive(docs, clone(docs), (5, 2, []), [])
    check("identical relays exit 0",
          code == 0 and "requests: 4, identical: 4, different: 0" in text)
    code, text = drive(docs, docs_b, (5, 2, [parse_ignore("workspaces[].branch"),
                                             parse_ignore("messages[].text"),
                                             parse_ignore(MAPKEY)]), [])
    check("--ignore applied through run", code == 0)

    # N = 0 descends into nothing.
    log = []
    drive(docs, docs, (0, 2, []), log)
    check("N = 0 workspaces requests only the state",
          sorted({p for _, _, _, p in log}) == ["/api/state"])
    log = []
    drive(docs, docs, (5, 0, []), log)
    check("N = 0 sessions requests no messages",
          not any("/messages" in p for _, _, _, p in log) and len(log) == 4)
    log = []
    drive(docs, docs, (1, 1, []), log)
    check("N limits the descent", len([1 for s, _, _, p in log if s == "A" and "/messages" in p]) == 1)

    # The script stops after the state line when the state is not good.
    for label, bad in (("non-200", (404, NOT_JSON)), ("non-JSON", (200, NOT_JSON))):
        calls = []

        def ga(p):
            calls.append(p)
            return 200, state

        def gb(p, bad=bad):
            calls.append(p)
            return bad
        code, text = capture(lambda: run(ga, gb, 5, 2, []))
        check("state %s stops after the state line" % label,
              code == 1 and len(calls) == 2
              and "requests: 1, identical: 0, different: 1" in text)

    # Cap of 40 paths, each with a count.
    many_a = {"k%d" % i: "v" for i in range(60)}
    many_b = {"k%d" % i: "w" for i in range(60)}
    _, text = capture(lambda: run(lambda p: (200, many_a), lambda p: (200, many_b), 0, 0, []))
    check("at most 40 paths are printed", text.count("string(1) != string(1)") == MAX_PATHS
          and "20 more paths not shown" in text)
    same = [{"v": "a"}] * 3
    same_b = [{"v": "bb"}] * 3
    _, text = capture(lambda: run(lambda p: (200, {"l": same}), lambda p: (200, {"l": same_b}), 0, 0, []))
    rows = [ln for ln in text.splitlines() if ln.startswith("  ")]
    check("each path is printed once", len(rows) == 3 and len(set(rows)) == 3)
    rep_a = {"x": "a"}
    rep_b = {"x": "bb"}
    _, text = capture(lambda: run(lambda p: (200, rep_a if p == "/api/state" else {"workspaces": []}),
                                  lambda p: (200, rep_b if p == "/api/state" else {"workspaces": []}), 0, 0, []))
    check("count column", "x: string(1) != string(2) (x1)" in text)

    # Token files and URLs, through main, with a fetcher that must not be used.
    attempts = []

    def never(side, base, token, path):
        attempts.append(path)
        raise SelfTestFailure("a request was made")

    with tempfile.TemporaryDirectory(prefix="paritytest") as tmp:
        def write(name, data):
            p = os.path.join(tmp, name)
            with open(p, "wb") as f:
                f.write(data)
            return p

        good = write("good", b"GoodTok-123_abc\n")
        env_for = lambda a_tok, b_tok=None, a_url="http://127.0.0.1:9", b_url="http://127.0.0.1:9": {
            "PARITY_A_URL": a_url, "PARITY_B_URL": b_url,
            "PARITY_A_TOKEN_FILE": a_tok, "PARITY_B_TOKEN_FILE": b_tok or good}

        bad_files = {
            "empty": b"",
            "newline-only": b"\n",
            "two-lines": b"QRSTUVONE\nQRSTUVTWO\n",
            "two-lines-crlf": b"QRSTUVONE\r\nQRSTUVTWO\r\n",
            "double-newline": b"QRSTUVONE\n\n",
            "bare-cr": b"QRSTUVONE\r",
            "space": b"QRSTUVONE QRSTUVTWO\n",
            "leading-space": b" QRSTUVONE\n",
            "tab": b"QRSTUVONE\tQRSTUVTWO\n",
            "control": b"QRSTUVONE\x01QRSTUVTWO\n",
            "del": b"QRSTUVONE\x7fQRSTUVTWO\n",
            "nul": b"QRSTUVONE\x00QRSTUVTWO\n",
            "byte-0xff": b"QRSTUVONE\xffQRSTUVTWO\n",
            "invalid-utf8": b"QRSTUVONE\xc3\x28QRSTUVTWO\n",
            "non-ascii": "QRSTUVONE\u00e9QRSTUVTWO\n".encode("utf-8"),
            "too-long": b"QRSTUV" * 86 + b"\n",
        }
        check("the too-long sample is over 512 bytes", len(bad_files["too-long"]) > 513)
        for name, data in bad_files.items():
            for side in ("A", "B"):
                path = write("bad-" + name, data)
                env = env_for(path) if side == "A" else env_for(good, path)
                code, text = capture(lambda: main([], env, never))
                check("bad token file (%s, %s) exits 2" % (name, side), code == 2)
                check("bad token file (%s, %s) is named by path" % (name, side),
                      "error: token file %s must hold a single line of printable ASCII "
                      "without spaces" % path in text)
                frags = re.findall(rb"[\x21-\x7e]{3,}", data)
                check("bad token file (%s, %s) leaks no bytes" % (name, side),
                      all(f.decode("ascii") not in text for f in frags)
                      and "\ufffd" not in text and "\xff" not in text)
        code, text = capture(lambda: main([], env_for(os.path.join(tmp, "missing")), never))
        check("missing token file exits 2 with the path only",
              code == 2 and "cannot read token file" in text and "No such file" not in text)
        code, text = capture(lambda: main([], env_for(tmp), never))
        check("a directory as token file exits 2", code == 2 and "cannot read token file" in text)
        check("token file errors print no traceback", "Traceback" not in text)

        # Good token files are accepted and sent as the bearer token only.
        for name, data, tok in (("lf", b"abc.DEF-123~\n", "abc.DEF-123~"),
                                ("crlf", b"abc.DEF-123~\r\n", "abc.DEF-123~"),
                                ("bare", b"abc.DEF-123~", "abc.DEF-123~"),
                                ("max", b"z" * 512 + b"\n", "z" * 512),
                                ("one", b"!", "!")):
            path = write("ok-" + name, data)
            log = []
            f = fake({"/api/state": {"workspaces": []}}, log)
            code, text = capture(lambda: main([], env_for(path, path), f))
            check("valid token file (%s) is accepted" % name, code == 0)
            check("valid token file (%s) is sent as the token" % name,
                  [t for _, _, t, _ in log] == [tok, tok])
            check("valid token file (%s) is not printed" % name, tok not in text)
        a_path, b_path = write("tok-a", b"TOKENAAA\n"), write("tok-b", b"TOKENBBB\n")
        log = []
        f = fake({"/api/state": {"workspaces": []}}, log)
        capture(lambda: main([], env_for(a_path, b_path, "http://a.invalid:1", "https://b.invalid:2/base/"), f))
        check("tokens and bases go to the right side",
              log == [("A", "http://a.invalid:1", "TOKENAAA", "/api/state"),
                      ("B", "https://b.invalid:2/base", "TOKENBBB", "/api/state")])

        # URLs.
        for url in ("ftp://MARKERHOST.invalid/PATHMARK", "http://", "MARKERHOST", "",
                    "file:///MARKERHOST", "http://MARKERHOST:99999", "http:///PATHMARK",
                    "http://user:PWMARK@MARKERHOST", "http://MARKER HOST", "//MARKERHOST",
                    "http://MARKERHOST\n", "javascript:MARKERHOST"):
            for var in ("PARITY_A_URL", "PARITY_B_URL"):
                env = env_for(good)
                env[var] = url
                code, text = capture(lambda: main([], env, never))
                check("bad URL in %s exits 2" % var, code == 2)
                check("bad URL message names the variable and not the value",
                      text == "error: %s is not an http(s) URL\n" % var)
        for url in ("http://127.0.0.1:8790", "https://relay.example.invalid/", "http://[::1]:80"):
            check("a good URL is accepted", check_base_url("X", url) != "")
        check("a good URL is normalised",
              check_base_url("X", "http://127.0.0.1:8790/") == "http://127.0.0.1:8790")
        env = env_for(good)
        del env["PARITY_B_URL"]
        code, text = capture(lambda: main([], env, never))
        check("a missing variable exits 2", code == 2 and "PARITY_B_URL is not set" in text)

    check("no request was made for any bad input", attempts == [])

    # Command line.
    for argv in (["--workspaces", "ARGMARK"], ["--bogus", "ARGMARK"], ["ARGMARK"],
                 ["--workspaces", "-1"], ["--ignore", "a..b"], ["--ignore"]):
        code, text = capture(lambda: main(argv, {}, never))
        check("bad command line exits 2 without echoing it",
              code == 2 and "ARGMARK" not in text and "Traceback" not in text)
    check("no request was made for a bad command line", attempts == [])

    # Failure handling in main: class name only.
    with tempfile.TemporaryDirectory(prefix="paritytest") as tmp:
        p = os.path.join(tmp, "t")
        with open(p, "wb") as f:
            f.write(b"TOKENVALUE\n")
        env = {"PARITY_A_URL": "http://a.invalid", "PARITY_B_URL": "http://b.invalid",
               "PARITY_A_TOKEN_FILE": p, "PARITY_B_TOKEN_FILE": p}

        def boom(side, base, token, path):
            raise RuntimeError("EXCMESSAGE TOKENVALUE")

        code, text = capture(lambda: main([], env, boom))
        check("an unexpected exception exits 2", code == 2)
        check("an unexpected exception prints its class name only",
              text == "error: unexpected RuntimeError\n")

        def conn(side, base, token, path):
            raise ConnectError(side, "ConnectionRefusedError")

        code, text = capture(lambda: main([], env, conn))
        check("a connection error names the side and the class",
              code == 2 and text == "error: cannot reach A: ConnectionRefusedError\n")

        def conn_b(side, base, token, path):
            if side == "B":
                raise ConnectError("B", "TimeoutError")
            return 200, {}

        code, text = capture(lambda: main([], env, conn_b))
        check("a connection error on B names B",
              code == 2 and text == "error: cannot reach B: TimeoutError\n")

        def interrupted(side, base, token, path):
            raise KeyboardInterrupt()

        code, text = capture(lambda: main([], env, interrupted))
        check("an interrupt is handled", code == 2 and "Traceback" not in text)

    # Transport helpers, without a network.
    check("a redirect is never followed",
          NoRedirect().redirect_request(None, None, 302, "Found", {}, "http://other.invalid/") is None)
    check("exception class name comes from the reason",
          exc_class(urllib.error.URLError(ConnectionRefusedError("MSG"))) == "ConnectionRefusedError"
          and exc_class(urllib.error.URLError("MSG")) == "URLError"
          and exc_class(TimeoutError("MSG")) == "TimeoutError")
    check("printable() removes control characters",
          printable("a\x1b[31m\u00e9") == "a?[31m?")


def self_test():
    try:
        capture(self_test_body)
    except SelfTestFailure as e:
        print("self-test FAILED: %s" % e)
        return 1
    except Exception as e:
        print("self-test FAILED: unexpected %s" % type(e).__name__)
        return 1
    print("self-test ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
