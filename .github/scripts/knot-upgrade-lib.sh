#!/usr/bin/env bash
# Helpers for .github/workflows/helm-upgrade.yaml.
#
# Sourced (not executed) by the workflow steps so they can share a port-forward
# and a cookie without repeating curl incantations. Expects NAMESPACE and
# RELEASE in the environment.

BASE_URL="http://127.0.0.1:8080"

# Forward the knot Service to $BASE_URL and block until it answers.
#
# The forward binds to whichever pod is backing the Service at connect time, so
# it does NOT survive a rollout — call this again after `helm upgrade`.
port_forward() {
  local svc
  svc=$(kubectl -n "$NAMESPACE" get svc \
    -l "app.kubernetes.io/name=knot,app.kubernetes.io/instance=$RELEASE" \
    -o jsonpath='{.items[0].metadata.name}')
  if [ -z "$svc" ]; then
    echo "::error::no knot Service found in $NAMESPACE"
    return 1
  fi

  kubectl -n "$NAMESPACE" port-forward "svc/$svc" 8080:80 >/tmp/pf.log 2>&1 &
  echo $! >/tmp/pf.pid

  local _attempt
  for _attempt in $(seq 1 60); do
    if curl -sf "$BASE_URL/api/healthz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done

  echo "::error::port-forward to svc/$svc never became healthy"
  cat /tmp/pf.log
  return 1
}

stop_port_forward() {
  if [ -f /tmp/pf.pid ]; then
    kill "$(cat /tmp/pf.pid)" 2>/dev/null || true
    rm -f /tmp/pf.pid
  fi
}

# cookie <name> — read one cookie value out of a curl header dump on stdin.
#
# Header lines carry a trailing CR; leaving it in silently corrupts the value
# and shows up much later as a puzzling 401.
cookie() {
  tr -d '\r' | sed -n "s/^[Ss]et-[Cc]ookie: $1=\([^;]*\).*/\1/p" | head -1
}

# api <METHOD> <PATH> [json-body] — call the API as the seeded user.
#
# Sends the session by explicit header rather than a cookie jar: the server sets
# `Secure` on both cookies (KNOT_COOKIE_SECURE defaults true and the chart does
# not expose it), which stops curl resending them over the plain-HTTP forward.
# Unsafe methods also get the CSRF header, which the server requires once a
# session is present.
api() {
  local method="$1" path="$2" body="${3:-}"
  local -a args=(-sSf -X "$method" "$BASE_URL$path")

  if [ -f /tmp/sid ]; then
    local jar
    jar="sid=$(cat /tmp/sid)"
    [ -f /tmp/csrf ] && jar="$jar; csrf=$(cat /tmp/csrf)"
    args+=(-H "cookie: $jar")
    if [ -f /tmp/csrf ] && [ "$method" != "GET" ]; then
      args+=(-H "x-csrf-token: $(cat /tmp/csrf)")
    fi
  fi

  if [ -n "$body" ]; then
    args+=(-H 'content-type: application/json' -d "$body")
  fi

  curl "${args[@]}"
}
