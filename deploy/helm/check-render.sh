#!/usr/bin/env bash
# Chart render check: renders deploy/helm/knot with a few value sets and
# asserts what the ConfigMap and PrometheusRule carry.
#
# The main thing it guards: Helm parses YAML numbers from a values file as
# float64, so a large number rendered without `| int64` comes out as
# `1.073741824e+10`, which figment refuses to read into a u64 and the pod
# crash-loops at startup. No rendered ConfigMap value may contain `e+`.
#
# Usage: deploy/helm/check-render.sh   (needs `helm` on PATH; run from anywhere)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
chart_dir="$script_dir/knot"
ci_values="$chart_dir/ci/ct-values.yaml"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

failures=0

fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# render <name> [helm args...] -> writes the ConfigMap's data to $work_dir/<name>.cm
# and the full output to $work_dir/<name>.all
render() {
  local case_name="$1"
  shift
  if ! helm template knot "$chart_dir" -f "$ci_values" "$@" >"$work_dir/$case_name.all" 2>"$work_dir/$case_name.err"; then
    fail "$case_name: helm template failed: $(cat "$work_dir/$case_name.err")"
    : >"$work_dir/$case_name.cm"
    return
  fi
  awk '/^# Source: knot\/templates\/configmap.yaml/{inside=1; next} /^---/{inside=0} inside' \
    "$work_dir/$case_name.all" >"$work_dir/$case_name.cm"
}

# expect_refused <name> [helm args...] -> helm template must fail (schema)
expect_refused() {
  local case_name="$1"
  shift
  if helm template knot "$chart_dir" -f "$ci_values" "$@" >/dev/null 2>&1; then
    fail "$case_name: expected the chart to refuse these values"
  fi
}

expect_line() {
  local case_name="$1" expected_line="$2"
  grep -qxF "  $expected_line" "$work_dir/$case_name.cm" ||
    fail "$case_name: ConfigMap lacks '$expected_line'"
}

expect_no_key() {
  local case_name="$1" key="$2"
  if grep -q "^  $key:" "$work_dir/$case_name.cm"; then
    fail "$case_name: ConfigMap should not carry $key"
  fi
}

expect_no_exponent() {
  local case_name="$1"
  if grep -n 'e+' "$work_dir/$case_name.cm"; then
    fail "$case_name: ConfigMap renders a number in exponent notation (missing | int64?)"
  fi
}

# 1. Defaults: limits on, scale 1, 64 sockets, quotas off, no trusted proxies.
render defaults
expect_line defaults 'KNOT_RATE_LIMIT_ENABLED: "true"'
expect_line defaults 'KNOT_RATE_LIMIT_SCALE: "1"'
expect_line defaults 'KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS: "64"'
expect_line defaults 'KNOT_QUOTA_PAGES_PER_USER: "0"'
expect_line defaults 'KNOT_QUOTA_PAGES_PER_WORKSPACE: "0"'
expect_line defaults 'KNOT_QUOTA_BLOB_BYTES_PER_USER: "0"'
expect_line defaults 'KNOT_QUOTA_BLOB_BYTES_PER_WORKSPACE: "0"'
expect_line defaults 'KNOT_HISTORY_REPLAY_MAX_BYTES: "4194304"'
expect_no_key defaults KNOT_TRUSTED_PROXIES
expect_no_exponent defaults

# 2. Large numbers from a values file (parsed as float64 by Helm).
cat >"$work_dir/large.yaml" <<'YAML'
rateLimit:
  enabled: false
  scale: 0.5
  collabMaxSockets: 100000000
quotas:
  pagesPerUser: 50000000
  pagesPerWorkspace: 1000000000
  blobBytesPerUser: 1073741824
  blobBytesPerWorkspace: 10737418240
history:
  replayMaxBytes: 1073741824
YAML
render large -f "$work_dir/large.yaml"
expect_line large 'KNOT_RATE_LIMIT_ENABLED: "false"'
expect_line large 'KNOT_RATE_LIMIT_SCALE: "0.5"'
expect_line large 'KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS: "100000000"'
expect_line large 'KNOT_QUOTA_PAGES_PER_USER: "50000000"'
expect_line large 'KNOT_QUOTA_PAGES_PER_WORKSPACE: "1000000000"'
expect_line large 'KNOT_QUOTA_BLOB_BYTES_PER_USER: "1073741824"'
expect_line large 'KNOT_QUOTA_BLOB_BYTES_PER_WORKSPACE: "10737418240"'
expect_line large 'KNOT_HISTORY_REPLAY_MAX_BYTES: "1073741824"'
expect_no_exponent large

# 3. trustedProxies as a list.
render proxies_list --set-json 'trustedProxies=["10.0.0.0/8","192.168.1.5","fd00::/8"]'
expect_line proxies_list 'KNOT_TRUSTED_PROXIES: "10.0.0.0/8,192.168.1.5,fd00::/8"'
expect_no_exponent proxies_list

# 4. trustedProxies as a comma-separated string.
render proxies_string --set-string 'trustedProxies=10.0.0.0/8\, fd00::/8'
expect_line proxies_string 'KNOT_TRUSTED_PROXIES: "10.0.0.0/8, fd00::/8"'

# 5. Values the server would refuse at startup are refused at render time.
expect_refused proxies_any_v4_list --set-json 'trustedProxies=["10.0.0.0/8","0.0.0.0/0"]'
expect_refused proxies_any_v6_list --set-json 'trustedProxies=["::/0"]'
expect_refused proxies_any_v4_string --set-string 'trustedProxies=10.0.0.0/8\,0.0.0.0/0'
expect_refused proxies_any_v6_string --set-string 'trustedProxies=::/0'
expect_refused scale_zero --set rateLimit.scale=0
expect_refused scale_too_big --set rateLimit.scale=101
expect_refused sockets_zero --set rateLimit.collabMaxSockets=0
expect_refused sockets_past_u32 --set rateLimit.collabMaxSockets=4294967296
expect_refused quota_negative --set quotas.pagesPerUser=-1
# --set leaves 0.5 a string, which the schema refuses; --set-json (below) or
# a values file passes a number.
expect_refused scale_string_via_set --set rateLimit.scale=0.5
# Helm holds values-file numbers as float64: past 2^53 - 1 they lose
# precision, so the schema caps every quota there.
expect_refused quota_past_float_precision --set quotas.blobBytesPerWorkspace=9007199254740992
# The schema checks only the charset and /0 (zero-padded too); knot checks
# the rest of each entry at startup.
expect_refused proxies_zero_padded_v6_list --set-json 'trustedProxies=["::/00"]'
expect_refused proxies_zero_padded_v4_string --set-string 'trustedProxies=10.0.0.0/8\,0.0.0.0/000'
expect_refused proxies_hostname_list --set-json 'trustedProxies=["ingress-nginx.ingress"]'
expect_refused proxies_hostname_string --set-string 'trustedProxies=10.0.0.0/8\,ingress-nginx'
expect_refused proxies_empty_list_item --set-json 'trustedProxies=["10.0.0.0/8",""]'

# 5b. The largest quota the schema takes renders exactly, and an
# IPv4-mapped entry passes the charset (knot maps it to IPv4).
render quota_at_float_precision --set quotas.blobBytesPerWorkspace=9007199254740991
expect_line quota_at_float_precision 'KNOT_QUOTA_BLOB_BYTES_PER_WORKSPACE: "9007199254740991"'
expect_no_exponent quota_at_float_precision
render scale_via_set_json --set-json rateLimit.scale=0.5
expect_line scale_via_set_json 'KNOT_RATE_LIMIT_SCALE: "0.5"'
render proxies_mapped --set-json 'trustedProxies=["::ffff:10.0.0.0/104"]'
expect_line proxies_mapped 'KNOT_TRUSTED_PROXIES: "::ffff:10.0.0.0/104"'

# 6. The rate-limit alert renders with Prometheus' own template intact.
render alerts --set serviceMonitor.enabled=true --set alerting.enabled=true
grep -q 'alert: KnotRateLimitedSustained' "$work_dir/alerts.all" ||
  fail "alerts: KnotRateLimitedSustained missing"
grep -qF 'sum by (bucket) (rate(knot_rate_limited_total[15m])) > 1' "$work_dir/alerts.all" ||
  fail "alerts: KnotRateLimitedSustained expression or default threshold changed"
# shellcheck disable=SC2016 # a literal Prometheus template, not a shell expansion
grep -qF '{{ $labels.bucket }}' "$work_dir/alerts.all" ||
  fail "alerts: the description should keep Prometheus' {{ \$labels.bucket }}"
grep -qF 'has refused requests at more than 1/s' "$work_dir/alerts.all" ||
  fail "alerts: the description should state the threshold as a rate"

if [ "$failures" -gt 0 ]; then
  echo "$failures chart render check(s) failed" >&2
  exit 1
fi
echo "chart render checks passed"
