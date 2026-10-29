"""Capture fresh authenticated Operations and Poolrad dashboard PNGs.

The helper reads the local owner credential, writes two private images and a
source manifest, and never posts a brief or prints the credential.
"""

import argparse
from datetime import datetime
from hashlib import sha256
import json
import os
from pathlib import Path
from zoneinfo import ZoneInfo

from playwright.sync_api import sync_playwright


ROOT = Path(__file__).resolve().parent
BASE = os.getenv("VP_BASE_URL", "http://127.0.0.1:8787")


def chromium_path():
    configured = os.getenv("PLAYWRIGHT_CHROMIUM_EXECUTABLE")
    if configured:
        return configured
    cached = sorted((Path.home() / ".cache/ms-playwright").glob(
        "chromium-*/chrome-linux64/chrome"
    ))
    return str(cached[-1]) if cached else None


def password():
    path = Path(os.getenv("VP_TEST_CREDENTIALS_PATH", ROOT / "data/owner-credentials"))
    fields = dict(line.split(": ", 1) for line in path.read_text().splitlines())
    return fields["Password"]


def digest(path):
    return sha256(path.read_bytes()).hexdigest()


def capture(prefix):
    prefix.parent.mkdir(parents=True, exist_ok=True)
    operations = prefix.with_name(prefix.name + "-operations.png")
    poolrad = prefix.with_name(prefix.name + "-poolrad.png")
    manifest = prefix.with_name(prefix.name + "-sources.json")
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(
            headless=True, executable_path=chromium_path(), args=["--no-sandbox"]
        )
        try:
            page = browser.new_page(viewport={"width": 1920, "height": 1200})
            page.goto(BASE, wait_until="networkidle")
            page.locator("#password").fill(password())
            page.get_by_role("button", name="Open desk").click()
            page.get_by_role("button", name="Operations dashboard").click()
            page.get_by_role("button", name="Refresh data").click()
            page.get_by_role("button", name="Refresh data").wait_for(state="visible")
            page.locator(".ops-metrics").get_by_text(
                "Recreate all 815 candidate bodies"
            ).wait_for()
            if page.locator(".dashboard-error").count():
                raise RuntimeError(page.locator(".dashboard-error").inner_text())
            operations_source = page.locator(".ops-source-line").inner_text()
            page.screenshot(path=str(operations), full_page=False)

            page.get_by_role("button", name="Poolrad dashboard").click()
            frame = page.frame_locator(
                'iframe[title="Poolrad function progress and evidence"]'
            )
            frame.get_by_text("POOLRAD // ORIGINAL C BODIES").wait_for()
            if page.locator(".dashboard-error").count():
                raise RuntimeError(page.locator(".dashboard-error").inner_text())
            poolrad_source = frame.locator("body").inner_text().splitlines()[:7]
            page.screenshot(path=str(poolrad), full_page=False)
        finally:
            browser.close()
    record = {
        "captured_at": datetime.now(ZoneInfo("America/Los_Angeles")).isoformat(),
        "operations": {"path": str(operations), "sha256": digest(operations),
                       "source": operations_source},
        "poolrad": {"path": str(poolrad), "sha256": digest(poolrad),
                    "source_lines": poolrad_source},
    }
    manifest.write_text(json.dumps(record, indent=2) + "\n")
    return {"operations": str(operations), "poolrad": str(poolrad),
            "manifest": str(manifest), "captured_at": record["captured_at"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(capture(args.prefix), indent=2))
