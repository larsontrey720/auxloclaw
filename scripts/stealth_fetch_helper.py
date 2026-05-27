#!/usr/bin/env python3
"""Stealth fetch helper - called by auxloclaw StealthFetchTool."""

import argparse
import json
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["stealth", "simple", "dynamic"], default="stealth")
    parser.add_argument("--method", choices=["GET", "POST"], default="GET")
    parser.add_argument("--url", required=True)
    parser.add_argument("--body", default="")
    parser.add_argument("--headers", default="{}")
    parser.add_argument("--selector", default="")
    args = parser.parse_args()

    extra_headers = json.loads(args.headers) if args.headers else {}

    if args.mode == "stealth":
        from scrapling import StealthyFetcher
        fetcher = StealthyFetcher()
    elif args.mode == "dynamic":
        from scrapling import DynamicFetcher
        fetcher = DynamicFetcher()
    else:
        from scrapling import Fetcher
        fetcher = Fetcher()

    try:
        if args.mode == "simple":
            # Fetcher has explicit get/post/put/delete methods
            if args.method == "POST":
                page = fetcher.post(args.url, data=args.body, headers=extra_headers if extra_headers else None)
            else:
                page = fetcher.get(args.url, headers=extra_headers if extra_headers else None)
        else:
            # StealthyFetcher and DynamicFetcher use .fetch(url) method
            page = fetcher.fetch(args.url)

        if args.selector:
            results = page.css(args.selector).getall()
            if results:
                print(json.dumps(results, indent=2))
            else:
                print(json.dumps([]))
        else:
            # page.text may be empty; page.body (bytes) is reliable
            if hasattr(page, 'body') and page.body:
                text = page.body.decode('utf-8', errors='replace')
            elif hasattr(page, 'text') and page.text:
                text = str(page.text)
            elif hasattr(page, 'html_content') and page.html_content:
                text = str(page.html_content)
            else:
                text = str(page)
            status = page.status if hasattr(page, 'status') else 'unknown'
            print(f"[Status: {status}]\n{text}")

    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
