// Base policies (spec/security.md §7.3): invariants that no role, link
// or site policy can override.

@id("base.disabled-user")
forbid (principal is Glidex::User, action, resource) when { principal.disabled };

@id("base.expired-token")
forbid (principal is Glidex::Token, action, resource) when { principal.expired };

// Step-up: host-wide network changes and policy writes need a login in
// the last 10 minutes.
@id("base.step-up")
forbid (principal, action in Glidex::Action::"step-up", resource)
unless { context.auth.age_secs <= 600 };

// A VM only uses disks and credentials of its own project.
@id("base.same-project")
forbid (principal, action in [Glidex::Action::"useDisk", Glidex::Action::"useCredential"], resource)
unless { resource.project == context.project };

// Which networks a project may use. A forbid, so no role can widen it.
@id("base.network-grant")
forbid (principal, action == Glidex::Action::"useNetwork", resource)
unless {
  resource.all_projects || resource.grants.contains(context.project) ||
  resource.tenant_grants.contains(context.project.tenant) ||
  (resource has project && resource.project == context.project) ||
  (resource has project && resource.shares.contains(context.project))
};

// Who may attach is decided by attachNetwork; useNetwork only checks the
// network against the project.
@id("base.network-use")
permit (principal, action == Glidex::Action::"useNetwork", resource);

// Which PCI devices a project may use without host.devices.
@id("base.pci-grant")
permit (principal, action == Glidex::Action::"usePciDevice", resource)
when {
  resource.grants.contains(context.project) ||
  resource.tenant_grants.contains(context.project.tenant)
};

// Project networks are never opened up by net-admin grants; sharing them
// takes the two-sided offer/accept.
@id("base.project-network-private")
forbid (principal, action == Glidex::Action::"grantNetwork", resource)
when { resource has project };

@id("base.project-network-only")
forbid (principal, action in [Glidex::Action::"deleteProjectNetwork",
                              Glidex::Action::"offerNetworkShare",
                              Glidex::Action::"unshareNetwork"], resource)
unless { resource has project };

@id("base.share-project-network-only")
forbid (principal, action in [Glidex::Action::"acceptNetworkShare",
                              Glidex::Action::"leaveNetworkShare"], resource)
unless { context.network has project };

// Offering a network to its own project means nothing.
@id("base.share-elsewhere")
forbid (principal, action == Glidex::Action::"offerNetworkShare", resource)
when { resource has project && resource.project == context.project };

// The shared image library, network names and datastores are visible to
// every user.
@id("base.read-shared")
permit (principal, action in [Glidex::Action::"readImage", Glidex::Action::"readNetwork", Glidex::Action::"readDatastore"], resource);

// Every user may put disks on any datastore; a site policy can forbid one
// to some (spec/storage.md §13).
@id("base.use-datastore")
permit (principal, action == Glidex::Action::"useDatastore", resource);

// Users manage their own sign-in methods (with a recent login: step-up).
@id("base.own-sign-ins")
permit (principal is Glidex::User, action == Glidex::Action::"manageOwnSignIns", resource)
when { resource == principal };

// Users manage their own tokens.
@id("base.own-tokens")
permit (principal is Glidex::User, action == Glidex::Action::"manageOwnTokens", resource)
when { resource == principal };

// ---- tenants, operators and directories (spec/tenancy.md §10.2) ----------

// D1, D15: a tenant's resources are its members'. Members of the
// directory's owner tenant may be linked in (delegation); members of the
// cluster's operator reach what is on their cluster (support).
@id("base.tenant-boundary")
forbid (principal, action in Glidex::Action::"tenant-scoped", resource)
unless {
  context has tenant && (
    principal in context.tenant ||
    principal in context.tenant.directory.owner ||
    (context has cluster && principal in context.cluster.operator && resource in context.cluster))
};

// D4: a cluster's hosts belong to its operator tenant.
@id("base.host-operator")
forbid (principal, action in Glidex::Action::"host-scoped", resource)
unless { context has cluster && principal in context.cluster.operator };

// D7: a directory belongs to its owner tenant.
@id("base.directory-owner")
forbid (principal, action in Glidex::Action::"directory-scoped", resource)
unless { context has directory && principal in context.directory.owner };

// D9: resources are allocated on a cluster only for a tenant it admits
// and that opted in to it.
@id("base.admission")
forbid (principal, action == Glidex::Action::"useCluster", resource)
unless {
  resource.admitted.contains(context.project.tenant) &&
  context.project.tenant.clusters.contains(resource)
};

// Who may allocate is decided by createVm & co.; useCluster only checks
// the place.
@id("base.cluster-use")
permit (principal, action == Glidex::Action::"useCluster", resource);

// D25: project networks are shared within one tenant only.
@id("base.share-same-tenant")
forbid (principal, action == Glidex::Action::"offerNetworkShare", resource)
when { resource has project && resource.project.tenant != context.project.tenant };

// A suspended tenant's members can't act on its resources.
@id("base.tenant-suspended")
forbid (principal, action in Glidex::Action::"tenant-scoped", resource)
when { context has tenant && context.tenant.suspended && principal in context.tenant };

// Break-glass: root and glidex-admin over api.sock (spec §5.2).
@id("base.break-glass")
permit (principal in Glidex::Team::"unix:glidex-admin", action, resource);
