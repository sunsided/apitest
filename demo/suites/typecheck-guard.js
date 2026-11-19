// Guards `task npm:check`: if the compiler ever stops reporting errors in checkJs mode,
// this directive is unused and the check fails. Not a suite; apitest.nova does not load it.
// @ts-expect-error a number is not a string
const probe = /** @type {string} */ (1);
