// A deliberate type error against the published declarations. `task npm:check` asserts that
// tsc reports exactly this error (TS2345 on suite's name), which proves that JS suites are
// still checked against npm/cli/types/index.d.ts. Expression only: it declares no globals.
suite(42);
