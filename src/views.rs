//! Column definitions: the generic `k8s()` view and typed views such as `k8s_pods()`.
//!
//! Each column is a name, a type, and an extractor over the object's JSON, so a view
//! is plain data and only the projected columns are ever computed.

use duckdb_tables::{Cell, ColType};
use serde_json::Value;
use std::sync::Arc;

pub type Column = duckdb_tables::Column<Obj>;

/// One listed object plus the facts about the list it came from.
pub struct Obj {
    pub context: Arc<str>,
    pub api_version: Arc<str>,
    pub kind: Arc<str>,
    pub json: Value,
}

pub struct View {
    /// Resource to list. `None` for the generic `k8s(resource)` function.
    pub resource: Option<&'static str>,
    pub doc: &'static str,
    pub columns: &'static [Column],
}

// ---- JSON helpers --------------------------------------------------------

fn at<'a>(o: &'a Obj, ptr: &str) -> Option<&'a Value> {
    o.json.pointer(ptr).filter(|v| !v.is_null())
}

/// Kubernetes often serialises "unknown" as an empty string; typed columns report it as NULL.
fn string(o: &Obj, ptr: &str) -> Cell {
    at(o, ptr)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .filter(|s| !s.is_empty())
        .into()
}

fn int(o: &Obj, ptr: &str) -> Option<i64> {
    at(o, ptr).and_then(Value::as_i64)
}

fn json(o: &Obj, ptr: &str) -> Cell {
    at(o, ptr).map(Value::to_string).into()
}

fn parse_ts(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_microsecond())
}

fn timestamp(o: &Obj, ptr: &str) -> Option<i64> {
    at(o, ptr).and_then(Value::as_str).and_then(parse_ts)
}

fn ts_cell(v: Option<i64>) -> Cell {
    v.map_or(Cell::Null, Cell::Ts)
}

fn array<'a>(o: &'a Obj, ptr: &str) -> &'a [Value] {
    at(o, ptr)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn images(o: &Obj, containers_ptr: &str) -> Cell {
    Cell::List(
        array(o, containers_ptr)
            .iter()
            .filter_map(|c| c["image"].as_str().map(String::from))
            .collect(),
    )
}

fn condition_true(o: &Obj, ty: &str) -> Cell {
    let found = array(o, "/status/conditions")
        .iter()
        .find(|c| c["type"] == ty)
        .map(|c| c["status"] == "True");
    found.into()
}

// ---- shared columns ------------------------------------------------------

const CONTEXT: Column = Column {
    name: "context",
    ty: ColType::Varchar,
    doc: "kubeconfig context the row came from.",
    get: |o| Cell::Str(o.context.to_string()),
};
const NAMESPACE: Column = Column {
    name: "namespace",
    ty: ColType::Varchar,
    doc: "Namespace; NULL for cluster-scoped resources.",
    get: |o| string(o, "/metadata/namespace"),
};
const NAME: Column = Column {
    name: "name",
    ty: ColType::Varchar,
    doc: "Object name.",
    get: |o| string(o, "/metadata/name"),
};
const CREATED: Column = Column {
    name: "created",
    ty: ColType::Timestamp,
    doc: "metadata.creationTimestamp, UTC.",
    get: |o| ts_cell(timestamp(o, "/metadata/creationTimestamp")),
};
const LABELS: Column = Column {
    name: "labels",
    ty: ColType::Varchar,
    doc: "Labels as a JSON object string; read one with labels->>'$.app'.",
    get: |o| json(o, "/metadata/labels"),
};
const OBJECT: Column = Column {
    name: "object",
    ty: ColType::Varchar,
    doc: "The whole object as JSON. Only serialised when selected.",
    get: |o| Cell::Str(o.json.to_string()),
};

// ---- k8s(resource) -------------------------------------------------------

pub static GENERIC: View = View {
    resource: None,
    doc: "Any resource the API server knows, including CRDs: plural (pods), kind (Pod) or group-qualified (deployments.apps).",
    columns: &[
        CONTEXT,
        Column {
            name: "api_version",
            ty: ColType::Varchar,
            doc: "apiVersion, such as apps/v1.",
            get: |o| Cell::Str(o.api_version.to_string()),
        },
        Column {
            name: "kind",
            ty: ColType::Varchar,
            doc: "Kind, such as Deployment.",
            get: |o| Cell::Str(o.kind.to_string()),
        },
        NAMESPACE,
        NAME,
        Column {
            name: "uid",
            ty: ColType::Varchar,
            doc: "metadata.uid.",
            get: |o| string(o, "/metadata/uid"),
        },
        CREATED,
        LABELS,
        Column {
            name: "annotations",
            ty: ColType::Varchar,
            doc: "Annotations as a JSON object string.",
            get: |o| json(o, "/metadata/annotations"),
        },
        OBJECT,
    ],
};

// ---- k8s_pods() ----------------------------------------------------------

/// kubectl's STATUS column: the most specific reason available, falling back to the phase.
fn pod_status(o: &Obj) -> Cell {
    if at(o, "/metadata/deletionTimestamp").is_some() {
        return Cell::Str("Terminating".into());
    }
    let reason = array(o, "/status/containerStatuses").iter().find_map(|c| {
        c.pointer("/state/waiting/reason")
            .or_else(|| c.pointer("/state/terminated/reason"))
            .and_then(Value::as_str)
    });
    match reason {
        Some(r) => Cell::Str(r.into()),
        None => match at(o, "/status/reason") {
            Some(_) => string(o, "/status/reason"),
            None => string(o, "/status/phase"),
        },
    }
}

pub static PODS: View = View {
    resource: Some("pods"),
    doc: "Pods, with kubectl's STATUS, readiness and restarts.",
    columns: &[
        CONTEXT,
        NAMESPACE,
        NAME,
        Column {
            name: "phase",
            ty: ColType::Varchar,
            doc: "status.phase: Pending, Running, Succeeded, Failed or Unknown.",
            get: |o| string(o, "/status/phase"),
        },
        Column {
            name: "status",
            ty: ColType::Varchar,
            doc: "kubectl's STATUS column: the most specific reason (CrashLoopBackOff, Terminating, ...), falling back to the phase.",
            get: pod_status,
        },
        Column {
            name: "ready",
            ty: ColType::Boolean,
            doc: "The Ready condition; NULL until it's reported.",
            get: |o| condition_true(o, "Ready"),
        },
        Column {
            name: "containers_ready",
            ty: ColType::Bigint,
            doc: "Containers reporting ready.",
            get: |o| {
                Cell::Int(
                    array(o, "/status/containerStatuses")
                        .iter()
                        .filter(|c| c["ready"] == true)
                        .count() as i64,
                )
            },
        },
        Column {
            name: "containers",
            ty: ColType::Bigint,
            doc: "Containers in the pod spec.",
            get: |o| Cell::Int(array(o, "/spec/containers").len() as i64),
        },
        Column {
            name: "restarts",
            ty: ColType::Bigint,
            doc: "Total container restarts.",
            get: |o| {
                Cell::Int(
                    array(o, "/status/containerStatuses")
                        .iter()
                        .filter_map(|c| c["restartCount"].as_i64())
                        .sum(),
                )
            },
        },
        Column {
            name: "node",
            ty: ColType::Varchar,
            doc: "Node the pod is scheduled on.",
            get: |o| string(o, "/spec/nodeName"),
        },
        Column {
            name: "pod_ip",
            ty: ColType::Varchar,
            doc: "Pod IP.",
            get: |o| string(o, "/status/podIP"),
        },
        Column {
            name: "images",
            ty: ColType::VarcharList,
            doc: "Container images, in spec order.",
            get: |o| images(o, "/spec/containers"),
        },
        CREATED,
        LABELS,
        OBJECT,
    ],
};

// ---- k8s_deployments() ---------------------------------------------------

pub static DEPLOYMENTS: View = View {
    resource: Some("deployments.apps"),
    doc: "Deployments and their rollout state.",
    columns: &[
        CONTEXT,
        NAMESPACE,
        NAME,
        Column {
            name: "replicas",
            ty: ColType::Bigint,
            doc: "Desired replicas.",
            get: |o| int(o, "/spec/replicas").into(),
        },
        Column {
            name: "ready_replicas",
            ty: ColType::Bigint,
            doc: "Ready replicas.",
            get: |o| Cell::Int(int(o, "/status/readyReplicas").unwrap_or(0)),
        },
        Column {
            name: "updated_replicas",
            ty: ColType::Bigint,
            doc: "Replicas running the current pod template.",
            get: |o| Cell::Int(int(o, "/status/updatedReplicas").unwrap_or(0)),
        },
        Column {
            name: "available_replicas",
            ty: ColType::Bigint,
            doc: "Available replicas.",
            get: |o| Cell::Int(int(o, "/status/availableReplicas").unwrap_or(0)),
        },
        Column {
            name: "images",
            ty: ColType::VarcharList,
            doc: "Container images in the pod template.",
            get: |o| images(o, "/spec/template/spec/containers"),
        },
        CREATED,
        LABELS,
        OBJECT,
    ],
};

// ---- k8s_nodes() ---------------------------------------------------------

const ROLE_PREFIX: &str = "node-role.kubernetes.io/";

pub static NODES: View = View {
    resource: Some("nodes"),
    doc: "Nodes, with readiness, roles and allocatable resources.",
    columns: &[
        CONTEXT,
        NAME,
        Column {
            name: "ready",
            ty: ColType::Boolean,
            doc: "The Ready condition.",
            get: |o| condition_true(o, "Ready"),
        },
        Column {
            name: "roles",
            ty: ColType::VarcharList,
            doc: "Roles from node-role.kubernetes.io/<role> labels, sorted.",
            get: |o| {
                let mut roles: Vec<String> = at(o, "/metadata/labels")
                    .and_then(Value::as_object)
                    .map(|l| {
                        l.keys()
                            .filter_map(|k| k.strip_prefix(ROLE_PREFIX))
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                roles.sort();
                Cell::List(roles)
            },
        },
        Column {
            name: "unschedulable",
            ty: ColType::Boolean,
            doc: "Cordoned.",
            get: |o| Cell::Bool(at(o, "/spec/unschedulable") == Some(&Value::Bool(true))),
        },
        Column {
            name: "version",
            ty: ColType::Varchar,
            doc: "Kubelet version.",
            get: |o| string(o, "/status/nodeInfo/kubeletVersion"),
        },
        Column {
            name: "internal_ip",
            ty: ColType::Varchar,
            doc: "InternalIP address.",
            get: |o| {
                let ip = array(o, "/status/addresses")
                    .iter()
                    .find(|a| a["type"] == "InternalIP");
                ip.and_then(|a| a["address"].as_str())
                    .map(String::from)
                    .into()
            },
        },
        Column {
            name: "cpu",
            ty: ColType::Varchar,
            doc: "Allocatable CPU, as a Kubernetes quantity.",
            get: |o| string(o, "/status/allocatable/cpu"),
        },
        Column {
            name: "memory",
            ty: ColType::Varchar,
            doc: "Allocatable memory, as a Kubernetes quantity such as 16Gi.",
            get: |o| string(o, "/status/allocatable/memory"),
        },
        CREATED,
        LABELS,
        OBJECT,
    ],
};

// ---- k8s_services() ------------------------------------------------------

pub static SERVICES: View = View {
    resource: Some("services"),
    doc: "Services, with their addresses and ports.",
    columns: &[
        CONTEXT,
        NAMESPACE,
        NAME,
        Column {
            name: "type",
            ty: ColType::Varchar,
            doc: "ClusterIP, NodePort, LoadBalancer or ExternalName.",
            get: |o| string(o, "/spec/type"),
        },
        Column {
            name: "cluster_ip",
            ty: ColType::Varchar,
            doc: "Cluster IP.",
            get: |o| string(o, "/spec/clusterIP"),
        },
        Column {
            name: "external_ips",
            ty: ColType::VarcharList,
            doc: "Load balancer ingress IPs or hostnames, then spec.externalIPs.",
            get: |o| {
                let lb = array(o, "/status/loadBalancer/ingress")
                    .iter()
                    .filter_map(|i| i["ip"].as_str().or_else(|| i["hostname"].as_str()));
                let external = array(o, "/spec/externalIPs")
                    .iter()
                    .filter_map(Value::as_str);
                Cell::List(lb.chain(external).map(String::from).collect())
            },
        },
        Column {
            name: "ports",
            ty: ColType::VarcharList,
            doc: "port/protocol, or port:nodePort/protocol for node ports.",
            // kubectl style: "80/TCP", or "80:30080/TCP" with a node port
            get: |o| {
                let ports = array(o, "/spec/ports").iter().map(|p| {
                    let proto = p["protocol"].as_str().unwrap_or("TCP");
                    match p["nodePort"].as_i64() {
                        Some(np) => format!("{}:{np}/{proto}", p["port"]),
                        None => format!("{}/{proto}", p["port"]),
                    }
                });
                Cell::List(ports.collect())
            },
        },
        Column {
            name: "selector",
            ty: ColType::Varchar,
            doc: "Pod selector as a JSON object string.",
            get: |o| json(o, "/spec/selector"),
        },
        CREATED,
        LABELS,
        OBJECT,
    ],
};

// ---- k8s_events() --------------------------------------------------------

pub static EVENTS: View = View {
    resource: Some("events"),
    doc: "Events, from both the core and events.k8s.io formats.",
    columns: &[
        CONTEXT,
        NAMESPACE,
        Column {
            name: "type",
            ty: ColType::Varchar,
            doc: "Normal or Warning.",
            get: |o| string(o, "/type"),
        },
        Column {
            name: "reason",
            ty: ColType::Varchar,
            doc: "Short reason, such as BackOff.",
            get: |o| string(o, "/reason"),
        },
        Column {
            name: "object_kind",
            ty: ColType::Varchar,
            doc: "Kind of the object the event is about.",
            get: |o| string(o, "/involvedObject/kind"),
        },
        Column {
            name: "object_name",
            ty: ColType::Varchar,
            doc: "Name of the object the event is about.",
            get: |o| string(o, "/involvedObject/name"),
        },
        Column {
            name: "message",
            ty: ColType::Varchar,
            doc: "Event message.",
            get: |o| string(o, "/message"),
        },
        Column {
            name: "count",
            ty: ColType::Bigint,
            doc: "How many times it occurred.",
            get: |o| {
                Cell::Int(
                    int(o, "/count")
                        .or_else(|| int(o, "/series/count"))
                        .unwrap_or(1),
                )
            },
        },
        Column {
            name: "first_seen",
            ty: ColType::Timestamp,
            doc: "First occurrence, UTC.",
            get: |o| {
                ts_cell(timestamp(o, "/firstTimestamp").or_else(|| timestamp(o, "/eventTime")))
            },
        },
        Column {
            name: "last_seen",
            ty: ColType::Timestamp,
            doc: "Most recent occurrence, UTC.",
            get: |o| {
                ts_cell(
                    timestamp(o, "/lastTimestamp")
                        .or_else(|| timestamp(o, "/series/lastObservedTime"))
                        .or_else(|| timestamp(o, "/eventTime"))
                        .or_else(|| timestamp(o, "/metadata/creationTimestamp")),
                )
            },
        },
        Column {
            name: "source",
            ty: ColType::Varchar,
            doc: "Component that reported it, such as kubelet.",
            get: |o| match at(o, "/source/component") {
                Some(_) => string(o, "/source/component"),
                None => string(o, "/reportingComponent"),
            },
        },
        NAME,
        OBJECT,
    ],
};

/// Typed views registered as `k8s_<name>()`.
pub const TYPED: &[(&str, &View)] = &[
    ("k8s_pods", &PODS),
    ("k8s_deployments", &DEPLOYMENTS),
    ("k8s_nodes", &NODES),
    ("k8s_services", &SERVICES),
    ("k8s_events", &EVENTS),
];
