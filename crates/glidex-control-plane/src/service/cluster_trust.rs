//! `glidex.directory.v1.ClusterTrustService`: clusters of other
//! organizations the caller's directory trusts as a whole
//! (spec/19-tenancy.md §12.6, D92, D94). Its owner redeems a cluster's
//! directory offer; the directory's tenants then opt in to it without an
//! offer of their own (D93).

use super::tenancy::{cluster_record, redeem_offer};
use super::{Call, ServiceError, ServiceResult};
use crate::trust::{AdmitSetting, TrustedCluster};
use axum::http::StatusCode;
use glidex_api::proto::glidex::directory::v1 as pb;

fn ts(secs: u64) -> buffa_types::google::protobuf::Timestamp {
    buffa_types::google::protobuf::Timestamp::from_unix_secs(secs as i64)
}

fn trust_err(e: crate::trust::StoreError) -> ServiceError {
    ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

fn admit_pb(a: Option<AdmitSetting>) -> pb::AdmitSetting {
    match a {
        Some(AdmitSetting::Open) => pb::AdmitSetting::ADMIT_SETTING_OPEN,
        Some(AdmitSetting::Approval) => pb::AdmitSetting::ADMIT_SETTING_APPROVAL,
        None => pb::AdmitSetting::ADMIT_SETTING_UNSPECIFIED,
    }
}

/// Cluster `c` as its directory sees it: its tenants with their slugs.
fn cluster_pb(call: &Call, c: &TrustedCluster) -> pb::DirectoryCluster {
    let tenants = c
        .tenants
        .iter()
        .map(|id| pb::DirectoryClusterTenant {
            id: id.clone(),
            slug: call
                .auth()
                .tenants
                .get(id)
                .ok()
                .flatten()
                .map(|t| t.slug)
                .unwrap_or_default(),
            status: pb::ClusterAdmission::CLUSTER_ADMISSION_ADMITTED.into(),
            ..Default::default()
        })
        .collect();
    pb::DirectoryCluster {
        id: c.cluster_id.clone(),
        name: c.name.clone(),
        api: c.api.clone(),
        whole: c.whole.is_some(),
        admit: admit_pb(c.whole).into(),
        tenants,
        created_at: ts(c.created_at).into(),
        ..Default::default()
    }
}

pub fn list_directory_clusters(
    call: &Call,
    _req: pb::ListDirectoryClustersRequest,
) -> ServiceResult<pb::ListDirectoryClustersResponse> {
    call.require(call.directory_ent(), crate::authz::EntitySet::new())?;
    let all = call
        .auth()
        .trust
        .clusters(call.p.directory_id())
        .map_err(trust_err)?;
    Ok(pb::ListDirectoryClustersResponse {
        clusters: all.iter().map(|c| cluster_pb(call, c)).collect(),
        ..Default::default()
    })
}

/// Redeem a cluster's directory offer (D92): the cluster trusts this
/// directory as a whole. A cluster it already trusts through tenant
/// offers keeps its record and tenants, and becomes directory-wide.
pub async fn trust_cluster(
    call: &Call,
    req: pb::TrustClusterRequest,
) -> ServiceResult<pb::TrustClusterResponse> {
    call.require(call.directory_ent(), crate::authz::EntitySet::new())?;
    let dir = call.p.directory_id().to_string();
    let (token, answer) = redeem_offer(call, &dir, &req.offer, None).await?;
    let mut c = cluster_record(call, &dir, &token, &answer)?;
    // A cluster that answers with no setting admits by default (D93).
    c.whole = Some(serde_json::from_value(answer["admit"].clone()).unwrap_or(AdmitSetting::Open));
    call.auth().trust.put_cluster(&c).map_err(trust_err)?;
    call.set_target(format!("directory:{dir}"));
    call.detail("cluster", serde_json::json!(c.cluster_id));
    Ok(pb::TrustClusterResponse {
        cluster: cluster_pb(call, &c).into(),
        ..Default::default()
    })
}

/// End this directory's trust with cluster `cluster` as a whole (D94):
/// forgotten here with every tenant's opt-in, and the cluster told (best
/// effort; it also finds out when it is next refused).
pub async fn untrust_cluster(
    call: &Call,
    req: pb::UntrustClusterRequest,
) -> ServiceResult<pb::UntrustClusterResponse> {
    call.require(call.directory_ent(), crate::authz::EntitySet::new())?;
    let dir = call.p.directory_id().to_string();
    let c = call
        .auth()
        .trust
        .cluster(&dir, &req.cluster)
        .map_err(trust_err)?
        .ok_or_else(|| {
            ServiceError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "this directory doesn't trust that cluster",
            )
        })?;
    call.auth()
        .trust
        .remove_cluster(&dir, &c.cluster_id)
        .map_err(trust_err)?;
    call.set_target(format!("directory:{dir}"));
    call.detail("cluster", serde_json::json!(c.cluster_id));
    let iss = call
        .auth()
        .tenants
        .get_directory(&dir)
        .ok()
        .flatten()
        .and_then(|d| d.issuer);
    if let Some(iss) = iss
        && let Ok(a) = crate::directory::peer::assertion(
            &call.auth().connectors,
            &dir,
            &iss,
            &c.cluster_id,
            crate::trust::END,
            crate::tenancy::now(),
        )
        // No tenant: the whole directory (D94); either pin (D88).
        && let Err(e) = crate::directory::peer::post_any(
            &c.api,
            &c.pins(),
            "/trust/v1/end",
            &a,
            &serde_json::json!({ "issuer": iss, "tenant": "" }),
        )
        .await
    {
        tracing::warn!(cluster = %c.cluster_id, "telling the cluster trust ended: {}", e);
    }
    Ok(pb::UntrustClusterResponse::default())
}
