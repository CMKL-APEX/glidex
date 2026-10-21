# Managed applications

Status: **draft** (proposal); nothing implemented. Builds on
[workload-identity.md](workload-identity.md), which comes first, and
extends [data-model.md](data-model.md), [security.md](security.md),
[networking.md](networking.md) and [web-ui.md](web-ui.md). §13 lists the
changes other documents need once this is accepted.

Users ask for software installed **inside** glidex VMs and run for them:
a Kubernetes cluster first, PostgreSQL next, more later. Each of these is
a set of VMs, disks and networks plus software configured in the guests
and kept running: created, scaled, upgraded, backed up, deleted.

Glidex's core knows VMs, disks, images, networks, projects, quotas and
who may do what. It should not learn what a Kubernetes control plane or
a Postgres replica is: every application added to the core makes it
larger, slower to change and harder to secure, and each would need its
own UI pages. This document keeps the core generic and adds
applications as a **layer above it**:

- **The core** gains a few generic building blocks every application
  needs and that are useful without any (§4): labels, owners,
  user-data, fixed addresses, disk hot-plug, and two opaque resource
  kinds, `App` and `AppInstance`.
- **The apps service** (`glidex-apps`), a separate process like the web
  UI, runs each application's controller. A controller turns an
  instance's spec into VMs, disks and networks through the public API,
  as the instance's own service account, and reports status.
- **The web UI** gets one generic **Apps** area that renders every
  application from its descriptor: a create form from its JSON Schema,
  its status, outputs and actions. Adding an application adds no UI
  code.

## 1. Scope

- **In:** the layering (§2, §3); the core additions (§4); the `App`
  descriptor and `AppInstance` resource (§5); the apps service, its
  identity and controllers (§6); the Apps UI (§7); the first two
  applications: single-VM PostgreSQL as the pilot (§8) and Kubernetes
  (§9).
- **Later, designed for:** highly available PostgreSQL; applications
  built on the Kubernetes app (operators in its clusters); third-party
  applications with out-of-process controllers (§6.4).
- **Out:** object storage (backups go to an S3-compatible store the
  tenant brings, §8); a marketplace or billing per application
  (instances' VMs are metered as VMs, metering.md); load balancers
  (a networking.md item, which the Kubernetes app will use once it
  exists).

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| A1 | Layering | The core has **no application-specific code**. Applications live in `glidex-apps`, which uses only the public API (gRPC), and could run on another host. | The core stays small and generic; an application's bug can't break VMs; applications ship on their own schedule. |
| A2 | Instances are core resources | An application instance is an **`AppInstance`**: a generic, project-scoped core resource with an opaque `spec` and `status` (JSON), validated against its `App`'s JSON Schema. The core gives it what every resource has: Cedar, tenancy, audit, events and watch. | The CRD pattern: authorization, audit, quotas, the UI and gxctl work for every application without the apps service reimplementing them, and the core still knows nothing about what a spec means. |
| A3 | Descriptors | Each application is an **`App`** record: name, versions, JSON Schema of the spec, UI hints, status fields, outputs, actions, the permissions its instances need, and a resource estimate (§5.1). The apps service registers its applications' descriptors; operators enable them per cluster. | The UI and gxctl render any application from data. |
| A4 | Instance identity | Creating an instance creates a **service account owned by it** in its project, with the descriptor's permissions. The creator must hold those permissions (as when creating a token with roles), so an application never escalates. The apps service acts **as that service account**, through short-lived tokens it obtains by impersonation (A5); no instance secret is stored. | Every change an application makes is authorized, audited and quota-checked as that instance, within its project. |
| A5 | The apps service's own identity | `glidex-apps` runs as its own Unix user and is identified on `api.sock` by `SO_PEERCRED` (security.md D5), like the web UI. Its only rights are writing instances' `status`, registering descriptors, and **`impersonateServiceAccount`** on service accounts **owned by an `AppInstance`**, for 15 min tokens. Off-host it authenticates with workload identity (workload-identity.md). | No long-lived secret anywhere; a compromised apps service reaches only applications' projects, as those instances. |
| A6 | Owners and protection | Resources an instance creates carry an **owner** (`{kind: AppInstance, id}`) and the label `glidex.io/app-instance`. Deleting the instance deletes what it owns (a finalizer). Changing or deleting an owned resource directly needs **`overrideManaged`**, which base policies give only to project administrators; the UI shows *Managed by …* and locks the controls. | Users see what belongs to an application and don't break it by hand; an administrator can still repair it. |
| A7 | Guest configuration | Controllers configure guests with **`user_data`** (cloud-init, merged with the generated seed) and give guests **workload identity** for anything they must call. User-data never carries secrets. | Works with stock cloud images; no secret on a guest's disk or in a seed image. |
| A8 | Secrets for people | Credentials an application must show its users (a database password, a first kubeconfig) are **sealed outputs** of the instance: stored sealed with the directory's data key (tenancy.md §13), shown only with **`readAppSecrets`**, every read audited. Where a guest system can trust the directory instead (Kubernetes' OIDC authentication, after T6), it does, and there is no secret to show. | Few secrets, each read on purpose and recorded. |
| A9 | Quotas | An instance's VMs, disks and addresses count against its project's quotas like any others. The descriptor's estimate is checked before creating, so a create that can't fit is refused at once instead of failing halfway. | No new quota system. |
| A10 | First applications | **Single-VM PostgreSQL** first (§8): small, and it exercises every part. Then **Kubernetes** (§9). | Prove the layer on something simple before the hard one. |
| A11 | Kubernetes approach | A **Cluster API infrastructure provider** for glidex, run in a management cluster the apps service operates, with the kubeadm bootstrap and control-plane providers (§9). | Creation, upgrades, scaling and node repair come from the Kubernetes ecosystem's standard machinery (as OpenStack Magnum's Cluster API driver and Rancher do) instead of being rewritten here. To confirm (§12). |

## 3. How the pieces fit

```
  user ── UI / gxctl / API ──► core: AppInstance (spec)        App (descriptor)
                                  │ watch                         ▲ register
                                  ▼                               │
                           glidex-apps  ── impersonates the instance's service account
                              │  controller for that App
                              ▼
                           core API: VMs, disks, networks (owned, labelled)
                              │  user_data + workload identity
                              ▼
                           guests: Postgres / Kubernetes nodes
                              │  status, outputs
                              ▼
                           AppInstance.status ──► UI
```

1. A user creates an instance (`POST /projects/{p}/app-instances`) with
   an `app`, a `version` and a `spec`. The core validates the spec
   against the descriptor's schema, checks quotas against the estimate,
   creates the instance and its service account, and records an event.
2. The apps service, watching `APP_INSTANCE`, sees it and runs that
   application's controller as the instance's service account.
3. The controller creates owned resources and reports progress in
   `status` (conditions, outputs). The UI shows it as it changes.
4. Changes to the spec (scale, upgrade) and actions (back up now) are
   reconciled the same way. Deleting the instance runs the controller's
   teardown, then the owner finalizer removes anything left.

## 4. Core additions

Each is generic and useful without applications.

| Addition | What | Notes |
|---|---|---|
| **Labels and annotations** | `labels` (`map<string,string>`, selectable) and `annotations` (not selectable) on VMs, disks, networks and app instances; `label_selector` on their list calls and on `Watch`. Keys `prefix/name`; the `glidex.io/` prefix is reserved. | Today only cluster nodes have labels. |
| **Owners** | `owner {kind, id}` on VMs, disks and networks; an owner finalizer deletes owned resources with their owner; `overrideManaged` (A6). | Generalizes the VM-owned root disk (images.md §7, `vm.owned-disk`). |
| **`user_data`** | A VM field (≤ 64 KiB) with cloud-init user-data, merged with the generated seed as a second MIME part, so credentials, growpart and networking still apply. Readable by whoever can read the VM: never a secret. | Today a custom seed needs a host path (`cloud_init_path`, `useHostPath`). |
| **Fixed addresses** | `ipv4` on a VM's network attachment: an IPAM reservation of that address (refused if taken or outside the network). | Stable endpoints for databases and control planes. |
| **Allowed addresses** | `allowed_addresses` on a NIC: extra addresses (and prefixes) the port may use, for in-guest failover (keepalived, kube-vip). OVN: port security's extra addresses; node networks: netd's filters. Needs `useNetwork` plus `assignAllowedAddress`. | A highly available endpoint without load balancers. |
| **Disk hot-plug** | Attaching and detaching a data disk on a running VM takes effect at once. | Today it waits for the next start (rest-api.md); a Kubernetes storage driver needs it. |
| **`App` and `AppInstance`** | The two resource kinds of §5, their API, Cedar actions (§5.3) and the `APP`/`APP_INSTANCE` watch kinds. | Opaque to the core beyond schema validation. |

## 5. Resources

### 5.1 `App` (descriptor)

```json
{
  "name": "postgres",
  "display_name": "PostgreSQL",
  "description": "A PostgreSQL server on one VM, with backups.",
  "versions": [{ "version": "17", "default": true }, { "version": "16" }],
  "spec_schema": {
    "type": "object",
    "required": ["size"],
    "properties": {
      "size": { "enum": ["small", "medium", "large"], "default": "small" },
      "disk_gib": { "type": "integer", "minimum": 10, "default": 20 },
      "network": { "type": "string", "format": "glidex-network" },
      "backups": { "type": "object", "properties": {
        "s3_endpoint": { "type": "string", "format": "uri" },
        "bucket": { "type": "string" },
        "schedule": { "type": "string", "default": "daily" } } }
    }
  },
  "ui": { "order": ["size", "disk_gib", "network", "backups"],
          "size": { "labels": { "small": "1 vCPU, 2 GiB", "medium": "2 vCPU, 8 GiB", "large": "4 vCPU, 16 GiB" } } },
  "status_fields": [{ "path": "endpoint", "label": "Endpoint" }, { "path": "last_backup", "label": "Last backup" }],
  "outputs": [{ "name": "password", "sealed": true }, { "name": "connection_string" }],
  "actions": [{ "name": "backup", "label": "Back up now" },
              { "name": "restart", "label": "Restart", "confirm": true }],
  "permissions": ["vm.write", "network.use"],
  "estimate": { "vms": 1, "vcpus": "spec.size", "disk_gib": "spec.disk_gib" }
}
```

- `format` values `glidex-network`, `glidex-image`, `glidex-credential`
  and `glidex-disk` make the UI offer a picker of the project's
  resources.
- `permissions` are permission groups (security.md §7.2), scoped to the
  instance's project.
- An `App` is a cluster-wide record. `registerApp` (the apps service)
  creates and updates them; `enableApp` (operators) makes one available,
  per cluster and optionally per tenant.

### 5.2 `AppInstance`

```json
{
  "id": "…", "name": "orders-db", "project": "web",
  "app": "postgres", "version": "17",
  "spec": { "size": "medium", "disk_gib": 50, "network": "backend" },
  "service_account": "<token id>",
  "labels": { "team": "orders" },
  "status": {
    "phase": "Running",
    "conditions": [{ "kind": "Ready", "status": "CONDITION_STATUS_TRUE", "reason": "Accepting connections" }],
    "fields": { "endpoint": "10.20.0.15:5432", "last_backup": "2026-10-21T02:00:00Z" },
    "outputs": { "connection_string": "postgres://app@10.20.0.15:5432/app" },
    "observed_generation": 3
  },
  "generation": 3
}
```

- `spec` changes bump `generation`; the controller sets
  `observed_generation` when it has acted on one.
- `status` is written only by the apps service (`updateAppStatus`).
- Sealed outputs are never in `status`: `GET /app-instances/{id}/secrets`
  (`readAppSecrets`) returns them.
- An action is `POST /app-instances/{id}/actions/{name}` with parameters
  per the descriptor; it is recorded as an event and the controller
  acknowledges it in `status`.

### 5.3 Cedar actions

| Action | Resource | Group | Who, by default |
|---|---|---|---|
| `readApp` | `App` | (none) | Everyone admitted on the cluster, for enabled apps |
| `createAppInstance` | `Project` | `app.write` | Project members who could create the descriptor's resources |
| `readAppInstance`, `updateAppInstance`, `deleteAppInstance`, `runAppAction` | `AppInstance` | `app.read` / `app.write` | Project roles |
| `readAppSecrets` | `AppInstance` | `app.secrets` | Project administrators |
| `overrideManaged` | VM, disk, network | `app.override` | Project administrators |
| `registerApp`, `updateAppStatus`, `impersonateServiceAccount` | `App`, `AppInstance`, `Token` | `system.apps` | The apps service's principal only |
| `enableApp` | `Cluster` | `app.manage` | Operators |

## 6. The apps service

### 6.1 Process

`glidex-apps` is installed beside the control plane, as its own Unix user
in `glidex-users`. It talks to `api.sock`, list-then-watches `APP_INSTANCE`
(and, for each instance, its owned resources by label), and runs one
controller loop per instance, level-triggered and idempotent like the
core's controllers (reconciliation.md). Several copies may run; one holds
each instance's lease.

### 6.2 Controllers

A controller is code for one application, compiled into `glidex-apps`
(first-party applications). It gets the instance and a client already
acting as the instance's service account, and implements: reconcile
(make the world match the spec), actions, and teardown. It reports in
`status` and emits events on the instance.

### 6.3 Versions

An `App` version names what its instances run (Postgres 17, Kubernetes
1.34). Changing an instance's `version` is an upgrade, done by its
controller. A new `glidex-apps` release may add versions and must keep
reconciling existing instances' versions until they're upgraded.

### 6.4 Later: third-party applications

Out-of-process controllers (a container or VM speaking a small gRPC
protocol to `glidex-apps`) would let others add applications. They need
a trust model (who may register, signed descriptors, sandboxing), so
they wait until two or three first-party applications have shaped the
interfaces.

## 7. Web UI

- **Apps** in the navigation: the enabled applications as cards, and
  the project's instances with their phase and `Ready` condition.
- **Create** renders the form from `spec_schema` and `ui`, with pickers
  for `glidex-*` formats, shows the estimate against the project's
  remaining quota, and creates the instance.
- **Instance page:** status fields, conditions, outputs (sealed ones
  behind *Show*, which asks `readAppSecrets`), actions as buttons, the
  owned resources (by owner) with links, events, and *Edit* for the spec
  (the same form).
- **Resource pages** of owned VMs, disks and networks show *Managed by
  orders-db (PostgreSQL)* and disable editing unless the user holds
  `overrideManaged`.
- gxctl mirrors this: `app list`, `app create <app> --set key=value`,
  `app get|edit|delete <instance>`, `app action <instance> <action>`,
  `app secrets <instance>`.

## 8. PostgreSQL (pilot)

- One VM from the catalog's Ubuntu image, a data disk (owned), a fixed
  address on the chosen network, `user_data` that installs PostgreSQL
  from the distribution, puts its data on the data disk and generates
  the password inside the guest.
- The guest reports readiness and its generated password to the
  controller over workload identity (a small endpoint of `glidex-apps`
  that accepts a VM identity token for the instance's VMs); the password
  becomes a sealed output.
- Backups (optional): `pgBackRest` to the tenant's S3-compatible store,
  which trusts the directory's issuer (MinIO and Ceph RGW accept web
  identity), so no S3 key is stored either; `backup` runs one now.
- Upgrades between major versions are out of the pilot; minor updates
  are the guest's package updates.

## 9. Kubernetes

- **Management cluster.** The apps service runs one small Kubernetes
  cluster (VMs in a system project) with Cluster API and the glidex
  infrastructure provider. Each instance is a `Cluster` there, in a
  namespace per instance; the provider acts as the instance's service
  account (A4, A5).
- **Provider** (`cluster-api-provider-glidex`): `GlidexCluster` (network,
  control-plane endpoint as an allowed address held by kube-vip) and
  `GlidexMachine` (a VM from an image with a matching kernel; user-data
  from the kubeadm bootstrap provider).
- **In each cluster:** a glidex cloud controller (node addresses and
  lifecycle; load balancers once networking has them) and a CSI driver
  (glidex disks as persistent volumes, needs disk hot-plug), both
  authenticating with workload identity; pods kept from the metadata
  endpoint (workload-identity.md §9).
- **People** reach the API server with a kubeconfig shown as a sealed
  output; after T6, the API server trusts the directory as its OIDC
  issuer and people use their own login.
- **Instance spec:** version, control-plane count (1 or 3), worker pools
  (size, count), network, pod and service CIDRs. Actions: upgrade
  (rolling, by Cluster API), scale.

## 10. Security considerations

- Instances act only through their own service accounts, inside their
  projects, with permissions their creators already had (A4).
- The apps service holds no instance secret and can impersonate only
  instance-owned service accounts (A5); its other rights are writing
  status and registering descriptors.
- User-data is visible to anyone who can read the VM; controllers never
  put secrets there (A7).
- Sealed outputs are the only stored secrets; reading one is audited
  (A8).
- Owned resources are protected from accidental edits, not from their
  project's administrators (A6).
- The management cluster (§9) holds no tenant credentials: its provider
  impersonates per instance.

## 11. Testing

- Core: labels and selectors, owners and the finalizer, `user_data`
  merging, fixed and allowed addresses, disk hot-plug, `App` and
  `AppInstance` API, schema validation, the Cedar actions.
- Apps service: a test application whose controller creates a VM and
  reports status, driven through create, edit, action and delete, with
  impersonation and audit checked.
- PostgreSQL: create, connect with the sealed password, back up to a
  MinIO trusting the test directory, delete leaves nothing.
- Kubernetes: create a 1 + 1 cluster, run a pod with a persistent
  volume, scale, upgrade one minor version, delete.
- UI e2e: create an instance from the generated form, see status and
  outputs, run an action, see *Managed by* on its VM.

## 12. Open questions

1. **Kubernetes through Cluster API (A11)** needs a management cluster
   the operator runs. The alternative is a smaller controller in
   `glidex-apps` that drives kubeadm itself (no management cluster, but
   upgrades and node repair written here). Recommendation: Cluster API.
2. **Where `glidex-apps` runs in a cluster of servers:** on every server
   with a lease per instance (recommended), or on one.
3. **Who enables applications:** per cluster by operators (proposed),
   or also per tenant by directory owners.
4. **Object storage**: backups need an S3-compatible store; glidex has
   none. Bring-your-own for now; a managed one could itself be an
   application later.

## 13. Changes to other specs (on acceptance)

- **data-model.md:** labels, annotations and owners; `user_data`;
  `ipv4` and `allowed_addresses` on NICs; `App`, `AppInstance`.
- **security.md:** the actions of §5.3, `assignAllowedAddress`; the apps
  service's principal.
- **networking.md:** fixed addresses and allowed addresses on node and
  OVN networks.
- **images.md / rest-api.md:** disk hot-plug on running VMs.
- **web-ui.md:** the Apps routes and *Managed by*.
- **cli.md:** `gxctl app`.
- **workload-identity.md:** `glidex.owner` in tokens (§4.2).

## 14. Milestones

| # | Scope | Needs | Acceptance |
|---|---|---|---|
| **A0** core | Labels and selectors, owners and `overrideManaged`, `user_data`, fixed addresses, allowed addresses, disk hot-plug | | §11 core tests. |
| **A1** framework | `App`, `AppInstance`, their Cedar actions and watch kinds; `glidex-apps` with impersonation; the Apps UI and `gxctl app`; a test application | A0, WI1 | §11 apps service tests and UI e2e. |
| **A2** PostgreSQL | §8 | A1 | §11 PostgreSQL tests. |
| **A3** Kubernetes | §9: management cluster, provider, cloud controller, CSI driver | A1, WI1, WI2 | §11 Kubernetes tests. |
| **A4** more | Highly available PostgreSQL; applications on the Kubernetes app; third-party controllers (§6.4) | A3 | Their own designs. |

Workload identity (WI0, WI1) comes before A1: every application uses it.

## 15. References

- Kubernetes CustomResourceDefinitions and owner references (the
  pattern of A2 and A6).
- Cluster API, its kubeadm providers, and infrastructure providers such
  as CAPO (OpenStack) and CAPV (vSphere); OpenStack Magnum's Cluster API
  driver.
- JSON Schema; schema-driven forms (react-jsonschema-form).
- cloud-init MIME multi-part user-data.
- pgBackRest; MinIO and Ceph RGW `AssumeRoleWithWebIdentity`.
