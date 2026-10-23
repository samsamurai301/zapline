#!/usr/bin/env python3
"""Check a running TFB implementation the way TechEmpower's toolset does.

    python3 bench/tfb/verify.py http://127.0.0.1:8080
    python3 bench/tfb/verify.py http://127.0.0.1:8080 --psql "psql -h /var/tmp/zapline-tfb-pg -p 5433 -U postgres -d hello_world"

This mirrors toolset/test_types/*/ and toolset/test_types/verifications.py in
https://github.com/TechEmpower/FrameworkBenchmarks:

- required headers (Server, Date, Content-Type, Content-Length or
  Transfer-Encoding), the Date format, a Date that changes between requests,
  and the Content-Type for each test;
- plaintext and JSON bodies;
- /db returns one object, /queries, /updates and /cached-queries return
  arrays of {id, randomNumber} with the count clamped to 1..500 for
  `2`, `0`, `foo`, `501` and an empty value;
- /fortunes, normalized by a copy of TFB's FortuneHTMLParser and compared
  with the expected page (ordering, the added fortune, escaping);
- /updates really changes rows (the world table is compared before and
  after a 500-row update, as TFB does) and every returned row matches what
  is stored;
- with --psql: the number of queries, rows read and rows updated per request,
  counted with pg_stat_statements around a burst of 512 requests, with TFB's
  margins (no slack on queries, 1.1% on rows, 5% on bulk updates).

Only the standard library is used. Exit status 0 means every check passed;
warnings are printed but do not fail the run.
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from html.parser import HTMLParser

ACCEPT = {
    "json": "application/json,text/html;q=0.9,application/xhtml+xml;q=0.9,application/xml;q=0.8,*/*;q=0.7",
    "html": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "plaintext": "text/plain,text/html;q=0.9,application/xhtml+xml;q=0.9,application/xml;q=0.8,*/*;q=0.7",
}
CONTENT_TYPES = {
    "json": r"^application/json(; ?charset=(UTF|utf)-8)?$",
    "html": r"^text/html; ?charset=(UTF|utf)-8$",
    "plaintext": r"^text/plain(; ?charset=(UTF|utf)-8)?$",
}
# The cases TFB requests for /queries, /updates and /cached-queries, with
# what a wrong answer costs.
QUERY_CASES = [("2", "fail"), ("0", "fail"), ("foo", "fail"), ("501", "warn"), ("", "fail")]
CONCURRENCY = 512  # max(concurrency_levels) in TFB
ROW_MARGIN = 1.011  # AbstractDatabase.margin


class Report:
    def __init__(self):
        self.failures = 0
        self.warnings = 0
        self.section_start = 0
        self.section_name = None

    def section(self, name):
        """Start a test type; prints whether the previous one passed."""
        self.end_section()
        self.section_name, self.section_start = name, self.failures

    def end_section(self):
        if self.section_name is not None:
            verdict = "ok" if self.failures == self.section_start else "FAILED"
            print(f"{self.section_name}: {verdict}")

    def check(self, ok, what, level="fail"):
        if ok:
            return True
        if level == "warn":
            self.warnings += 1
            print(f"  WARN  {what}")
        else:
            self.failures += 1
            print(f"  FAIL  {what}")
        return False

    def ok(self, what):
        print(f"  pass  {what}")


def get(url, kind="json"):
    req = urllib.request.Request(url, headers={"Accept": ACCEPT[kind]})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, {k.lower(): v for k, v in resp.headers.items()}, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()


def verify_headers(report, url, headers, kind, check_date_changes):
    for h in ("Server", "Date", "Content-Type"):
        report.check(h.lower() in headers, f"{url}: required header missing: {h}")
    report.check(
        "content-length" in headers or "transfer-encoding" in headers,
        f"{url}: neither Content-Length nor Transfer-Encoding",
    )
    date = headers.get("date")
    if date is not None:
        try:
            datetime.strptime(date, "%a, %d %b %Y %H:%M:%S %Z")
        except ValueError:
            report.check(False, f"{url}: invalid Date header {date!r}", "warn")
    if check_date_changes:
        # TFB sleeps 3 s; one second and a bit is enough to see a new Date.
        time.sleep(1.1)
        _, second, _ = get(url, kind)
        report.check(second.get("date") != date, f"{url}: Date header is cached ({date!r} twice)")
    ctype = headers.get("content-type")
    if ctype is not None:
        report.check(
            re.match(CONTENT_TYPES[kind], ctype) is not None,
            f"{url}: Content-Type {ctype!r} does not match {CONTENT_TYPES[kind]}",
        )


def verify_world(report, url, obj, level="fail"):
    if not report.check(isinstance(obj, dict), f"{url}: expected a JSON object, got {str(obj)[:20]!r}", level):
        return False
    lower = {k.lower(): v for k, v in obj.items()}
    good = True
    for key in ("id", "randomnumber"):
        good &= report.check(key in lower, f"{url}: object is missing {key}", level)
    if not good:
        return False
    report.check(len(lower) == 2, f"{url}: extra keys in {obj}", "warn")
    good &= report.check(isinstance(lower["id"], int), f"{url}: id is not an integer: {obj}", level)
    good &= report.check(
        isinstance(lower["randomnumber"], int), f"{url}: randomNumber is not an integer: {obj}", level
    )
    if good:
        report.check(1 <= lower["id"] <= 10000, f"{url}: id out of 1..10000: {obj}", "warn")
        report.check(lower["randomnumber"] <= 10000, f"{url}: randomNumber over 10000: {obj}", "warn")
    return good


def verify_world_list(report, url, body, expected_len, level):
    try:
        data = json.loads(body)
    except ValueError as e:
        report.check(False, f"{url}: invalid JSON: {e}", level)
        return None
    if not report.check(isinstance(data, list), f"{url}: top-level JSON is not an array", level):
        return None
    report.check(len(data) == expected_len, f"{url}: {len(data)} items, expected {expected_len}", level)
    bad = 0
    for item in data:
        if not verify_world(report, url, item, level):
            bad += 1
            if bad >= 5:
                break
    return data


# A copy of TFB's FortuneHTMLParser (toolset/test_types/fortune/
# fortune_html_parser.py): it normalizes the equivalent escapings, drops
# whitespace-only text, and rebuilds the page as one string.
VALID_FORTUNE = """<!doctype html><html>
<head><title>Fortunes</title></head>
<body><table>
<tr><th>id</th><th>message</th></tr>
<tr><td>11</td><td>&lt;script&gt;alert(&quot;This should not be displayed in a browser alert box.&quot;);&lt;/script&gt;</td></tr>
<tr><td>4</td><td>A bad random number generator: 1, 1, 1, 1, 1, 4.33e+67, 1, 1, 1</td></tr>
<tr><td>5</td><td>A computer program does what you tell it to do, not what you want it to do.</td></tr>
<tr><td>2</td><td>A computer scientist is someone who fixes things that aren&apos;t broken.</td></tr>
<tr><td>8</td><td>A list is only as strong as its weakest link. — Donald Knuth</td></tr>
<tr><td>0</td><td>Additional fortune added at request time.</td></tr>
<tr><td>3</td><td>After enough decimal places, nobody gives a damn.</td></tr>
<tr><td>7</td><td>Any program that runs right is obsolete.</td></tr>
<tr><td>10</td><td>Computers make very fast, very accurate mistakes.</td></tr>
<tr><td>6</td><td>Emacs is a nice operating system, but I prefer UNIX. — Tom Christaensen</td></tr>
<tr><td>9</td><td>Feature: A bug with seniority.</td></tr>
<tr><td>1</td><td>fortune: No such file or directory</td></tr>
<tr><td>12</td><td>フレームワークのベンチマーク</td></tr>
</table></body></html>"""


class FortuneHTMLParser(HTMLParser):
    IGNORED_TAGS = ("<meta>", "</meta>", "<link>", "</link>", "<script>", "</script>",
                    "<thead>", "</thead>", "<tbody>", "</tbody>")
    CHARREFS = {
        ("34", "034", "x22"): "&quot;",
        ("39", "039", "x27"): "&apos;",
        ("43", "043", "x2b"): "+",
        ("62", "062", "x3e"): "&gt;",
        ("60", "060", "x3c"): "&lt;",
        ("47", "047", "x2f"): "/",
        ("40", "040", "x28"): "(",
        ("41", "041", "x29"): ")",
    }

    def __init__(self):
        super().__init__(convert_charrefs=False)
        self.ignore_content = False
        self.body = []

    def handle_decl(self, decl):
        self.append(f"<!{decl.lower()}>")

    def handle_charref(self, name):
        val = name.lower()
        for names, out in self.CHARREFS.items():
            if val in names:
                self.append(out)

    def handle_entityref(self, name):
        self.append("—" if name == "mdash" else f"&{name};")

    def handle_starttag(self, tag, attrs):
        self.append(f"<{tag}>")
        if tag.lower() in ("table", "html"):
            self.append("\n")

    def handle_data(self, data):
        if data.strip() != "":
            data = data.replace("'", "&apos;").replace('"', "&quot;").replace(">", "&gt;")
            self.append(data)

    def handle_endtag(self, tag):
        self.append(f"</{tag}>")
        if tag.lower() in ("tr", "head"):
            self.append("\n")

    def append(self, item):
        self.ignore_content = item == "<script>" or (self.ignore_content and item != "</script>")
        if not (self.ignore_content or item in self.IGNORED_TAGS):
            self.body.append(item)


class Db:
    """pg_stat_statements and the world table, read through psql."""

    def __init__(self, psql):
        self.psql = shlex.split(psql)
        self.scalar("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")

    def scalar(self, sql):
        out = subprocess.run(self.psql + ["-AtX", "-c", sql], check=True, capture_output=True, text=True)
        return out.stdout.strip()

    def stat(self, table, kind=None):
        column = "calls" if kind is None else "rows"
        cond = "" if kind is None else f" AND query ~* '{kind}'"
        v = self.scalar(f"SELECT COALESCE(SUM({column}), 0) FROM pg_stat_statements "
                        f"WHERE query ~* '[[:<:]]{table}[[:>:]]'{cond}")
        return int(v or 0)

    def worlds(self):
        rows = self.scalar("SELECT id || ':' || randomnumber FROM world").split()
        return {int(a): int(b) for a, b in (r.split(":") for r in rows)}


def burst(url, kind, n=CONCURRENCY):
    """`siege -c 512 -r 1`: n requests, counting failed ones."""
    def one(_):
        try:
            status, _, _ = get(url, kind)
            return status == 200
        except Exception:
            return False
    with ThreadPoolExecutor(max_workers=64) as pool:
        return sum(1 for ok in pool.map(one, range(n)) if not ok)


def verify_counts(report, db, table, url, kind, expected_queries, expected_rows, updates=False):
    if db is None:
        return
    before = (db.stat(table), db.stat(table, "select"), db.stat(table, "update") if updates else 0)
    failed = burst(url, kind)
    report.check(failed == 0, f"{url}: {failed} of {CONCURRENCY} requests failed")
    # pg_stat_statements is updated when each statement finishes; give the
    # last ones a moment.
    time.sleep(0.5)
    queries = db.stat(table) - before[0]
    rows = db.stat(table, "select") - before[1]
    rows_updated = (db.stat(table, "update") - before[2]) if updates else 0

    bulk = updates and 0.999 * expected_queries < queries < 1.001 * expected_queries
    if updates and not bulk:
        expected_queries = (expected_queries - CONCURRENCY) * 2

    def result(value, expected, shown, caption):
        if value > expected * 1.05:
            report.check(False, f"{url}: {shown} {caption}, expected {expected} (excessively high)", "warn")
        elif report.check(value >= expected, f"{url}: only {shown} {caption} out of {expected} expected"):
            report.ok(f"{url}: {caption} {shown}/{expected}")

    result(queries, expected_queries, queries, "executed queries")
    result(rows, expected_rows, int(rows / ROW_MARGIN), "rows read")
    if updates:
        result(rows_updated * (1.05 if bulk else 1), expected_rows, int(rows_updated / ROW_MARGIN), "rows updated")


def verify_query_cases(report, db, base, path, name, table=None, updates=False):
    url = base + path
    before = db.worlds() if (updates and db) else None
    for q, level in QUERY_CASES:
        case_url = url + q
        status, headers, body = get(case_url)
        report.check(status == 200, f"{case_url}: status {status}")
        try:
            expected = min(max(int(q), 1), 500)
        except ValueError:
            expected = 1
        data = verify_world_list(report, case_url, body, expected, level)
        verify_headers(report, case_url, headers, "json", check_date_changes=(q == "2"))
        if updates and db and data and q == "501":
            after = db.worlds()
            changed = sum(1 for k, v in after.items() if before.get(k) != v)
            if report.check(changed > 0, f"{case_url}: no rows were updated in the database"):
                report.check(changed > 500 * 0.90, f"{case_url}: only {changed} of ~500 rows changed")
                report.check(changed > 500 * 0.95, f"{case_url}: only {changed} of ~500 rows changed", "warn")
            stored = {w["id"]: w["randomNumber"] for w in data}
            mismatched = [i for i, v in stored.items() if after.get(i) != v]
            report.check(not mismatched, f"{case_url}: rows {mismatched[:5]} differ from the response")
    if table:
        expected_queries = 20 * CONCURRENCY + (CONCURRENCY if updates else 0)
        verify_counts(report, db, table, url + "20", "json", expected_queries, 20 * CONCURRENCY, updates)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("base", nargs="?", default="http://127.0.0.1:8080")
    p.add_argument("--psql", default=os.environ.get("TFB_PSQL"),
                   help="psql command for the hello_world database (enables query counting and the update check)")
    p.add_argument("--plaintext", default="/plaintext")
    p.add_argument("--json", default="/json")
    p.add_argument("--db", default="/db")
    p.add_argument("--query", default="/queries?queries=")
    p.add_argument("--fortune", default="/fortunes")
    p.add_argument("--update", default="/updates?queries=")
    p.add_argument("--cached-query", default="/cached-queries?count=")
    args = p.parse_args()
    base = args.base.rstrip("/")
    report = Report()
    db = Db(args.psql) if args.psql else None
    if db is None:
        print("note: no --psql, so query counts and stored updates are not checked")

    report.section("plaintext")
    url = base + args.plaintext
    status, headers, body = get(url, "plaintext")
    report.check(status == 200, f"{url}: status {status}")
    report.check(b"hello, world!" in body.lower(), f"{url}: body {body[:40]!r}")
    report.check(len(body) == 13, f"{url}: {len(body) - 13} extra bytes", "warn")
    verify_headers(report, url, headers, "plaintext", True)

    report.section("json")
    url = base + args.json
    status, headers, body = get(url)
    report.check(status == 200, f"{url}: status {status}")
    try:
        obj = json.loads(body)
        lower = {k.lower(): v.lower() for k, v in obj.items()}
        report.check(lower.get("message") == "hello, world!", f"{url}: body {body!r}")
        report.check(len(lower) == 1, f"{url}: extra keys in {body!r}", "warn")
    except (ValueError, AttributeError) as e:
        report.check(False, f"{url}: not a JSON object: {e}")
    verify_headers(report, url, headers, "json", True)

    report.section("db")
    url = base + args.db
    status, headers, body = get(url)
    report.check(status == 200, f"{url}: status {status}")
    try:
        obj = json.loads(body)
        if isinstance(obj, list):
            report.check(False, f"{url}: response is an array, expected an object", "warn")
            obj = obj[0] if obj else None
        verify_world(report, url, obj)
    except ValueError as e:
        report.check(False, f"{url}: invalid JSON: {e}")
    verify_headers(report, url, headers, "json", True)
    verify_counts(report, db, "World", url, "json", CONCURRENCY, CONCURRENCY)

    report.section("query")
    verify_query_cases(report, db, base, args.query, "query", table="world")

    report.section("fortune")
    url = base + args.fortune
    status, headers, body = get(url, "html")
    report.check(status == 200, f"{url}: status {status}")
    parser = FortuneHTMLParser()
    parser.feed(body.decode())
    page = "".join(parser.body)
    if not report.check(page == VALID_FORTUNE, f"{url}: invalid according to FortuneHTMLParser"):
        import difflib
        for line in difflib.unified_diff(VALID_FORTUNE.split("\n"), page.split("\n"), "valid", "response", n=0, lineterm=""):
            print("        " + line)
    verify_headers(report, url, headers, "html", True)
    verify_counts(report, db, "fortune", url, "html", CONCURRENCY, 12 * CONCURRENCY)

    report.section("update")
    verify_query_cases(report, db, base, args.update, "update", table="world", updates=True)

    report.section("cached-query")
    verify_query_cases(report, db, base, args.cached_query, "cached-query")

    report.end_section()
    print()
    if report.failures:
        print(f"FAILED: {report.failures} failure(s), {report.warnings} warning(s)")
        sys.exit(1)
    print(f"OK: all TFB checks passed ({report.warnings} warning(s))")


if __name__ == "__main__":
    main()
