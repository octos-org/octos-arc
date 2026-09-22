#!/usr/bin/env python3
"""ARC-Bench scoreboard: leaderboards for every track + all of our runs.

Reads the public leaderboard endpoints (no login) and, when a session is
available, our own run history (login required). Prints one Markdown table
per section and optionally writes them to a file (docs/results.md).

Session sources, first one that works wins:
  --cookie-jar PATH        Netscape cookie jar (curl -c format) with the
                           arcbench_session cookie; default $ARC_COOKIE_JAR
                           or ~/.arc-cookies
  --email/--password       site login (NOT the model API key); the resulting
                           cookie is kept in memory only
Without a session the leaderboards still print; the "our runs" table says so.

Ranks follow the official leaderboard response order without excluding entries.
Cost and runtime alone do not establish how an application was generated.

usage:
  python3 arc/scoreboard.py                       # print
  python3 arc/scoreboard.py --out docs/results.md # print + write
  python3 arc/scoreboard.py --json runs.json      # dump raw payloads too
"""
from __future__ import annotations

import argparse
import datetime as dt
import http.cookiejar
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

BASE = "https://arc-bench.com/api"
TRACKS = ["smoke", "smoke-evolution", "ticket-booking", "arc-bench-web"]
NOTES_MARKER = "<!-- notes: everything below this line is kept across regenerations -->"


# ------------------------------------------------------------------ http

class Client:
    def __init__(self, jar: http.cookiejar.CookieJar | None = None):
        self.jar = jar or http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.jar))

    def get(self, path: str, **params) -> object:
        url = BASE + path
        if params:
            url += ("&" if "?" in url else "?") + urllib.parse.urlencode(params)
        with self.opener.open(url, timeout=60) as resp:
            return json.load(resp)

    def post_json(self, path: str, body: dict) -> object:
        req = urllib.request.Request(
            BASE + path, data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method="POST")
        with self.opener.open(req, timeout=60) as resp:
            return json.load(resp)

    def logged_in(self) -> dict | None:
        try:
            me = self.get("/auth/me")
        except urllib.error.HTTPError:
            return None
        return me.get("user") if isinstance(me, dict) else None


def load_jar(path: str) -> http.cookiejar.CookieJar | None:
    """Parse a Netscape cookie jar by hand: MozillaCookieJar asserts on
    host-only cookies written by some exporters (#HttpOnly_ prefix, no dot)."""
    if not path or not os.path.isfile(path):
        return None
    jar = http.cookiejar.CookieJar()
    with open(path, encoding="utf-8") as f:
        for line in f:
            raw = line.rstrip("\n")
            if raw.startswith("#HttpOnly_"):
                raw = raw[len("#HttpOnly_"):]
            if not raw or raw.startswith("#"):
                continue
            parts = raw.split("\t")
            if len(parts) < 7:
                continue
            domain, _flag, cpath, secure, expires, name, value = parts[:7]
            jar.set_cookie(http.cookiejar.Cookie(
                0, name, value, None, False, domain, domain.startswith("."), domain.startswith("."),
                cpath, True, secure.upper() == "TRUE", int(expires) or None, False, None, None, {}))
    return jar


# ------------------------------------------------------------------ data

def fetch_leaderboards(c: Client) -> dict[str, list[dict]]:
    out = {}
    for t in TRACKS:
        try:
            out[t] = c.get("/competitions/leaderboard", track="all", competition_id=t)
        except urllib.error.HTTPError as exc:
            print(f"[warn] leaderboard {t}: HTTP {exc.code}", file=sys.stderr)
            out[t] = []
    return out


def fetch_runs(c: Client, limit: int = 200) -> list[dict]:
    runs: list[dict] = []
    offset = 0
    while True:
        page = c.get("/runs", limit=limit, offset=offset)
        items = page["runs"] if isinstance(page, dict) and "runs" in page else page
        if not isinstance(items, list):
            break
        runs.extend(items)
        if len(items) < limit:
            break
        offset += limit
    return runs


# ------------------------------------------------------------------ format

def pct(v) -> str:
    return "—" if v is None else f"{v:.0f}%"


def cny(v) -> str:
    return "—" if v is None else f"¥{v:.2f}" if v >= 0.01 else f"¥{v:.6f}".rstrip("0").rstrip(".")


def secs(v) -> str:
    if v is None:
        return "—"
    v = int(v)
    return f"{v}s" if v < 120 else f"{v // 60}m{v % 60:02d}s"


def md_table(header: list[str], rows: list[list[str]]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(str(x) for x in r) + " |" for r in rows]
    return "\n".join(lines)


def summary_table(boards: dict[str, list[dict]], runs: list[dict], me: str | None) -> str:
    rows = []
    for t in TRACKS:
        board = boards.get(t) or []
        ours_all = [(i + 1, e) for i, e in enumerate(board) if me and e.get("username") in me]
        if not ours_all:
            rows.append([t, f"未上榜（榜共 {len(board)} 条）", "—", "—", "—", "—", "—"])
            continue
        rank, e = ours_all[0]  # best-ranked of our accounts
        run_ids = ", ".join(
            r["id"] for r in runs
            if r.get("competition_id") == t and r.get("status") in ("PASSED", "FAILED")
            and (r.get("passed_count") or 0) > 0)
        rows.append([
            t,
            f"{rank}/{len(board)}",
            pct(e.get("avg_pass_rate")), pct(e.get("avg_feature_implementation_rate")),
            cny(e.get("total_token_cost")), secs(e.get("avg_runtime_seconds")),
            run_ids or "—",
        ])
    return md_table(["赛道", "名次", "通过率", "功能率", "费用", "耗时", "运行编号（有效）"], rows)


def runs_table(runs: list[dict]) -> str:
    rows = []
    for r in sorted(runs, key=lambda r: r.get("created_at") or "", reverse=True):
        total = (r.get("passed_count") or 0) + (r.get("failed_count") or 0)
        rows.append([
            r["id"], r.get("competition_id"), r.get("requirement_id"), r.get("status"),
            f"{r.get('passed_count') or 0}/{total}" if total else "—",
            f"{r.get('feature_implemented_count') or 0}/{r.get('feature_total_count') or 0}",
            cny(r.get("token_cost_usd")),
            f"{(r.get('token_count') or 0) / 1e6:.2f}M" if r.get("token_count") else "—",
            secs(r.get("run_duration_seconds")),
            (r.get("created_at") or "")[:16].replace("T", " "),
            r.get("display_name") or "",
        ])
    return md_table(["运行编号", "赛道", "题目", "状态", "通过", "功能", "费用", "Token", "耗时", "创建(UTC)", "提交名"], rows)


def board_table(track: str, board: list[dict], me: str | None, top: int) -> str:
    rows = []
    for i, e in enumerate(board):
        if i >= top and not (me and e.get("username") in me):
            continue
        name = e.get("username") or ""
        if me and name in me:
            name = f"**{name}**"
        rows.append([
            i + 1, name,
            pct(e.get("avg_pass_rate")), pct(e.get("avg_feature_implementation_rate")),
            cny(e.get("total_token_cost")), secs(e.get("avg_runtime_seconds")),
            e.get("seniority_label") or "",
        ])
    return md_table(["官方排名", "队伍", "通过率", "功能率", "费用", "耗时", "级别"], rows)


def render(boards, runs, me, top) -> str:
    now = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    parts = [f"# ARC-Bench 成绩看板\n\n生成时间：{now}；账号：{', '.join(me) if me else '未登录'}；"
             "排名按官方榜单顺序，不排除任何参赛条目。\n",
             "## 各赛道我们的位置\n", summary_table(boards, runs, me), ""]
    if runs:
        parts += ["## 我们的全部运行\n", runs_table(runs), ""]
    else:
        parts += ["## 我们的全部运行\n\n（未登录，无法读取运行列表）\n"]
    for t in TRACKS:
        board = boards.get(t) or []
        parts += [f"## 榜单 · {t}（{len(board)} 条，显示前 {top} 名与我们）\n",
                  board_table(t, board, me, top), ""]
    return "\n".join(parts)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cookie-jar", action="append", default=None,
                    help="Netscape cookie jar; repeat for several accounts (all counted as \"us\"). Default $ARC_COOKIE_JAR or ~/.arc-cookies")
    ap.add_argument("--email"); ap.add_argument("--password")
    ap.add_argument("--username", help="comma-separated leaderboard display names counted as ours (default: from /auth/me of every jar)")
    ap.add_argument("--top", type=int, default=10)
    ap.add_argument("--out", help="write markdown here (e.g. docs/results.md)")
    ap.add_argument("--json", help="dump raw leaderboards + runs to this JSON file")
    args = ap.parse_args()

    jars = args.cookie_jar or [os.environ.get("ARC_COOKIE_JAR", os.path.expanduser("~/.arc-cookies"))]
    clients, users = [], []
    for jar in jars:
        c = Client(load_jar(jar))
        u = c.logged_in()
        if not u and args.email and args.password:
            try:
                c.post_json("/auth/login", {"email": args.email, "password": args.password})
                u = c.logged_in()
            except urllib.error.HTTPError as exc:
                print(f"[warn] login failed: HTTP {exc.code}", file=sys.stderr)
        clients.append(c); users.append(u)
    client = clients[0]; user = users[0]
    names = [n.strip() for n in args.username.split(",")] if args.username else [u.get("display_name") for u in users if u]
    me = names or None

    boards = fetch_leaderboards(client)
    runs = []
    for c, u in zip(clients, users):
        if u:
            runs += fetch_runs(c)
    text = render(boards, runs, me, args.top)
    print(text)
    if args.out:
        # Keep any hand-written section after the marker across regenerations.
        notes = ""
        if os.path.isfile(args.out):
            prev = open(args.out, encoding="utf-8").read()
            if NOTES_MARKER in prev:
                notes = prev[prev.index(NOTES_MARKER):]
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text + "\n" + (notes if notes else NOTES_MARKER + "\n"))
        print(f"[scoreboard] wrote {args.out}", file=sys.stderr)
    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump({"leaderboards": boards, "runs": runs, "user": user}, f, ensure_ascii=False, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())
