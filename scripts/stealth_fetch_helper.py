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
        if args.method == "POST":
            page = fetcher.post(args.url, data=args.body, headers=extra_headers if extra_headers else None)
        else:
            page = fetcher.get(args.url, headers=extra_headers if extra_headers else None)

        if args.selector:
            results = page.css(args.selector).getall()
            if results:
                print(json.dumps(results, indent=2))
            else:
                print(json.dumps([]))
        else:
            text = page.text if hasattr(page, 'text') else str(page)
            status = page.status if hasattr(page, 'status') else 'unknown'
            print(f"[Status: {status}]\n{text}")

    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
