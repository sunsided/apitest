# Reviewing changes

The review rules live in [`docs/review/README.md`](https://github.com/sunsided/apitest/blob/main/docs/review/README.md).
That index lists each topic with the files it applies to. Read the topics that match the
files in the change, and skip the rest.

Before you review, check that CI is green. CI already enforces formatting, pedantic clippy,
docs, tests, cargo-deny, MSRV, typos, REUSE, fuzz-target builds and the workflow lints, so a review does not
need to repeat those. The review focuses on what tools cannot check: design, naming,
documentation quality, test coverage of edge cases, and the topic rules.
