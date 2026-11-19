// Billing suite in JavaScript (QuickJS). Run: apitest run demo/suites/billing.js
// Scripts are synchronous, like k6: cx.http()/cx.call() block and return the response.
const s = suite("billing-js");

s.contract("billing", "../specs/billing-1.0.yaml", { liveSpec: "/openapi.json", mode: "tolerant" });

// A VU-scoped fixture: once per virtual user (once in functional runs, once per VU under load).
s.fixture("token", "vu",
  (cx) => {
    const r = cx.call("billing", "issueToken", {
      form: { user: cx.env("API_USER", "demo"), pass: cx.env("API_PASS", "secret") },
    }).expect(200);
    cx.header("Authorization", `Bearer ${r.body.access_token}`);
    return r.body.access_token;
  },
  (cx) => { cx.call("billing", "revokeToken").expect(204); });

s.setup((cx) => cx.log("suite setup"));
s.teardown((cx) => cx.log("suite teardown"));

s.test("create and fetch invoice", { parallel: true }, (cx) => {
  const inv = cx.call("billing", "createInvoice", { json: { amount: 4200, currency: "EUR" } }).expect(201).body;
  cx.defer(() => cx.call("billing", "deleteInvoice", { path: { id: inv.id } }).expect(204));

  const got = cx.call("billing", "getInvoice", { path: { id: inv.id } }).expect(200).body;
  assertEq(got.amount, 4200);
});

s.test("unknown invoice is 404", { parallel: true }, (cx) => {
  cx.call("billing", "getInvoice", { path: { id: 999999 } }).expect(404);
});

s.test("per-test setup and teardown", {
  parallel: true,
  resources: ["invoices"],
  setup: (cx) => {
    const inv = cx.call("billing", "createInvoice", { json: { amount: 100, currency: "USD" } }).expect(201).body;
    cx.set("invoice_id", inv.id);
  },
  teardown: (cx) => cx.call("billing", "deleteInvoice", { path: { id: cx.get("invoice_id") } }).expect(204),
}, (cx) => {
  const got = cx.call("billing", "getInvoice", { path: { id: cx.get("invoice_id") } }).expect(200).body;
  assertEq(got.currency, "USD");
});

s.test("listing sees exactly what this test created", { serial: true }, (cx) => {
  for (const amount of [1, 2]) {
    const id = cx.call("billing", "createInvoice", { json: { amount, currency: "EUR" } }).expect(201).body.id;
    cx.defer(() => cx.call("billing", "deleteInvoice", { path: { id } }).expect(204));
  }
  const list = cx.call("billing", "listInvoices", { query: { currency: "EUR" } }).expect(200).body;
  assertEq(list.length, 2);
});
