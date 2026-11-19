"""apitest: API tests, load tests and OpenAPI contract pinning.

A thin, decorator-based layer over the Rust core (``apitest._native``)::

    import apitest

    s = apitest.Suite("billing")
    s.contract("billing", "specs/billing-1.0.yaml", live_spec="/openapi.json")

    @s.fixture(scope="suite")
    def token(cx):
        r = cx.call("billing", "issueToken", form={"user": "demo", "pass": "secret"}).expect(200)
        cx.header("Authorization", f"Bearer {r.body['access_token']}")
        yield r.body["access_token"]                       # value of the fixture
        cx.call("billing", "revokeToken").expect(204)      # teardown (always runs)

    @s.test(parallel=True)
    def create_invoice(cx):
        inv = cx.call("billing", "createInvoice", json={"amount": 4200, "currency": "EUR"}).expect(201).body
        cx.defer(lambda cx: cx.call("billing", "deleteInvoice", path={"id": inv["id"]}).expect(204))

    report = s.run("http://127.0.0.1:8080")
    assert report["passed"]

Tests, fixtures and hooks are plain synchronous functions taking ``cx``. HTTP calls release
the GIL, so ``parallel=True`` tests and load-test VUs overlap on I/O.
"""

from __future__ import annotations

import functools
import inspect
from typing import Any, Callable, Iterable, Optional

from ._native import Ctx, Response
from ._native import Suite as _NativeSuite

__all__ = ["Suite", "Ctx", "Response", "SuiteFailed"]


class SuiteFailed(AssertionError):
    """Raised by :meth:`Suite.check` when a suite did not pass."""


def _generator_fixture(fn: Callable[[Ctx], Any]) -> Callable[[Ctx], Any]:
    """Turns a pytest-style ``yield`` fixture into setup + deferred teardown."""

    @functools.wraps(fn)
    def setup(cx: Ctx) -> Any:
        gen = fn(cx)
        value = next(gen)

        def finish(_cx: Ctx) -> None:
            try:
                next(gen)
            except StopIteration:
                return
            raise RuntimeError(f"fixture {fn.__name__!r} yielded more than once")

        cx.defer(finish)
        return value

    return setup


def _name(fn: Callable, name: Optional[str]) -> str:
    return name or fn.__name__.replace("_", " ")


class Suite:
    """A suite of API tests sharing contracts, fixtures and setup/teardown."""

    def __init__(self, name: str) -> None:
        self._native = _NativeSuite(name)

    @property
    def name(self) -> str:
        return self._native.name

    @property
    def tests(self) -> list[str]:
        return self._native.tests

    def contract(self, name: str, path: str, *, live_spec: Optional[str] = None, mode: Optional[str] = None) -> "Suite":
        """Pins an OpenAPI document. Calls go through ``cx.call(name, operationId)``."""
        self._native.contract(name, path, live_spec, mode)
        return self

    def fixture(
        self,
        fn: Optional[Callable] = None,
        *,
        name: Optional[str] = None,
        scope: str = "test",
        teardown: Optional[Callable[[Ctx], Any]] = None,
    ):
        """Registers a fixture (scope: ``suite``, ``vu`` or ``test``).

        Either ``yield`` the value (code after ``yield`` is the teardown), return it and pass
        ``teardown=``, or register cleanups with ``cx.defer``. The value is ``cx.get(name)``.
        """

        def register(f: Callable) -> Callable:
            setup = _generator_fixture(f) if inspect.isgeneratorfunction(f) else f
            self._native.add_fixture(name or f.__name__, scope, setup, teardown)
            return f

        return register(fn) if fn is not None else register

    def _hook(self, kind: str, fn: Callable) -> Callable:
        self._native.add_hook(kind, fn)
        return fn

    def setup(self, fn: Callable) -> Callable:
        """Suite setup (runs once, after suite fixtures)."""
        return self._hook("before_all", fn)

    def teardown(self, fn: Callable) -> Callable:
        """Suite teardown (always runs)."""
        return self._hook("after_all", fn)

    def before_each(self, fn: Callable) -> Callable:
        return self._hook("before_each", fn)

    def after_each(self, fn: Callable) -> Callable:
        return self._hook("after_each", fn)

    def test(
        self,
        fn: Optional[Callable] = None,
        *,
        name: Optional[str] = None,
        parallel: bool = False,
        resources: Iterable[str] = (),
        timeout_ms: Optional[int] = None,
        setup: Optional[Callable[[Ctx], Any]] = None,
        teardown: Optional[Callable[[Ctx], Any]] = None,
        tags: Iterable[str] = (),
    ):
        """Registers a test. ``parallel=False`` (default) runs it alone."""

        def register(f: Callable) -> Callable:
            self._native.add_test(
                _name(f, name), f, parallel, list(resources), timeout_ms, setup, teardown, list(tags)
            )
            return f

        return register(fn) if fn is not None else register

    def run(self, base_url: str, **kwargs: Any) -> dict:
        """Runs the suite; returns the report dict. kwargs: vars, lockfile, update_lock,
        check_drift, only, max_parallel, junit, quiet."""
        return self._native.run(base_url, **kwargs)

    def check(self, base_url: str, **kwargs: Any) -> dict:
        """Like :meth:`run` but raises :class:`SuiteFailed` unless everything passed."""
        report = self.run(base_url, **kwargs)
        if not report["passed"]:
            raise SuiteFailed(_summary(report))
        return report

    def load(self, test: str, base_url: str, **kwargs: Any) -> dict:
        """Runs one test as a load scenario. kwargs: vus, duration, iterations, stages,
        thresholds, vars, validate_every."""
        return self._native.load(test, base_url, **kwargs)


def _summary(report: dict) -> str:
    lines = [f"suite {report['suite']} failed"]
    if report.get("setup_error"):
        lines.append(f"  setup: {report['setup_error']}")
    for t in report["tests"]:
        o = t["outcome"]
        if o["status"] == "failed":
            lines.append(f"  {t['name']}: {o['message']}")
        for e in t["teardown_errors"]:
            lines.append(f"  {t['name']} cleanup: {e}")
    lines += [f"  teardown: {e}" for e in report["teardown_errors"]]
    return "\n".join(lines)
