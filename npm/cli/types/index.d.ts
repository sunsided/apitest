// Ambient declarations for apitest JavaScript suites.
//
// Suites run inside apitest's embedded QuickJS runtime, NOT in Node.js: there is no
// `require`, no `import` of npm packages and no Node built-ins. All calls are
// synchronous (like k6): `cx.http(...)` blocks and returns the response.
//
// Enable in a suite file:
//   // @ts-check
//   /// <reference types="@apitest/cli" />
// or with a jsconfig.json: { "compilerOptions": { "checkJs": true, "lib": ["es2023"], "types": ["@apitest/cli"] } }

declare namespace apitest {
  /** Lifetime of a fixture value. */
  type Scope = "suite" | "vu" | "test";

  type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

  /** Request options shared by `cx.http` and `cx.call`. */
  interface RequestOpts {
    /** Values for `{placeholders}` in the path template. */
    path?: Record<string, string | number>;
    query?: Record<string, string | number | boolean>;
    headers?: Record<string, string>;
    /** JSON body (validated against the pinned spec for contract calls). */
    json?: unknown;
    /** `application/x-www-form-urlencoded` body. */
    form?: Record<string, string | number | boolean>;
    /** Raw body. */
    body?: string;
    /** Shorthand for `Authorization: Bearer <token>`. */
    bearer?: string;
    /** Credentials for this request. */
    auth?: Auth;
    /** Send as a named actor from apitest.nova (`actors:`): its service, headers and auth. */
    as?: string;
    /** Named service for raw requests (`services:` in apitest.nova). */
    service?: string;
    /** `false` suppresses trace headers, `true` forces them. */
    trace?: boolean;
  }

  type Auth =
    | { type: "none" }
    | { type: "bearer"; token: string }
    | { type: "basic"; username: string; password: string }
    | { type: "header"; name: string; value: string }
    | { type: "login"; service?: string; username: string; password: string }
    | {
        type: "client_credentials";
        token_url: string;
        client_id: string;
        client_secret: string;
        scope?: string;
        audience?: string;
        basic_auth?: boolean;
      };

  interface TraceContext {
    /** 32 lowercase hex digits. */
    trace_id: string;
    sampled: boolean;
    tracestate: string | null;
    parent_span_id: string | null;
  }

  interface SpanContext {
    trace_id: string;
    /** 16 lowercase hex digits, new for every request. */
    span_id: string;
    sampled: boolean;
  }

  /** The request as it was sent (after defaults, auth and trace headers). */
  interface SentRequest {
    method: string;
    url: string;
    headers: Record<string, string>;
  }

  interface Response<T = any> {
    status: number;
    headers: Record<string, string>;
    /** Parsed JSON body, the text if it was not JSON, `null` if empty. */
    body: T;
    text: string;
    elapsed_ms: number;
    /** `contract:operationId` for contract calls, otherwise `null`. */
    operation: string | null;
    /** What was sent: method, URL and all headers (traceparent, User-Agent, ...). */
    request: SentRequest;
    /** The span this request was sent as; `null` if tracing was off for it. */
    trace: SpanContext | null;
    /** Throws unless the status matches; returns the response for chaining. */
    expect(status: number): Response<T>;
  }

  /** The context passed to fixtures, hooks, tests and cleanups. */
  interface Ctx {
    /** A fixture or value from this scope or a parent scope (`null` if missing). */
    get<T = any>(key: string): T;
    /** Stores a JSON-serializable value in the current scope. */
    set(key: string, value: unknown): void;
    /** A `--var` or environment variable; throws if missing and no default is given. */
    env(key: string, defaultValue?: string): string;
    /** Default header for every request from this scope and its children. */
    header(name: string, value: string | number): void;
    log(...args: unknown[]): void;
    /** Virtual-user index (0 in functional runs). */
    vu(): number;
    /** The current test's trace (W3C trace id etc.). */
    trace(): TraceContext;
    /** `Authorization: Bearer <token>` for the rest of this scope. */
    bearer(token: string): void;
    /** HTTP Basic auth for the rest of this scope. */
    basic(username: string, password: string): void;
    /** Any credentials for the rest of this scope. */
    auth(auth: Auth): void;
    /** Raw request. Relative paths are resolved against the base URL. */
    http<T = any>(method: string, path: string, opts?: RequestOpts): Response<T>;
    /** Contract call by operationId; request and response are validated against the pinned spec. */
    call<T = any>(contract: string, operationId: string, opts?: RequestOpts): Response<T>;
    /** Alias of `call` (the Rhai frontend's name). */
    op<T = any>(contract: string, operationId: string, opts?: RequestOpts): Response<T>;
    /** Runs `fn` when the current scope ends (LIFO), even if the test failed. */
    defer(fn: (cx: Ctx) => unknown): void;
  }

  interface ContractOpts {
    /** URL or path (relative to the base URL) of the service's current spec. */
    liveSpec?: string;
    /** `tolerant` (default): additive changes are fine. `strict`: any change to the used surface is breaking. */
    mode?: "tolerant" | "strict";
  }

  interface TestOpts {
    /** Run concurrently with other parallel tests. Default: serial (runs alone). */
    parallel?: boolean;
    serial?: boolean;
    /** Named locks; parallel tests sharing one never overlap. */
    resources?: string | string[];
    timeoutMs?: number;
    timeout_ms?: number;
    tags?: string[];
    /** Runs before the body, inside the test scope. */
    setup?: (cx: Ctx) => unknown;
    /** Runs after the body, even if it failed. */
    teardown?: (cx: Ctx) => unknown;
  }

  type Callback = (cx: Ctx) => unknown;

  interface SuiteBuilder {
    contract(name: string, specPath: string, opts?: ContractOpts): SuiteBuilder;
    /** The setup's return value becomes `cx.get(name)`; `teardown` runs when the scope ends. */
    fixture<T>(name: string, scope: Scope, setup: (cx: Ctx) => T, teardown?: Callback): SuiteBuilder;
    test(name: string, body: Callback): SuiteBuilder;
    test(name: string, opts: TestOpts, body: Callback): SuiteBuilder;
    /** Suite setup (alias of `beforeAll`). */
    setup(fn: Callback): SuiteBuilder;
    /** Suite teardown (alias of `afterAll`); always runs. */
    teardown(fn: Callback): SuiteBuilder;
    beforeAll(fn: Callback): SuiteBuilder;
    afterAll(fn: Callback): SuiteBuilder;
    beforeEach(fn: Callback): SuiteBuilder;
    afterEach(fn: Callback): SuiteBuilder;
  }
}

/** Declares a suite. A file may declare several. */
declare function suite(name: string): apitest.SuiteBuilder;
declare function assert(condition: unknown, message?: string): asserts condition;
/** Deep equality via JSON comparison. */
declare function assertEq(actual: unknown, expected: unknown, message?: string): void;

// Same shape as lib.dom / @types/node, so these declarations merge instead of clashing.
interface Console {
  log(...data: any[]): void;
}
declare var console: Console;
