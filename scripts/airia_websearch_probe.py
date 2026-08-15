#!/usr/bin/env python3
"""Probe the Airia gateway's /v1/responses endpoint directly to capture
the RAW response shape for a web_search call, bypassing the proxy.

Reads proxy.json for the upstream URL + API key so this matches the
proxy's exact configuration. Prints the raw JSON response to stdout.
"""
import json
import os
import sys
import urllib.request
import urllib.error

HERE = os.path.dirname(os.path.abspath(__file__))
PROXY_JSON = os.path.join(HERE, "..", "proxy.json")


def load_config():
    with open(PROXY_JSON, "r", encoding="utf-8") as f:
        return json.load(f)


def main():
    cfg = load_config()
    base = cfg["upstream_base_url"].rstrip("/")
    path = cfg.get("upstream_path") or "/v1/responses"
    url = base + path
    api_key = cfg["upstream_api_key"]
    model = cfg.get("model_aliases", {}).get("default_model", "gpt-5.4-mini")

    body = {
        "model": model,
        "instructions": "You are a helpful assistant. Use web search when asked about current events or weather.",
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "What is the current weather in Townsville?"}
                ],
            }
        ],
        "tools": [
            {
                "type": "web_search",
                "max_uses": None,
                "search_context_size": "medium",
                "allowed_domains": ["weather.com", "bom.gov.au", "accuweather.com"],
            }
        ],
        "tool_choice": "auto",
        "reasoning": {"effort": "low"},
        "max_output_tokens": 1024,
        "stream": False,
        "store": False,
    }

    print(f"POST {url}", file=sys.stderr)
    print(f"model={model}", file=sys.stderr)
    print(f"request body:\n{json.dumps(body, indent=2)}\n", file=sys.stderr)

    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {api_key}",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            raw = resp.read().decode("utf-8")
            print("HTTP", resp.status, file=sys.stderr)
            print("--- RAW BODY ---")
            print(raw)
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        print("HTTP ERROR", e.code, file=sys.stderr)
        print("--- ERROR BODY ---")
        print(raw)
    except Exception as e:  # noqa: BLE001
        print("ERROR", repr(e), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
