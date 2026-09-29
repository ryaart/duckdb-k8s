# Writes object statuses that a kubelet/controller-manager would normally set.
# Sourced by scripts/test-cluster.sh with KUBECONFIG pointing at the test cluster.
status() { kubectl patch "$1" "$2" ${3:+-n "$3"} --subresource=status --type=merge -p "$4" >/dev/null; }

status pod web-1 shop '{"status":{"phase":"Running","podIP":"10.1.0.5",
  "conditions":[{"type":"Ready","status":"True"}],
  "containerStatuses":[{"name":"web","image":"nginx:1.27","imageID":"","ready":true,"restartCount":0,"state":{"running":{}}}]}}'

status pod web-2 shop '{"status":{"phase":"Running","podIP":"10.1.0.6",
  "conditions":[{"type":"Ready","status":"False"}],
  "containerStatuses":[
    {"name":"web","image":"nginx:1.27","imageID":"","ready":false,"restartCount":7,"state":{"waiting":{"reason":"CrashLoopBackOff"}}},
    {"name":"sidecar","image":"envoyproxy/envoy:v1.33","imageID":"","ready":true,"restartCount":1,"state":{"running":{}}}]}}'

status deployment api shop '{"status":{"replicas":2,"readyReplicas":1,"updatedReplicas":2,"availableReplicas":1}}'

status node node-1 "" '{"status":{"conditions":[{"type":"Ready","status":"True"}],
  "nodeInfo":{"kubeletVersion":"v1.37.0","machineID":"","systemUUID":"","bootID":"","kernelVersion":"","osImage":"","containerRuntimeVersion":"","kubeProxyVersion":"","operatingSystem":"linux","architecture":"arm64"},
  "addresses":[{"type":"InternalIP","address":"10.0.0.10"},{"type":"Hostname","address":"node-1"}],
  "allocatable":{"cpu":"4","memory":"16Gi"}}}'

status node node-2 "" '{"status":{"conditions":[{"type":"Ready","status":"False"}],
  "addresses":[{"type":"InternalIP","address":"10.0.0.11"}],"allocatable":{"cpu":"2","memory":"8Gi"}}}'
