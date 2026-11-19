// Billing suite in Rhai. Run: apitest run demo/suites/billing.rhai
let s = suite("billing-rhai");

// Pin the API: calls go through operationIds of this spec, and the live spec
// served by the service is diffed against it before any test runs.
s.contract("billing", "../specs/billing-1.0.yaml", #{ live_spec: "/openapi.json", mode: "tolerant" });

// Suite fixture with setup + teardown: one token for the whole suite.
s.fixture("token", "suite",
    |cx| {
        let r = cx.op("billing", "issueToken", #{
            form: #{ user: cx.env("API_USER", "demo"), pass: cx.env("API_PASS", "secret") }
        }).expect(200);
        cx.header("Authorization", `Bearer ${r.body.access_token}`);
        r.body.access_token
    },
    |cx| {
        cx.op("billing", "revokeToken").expect(204);
    });

s.test("create and fetch invoice", #{ parallel: true }, |cx| {
    let inv = cx.op("billing", "createInvoice", #{ json: #{ amount: 4200, currency: "EUR" } }).expect(201).body;
    let id = inv.id;
    cx.defer(|cx| { cx.op("billing", "deleteInvoice", #{ path: #{ id: id } }).expect(204); });

    let got = cx.op("billing", "getInvoice", #{ path: #{ id: id } }).expect(200).body;
    assert_eq(got.amount, 4200);
});

s.test("unknown invoice is 404", #{ parallel: true }, |cx| {
    cx.op("billing", "getInvoice", #{ path: #{ id: 999999 } }).expect(404);
});

s.test("per-test setup and teardown", #{
    parallel: true,
    resources: ["invoices"],
    setup: |cx| {
        let inv = cx.op("billing", "createInvoice", #{ json: #{ amount: 100, currency: "USD" } }).expect(201).body;
        cx.set("invoice_id", inv.id);
    },
    teardown: |cx| {
        cx.op("billing", "deleteInvoice", #{ path: #{ id: cx.get("invoice_id") } }).expect(204);
    }
}, |cx| {
    let got = cx.op("billing", "getInvoice", #{ path: #{ id: cx.get("invoice_id") } }).expect(200).body;
    assert_eq(got.currency, "USD");
});

s.test("listing sees exactly what this test created", #{ serial: true }, |cx| {
    for amount in [1, 2] {
        let id = cx.op("billing", "createInvoice", #{ json: #{ amount: amount, currency: "EUR" } }).expect(201).body.id;
        cx.defer(|cx| { cx.op("billing", "deleteInvoice", #{ path: #{ id: id } }).expect(204); });
    }
    let list = cx.op("billing", "listInvoices", #{ query: #{ currency: "EUR" } }).expect(200).body;
    assert_eq(list.len(), 2);
});

s
