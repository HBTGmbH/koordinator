Coordinate the execution of multiple containers in a pod, allowing to model start and stop dependencies amongst them.
This is heavily inspired by the https://github.com/karlkfi/kubexit project, but implementing additional features requested for it.

The executable does not have _any_ (dynamically linked) dependencies other than a Linux kernel (which is already necessary to run containers, like in a Kubernetes environment), so it can be used in a "distroless" container or with any distribution like Debian, Alpine, etc.

Please see the [Wiki pages](https://github.com/HBTGmbH/koordinator/wiki) for more information.

## Use case

You can use `koordinator` to start multiple containers in a pod in a specific order. This can be used to ensure that (main) containers are started only after (sidecar) containers have started and became ready, or that sidecar containers are terminated once the main container exited.

Especially _stopping_ sidecar containers reliably when the main container exited is a problem in Kubernetes when using CronJobs/Jobs. The `koordinator` executable can be used to solve this problem.

The following is an example of a Job specification containing two containers. The first container is a sidecar container that provides a database connection. The second container is the main container that uses the database.
The main container specifies the sidecar container as a start dependency and, likewise, the sidecar container specifies the main container as a stop dependency. This ensures that the main container only starts once the sidecar container became ready and that the sidecar container is stopped after the main container exited.

```yaml
apiVersion: batch/v1
kind: Job
spec:
  template:
    spec:
      volumes:
      # This volume is used to copy the koordinator executable from
      # the container's filesystem to an emptyDir volume.
      - name: koordinator
        emptyDir: {}
      # This volume is used to pass the pod's name and namespace to
      # the koordinator executable.
      - name: podinfo
        downwardAPI:
          items:
          - path: name
            fieldRef:
              fieldPath: metadata.name
          - path: namespace
            fieldRef:
              fieldPath: metadata.namespace
      initContainers:
      - name: koordinator-init
        image: koordinator:latest
        args: [/koordinator]
        volumeMounts:
        - name: koordinator
          mountPath: /koordinator
      containers:
      - name: cloud-sql-proxy
        image: eu.gcr.io/cloud-sql-connectors/cloud-sql-proxy:2.3.0
        env:
        - name: KOOORDINATOR_SPEC
          value: '{"stopDeps":["main-container"]}'
        command: [/koordinator/koordinator, /cloud-sql-proxy]
        args:
        - --health-check
        - --http-port=9090
        - --http-host=0.0.0.0
        ...
        readinessProbe:
          httpGet:
            path: /readiness
            port: 9090
        volumeMounts:
        - name: koordinator
          mountPath: /koordinator
        - name: podinfo
          mountPath: /etc/podinfo
      - name: main-container
        image: main-app-image
        command: [/koordinator/koordinator, /main-app]
        env:
        - name: KOOORDINATOR_SPEC
          value: '{"startDeps":["cloud-sql-proxy"],"stopDelay":"10s"}'
        ...
        volumeMounts:
        - name: koordinator
          mountPath: /koordinator
        - name: podinfo
          mountPath: /etc/podinfo
        ...
```

Because the koordinator will watch the current pod for changes to the containers' readiness status, it needs RBAC permissions to do so. The following is an example of a Role and RoleBinding that grants the necessary permissions to the koordinator.

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: koordinator
rules:
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: koordinator
roleRef:
    apiGroup: rbac.authorization.k8s.io
    kind: Role
    name: koordinator
subjects:
- kind: ServiceAccount
  name: default
  namespace: default
```