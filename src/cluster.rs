//! Kubernetes access: kubeconfig contexts, clients, API discovery and paginated LISTs.
//!
//! DuckDB calls table functions synchronously, so all async kube-rs work runs on a
//! single process-wide Tokio runtime via `block_on`.

use k8s_openapi::api::core::v1::Pod;
use kube::{
    Client, Config,
    api::{Api, DynamicObject, ListParams, LogParams},
    config::{KubeConfigOptions, Kubeconfig},
    discovery::{ApiCapabilities, ApiResource, Discovery, Scope},
};
use std::{
    collections::HashMap,
    error::Error,
    sync::{Mutex, OnceLock},
};

type BoxError = Box<dyn Error>;

/// API errors print as a large debug struct; surface just the server's message.
fn api_error(e: kube::Error) -> BoxError {
    match e {
        kube::Error::Api(status) if !status.message.is_empty() => {
            format!("{} (HTTP {})", status.message, status.code).into()
        }
        other => other.into(),
    }
}

/// With timestamps=true the kubelet prefixes each line with "<RFC 3339> ".
fn split_timestamp(line: &str) -> (Option<&str>, &str) {
    match line.split_once(' ') {
        Some((ts, rest)) if ts.parse::<jiff::Timestamp>().is_ok() => (Some(ts), rest),
        _ => (None, line),
    }
}

/// Page size for LIST calls, so large clusters are fetched in bounded chunks.
const PAGE_SIZE: u32 = 500;

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        // kube's rustls feature doesn't pick a crypto backend; choose ring explicitly.
        // Ignore the error if the host process already installed one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("duckdb-k8s")
            .enable_all()
            .build()
            .expect("failed to start tokio runtime")
    })
}

pub struct ContextInfo {
    pub name: String,
    pub cluster: Option<String>,
    pub user: Option<String>,
    pub namespace: Option<String>,
    pub current: bool,
}

pub fn contexts() -> Result<Vec<ContextInfo>, BoxError> {
    let kc = Kubeconfig::read()?;
    let current = kc.current_context.clone();
    Ok(kc
        .contexts
        .into_iter()
        .map(|c| {
            let ctx = c.context.unwrap_or_default();
            ContextInfo {
                current: current.as_deref() == Some(c.name.as_str()),
                name: c.name,
                cluster: Some(ctx.cluster).filter(|s| !s.is_empty()),
                user: ctx.user,
                namespace: ctx.namespace,
            }
        })
        .collect())
}

/// Resolves the context the user asked for, or the kubeconfig's current context.
/// Returns "in-cluster" when running inside a pod with no kubeconfig.
fn resolve_context(context: Option<&str>) -> String {
    if let Some(c) = context {
        return c.to_string();
    }
    Kubeconfig::read()
        .ok()
        .and_then(|kc| kc.current_context)
        .unwrap_or_else(|| "in-cluster".to_string())
}

async fn client_for(context: &str) -> Result<Client, BoxError> {
    let config = if context == "in-cluster" {
        Config::incluster()?
    } else {
        let opts = KubeConfigOptions {
            context: Some(context.to_string()),
            ..Default::default()
        };
        Config::from_kubeconfig(&opts).await?
    };
    Ok(Client::try_from(config)?)
}

/// Discovery is one API call per group, so cache it per context for the process lifetime.
type DiscoveredResources = Vec<(ApiResource, ApiCapabilities)>;

async fn discover(context: &str, client: &Client) -> Result<DiscoveredResources, BoxError> {
    static CACHE: OnceLock<Mutex<HashMap<String, DiscoveredResources>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().unwrap().get(context) {
        return Ok(found.clone());
    }
    let discovery = Discovery::new(client.clone())
        .run()
        .await
        .map_err(api_error)?;
    let resources: DiscoveredResources = discovery
        .groups()
        .flat_map(|g| g.recommended_resources())
        .collect();
    cache
        .lock()
        .unwrap()
        .insert(context.to_string(), resources.clone());
    Ok(resources)
}

/// Matches `pods`, `pod`, `Pod`, or a group-qualified name like `deployments.apps`.
/// Core and built-in groups win over CRDs that reuse a plural (e.g. `events`).
fn find_resource<'a>(
    resources: &'a DiscoveredResources,
    name: &str,
) -> Option<&'a (ApiResource, ApiCapabilities)> {
    let name = name.to_lowercase();
    let (plural, group) = match name.split_once('.') {
        Some((p, g)) => (p.to_string(), Some(g.to_string())),
        None => (name.clone(), None),
    };
    let matches = |ar: &ApiResource| {
        (ar.plural == plural || ar.kind.to_lowercase() == plural)
            && group.as_ref().is_none_or(|g| &ar.group == g)
    };
    let rank = |ar: &ApiResource| match ar.group.as_str() {
        "" => 0,
        g if !g.contains('.') || g.ends_with(".k8s.io") => 1,
        _ => 2,
    };
    resources
        .iter()
        .filter(|(ar, _)| matches(ar))
        .min_by_key(|(ar, _)| rank(ar))
}

pub struct ListRequest {
    pub resource: String,
    pub context: Option<String>,
    pub namespace: Option<String>,
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
}

pub struct ListResult {
    pub context: String,
    pub api_version: String,
    pub kind: String,
    pub items: Vec<DynamicObject>,
}

pub struct LogRequest {
    pub pod: String,
    pub context: Option<String>,
    pub namespace: Option<String>,
    /// All containers when `None`.
    pub container: Option<String>,
    pub tail: Option<i64>,
    pub since_seconds: Option<i64>,
    pub previous: bool,
}

pub struct LogLine {
    pub container: String,
    /// The timestamp the kubelet prefixed to the line (RFC 3339), if present.
    pub timestamp: Option<String>,
    pub message: String,
}

pub struct LogResult {
    pub context: String,
    pub namespace: String,
    pub lines: Vec<LogLine>,
}

pub fn logs(req: &LogRequest) -> Result<LogResult, BoxError> {
    runtime().block_on(logs_async(req))
}

async fn logs_async(req: &LogRequest) -> Result<LogResult, BoxError> {
    let context = resolve_context(req.context.as_deref());
    let client = client_for(&context).await?;
    let namespace = match &req.namespace {
        Some(ns) => ns.clone(),
        None => client.default_namespace().to_string(),
    };
    let api: Api<Pod> = Api::namespaced(client, &namespace);

    let containers = match &req.container {
        Some(c) => vec![c.clone()],
        None => {
            let pod = api.get(&req.pod).await.map_err(api_error)?;
            let spec = pod.spec.unwrap_or_default();
            spec.init_containers
                .unwrap_or_default()
                .into_iter()
                .chain(spec.containers)
                .map(|c| c.name)
                .collect()
        }
    };

    let mut lines = Vec::new();
    for container in containers {
        let params = LogParams {
            container: Some(container.clone()),
            tail_lines: req.tail,
            since_seconds: req.since_seconds,
            previous: req.previous,
            timestamps: true,
            ..Default::default()
        };
        let text = api.logs(&req.pod, &params).await.map_err(api_error)?;
        for line in text.lines() {
            let (timestamp, message) = split_timestamp(line);
            lines.push(LogLine {
                container: container.clone(),
                timestamp: timestamp.map(String::from),
                message: message.to_string(),
            });
        }
    }
    Ok(LogResult {
        context,
        namespace,
        lines,
    })
}

pub fn list(req: &ListRequest) -> Result<ListResult, BoxError> {
    runtime().block_on(list_async(req))
}

async fn list_async(req: &ListRequest) -> Result<ListResult, BoxError> {
    let context = resolve_context(req.context.as_deref());
    let client = client_for(&context).await?;
    let resources = discover(&context, &client).await?;
    let (ar, caps) = find_resource(&resources, &req.resource).ok_or_else(|| {
        format!(
            "unknown resource '{}' in context '{}'",
            req.resource, context
        )
    })?;

    let api: Api<DynamicObject> = match (&caps.scope, &req.namespace) {
        (Scope::Namespaced, Some(ns)) => Api::namespaced_with(client, ns, ar),
        _ => Api::all_with(client, ar),
    };

    let mut items = Vec::new();
    let mut continue_token: Option<String> = None;
    loop {
        let mut lp = ListParams::default().limit(PAGE_SIZE);
        if let Some(l) = &req.label_selector {
            lp = lp.labels(l);
        }
        if let Some(f) = &req.field_selector {
            lp = lp.fields(f);
        }
        if let Some(token) = &continue_token {
            lp = lp.continue_token(token);
        }
        let page = api.list(&lp).await.map_err(api_error)?;
        items.extend(page.items);
        continue_token = page.metadata.continue_.filter(|t| !t.is_empty());
        if continue_token.is_none() {
            break;
        }
    }

    Ok(ListResult {
        context,
        api_version: ar.api_version.clone(),
        kind: ar.kind.clone(),
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::split_timestamp;

    #[test]
    fn splits_kubelet_timestamp_prefix() {
        assert_eq!(
            split_timestamp("2026-09-29T10:00:00.123456789Z GET /healthz 200"),
            (Some("2026-09-29T10:00:00.123456789Z"), "GET /healthz 200")
        );
    }

    #[test]
    fn keeps_lines_without_a_timestamp() {
        assert_eq!(split_timestamp("plain message"), (None, "plain message"));
        assert_eq!(split_timestamp(""), (None, ""));
    }
}
