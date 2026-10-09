//! The web UI checks a fixed list of host-wide capabilities at login
//! (`crates/glidex-ui/ui/src/session.tsx`, `HOST_ACTION_RESOURCE`), each on
//! the resource type listed there.
//! A check on a type the schema's `appliesTo` doesn't list is never allowed,
//! so the UI silently hides pages and buttons when the two drift (as when the
//! cluster-wide actions moved from `Host` to `Cluster`).

use std::collections::HashMap;
use std::path::Path;

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// action → its `appliesTo` resource types, from the Cedar schema.
fn schema_resources() -> HashMap<String, Vec<String>> {
    let schema = read("policies/glidex.cedarschema");
    let mut out = HashMap::new();
    let mut rest = schema.as_str();
    while let Some(i) = rest.find("action ") {
        rest = &rest[i + "action ".len()..];
        let Some(end) = rest.find(';') else { break };
        let decl = &rest[..end];
        rest = &rest[end..];
        let names = decl.split(" in ").next().unwrap_or("").split("appliesTo").next().unwrap_or("");
        let Some(r) = decl.find("resource:") else { continue };
        let list = &decl[r + "resource:".len()..];
        let (Some(a), Some(b)) = (list.find('['), list.find(']')) else { continue };
        let types: Vec<String> = list[a + 1..b].split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        for n in names.split(',') {
            let n = n.trim().trim_matches('"');
            if !n.is_empty() {
                out.insert(n.to_string(), types.clone());
            }
        }
    }
    out
}

/// The UI session's `HOST_ACTION_RESOURCE`: (action, resource type).
fn ui_host_actions() -> Vec<(String, String)> {
    let src = read("../glidex-ui/ui/src/session.tsx");
    let start = src.find("const HOST_ACTION_RESOURCE = {").expect("HOST_ACTION_RESOURCE in session.tsx");
    let body = &src[start..];
    let body = &body[body.find('{').unwrap() + 1..body.find("} as const").expect("HOST_ACTION_RESOURCE ends with `} as const`")];
    body.split(',')
        .filter_map(|e| e.split_once(':'))
        .map(|(a, t)| (a.trim().to_string(), t.trim().trim_matches('"').to_string()))
        .collect()
}

#[test]
fn every_ui_host_capability_is_checked_on_a_resource_its_action_applies_to() {
    let schema = schema_resources();
    let actions = ui_host_actions();
    assert!(actions.len() >= 10, "parsed too few UI host actions: {actions:?}");
    for (a, checked_on) in &actions {
        let applies = schema.get(a).unwrap_or_else(|| panic!("UI host action {a} is not in the Cedar schema"));
        assert!(
            applies.iter().any(|t| t == checked_on),
            "the UI checks {a} on {checked_on}, but the schema applies it to {applies:?}: fix HOST_ACTION_RESOURCE in session.tsx"
        );
    }
}
