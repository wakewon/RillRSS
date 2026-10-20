#!/usr/bin/env python3
"""Generate synthetic RSS feeds and serve them for load testing.

    python3 scripts/loadtest.py --feeds 10 --items 1000 --port 18998 --dir /tmp/rill-load

Then subscribe RillRSS to http://<host>:18998/feed0.xml ... feedN.xml (for many feeds,
import the generated feeds.opml). Set "首次抓取处理条数" (fetch.initial_items) high
enough and "忽略早于 N 天的条目" to 0 so every entry is processed. Generated text is
random words; nothing personal is involved.
"""
import argparse
import functools
import http.server
import os
import random

WORDS = (
    "government minister police officials protesters city council budget economy inflation workers union court "
    "judge election voters parliament agency report investigation hospital patients doctors school teachers "
    "students river storm flooding rescue teams airport flights border talks agreement sanctions market prices "
    "energy climate scientists study results company shares investors village residents museum festival "
    "football coach season victory defeat announced warned confirmed denied promised criticised welcomed"
).split()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--feeds", type=int, default=10)
    ap.add_argument("--items", type=int, default=1000)
    ap.add_argument("--port", type=int, default=18998)
    ap.add_argument("--dir", default="./loadtest-www")
    ap.add_argument("--seed", type=int, default=42)
    a = ap.parse_args()
    random.seed(a.seed)
    os.makedirs(a.dir, exist_ok=True)
    base = f"http://127.0.0.1:{a.port}"
    outlines = []
    for f in range(a.feeds):
        items = []
        for i in range(a.items):
            paras = "".join("<p>" + " ".join(random.choice(WORDS) for _ in range(40)) + ".</p>" for _ in range(6))
            items.append(
                f"<item><guid>feed{f}-item{i}</guid><title>Feed {f} story {i}</title>"
                f"<link>{base}/f{f}/{i}</link><pubDate>Tue, 20 Oct 2026 0{i % 10}:{i % 60:02d}:00 GMT</pubDate>"
                f"<description><![CDATA[{paras}]]></description></item>"
            )
        with open(os.path.join(a.dir, f"feed{f}.xml"), "w") as fh:
            fh.write(f"<?xml version='1.0'?><rss version='2.0'><channel><title>Load feed {f}</title><link>{base}/</link>{''.join(items)}</channel></rss>")
        outlines.append(f'<outline type="rss" text="Load feed {f}" xmlUrl="{base}/feed{f}.xml"/>')
    with open(os.path.join(a.dir, "feeds.opml"), "w") as fh:
        fh.write(f'<?xml version="1.0"?><opml version="2.0"><head><title>load</title></head><body><outline text="Load">{"".join(outlines)}</outline></body></opml>')
    print(f"generated {a.feeds} feeds x {a.items} items in {a.dir}; serving on {base}")
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=a.dir)
    http.server.ThreadingHTTPServer(("127.0.0.1", a.port), handler).serve_forever()


if __name__ == "__main__":
    main()
