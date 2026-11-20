"""Collect the resource, tee and DNS measurements needed by F07."""

from contextlib import closing
from http.client import HTTPConnection
import json
from math import ceil
import os
from pathlib import Path
import subprocess
import sys
import time


def percentile(values, percent):
    return sorted(values)[ceil(len(values) * percent / 100) - 1]


def sample(pid):
    proc_stat = Path(f"/proc/{pid}/stat")
    ticks_per_second = os.sysconf("SC_CLK_TCK") if sys.platform == "linux" else None
    while True:
        if ticks_per_second:
            try:
                # The comm field is parenthesized and may contain spaces. The rest starts at field 3.
                fields = proc_stat.read_text().rsplit(")", 1)[1].split()
            except FileNotFoundError:
                return
            cpu_seconds = (int(fields[11]) + int(fields[12])) / ticks_per_second
            rss_kb = int(fields[21]) * os.sysconf("SC_PAGE_SIZE") // 1024
        else:
            result = subprocess.run(
                ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
            )
            if result.returncode or not result.stdout.strip():
                return
            rss_kb = int(result.stdout)
            cpu_seconds = None
        print(json.dumps([time.monotonic(), rss_kb, cpu_seconds]), flush=True)
        time.sleep(1)


def summarize(samples_path, warmup_seconds, duration):
    samples = [json.loads(line) for line in Path(samples_path).read_text().splitlines()]
    steady = [row for row in samples if warmup_seconds <= row[0] - samples[0][0] <= duration]
    if len(steady) < 2:
        return {"rss_kb_p99": None, "cpu_cores_mean": None, "samples": len(steady)}
    start, _, start_cpu = steady[0]
    end, _, end_cpu = steady[-1]
    cpu_cores = (end_cpu - start_cpu) / (end - start) if start_cpu is not None else None
    return {
        "rss_kb_p99": percentile([row[1] for row in steady], 99),
        "cpu_cores_mean": cpu_cores,
        "samples": len(steady),
    }


def read_counters(port):
    with closing(HTTPConnection("127.0.0.1", port, timeout=5)) as conn:
        conn.request("GET", "/metrics")
        response = conn.getresponse()
        if response.status != 200:
            raise ValueError(f"指标端点返回 HTTP {response.status}")
        counters = {}
        for line in response.read().decode().splitlines():
            if line and not line.startswith("#"):
                name, value = line.split()[:2]
                counters[name] = float(value)
        return counters


def tee_snapshot(port, completed):
    deadline = time.monotonic() + 5
    while True:
        counters = read_counters(port)
        streams = int(counters.get("argus_tee_streams_total", 0))
        if streams >= completed:
            # The stream count is incremented after the two byte counters. Scrape again
            # so the final stream cannot straddle two counters in one scrape.
            counters = read_counters(port)
            streams = int(counters["argus_tee_streams_total"])
            break
        if time.monotonic() >= deadline:
            break
        time.sleep(0.1)
    forwarded = counters.get('argus_bytes_forwarded_total{direction="response"}')
    dropped = counters.get("argus_tee_dropped_bytes_total")
    return {
        "streams": streams,
        "bytes_forwarded": int(forwarded) if forwarded is not None else None,
        "bytes_dropped": int(dropped) if dropped is not None else None,
    }


def first_byte(conn):
    conn.connect()
    started = time.monotonic_ns()
    conn.request(
        "POST",
        "/provider/mock/v1/messages",
        body=b"{}",
        headers={"Content-Type": "application/json", "Connection": "close"},
    )
    response = conn.getresponse()
    if response.status != 200 or not response.read(1):
        response.close()
        raise ValueError(f"DNS 探测未收到正常的流式响应（HTTP {response.status}）")
    return response, (time.monotonic_ns() - started) // 1000


def dns_pair(port):
    with (
        closing(HTTPConnection("127.0.0.1", port, timeout=5)) as cold,
        closing(HTTPConnection("127.0.0.1", port, timeout=5)) as warm,
    ):
        before = read_counters(port)
        response, miss = first_byte(cold)
        # Keep the first stream active so the second request opens another upstream connection.
        with closing(response):
            middle = read_counters(port)
            response, hit = first_byte(warm)
            with closing(response):
                after = read_counters(port)
    hits = "argus_dns_cache_hits_total"
    misses = "argus_dns_cache_misses_total"
    if middle[misses] - before[misses] != 1 or middle[hits] != before[hits]:
        raise ValueError("DNS 冷缓存请求未单独触发一次未命中")
    if after[hits] - middle[hits] != 1 or after[misses] != middle[misses]:
        raise ValueError("DNS 热缓存请求未单独触发一次命中")
    return {"hit_us": hit, "miss_us": miss}


def dns_summary(path):
    pairs = [json.loads(line) for line in Path(path).read_text().splitlines()]
    return {
        key: {
            "p50": percentile([row[key] for row in pairs], 50),
            "p99": percentile([row[key] for row in pairs], 99),
            "samples": len(pairs),
        }
        for key in ("hit_us", "miss_us")
    }


if __name__ == "__main__":
    mode, *args = sys.argv[1:]
    if mode == "sample":
        sample(int(args[0]))
    elif mode == "summarize":
        print(json.dumps(summarize(args[0], int(args[1]), int(args[2]))))
    elif mode == "tee":
        print(json.dumps(tee_snapshot(*map(int, args))))
    elif mode == "dns-pair":
        print(json.dumps(dns_pair(int(args[0]))))
    elif mode == "dns-summary":
        print(json.dumps(dns_summary(args[0])))
    else:
        sys.exit(f"未知的指标采集命令：{mode}")
