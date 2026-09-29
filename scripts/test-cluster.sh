#!/usr/bin/env bash
# Starts a throwaway Kubernetes API server (etcd + kube-apiserver, no kubelet, no Docker)
# for tests, and writes a kubeconfig to .testenv/kubeconfig.
#   scripts/test-cluster.sh up     # start and seed fixtures
#   scripts/test-cluster.sh down   # stop and delete state
set -euo pipefail

VERSION="${ENVTEST_VERSION:-1.37.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/.testenv"
BIN="$DIR/bin"
PORT="${APISERVER_PORT:-16443}"
ETCD_PORT="${ETCD_PORT:-12379}"
TOKEN="duckdb-k8s-test-token"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m); [ "$arch" = x86_64 ] && arch=amd64; [ "$arch" = aarch64 ] && arch=arm64

download() {
  [ -x "$BIN/kube-apiserver" ] && return
  mkdir -p "$BIN"
  url="https://github.com/kubernetes-sigs/controller-tools/releases/download/envtest-v$VERSION/envtest-v$VERSION-$os-$arch.tar.gz"
  curl -sSfL "$url" | tar -xz -C "$BIN" --strip-components=2
}

up() {
  download
  mkdir -p "$DIR/etcd" "$DIR/certs"
  [ -f "$DIR/certs/sa.key" ] || openssl genrsa -out "$DIR/certs/sa.key" 2048 2>/dev/null
  echo "$TOKEN,admin,admin,system:masters" > "$DIR/tokens.csv"

  "$BIN/etcd" --data-dir "$DIR/etcd" \
    --listen-client-urls "http://127.0.0.1:$ETCD_PORT" --advertise-client-urls "http://127.0.0.1:$ETCD_PORT" \
    --listen-peer-urls "http://127.0.0.1:0" >"$DIR/etcd.log" 2>&1 &
  echo $! > "$DIR/etcd.pid"

  "$BIN/kube-apiserver" --etcd-servers "http://127.0.0.1:$ETCD_PORT" \
    --secure-port "$PORT" --bind-address 127.0.0.1 --cert-dir "$DIR/certs" \
    --token-auth-file "$DIR/tokens.csv" --authorization-mode RBAC \
    --disable-admission-plugins ServiceAccount \
    --service-account-issuer https://kubernetes.default.svc --service-account-key-file "$DIR/certs/sa.key" \
    --service-account-signing-key-file "$DIR/certs/sa.key" --service-cluster-ip-range 10.0.0.0/24 \
    >"$DIR/apiserver.log" 2>&1 &
  echo $! > "$DIR/apiserver.pid"

  cat > "$DIR/kubeconfig" <<KC
apiVersion: v1
kind: Config
clusters:
- name: test
  cluster: { server: "https://127.0.0.1:$PORT", insecure-skip-tls-verify: true }
users:
- name: admin
  user: { token: "$TOKEN" }
contexts:
- name: test
  context: { cluster: test, user: admin, namespace: default }
current-context: test
KC

  export KUBECONFIG="$DIR/kubeconfig"
  for _ in $(seq 60); do kubectl get --raw /readyz >/dev/null 2>&1 && break; sleep 0.5; done
  kubectl get --raw /readyz >/dev/null
  kubectl apply -f "$ROOT/test/fixtures/fixtures.yaml" >/dev/null
  source "$ROOT/test/fixtures/status.sh"
  echo "ready: export KUBECONFIG=$DIR/kubeconfig"
}

down() {
  for p in apiserver etcd; do
    if [ -f "$DIR/$p.pid" ]; then
      pid=$(cat "$DIR/$p.pid")
      kill "$pid" 2>/dev/null || true
      # wait for exit so a quick `down && up` doesn't race the old process for ports and data
      while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
      rm -f "$DIR/$p.pid"
    fi
  done
  rm -rf "$DIR/etcd"
}

"${1:-up}"
