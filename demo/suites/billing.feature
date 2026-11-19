# Billing suite in Gherkin. Run: apitest run demo/suites/billing.feature
#
# Same tests as billing.rhai and billing.js, on the same runner: the contract is
# pinned and drift-checked before any scenario runs, every call is validated
# against it, and cleanups run even when a scenario fails.
@contract:billing=../specs/billing-1.0.yaml @live_spec:billing=/openapi.json
Feature: billing-feature

  Invoices can be created, read, listed and deleted by a signed-in user.

  Background:
    Given "anon" is an anonymous actor on "billing"
    And "anon" calls "issueToken" expecting 200 with:
      | in   | name | value       |
      | form | user | ${API_USER} |
      | form | pass | ${API_PASS} |
    And I remember the response at "/access_token" as "token"
    And "user" is an actor on "billing" with the bearer token "{{token}}"
    And afterwards "user" calls "revokeToken" expecting 204

  @parallel
  Scenario: create and fetch invoice
    When "user" calls "createInvoice" expecting 201 with the JSON body:
      """json
      { "amount": 4200, "currency": "EUR" }
      """
    And afterwards "user" calls "deleteInvoice" expecting 204 with:
      | path | id | {{response:/id}} |
    And I remember the response at "/id" as "id"
    When "user" calls "getInvoice" with:
      | path | id | {{id}} |
    Then the status is 200
    And the response contains:
      """json
      { "id": {{id}}, "amount": 4200, "currency": "EUR", "status": "open" }
      """

  @parallel
  Scenario Outline: unknown invoice is <status>
    When "user" calls "getInvoice" with:
      | path | id | <id> |
    Then the status is <status>

    Examples:
      | id     | status |
      | 999999 | 404    |

  @serial
  Scenario: listing sees exactly what this test created
    When "user" calls "createInvoice" expecting 201 with:
      | json | amount   | 1     |
      | json | currency | "EUR" |
    And afterwards "user" calls "deleteInvoice" expecting 204 with:
      | path | id | {{response:/id}} |
    And "user" calls "createInvoice" expecting 201 with:
      | json | amount   | 2     |
      | json | currency | "EUR" |
    And afterwards "user" calls "deleteInvoice" expecting 204 with:
      | path | id | {{response:/id}} |
    When "user" calls "listInvoices" with:
      | query | currency | EUR |
    Then the status is 200
    And the response at "" has 2 items
    And the response at "/1/amount" is 2
