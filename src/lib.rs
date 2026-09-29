//! `k8s`: query Kubernetes clusters from DuckDB. Read-only by design.
//!
//!   SELECT * FROM k8s('pods', namespace := 'default');
//!   SELECT name, status, restarts FROM k8s_pods() WHERE NOT ready;
//!   SELECT * FROM k8s_logs('api-7f9c', tail := 100);
//!   SELECT * FROM k8s_describe();

mod cluster;
mod views;

use duckdb_tables::{Bind, BoxError, Cell, ColType, Column, Extension, ListTable, Param};
use std::sync::Arc;
use views::{Obj, View};

const fn param(name: &'static str, ty: ColType, doc: &'static str) -> Param {
    Param { name, ty, doc }
}

const CONTEXT: Param = param(
    "context",
    ColType::Varchar,
    "kubeconfig context. Defaults to the current context, or the in-cluster service account inside a pod.",
);

/// Named parameters shared by every resource-listing function. All but context are sent to the API server.
const LIST_PARAMS: &[Param] = &[
    CONTEXT,
    param(
        "namespace",
        ColType::Varchar,
        "Only this namespace. Ignored for cluster-scoped resources. Much cheaper than WHERE namespace = ..., which lists every namespace first.",
    ),
    param("label_selector", ColType::Varchar, "Label selector such as app=web,tier!=db, applied by the API server."),
    param("field_selector", ColType::Varchar, "Field selector such as status.phase=Running, applied by the API server."),
];

fn list_request(bind: &Bind, resource: String) -> cluster::ListRequest {
    cluster::ListRequest {
        resource,
        context: bind.named_str("context"),
        namespace: bind.named_str("namespace"),
        label_selector: bind.named_str("label_selector"),
        field_selector: bind.named_str("field_selector"),
    }
}

fn list_objects(req: &cluster::ListRequest) -> Result<Vec<Obj>, BoxError> {
    let result = cluster::list(req).map_err(|e| e.to_string())?;
    let (context, api_version, kind): (Arc<str>, Arc<str>, Arc<str>) =
        (result.context.into(), result.api_version.into(), result.kind.into());
    result
        .items
        .iter()
        .map(|item| {
            Ok(Obj {
                context: context.clone(),
                api_version: api_version.clone(),
                kind: kind.clone(),
                json: serde_json::to_value(item)?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// k8s(resource, ...): any resource type, generic columns
// ---------------------------------------------------------------------------

struct Generic;

impl ListTable for Generic {
    type Item = Obj;
    type Args = cluster::ListRequest;

    fn doc() -> &'static str {
        views::GENERIC.doc
    }

    fn columns() -> &'static [Column<Obj>] {
        views::GENERIC.columns
    }

    fn positional() -> &'static [Param] {
        const PARAMS: &[Param] = &[param(
            "resource",
            ColType::Varchar,
            "Plural (pods), kind (Pod) or group-qualified (deployments.apps). Built-in groups win over CRDs with the same name.",
        )];
        PARAMS
    }

    fn named() -> &'static [Param] {
        LIST_PARAMS
    }

    fn bind(bind: &Bind) -> Result<cluster::ListRequest, BoxError> {
        Ok(list_request(bind, bind.positional(0).to_string()))
    }

    fn list(req: &cluster::ListRequest) -> Result<Vec<Obj>, BoxError> {
        list_objects(req)
    }
}

// ---------------------------------------------------------------------------
// k8s_pods() etc.: a fixed resource with typed columns, one per entry in views::TYPED
// ---------------------------------------------------------------------------

struct Typed<const I: usize>;

impl<const I: usize> Typed<I> {
    fn view() -> &'static View {
        views::TYPED[I].1
    }
}

impl<const I: usize> ListTable for Typed<I> {
    type Item = Obj;
    type Args = cluster::ListRequest;

    fn doc() -> &'static str {
        Self::view().doc
    }

    fn columns() -> &'static [Column<Obj>] {
        Self::view().columns
    }

    fn named() -> &'static [Param] {
        LIST_PARAMS
    }

    fn bind(bind: &Bind) -> Result<cluster::ListRequest, BoxError> {
        let resource = Self::view().resource.expect("typed views name their resource");
        Ok(list_request(bind, resource.to_string()))
    }

    fn list(req: &cluster::ListRequest) -> Result<Vec<Obj>, BoxError> {
        list_objects(req)
    }
}

// ---------------------------------------------------------------------------
// k8s_logs(pod, namespace :=, container :=, tail :=, since_seconds :=, previous :=, context :=)
// ---------------------------------------------------------------------------

struct LogRow {
    context: Arc<str>,
    namespace: Arc<str>,
    pod: Arc<str>,
    container: String,
    line: i64,
    ts: Option<i64>,
    message: String,
}

struct Logs;

impl ListTable for Logs {
    type Item = LogRow;
    type Args = cluster::LogRequest;

    fn doc() -> &'static str {
        "A pod's logs, one row per line. Without container, returns every container including init containers."
    }

    fn columns() -> &'static [Column<LogRow>] {
        static COLUMNS: &[Column<LogRow>] = &[
            Column { name: "context", ty: ColType::Varchar, doc: "kubeconfig context.", get: |l| Cell::Str(l.context.to_string()) },
            Column { name: "namespace", ty: ColType::Varchar, doc: "Namespace.", get: |l| Cell::Str(l.namespace.to_string()) },
            Column { name: "pod", ty: ColType::Varchar, doc: "Pod name.", get: |l| Cell::Str(l.pod.to_string()) },
            Column { name: "container", ty: ColType::Varchar, doc: "Container name.", get: |l| Cell::Str(l.container.clone()) },
            Column { name: "line", ty: ColType::Bigint, doc: "Line number, from 1, across all returned containers.", get: |l| Cell::Int(l.line) },
            Column { name: "ts", ty: ColType::Timestamp, doc: "Timestamp the kubelet recorded for the line, UTC.", get: |l| l.ts.map_or(Cell::Null, Cell::Ts) },
            Column { name: "message", ty: ColType::Varchar, doc: "The log line.", get: |l| Cell::Str(l.message.clone()) },
        ];
        COLUMNS
    }

    fn positional() -> &'static [Param] {
        const PARAMS: &[Param] = &[param("pod", ColType::Varchar, "Pod name.")];
        PARAMS
    }

    fn named() -> &'static [Param] {
        const PARAMS: &[Param] = &[
            CONTEXT,
            param("namespace", ColType::Varchar, "The pod's namespace. Defaults to the context's namespace."),
            param("container", ColType::Varchar, "Only this container."),
            param("tail", ColType::Bigint, "Only the last N lines of each container."),
            param("since_seconds", ColType::Bigint, "Only lines from the last N seconds."),
            param("previous", ColType::Boolean, "Logs of the previous, terminated container instance, e.g. before a crash."),
        ];
        PARAMS
    }

    fn bind(bind: &Bind) -> Result<cluster::LogRequest, BoxError> {
        Ok(cluster::LogRequest {
            pod: bind.positional(0).to_string(),
            context: bind.named_str("context"),
            namespace: bind.named_str("namespace"),
            container: bind.named_str("container"),
            tail: bind.named_i64("tail"),
            since_seconds: bind.named_i64("since_seconds"),
            previous: bind.named_bool("previous").unwrap_or(false),
        })
    }

    fn list(req: &cluster::LogRequest) -> Result<Vec<LogRow>, BoxError> {
        let result = cluster::logs(req).map_err(|e| e.to_string())?;
        let (context, namespace, pod): (Arc<str>, Arc<str>, Arc<str>) =
            (result.context.into(), result.namespace.into(), req.pod.as_str().into());
        Ok(result
            .lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| LogRow {
                context: context.clone(),
                namespace: namespace.clone(),
                pod: pod.clone(),
                container: l.container,
                line: i as i64 + 1,
                ts: l.timestamp.and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(|t| t.as_microsecond()),
                message: l.message,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// k8s_contexts(): the contexts in the local kubeconfig
// ---------------------------------------------------------------------------

struct Contexts;

impl ListTable for Contexts {
    type Item = cluster::ContextInfo;
    type Args = ();

    fn doc() -> &'static str {
        "The contexts in your kubeconfig (KUBECONFIG or ~/.kube/config)."
    }

    fn columns() -> &'static [Column<cluster::ContextInfo>] {
        static COLUMNS: &[Column<cluster::ContextInfo>] = &[
            Column { name: "name", ty: ColType::Varchar, doc: "Context name; pass it as context := to other functions.", get: |c| Cell::Str(c.name.clone()) },
            Column { name: "cluster", ty: ColType::Varchar, doc: "Cluster name.", get: |c| c.cluster.clone().into() },
            Column { name: "user", ty: ColType::Varchar, doc: "User name.", get: |c| c.user.clone().into() },
            Column { name: "namespace", ty: ColType::Varchar, doc: "Default namespace.", get: |c| c.namespace.clone().into() },
            Column { name: "current", ty: ColType::Boolean, doc: "Whether this is the current context.", get: |c| Cell::Bool(c.current) },
        ];
        COLUMNS
    }

    fn bind(_: &Bind) -> Result<(), BoxError> {
        Ok(())
    }

    fn list(_: &()) -> Result<Vec<cluster::ContextInfo>, BoxError> {
        cluster::contexts().map_err(|e| e.to_string().into())
    }
}

duckdb_tables::entrypoint!(k8s_init_c_api, init);

fn init(ext: &Extension) -> Result<(), BoxError> {
    ext.register_describe("k8s_describe")?;
    ext.register::<Generic>("k8s")?;
    // Typed<I> reads views::TYPED[I]; this list must cover every entry.
    const _: () = assert!(views::TYPED.len() == 5, "register every views::TYPED entry");
    ext.register::<Typed<0>>(views::TYPED[0].0)?;
    ext.register::<Typed<1>>(views::TYPED[1].0)?;
    ext.register::<Typed<2>>(views::TYPED[2].0)?;
    ext.register::<Typed<3>>(views::TYPED[3].0)?;
    ext.register::<Typed<4>>(views::TYPED[4].0)?;
    ext.register::<Logs>("k8s_logs")?;
    ext.register::<Contexts>("k8s_contexts")?;
    Ok(())
}
