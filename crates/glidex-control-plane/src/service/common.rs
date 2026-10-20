//! Conversions to `glidex.common.v1`, for every family.

use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::common::v1 as pb;

pub fn ts(secs: u64) -> Timestamp {
    Timestamp::from_unix_secs(secs as i64)
}

/// A timestamp, or none for `0` (never).
pub fn ts_opt(secs: u64) -> Option<Timestamp> {
    (secs != 0).then(|| ts(secs))
}

pub fn arch_pb(a: crate::images::catalog::Arch) -> pb::Arch {
    use crate::images::catalog::Arch;
    match a {
        Arch::X86_64 => pb::Arch::ARCH_X86_64,
        Arch::Aarch64 => pb::Arch::ARCH_AARCH64,
    }
}

/// `None` for unspecified.
pub fn arch_of(a: pb::Arch) -> Option<crate::images::catalog::Arch> {
    use crate::images::catalog::Arch;
    match a {
        pb::Arch::ARCH_X86_64 => Some(Arch::X86_64),
        pb::Arch::ARCH_AARCH64 => Some(Arch::Aarch64),
        pb::Arch::ARCH_UNSPECIFIED => None,
    }
}

pub fn hypervisor_pb(h: crate::hypervisor::HypervisorType) -> pb::Hypervisor {
    use crate::hypervisor::HypervisorType;
    match h {
        HypervisorType::CloudHypervisor => pb::Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR,
        HypervisorType::Qemu => pb::Hypervisor::HYPERVISOR_QEMU,
    }
}

pub fn hypervisor_of(h: pb::Hypervisor) -> Option<crate::hypervisor::HypervisorType> {
    use crate::hypervisor::HypervisorType;
    match h {
        pb::Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR => Some(HypervisorType::CloudHypervisor),
        pb::Hypervisor::HYPERVISOR_QEMU => Some(HypervisorType::Qemu),
        pb::Hypervisor::HYPERVISOR_UNSPECIFIED => None,
    }
}

pub fn condition_pb(c: &crate::models::Condition) -> pb::Condition {
    use crate::models::Tristate;
    pb::Condition {
        kind: c.kind.clone(),
        status: match c.status {
            Tristate::True => pb::ConditionStatus::CONDITION_STATUS_TRUE,
            Tristate::False => pb::ConditionStatus::CONDITION_STATUS_FALSE,
            Tristate::Unknown => pb::ConditionStatus::CONDITION_STATUS_UNKNOWN,
        }
        .into(),
        reason: c.reason.clone(),
        message: c.message.clone(),
        last_transition_at: ts_opt(c.last_transition_at).into(),
        ..Default::default()
    }
}

pub fn event_pb(e: &crate::store::Event) -> pb::Event {
    use crate::store::EventKind;
    pb::Event {
        at: ts(e.at).into(),
        actor: e.actor.clone(),
        kind: match e.kind {
            EventKind::Normal => pb::EventKind::EVENT_KIND_NORMAL,
            EventKind::Warning => pb::EventKind::EVENT_KIND_WARNING,
        }
        .into(),
        reason: e.reason.clone(),
        message: e.message.clone(),
        ..Default::default()
    }
}

/// A Cedar entity as `{type, id}`.
pub fn entity_ref(e: &crate::authz::Ent) -> glidex_api::proto::glidex::api::v1::EntityRef {
    glidex_api::proto::glidex::api::v1::EntityRef {
        r#type: e.type_name().into(),
        id: Some(e.id().into_owned()),
        ..Default::default()
    }
}

/// Any Cedar entity from `{type, id}` (singletons without an id).
pub fn entity_of(
    r: &glidex_api::proto::glidex::api::v1::EntityRef,
) -> super::ServiceResult<crate::authz::Ent> {
    let mut v = serde_json::json!({ "type": r.r#type });
    if let Some(id) = &r.id {
        v["id"] = serde_json::json!(id);
    }
    serde_json::from_value(v).map_err(|_| {
        super::ServiceError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid",
            format!("no such entity type: {}", r.r#type),
        )
    })
}

/// A principal from `{type, id}`: a user, team or token.
pub fn principal_of(
    r: &glidex_api::proto::glidex::api::v1::EntityRef,
) -> super::ServiceResult<crate::authz::Ent> {
    use crate::authz::Ent;
    match entity_of(r)? {
        e @ (Ent::User(_) | Ent::Team(_) | Ent::Token(_)) => Ok(e),
        _ => Err(super::ServiceError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid",
            "principal must be a User, Team or Token with an id",
        )),
    }
}
