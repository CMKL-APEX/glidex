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
when { resource.grants.contains(context.project) };

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

// The shared image library and network names are visible to every user.
@id("base.read-shared")
permit (principal, action in [Glidex::Action::"readImage", Glidex::Action::"readNetwork"], resource);

// Users manage their own tokens.
@id("base.own-tokens")
permit (principal is Glidex::User, action == Glidex::Action::"manageOwnTokens", resource)
when { resource == principal };

// Break-glass: root and glidex-admin over api.sock (spec §5.2).
@id("base.break-glass")
permit (principal in Glidex::Team::"unix:glidex-admin", action, resource);
