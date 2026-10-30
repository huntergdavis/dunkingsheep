#!/usr/bin/env python3
"""Capture an authenticated served desk without rebuilding or sending app writes.

Use an explicitly selected origin and VP_TEST_CREDENTIALS_PATH (the existing
Password: ... harness format). Only the initial /api/login POST is allowed.
Every later non-GET attempt, including viewport telemetry, fails the capture.
No credentials, cookies, response bodies, or request headers enter the receipt.
"""
import argparse
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import sys
from urllib.parse import urlsplit

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tools.theme_capture import GUARD_SCRIPT

VIEWPORTS = ((1440, 900), (390, 844))
NOT_VERIFIED = [
    "Real sends, uploads, refresh writes, or gameplay",
    "Other viewports, themes, devices, engines, or physical browser zoom",
    "Full accessibility, contrast, or source-to-deployment identity",
    "Behaviour after the bounded capture observation window",
]


class Refused(Exception):
    """A fixed, nonsecret failure code suitable for an evidence receipt."""


def origin(url):
    parsed = urlsplit(url)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname
            or parsed.username or parsed.password):
        raise ValueError("Expected an HTTP(S) origin without credentials")
    return parsed.scheme, parsed.hostname, parsed.port or (443 if parsed.scheme == "https" else 80)


def closed(counts):
    return (set(counts) == {"role_dialog", "modal", "backdrop", "open_dialog"}
            and all(type(value) is int and value == 0 for value in counts.values()))


def read_password(path):
    try:
        lines = Path(path).read_text().splitlines()
        values = [line[len("Password: "):] for line in lines if line.startswith("Password: ")]
        if len(values) != 1 or not values[0]:
            raise ValueError()
        return values[0]
    except Exception:
        raise Refused("credential_input_invalid") from None


def chromium_path():
    configured = os.environ.get("PLAYWRIGHT_CHROMIUM_EXECUTABLE")
    if configured:
        return configured
    cached = sorted((Path.home() / ".cache/ms-playwright").glob("chromium-*/chrome-linux64/chrome"))
    return str(cached[-1]) if cached else None


def capture(args):
    # Never overwrite an earlier evidence run or retain a credential pathname.
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=False)
    report = dict(schema=1, started_at=datetime.now().astimezone().isoformat(),
                  requested_path=args.path, source="served HTTP response bytes; no rebuild",
                  emulation="viewport + deviceScaleFactor, not real browser zoom",
                  device_scale_factor=1, login_posts=0, cases=[], violations=[],
                  failures=[], passed=False, not_verified=NOT_VERIFIED)
    expected_origin = origin(args.url)
    asset_versions = {}
    expected_bundles = None
    stage = "setup"
    try:
        from playwright.sync_api import sync_playwright
        password = read_password(args.credentials_path)
        with sync_playwright() as pw:
            browser = pw.chromium.launch(headless=True, executable_path=chromium_path(),
                                         args=["--no-sandbox"])
            try:
                context = browser.new_context(service_workers="block", accept_downloads=False,
                                              device_scale_factor=1)
                try:
                    stage = "authentication"
                    report["login_posts"] = 1
                    response = context.request.post(
                        args.url + "/api/login", data={"password": password, "next": args.path},
                        headers={"Origin": args.url}, max_redirects=0, timeout=args.timeout_ms)
                    del password
                    report["login_status"] = response.status
                    if response.status != 200:
                        raise Refused("authentication_http_status")
                    response.dispose()

                    def readonly(route):
                        request = route.request
                        try:
                            same_origin = origin(request.url) == expected_origin
                        except ValueError:
                            same_origin = False
                        if request.method != "GET" or not same_origin:
                            # Do not serialize a URL/query/body: any may carry private data.
                            report["violations"].append(dict(method=request.method,
                                reason="non_get" if request.method != "GET" else "foreign_origin"))
                            route.abort()
                        else:
                            # Browser redirects after continue_() need not re-enter routing.
                            # Fetch exactly one hop, then refuse redirects before the browser
                            # can issue a follow-up request (even to another same-origin path).
                            try:
                                fetched = route.fetch(max_redirects=0, timeout=args.timeout_ms)
                                try:
                                    if 300 <= fetched.status < 400:
                                        report["violations"].append(dict(reason="redirect"))
                                        route.abort()
                                    else:
                                        route.fulfill(response=fetched)
                                finally:
                                    fetched.dispose()
                            except Exception:
                                report["violations"].append(dict(reason="get_fetch_failed"))
                                route.abort()

                    context.route("**/*", readonly)

                    def block_socket(route):
                        report["violations"].append(dict(reason="websocket"))
                        route.close()

                    context.route_web_socket("**/*", block_socket)
                    for width, height in VIEWPORTS:
                        stage = "page_capture"
                        record = dict(viewport=dict(width=width, height=height),
                                      requested_path=args.path, bundles=[], captured=False)
                        report["cases"].append(record)
                        page = context.new_page()
                        page.set_viewport_size(record["viewport"])
                        errors, bundle_errors = [], []
                        page.on("pageerror", lambda error: errors.append("javascript_error"))

                        def served_response(res):
                            if res.request.resource_type != "script":
                                return
                            try:
                                parsed = urlsplit(res.url)
                                if origin(res.url) != expected_origin or res.status != 200:
                                    raise ValueError()
                                name = Path(parsed.path).name
                                if not re.fullmatch(r"[A-Za-z0-9_.-]+\.m?js", name):
                                    raise ValueError()
                                data = res.body()
                                digest = hashlib.sha256(data).hexdigest()
                                asset_versions.setdefault(parsed.path, set()).add(digest)
                                record["bundles"].append(dict(name=name, http_status=res.status,
                                                              sha256=digest, bytes=len(data)))
                            except Exception:
                                bundle_errors.append("script_response_unavailable")

                        page.on("response", served_response)
                        try:
                            response = page.goto(args.url + args.path, wait_until="networkidle",
                                                 timeout=args.timeout_ms)
                            record["http_status"] = response.status if response else None
                            if not response or response.status != 200:
                                raise Refused("document_http_status")
                            if page.url != args.url + args.path:
                                raise Refused("unexpected_navigation")
                            if report["violations"]:
                                raise Refused("request_policy_violation")
                            if errors or bundle_errors or not record["bundles"]:
                                raise Refused("page_or_bundle_failure")
                            page.locator(args.ready_selector).first.wait_for(timeout=args.timeout_ms)
                            page.evaluate("document.fonts.ready")
                            page.wait_for_timeout(args.settle_ms)
                            if report["violations"]:
                                raise Refused("request_policy_violation")
                            if errors or bundle_errors or not record["bundles"]:
                                raise Refused("page_or_bundle_failure")
                            bundles = {(b["name"], b["sha256"]) for b in record["bundles"]}
                            if expected_bundles is None:
                                expected_bundles = bundles
                            elif bundles != expected_bundles:
                                raise Refused("bundle_changed_during_capture")
                            record["actual_viewport"] = page.evaluate(
                                "({width:innerWidth,height:innerHeight,dpr:devicePixelRatio})")
                            if record["actual_viewport"] != dict(width=width, height=height, dpr=1):
                                raise Refused("viewport_mismatch")
                            record["dialog_counts"] = page.evaluate(GUARD_SCRIPT)
                            record["dialog_closed"] = closed(record["dialog_counts"])
                            if not record["dialog_closed"]:
                                raise Refused("dialog_present")
                            filename = f"desk-{width}x{height}.png"
                            page.screenshot(path=str(out / filename), animations="disabled")
                            record["dialog_counts_after"] = page.evaluate(GUARD_SCRIPT)
                            if (not closed(record["dialog_counts_after"]) or report["violations"]
                                    or errors or bundle_errors):
                                raise Refused("capture_postcondition_failed")
                            if {(b["name"], b["sha256"]) for b in record["bundles"]} != expected_bundles:
                                raise Refused("bundle_changed_during_capture")
                            record.update(captured=True, screenshot=filename,
                                screenshot_sha256=hashlib.sha256((out / filename).read_bytes()).hexdigest())
                        finally:
                            page.close()
                    if any(len(values) != 1 for values in asset_versions.values()):
                        raise Refused("bundle_changed_during_capture")
                finally:
                    context.close()
            finally:
                browser.close()
        if report["violations"] or len(report["cases"]) != len(VIEWPORTS):
            raise Refused("incomplete_or_unsafe_capture")
        report["passed"] = all(case["captured"] for case in report["cases"])
    except Refused as error:
        report["failures"].append(dict(stage=stage, code=str(error)))
    except Exception:
        # Playwright errors can embed URLs, headers and source text. Never persist them.
        report["failures"].append(dict(stage=stage, code="capture_error_details_withheld"))
    finally:
        if not report["passed"]:
            for image in out.glob("*.png"):
                image.unlink()
            for case in report["cases"]:
                case["captured"] = False
                case.pop("screenshot", None)
                case.pop("screenshot_sha256", None)
        report["completed_at"] = datetime.now().astimezone().isoformat()
        (out / "evidence.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True, help="Explicit served origin; no path or credentials")
    parser.add_argument("--path", default="/dash/cos")
    parser.add_argument("--credentials-path", default=os.environ.get("VP_TEST_CREDENTIALS_PATH"))
    parser.add_argument("--output", required=True, help="New evidence directory")
    parser.add_argument("--ready-selector", default=".dash-nav")
    parser.add_argument("--timeout-ms", type=int, default=30000)
    parser.add_argument("--settle-ms", type=int, default=1000)
    args = parser.parse_args(argv)
    try:
        origin(args.url)
        parsed = urlsplit(args.url)
        if parsed.path not in ("", "/") or parsed.query or parsed.fragment:
            raise ValueError()
        args.url = args.url.rstrip("/")
        path = urlsplit(args.path)
        if (not args.path.startswith("/") or args.path.startswith("//") or path.netloc
                or path.query or path.fragment or "\\" in args.path):
            raise ValueError()
        if not args.credentials_path or args.timeout_ms <= 0 or args.settle_ms < 0:
            raise ValueError()
    except ValueError:
        parser.error("Use an explicit origin, local path without query/fragment, credential path and valid timeouts")
    if Path(args.output).exists():
        parser.error("Output directory must not already exist")
    report = capture(args)
    print(json.dumps(dict(passed=report["passed"], cases=len(report["cases"]),
                          failures=report["failures"])))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
