#!/usr/bin/env python3
"""li_stats.py — join LinkedIn's analytics export onto our posted-video log.

LinkedIn's `memberCreatorPostAnalytics` API would give these numbers directly,
but the `r_member_postAnalytics` scope is only granted to applications LinkedIn
approves, so the export from Analytics & Tools -> Export is the way in for now.
It identifies posts by their public slug URL, which embeds the ugcPost id we
already record in ~/li_push_log.jsonl — so the join is exact, not fuzzy, and
each row can be reported against the video it came from.

Stdlib only (an .xlsx is a zip of XML), matching the other tools here.

Usage:
    ./li_stats.py <AggregateAnalytics_*.xlsx> [--csv out.csv]
"""

import argparse
import json
import os
import re
import sys
import zipfile
from xml.etree import ElementTree as ET

NS = {"m": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"}
LOG = os.path.join(os.path.expanduser("~"), "li_push_log.jsonl")


def col_index(ref):
    """'BC12' -> zero-based column 54. Sparse rows omit empty cells, so the
    cell reference is the only reliable way to know which column a value is in."""
    letters = "".join(c for c in ref if c.isalpha())
    n = 0
    for c in letters:
        n = n * 26 + (ord(c.upper()) - 64)
    return n - 1


def read_sheet(path, wanted):
    """Return the named sheet as a list of row-lists, honouring sparse cells."""
    z = zipfile.ZipFile(path)
    shared = []
    if "xl/sharedStrings.xml" in z.namelist():
        for si in ET.fromstring(z.read("xl/sharedStrings.xml")).findall("m:si", NS):
            shared.append("".join(t.text or "" for t in si.iter("{%s}t" % NS["m"])))
    wb = ET.fromstring(z.read("xl/workbook.xml"))
    names = [s.get("name") for s in wb.iter("{%s}sheet" % NS["m"])]
    if wanted not in names:
        sys.exit(f"ERROR: no '{wanted}' sheet — found: {names}")
    root = ET.fromstring(z.read(f"xl/worksheets/sheet{names.index(wanted) + 1}.xml"))
    rows = []
    for row in root.iter("{%s}row" % NS["m"]):
        cells = {}
        for c in row.findall("m:c", NS):
            v = c.find("m:v", NS)
            if v is None or v.text is None:
                continue
            val = shared[int(v.text)] if c.get("t") == "s" else v.text
            cells[col_index(c.get("r", "A1"))] = val
        rows.append([cells.get(i, "") for i in range(max(cells) + 1)] if cells else [])
    return rows


def parse_metrics(rows):
    """Pull {post_id: {metric: value, ...}} out of the top-posts sheet.

    The sheet holds several independently ranked blocks side by side (Engagement
    in one, Impressions in another), each as URL | date | value. Locate them by
    header text rather than fixed columns, so a changed layout degrades to
    "metric missing" instead of silently reading the wrong column.
    """
    header_i = next(
        (i for i, r in enumerate(rows) if any("URL des Beitrags" in c or "Post URL" in c for c in r)),
        None,
    )
    if header_i is None:
        sys.exit("ERROR: could not find the header row in the top-posts sheet.")
    header = rows[header_i]
    blocks = [
        (i, header[i])
        for i, name in enumerate(header)
        if name and not any(k in name for k in ("URL", "veröffentlicht", "published"))
    ]
    out = {}
    for row in rows[header_i + 1:]:
        for value_col, metric in blocks:
            url_col = value_col - 2
            if url_col < 0 or url_col >= len(row) or value_col >= len(row):
                continue
            url, raw = row[url_col], row[value_col]
            m = re.search(r"ugcPost-(\d+)", url or "")
            if not m or not raw:
                continue
            rec = out.setdefault(m.group(1), {"url": url})
            try:
                rec[metric] = int(float(raw))
            except ValueError:
                pass
            date_col = value_col - 1
            if date_col < len(row) and row[date_col]:
                rec.setdefault("date", row[date_col])
    return out


def load_log():
    """{numeric ugcPost id: log entry} for everything li_push has posted."""
    posts = {}
    try:
        with open(LOG) as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    e = json.loads(line)
                except ValueError:
                    continue
                m = re.search(r"ugcPost:(\d+)", e.get("post_id", ""))
                if m:
                    posts[m.group(1)] = e
    except FileNotFoundError:
        sys.exit(f"ERROR: no upload log at {LOG}")
    return posts


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("xlsx", help="the AggregateAnalytics_*.xlsx from LinkedIn")
    ap.add_argument("--sheet", default="TOP-BEITRÄGE",
                    help="top-posts sheet name (TOP-BEITRÄGE / TOP POSTS)")
    ap.add_argument("--csv", help="also write the joined rows to this CSV")
    args = ap.parse_args()

    stats = parse_metrics(read_sheet(args.xlsx, args.sheet))
    log = load_log()

    metrics = sorted({k for v in stats.values() for k in v if k not in ("url", "date")})
    rows = []
    for pid, s in stats.items():
        entry = log.get(pid)
        rows.append({
            "youtube_id": (entry or {}).get("youtube_id", ""),
            "title": (entry or {}).get("title", "(not posted by li_push)"),
            "date": s.get("date", ""),
            "url": s["url"],
            **{m: s.get(m) for m in metrics},
        })
    # Rank by the widest-reach metric available.
    key = "Impressions" if "Impressions" in metrics else (metrics[0] if metrics else None)
    rows.sort(key=lambda r: (r.get(key) is None, -(r.get(key) or 0)))

    width = 46
    print(f"{'VIDEO':<{width}} " + " ".join(f"{m[:11]:>11}" for m in metrics))
    for r in rows:
        title = (r["title"] or "")[:width - 1]
        print(f"{title:<{width}} " +
              " ".join(f"{(r.get(m) if r.get(m) is not None else '-'):>11}" for m in metrics))

    matched = sum(1 for r in rows if r["youtube_id"])
    print(f"\n{len(rows)} posts in the export, {matched} matched to a video in the log.")
    for m in metrics:
        vals = [r[m] for r in rows if r.get(m) is not None]
        if vals:
            print(f"  {m}: total {sum(vals)}, best {max(vals)}, median {sorted(vals)[len(vals)//2]}")
    print("\nNB the export's top-posts sheet is capped (max 50 posts) and covers only "
          "the date range you chose, so this is not the full 132-post history.")

    if args.csv:
        import csv
        with open(args.csv, "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=["youtube_id", "title", "date", "url"] + metrics)
            w.writeheader()
            w.writerows(rows)
        print(f"Wrote {args.csv}")


if __name__ == "__main__":
    main()
