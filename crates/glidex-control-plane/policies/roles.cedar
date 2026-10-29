// Role templates (spec/11-security.md §7.3). A role assignment is a link of
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

// Tenant roles link to a Tenant, directory roles to a Directory
// (spec/19-tenancy.md §10.5).
@id("role.tenant-admin")
permit (principal in ?principal, action in Glidex::Action::"role.tenant-admin", resource in ?resource);

@id("role.tenant-auditor")
permit (principal in ?principal, action in Glidex::Action::"role.tenant-auditor", resource in ?resource);

@id("role.directory-admin")
permit (principal in ?principal, action in Glidex::Action::"role.directory-admin", resource in ?resource);

@id("role.directory-auditor")
permit (principal in ?principal, action in Glidex::Action::"role.directory-auditor", resource in ?resource);
