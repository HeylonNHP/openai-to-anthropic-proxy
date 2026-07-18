#!/usr/bin/env python3
"""Binary-search for the bad tool by sending subsets to airia."""
import json
import subprocess
import sys
import time

with open("target/last-upstream-body.json") as f:
    body = json.load(f)
body["stream"] = False
body["max_completion_tokens"] = 1024

AUTH = "Bearer agk-Mjk5MjAxMDk2NHwxNzgwODczMTUwOTU1fHRpLU1ERTVOMkUyTW1RdFltRTNaQzAzWXpFM0xUa3pObUl0TlRBeFpqUXlOakZoTWpsanwxfDE1NzI0MjgyODF8"


def send_tools(tools):
    body["tools"] = tools
    r = subprocess.run(
        [
            "curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
            "-X", "POST", "https://prodaus.gateway.airia.ai/v1/chat/completions",
            "-H", f"Authorization: {AUTH}",
            "-H", "Content-Type: application/json",
            "--data", json.dumps(body),
        ],
        capture_output=True, text=True, timeout=30,
    )
    return r.stdout.strip()


all_tools = body["tools"][:16]  # 0-15 is the bad range
print(f"Testing {len(all_tools)} tools in 0-15...")

# Try each individual tool
for i, t in enumerate(all_tools):
    code = send_tools([t])
    name = t["function"]["name"]
    print(f"  [{i:2d}] {name:25s} -> HTTP {code}")
    time.sleep(0.5)  # rate-limit ourselves
