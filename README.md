# k8s: SQL for your Kubernetes clusters

A read-only DuckDB extension that queries the Kubernetes API.

```sql
LOAD k8s;

-- what's broken?
SELECT namespace, name, status, restarts FROM k8s_pods() WHERE NOT ready;
SELECT * FROM k8s_logs('web-2', namespace := 'shop', tail := 100);
SELECT last_seen, reason, object_name, message FROM k8s_events() WHERE type = 'Warning' ORDER BY last_seen DESC;

-- any resource, including CRDs, with the raw object as JSON
SELECT name, object->>'$.spec.replicas' FROM k8s('deployments.apps', context := 'prod');
```

## Functions

Call `k8s_describe()` for every function, parameter and column, with descriptions.

All resource functions accept the same optional named parameters:
- `context`: a kubeconfig context. Defaults to the current context, or in-cluster config inside a pod.
- `namespace`: limits the query to one namespace (ignored for cluster-scoped resources).
- `label_selector`, `field_selector`: passed to the API server, so filtering happens server-side.

Each query does a fresh, paginated LIST (500 per page). API discovery is cached per context for the life of the process.
Only the columns a query uses are computed. For example, the `object` JSON is never serialised unless it's selected.

### `k8s(resource, ...)`

Lists any resource the API server knows about, including CRDs.
`resource` can be a plural (`pods`), a kind (`Pod`), or group-qualified (`deployments.apps`).
Built-in groups take precedence over CRDs with the same name.

Columns: `context, api_version, kind, namespace, name, uid, created, labels, annotations, object`.
`created` is a UTC TIMESTAMP. `labels`, `annotations` and `object` are JSON strings for `->>` / `json_extract`.

### Typed views

Each view is one resource type with extracted columns, plus `created`, `labels` and `object`.
Empty strings from the API are returned as NULL.

| function | columns |
|---|---|
| `k8s_pods()` | `context, namespace, name, phase, status` (kubectl's STATUS, e.g. `CrashLoopBackOff`), `ready, containers_ready, containers, restarts, node, pod_ip, images[]` |
| `k8s_deployments()` | `context, namespace, name, replicas, ready_replicas, updated_replicas, available_replicas, images[]` |
| `k8s_nodes()` | `context, name, ready, roles[], unschedulable, version, internal_ip, cpu, memory` (allocatable) |
| `k8s_services()` | `context, namespace, name, type, cluster_ip, external_ips[], ports[]` (`80/TCP`, `8080:30080/TCP`), `selector` |
| `k8s_events()` | `context, namespace, type, reason, object_kind, object_name, message, count, first_seen, last_seen, source, name` |

### `k8s_logs(pod, namespace :=, container :=, tail :=, since_seconds :=, previous :=, context :=)`

One row per log line: `context, namespace, pod, container, line, ts, message`.
Without `container`, returns every container in the pod, including init containers. `namespace` defaults to the context's namespace.

### `k8s_contexts()`

Returns the contexts in your kubeconfig: `name, cluster, user, namespace, current`.

## Authentication

Uses kube-rs, which reads `KUBECONFIG` / `~/.kube/config`, including exec plugins (EKS, GKE, and others), and falls back to the in-cluster service account.

## Design notes

- **Read-only by design.** There are no functions that modify the cluster, so it's safe to give to agents.
- **Built on [`duckdb-tables`](../duckdb-tables),** shared with `os` and `binaries`. Each view declares its columns as (name, type, description, getter), and the crate handles projection and `k8s_describe()`.
- **Typed views are written in Rust.** An extension can't register SQL macros globally: the system catalog rejects them, and temporary macros only exist in the connection that loads the extension.
- **Written in Rust on DuckDB's C extension API.** kube-rs handles kubeconfig and cloud authentication. The C API can't register custom catalogs, so `ATTACH ... (TYPE k8s)` isn't possible yet. Replacement scans are a possible alternative.
- **Pinned to one DuckDB version.** duckdb-rs uses the unstable C API, so each build works with exactly one DuckDB version (`TARGET_DUCKDB_VERSION` in the Makefile).

## Development

Requires Rust (the version is pinned in `rust-toolchain.toml`), Python 3, Make and kubectl. Docker is not needed.

```sh
git clone --recurse-submodules <repo>   # fetches duckdb-tables from GitHub
make configure        # python venv with the matching duckdb, platform detection
make debug            # build/debug/k8s.duckdb_extension
make integration      # starts etcd + kube-apiserver (scripts/test-cluster.sh), runs test/sql, stops it
```

For interactive work against the test cluster:

```sh
scripts/test-cluster.sh up
KUBECONFIG=.testenv/kubeconfig ./configure/venv/bin/python3 -c "
import duckdb; c = duckdb.connect(config={'allow_unsigned_extensions': 'true'})
c.execute(\"LOAD 'build/debug/k8s.duckdb_extension'\"); print(c.sql(\"FROM k8s('pods')\"))"
scripts/test-cluster.sh down
```

The test cluster has no kubelet or controllers. `test/fixtures/status.sh` writes the statuses they would normally set,
and `k8s_logs` can only be tested for errors there, because logs are served by a kubelet.
Fixtures are in `test/fixtures/`.
