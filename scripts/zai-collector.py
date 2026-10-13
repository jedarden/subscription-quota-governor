#!/usr/bin/env python3
"""Z.ai quota and stable full-replica saturation collector (stdlib only)."""
import argparse
import copy
import concurrent.futures
import json
import math
import os
import re
import signal
import ssl
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone

from subgov_adapter_common import atomic_json, locked, read_json

SAMPLE = re.compile(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)(?:\s+\S+)?$')
LABEL = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\.|[^"\\])*)"')
COUNTERS = ("requests", "completed", "retries", "capacity", "concurrency")
GENERATION_PATHS = {"/v1/messages", "/messages"}


def labels(raw):
    return {m.group(1): re.sub(r'\\(.)', lambda e: {"n": "\n", "r": "\r", "t": "\t"}.get(e[1], e[1]), m.group(2)) for m in LABEL.finditer(raw or "")}


def parse_metrics(body, now, stale_after=900):
    dedicated_concurrency = any(
        line.startswith(("# TYPE zai_proxy_upstream_concurrency_rejections_total ",
                         "zai_proxy_upstream_concurrency_rejections_total ",
                         "zai_proxy_upstream_concurrency_rejections_total{"))
        for line in body.splitlines())
    values = {key: 0 for key in COUNTERS}
    family = set()
    starts = set()
    usage, resets = {}, {}
    age = None
    generation_supported = False
    generation_503 = 0
    generation_status_complete = True
    for line in body.splitlines():
        if line.startswith("# TYPE "):
            parts = line.split()
            if len(parts) == 4:
                family.add(parts[2])
            continue
        match = SAMPLE.match(line)
        if not match:
            continue
        name, raw_labels, raw_value = match.groups()
        tags = labels(raw_labels)
        relevant = name.startswith("zai_proxy_") or name == "process_start_time_seconds"
        if not relevant:
            continue
        value = float(raw_value)
        if not math.isfinite(value) or value < 0:
            raise ValueError("invalid metrics value")
        family.add(name)
        if name == "process_start_time_seconds":
            starts.add(value)
        elif name == "zai_proxy_quota_usage_ratio":
            usage[tags.get("window")] = min(value, 1.0)
        elif name == "zai_proxy_quota_reset_time_seconds":
            resets[tags.get("window")] = value
        elif name == "zai_proxy_quota_sample_age_seconds":
            age = value
        else:
            field = None
            generation = tags.get("method") == "POST" and tags.get("path") in GENERATION_PATHS
            if name == "zai_proxy_requests_total" and generation:
                field = "requests"
                generation_supported = True
                status = tags.get("status_code", "")
                generation_status_complete &= re.fullmatch(r"[1-5]\d\d", status) is not None
                if status == "503":
                    generation_503 += value
            elif name == "zai_proxy_request_duration_seconds_count" and generation and re.fullmatch(r"2\d\d", tags.get("status_code", "")):
                field = "completed"
            elif name == "zai_proxy_retry_attempts_total" and tags.get("reason") == "retry":
                field = "retries"
            elif name == "zai_proxy_rate_limit_rejections_total":
                field = "capacity"
            elif name == "zai_proxy_upstream_concurrency_rejections_total":
                field = "concurrency"
            elif name == "zai_proxy_zai_errors_total" and tags.get("code") == "1302" and not dedicated_concurrency:
                field = "concurrency"
            if field:
                if not value.is_integer() or value > 2**64 - 1:
                    raise ValueError("invalid counter")
                values[field] += int(value)
    if age is None or not usage or None in usage or any(not resets.get(window) for window in usage):
        raise ValueError("quota metrics incomplete")
    moment = datetime.fromtimestamp(now, timezone.utc)
    windows = []
    for window, used in sorted(usage.items()):
        reset = datetime.fromtimestamp(resets[window], timezone.utc)
        # Z.ai starts the five-hour window on first use. A lapsed nonempty
        # window remains lapsed; only a verified zero-use window is projected.
        if reset <= moment and used == 0 and window == "five_hour":
            reset = moment + timedelta(minutes=300)
        item = {"id": window, "used_fraction": used, "resets_at": reset.isoformat().replace("+00:00", "Z"), "reached": used >= 1}
        if window == "five_hour":
            item["duration_minutes"] = 300
        windows.append(item)
    snapshot = {"observed_at": (moment - timedelta(seconds=age)).isoformat().replace("+00:00", "Z"), "fresh": age < stale_after, "windows": windows}
    required = {"zai_proxy_requests_total", "zai_proxy_request_duration_seconds", "zai_proxy_retry_attempts_total", "zai_proxy_rate_limit_rejections_total"}
    # Prometheus CounterVec omits the family until its first label is used.
    # handler.go's local admission gate *always* increments generation status
    # 503 requests as well as capacity rejects. A complete lifetime status
    # breakdown with no 503 proves that this generation has no local rejects.
    # Any 503 or unknown status prevents inferring zero from absent capacity.
    if generation_supported and generation_status_complete and generation_503 == 0:
        required.discard("zai_proxy_rate_limit_rejections_total")
    # The histogram's TYPE uses its base name, while samples use _count.
    if "zai_proxy_request_duration_seconds_count" in family:
        family.add("zai_proxy_request_duration_seconds")
    usable = len(starts) == 1 and next(iter(starts), 0) > 0 and required <= family and generation_supported
    sample = {"at": now, "start": next(iter(starts), None), "classified": bool({"zai_proxy_zai_errors_total", "zai_proxy_upstream_concurrency_rejections_total"} & family), **values} if usable else None
    return snapshot, sample


def update_saturation(readings, state, minimum_interval=60, maximum_interval=900, minimum_attempts=30):
    """Consume exactly one full, fresh scrape per stable replica membership.

    Never combine a known replica delta with a first-seen/restarted replica.
    Every emitted interval has the same baseline time across all replicas.
    """
    previous = state.get("replicas", {})
    comparable = previous.keys() == readings.keys() and bool(readings)
    elapsed = []
    if comparable:
        for key, current in readings.items():
            old = previous[key]
            duration = current["at"] - old["at"]
            if current["start"] != old["start"] or any(current[field] < old[field] for field in COUNTERS) or duration <= 0 or duration > maximum_interval:
                comparable = False
                break
            elapsed.append(duration)
    if not comparable:
        state["replicas"] = readings
        return None
    if min(elapsed) < minimum_interval:
        # Retain the baseline until enough time elapses; repeated reads cannot
        # reuse an already emitted delta because emission advances all replicas.
        return None
    totals = {field: sum(current[field] - previous[key][field] for key, current in readings.items()) for field in COUNTERS}
    state["replicas"] = readings
    attempts = totals["requests"] - totals["capacity"] + totals["retries"]
    if attempts < 0 or totals["concurrency"] > attempts or totals["completed"] > attempts:
        return None
    classified = all(current["classified"] and previous[key]["classified"] for key, current in readings.items())
    return {"completed_requests": totals["completed"], "window_seconds": int(round(max(elapsed))), "attempts": attempts, "upstream_concurrency_fraction": totals["concurrency"] / attempts if classified and attempts >= minimum_attempts and attempts > 0 else None}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("redirects disabled")


def validate_url(url):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError("metrics/API URLs must be credential-free HTTP URLs without query/fragment")
    return url.rstrip("/")


def fetch(url, insecure=False, timeout=8):
    context = ssl._create_unverified_context() if insecure else ssl.create_default_context()
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=context))
    with opener.open(url, timeout=timeout) as response:
        body = response.read(4_000_001)
    if len(body) > 4_000_000:
        raise ValueError("metrics response too large")
    return body.decode("utf-8")


def discover(args):
    base = validate_url(args.kube_api)
    namespace = urllib.parse.quote(args.namespace, safe="")
    query = urllib.parse.urlencode({"labelSelector": args.selector})
    pods = json.loads(fetch(f"{base}/api/v1/namespaces/{namespace}/pods?{query}", args.insecure_tls, timeout=2))
    endpoints = {}
    healthy = True
    for pod in pods["items"]:
        metadata = pod["metadata"]
        if metadata.get("deletionTimestamp"):
            healthy = False
        ready = any(item.get("type") == "Ready" and item.get("status") == "True" for item in pod.get("status", {}).get("conditions", []))
        healthy &= ready and pod.get("status", {}).get("phase") == "Running"
        name = urllib.parse.quote(metadata["name"], safe="")
        endpoints[metadata["uid"]] = f"{base}/api/v1/namespaces/{namespace}/pods/{name}:{args.port}/proxy/metrics"
    if not endpoints or len(endpoints) > 8:
        raise ValueError("pod discovery returned invalid replica count")
    return endpoints, healthy


def collect(args):
    membership = None
    membership_args = None
    if args.expected_replicas:
        if not args.metrics_url or len(args.metrics_url) != 1 or not args.membership_api:
            raise ValueError("load-balanced coverage requires one metrics URL and membership API")
        membership_args = copy.copy(args)
        membership_args.kube_api = args.membership_api
        members, member_healthy = discover(membership_args)
        membership = sorted(members)
        if len(membership) != args.expected_replicas or not member_healthy:
            raise ValueError("expected healthy membership unavailable")
    if args.metrics_url:
        urls = [validate_url(url) for url in args.metrics_url]
        if len(urls) != len(set(urls)) or not 1 <= len(urls) <= 8:
            raise ValueError("metrics URLs must be unique and at most eight")
        endpoints, healthy = dict(zip(urls, urls)), True
        if args.expected_replicas:
            endpoints = {str(index): urls[0] for index in range(min(8, args.expected_replicas * 3))}
    else:
        endpoints, healthy = discover(args)
    bodies = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        futures = {executor.submit(fetch, url, args.insecure_tls): key for key, url in endpoints.items()}
        for future in concurrent.futures.as_completed(futures):
            try:
                bodies[futures[future]] = future.result()
            except Exception:
                healthy = False
    now = time.time()
    snapshots, readings = [], {}
    for key, body in sorted(bodies.items()):
        try:
            quota, sample = parse_metrics(body, now, args.stale_after)
            snapshots.append(quota)
            if sample is None or not quota["fresh"]:
                healthy = False
            else:
                if args.expected_replicas:
                    # Several scrapes can hit one process. Use its latest full
                    # counters, never add repeated readings of that process.
                    identity = str(sample["start"])
                    old = readings.get(identity)
                    if old is None or all(sample[field] >= old[field] for field in COUNTERS):
                        readings[identity] = sample
                    elif not all(sample[field] <= old[field] for field in COUNTERS):
                        healthy = False
                else:
                    readings[key] = sample
        except (ValueError, OverflowError, OSError):
            healthy = False
    if args.kube_api:
        after, after_healthy = discover(args)
        healthy &= after == endpoints and after_healthy
    if membership_args:
        after, after_healthy = discover(membership_args)
        healthy &= sorted(after) == membership and after_healthy
    if not snapshots:
        raise ValueError("no usable quota response")
    # Prefer the freshest account-wide quota reading. Saturation still requires
    # every configured/discovered replica and every quota poller to be fresh.
    snapshot = max(snapshots, key=lambda quota: quota["observed_at"])
    with locked(args.state):
        state = read_json(args.state, {"version": 1, "replicas": {}})
        if state.get("version") != 1 or not isinstance(state.get("replicas"), dict):
            raise ValueError("unsupported baseline state")
        complete = len(readings) == args.expected_replicas if args.expected_replicas else readings.keys() == endpoints.keys()
        if state.get("membership") != membership:
            state["replicas"] = {}
        state["membership"] = membership
        if healthy and complete:
            saturation = update_saturation(readings, state, args.minimum_interval, args.maximum_interval, args.minimum_attempts)
            if saturation is not None:
                snapshot["saturation"] = saturation
        else:
            state["replicas"] = {}
            print("zai-collector: full fresh stable replica sample unavailable; saturation withheld", file=sys.stderr)
        atomic_json(args.state, state)
    if args.backlog_capacity_file:
        capacity = read_json(args.backlog_capacity_file)
        if not isinstance(capacity, dict) or not isinstance(capacity.get("observed_at"), str):
            raise ValueError("eligible backlog capacity requires timestamped object")
        stamp = datetime.fromisoformat(capacity["observed_at"].replace("Z", "+00:00"))
        age = (datetime.now(timezone.utc) - stamp).total_seconds()
        value = capacity.get("eligible_backlog_capacity")
        if type(value) is not int or not 0 <= value <= 2**32 - 1:
            raise ValueError("invalid eligible backlog capacity")
        if 0 <= age < args.stale_after:
            snapshot["eligible_backlog_capacity"] = value
        else:
            print("zai-collector: backlog sample stale; eligible capacity withheld", file=sys.stderr)
    return snapshot


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--metrics-url", action="append", help="stable full metrics URL for EACH replica; repeat for all replicas")
    source.add_argument("--kube-api", help="credential-free read-only Kubernetes API proxy")
    parser.add_argument("--namespace", default="mcp")
    parser.add_argument("--selector", default="app=zai-proxy")
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--state", required=True)
    parser.add_argument("--expected-replicas", type=int, help="bounded LB coverage; requires membership API and one metrics URL")
    parser.add_argument("--membership-api", help="read-only Kubernetes API used to prove stable LB membership")
    parser.add_argument("--backlog-capacity-file", help="timestamped JSON eligible_backlog_capacity from a trusted backlog observer")
    parser.add_argument("--stale-after", type=float, default=900)
    parser.add_argument("--minimum-interval", type=int, default=60)
    parser.add_argument("--maximum-interval", type=int, default=900)
    parser.add_argument("--minimum-attempts", type=int, default=30)
    parser.add_argument("--deadline-seconds", type=int, default=12,
                        help="collection deadline; values above 12 require an asynchronous snapshot publisher")
    parser.add_argument("--insecure-tls", action="store_true", help="explicit opt-in for trusted site-local TLS endpoints only")
    args = parser.parse_args()
    if not 1 <= args.minimum_interval <= args.maximum_interval or args.minimum_attempts < 1 or args.stale_after <= 0 or not 1 <= args.port <= 65535:
        parser.error("invalid interval, threshold, or port")
    if args.expected_replicas is not None and not 1 <= args.expected_replicas <= 8:
        parser.error("expected replicas must be between one and eight")
    def deadline(_signum, _frame):
        print("zai-collector: collection deadline exceeded", file=sys.stderr, flush=True)
        # Also terminate stuck scrape threads. Direct command sources retain
        # the default 12s deadline; longer deadlines require a file publisher.
        os._exit(1)
    signal.signal(signal.SIGALRM, deadline)
    if not 1 <= args.deadline_seconds <= 120:
        parser.error("deadline must be between 1 and 120 seconds")
    signal.alarm(args.deadline_seconds)
    try:
        print(json.dumps(collect(args), allow_nan=False))
    except Exception as error:
        print(f"zai-collector: collection failed ({type(error).__name__})", file=sys.stderr)
        return 1
    finally:
        signal.alarm(0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
