"""pytest integration: start the demo service, run the Python suite, check cleanup.

    cargo build -p apitest --example demo_server
    cd crates/apitest-py && maturin develop && pytest -v
"""

import json
import os
import pathlib
import socket
import subprocess
import time
import urllib.request

import pytest

import apitest

ROOT = pathlib.Path(__file__).resolve().parents[3]
SPECS = ROOT / "demo" / "specs"
SERVER = ROOT / "target" / "debug" / "examples" / "demo_server"


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(params=["1.0", "2.0"])
def service(request):
    if not SERVER.exists():
        pytest.skip("build the demo server first: cargo build -p apitest --example demo_server")
    port = _free_port()
    proc = subprocess.Popen([str(SERVER), str(port), request.param], stderr=subprocess.DEVNULL)
    url = f"http://127.0.0.1:{port}"
    for _ in range(100):
        try:
            urllib.request.urlopen(url + "/_stats")
            break
        except OSError:
            time.sleep(0.05)
    yield request.param, url
    proc.terminate()
    proc.wait()


def stats(url):
    return json.loads(urllib.request.urlopen(url + "/_stats").read())


def billing_suite() -> apitest.Suite:
    s = apitest.Suite("billing-py")
    s.contract("billing", str(SPECS / "billing-1.0.yaml"), live_spec="/openapi.json")

    @s.fixture(scope="suite")
    def token(cx):
        r = cx.call("billing", "issueToken", form={"user": cx.env("API_USER"), "pass": cx.env("API_PASS")}).expect(200)
        cx.header("Authorization", f"Bearer {r.body['access_token']}")
        yield r.body["access_token"]
        cx.call("billing", "revokeToken").expect(204)

    @s.test(parallel=True)
    def create_and_fetch_invoice(cx):
        inv = cx.call("billing", "createInvoice", json={"amount": 4200, "currency": "EUR"}).expect(201).body
        cx.defer(lambda cx: cx.call("billing", "deleteInvoice", path={"id": inv["id"]}).expect(204))
        got = cx.call("billing", "getInvoice", path={"id": inv["id"]}).expect(200).body
        assert got["amount"] == 4200

    def make_invoice(cx):
        cx.set("invoice_id", cx.call("billing", "createInvoice", json={"amount": 5, "currency": "USD"}).expect(201).body["id"])

    def drop_invoice(cx):
        cx.call("billing", "deleteInvoice", path={"id": cx.get("invoice_id")}).expect(204)

    @s.test(parallel=True, resources=["invoices"], setup=make_invoice, teardown=drop_invoice)
    def per_test_setup_and_teardown(cx):
        assert cx.call("billing", "getInvoice", path={"id": cx.get("invoice_id")}).expect(200).body["currency"] == "USD"

    @s.test
    def failing_test_still_cleans_up(cx):
        inv = cx.call("billing", "createInvoice", json={"amount": 1, "currency": "EUR"}).expect(201).body
        cx.defer(lambda cx: cx.call("billing", "deleteInvoice", path={"id": inv["id"]}).expect(204))
        raise ValueError("intentional failure")

    return s


VARS = {"API_USER": "demo", "API_PASS": "secret"}


def test_python_suite(service, tmp_path):
    version, url = service
    junit = tmp_path / "junit.xml"
    report = billing_suite().run(url, vars=VARS, junit=str(junit), quiet=True)
    outcomes = {t["name"]: t["outcome"] for t in report["tests"]}
    assert all(not t["teardown_errors"] for t in report["tests"]), report["tests"]
    assert not report["teardown_errors"], report["teardown_errors"]
    if version == "1.0":
        assert outcomes["create and fetch invoice"]["status"] == "passed"
        assert outcomes["per test setup and teardown"]["status"] == "passed"
        failed = outcomes["failing test still cleans up"]
        assert failed["status"] == "failed" and "intentional failure" in failed["message"]
        assert "ValueError" in failed["message"]
    else:
        assert "incompatible" in report["setup_error"]
        assert all(o["status"] == "skipped" for o in outcomes.values())
        assert any(f["severity"] == "breaking" for f in report["drift"][0]["findings"])
    assert stats(url) == {"tokens": 0, "invoices": 0}
    assert "<testsuites" in junit.read_text()


def test_python_load(service):
    version, url = service
    if version != "1.0":
        pytest.skip("load only against the compatible version")
    report = billing_suite().load(
        "create and fetch invoice", url, vus=4, duration="1s",
        thresholds=["iterations.error_rate == 0", "http.p95 < 200ms"], vars=VARS,
    )
    assert report["passed"], report
    assert report["scenarios"][0]["iterations"]["count"] > 10
    assert stats(url) == {"tokens": 0, "invoices": 0}
