// Same rule as the server (`knot_storage::is_short_code`): 7 symbols without
// look-alikes, at least one digit — which keeps codes apart from app routes.
const SHAPE = /^[2-9a-kmnp-zA-HJ-NP-Z]{7}$/;
const DIGIT = /[2-9]/;

export function isShortCode(s: string): boolean {
  return SHAPE.test(s) && DIGIT.test(s);
}
