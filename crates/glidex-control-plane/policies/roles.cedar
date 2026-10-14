// Role templates (spec/security.md §7.3). A role assignment is a link of
// one of these: ?principal is a User, Team or Token; ?resource is a
// Project (project roles), Cluster (system roles) or a Host (host-specific
// roles and grants).

@id("role.viewer")
permit (principal in ?principal, action in Glidex::Action::"role.viewer", resource in ?resource);

@id("role.operator")
permit (principal in ?principal, action in Glidex::Action::"role.operator", resource in ?resource);

@id("role.editor")
permit (principal in ?principal, action in Glidex::Action::"role.editor", resource in ?resource);

@id("role.owner")
permit (principal in ?principal, action in Glidex::Action::"role.owner", resource in ?resource);

@id("role.auditor")
permit (principal in ?principal, action in Glidex::Action::"role.auditor", resource in ?resource);

@id("role.image-admin")
permit (principal in ?principal, action in Glidex::Action::"role.image-admin", resource in ?resource);

@id("role.net-admin")
permit (principal in ?principal, action in Glidex::Action::"role.net-admin", resource in ?resource);

@id("role.system-admin")
permit (principal in ?principal, action in Glidex::Action::"role.system-admin", resource in ?resource);

@id("grant.host-paths")
permit (principal in ?principal, action in Glidex::Action::"host.paths", resource in ?resource);
