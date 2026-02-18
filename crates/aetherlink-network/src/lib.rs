#![forbid(unsafe_code)]

use aetherlink_core::TimingProfile;
use aetherlink_proto::v1::{PathType, SessionErrorCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateKind {
    DirectIpv6,
    DirectLan,
    ServerReflexive,
    Relay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub address: String,
    pub priority: u32,
    pub kind: CandidateKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialPhase {
    Direct,
    HolePunch,
    Relay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialPlan {
    pub phase: DialPhase,
    pub start_after_ms: u64,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PlannerError {
    #[error("candidate list is empty")]
    EmptyCandidates,
}

pub fn plan_dial_race(timing: &TimingProfile) -> [DialPlan; 3] {
    [
        DialPlan {
            phase: DialPhase::Direct,
            start_after_ms: 0,
            timeout_ms: timing.direct_dial_budget_ms,
        },
        DialPlan {
            phase: DialPhase::HolePunch,
            start_after_ms: 200,
            timeout_ms: timing.punch_budget_ms,
        },
        DialPlan {
            phase: DialPhase::Relay,
            start_after_ms: 1600,
            timeout_ms: timing.relay_dial_timeout_ms,
        },
    ]
}

pub fn select_primary_candidate(candidates: &[Candidate]) -> Result<&Candidate, PlannerError> {
    candidates
        .iter()
        .max_by_key(|item| {
            let kind_weight = match item.kind {
                CandidateKind::DirectIpv6 => 40_000_u64,
                CandidateKind::DirectLan => 30_000_u64,
                CandidateKind::ServerReflexive => 20_000_u64,
                CandidateKind::Relay => 10_000_u64,
            };
            kind_weight.saturating_add(item.priority as u64)
        })
        .ok_or(PlannerError::EmptyCandidates)
}

pub fn path_type_to_proto(path: DialPhase) -> PathType {
    match path {
        DialPhase::Direct => PathType::Direct,
        DialPhase::HolePunch => PathType::HolePunch,
        DialPhase::Relay => PathType::Relay,
    }
}

pub fn classify_failure_to_error_code(code: &str) -> SessionErrorCode {
    match code {
        "discovery_timeout" => SessionErrorCode::DiscoveryTimeout,
        "dial_timeout" => SessionErrorCode::DialTimeout,
        "punch_failed" => SessionErrorCode::HolePunchFailed,
        "relay_unavailable" => SessionErrorCode::RelayUnavailable,
        "auth_failed" => SessionErrorCode::AuthFailed,
        "permission_denied" => SessionErrorCode::PermissionDenied,
        _ => SessionErrorCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_direct_ipv6_over_relay() {
        let candidates = [
            Candidate {
                address: "relay://x".to_string(),
                priority: 500,
                kind: CandidateKind::Relay,
            },
            Candidate {
                address: "/ip6/::1/udp/9000/quic-v1".to_string(),
                priority: 1,
                kind: CandidateKind::DirectIpv6,
            },
        ];
        let best = select_primary_candidate(&candidates).unwrap();
        assert!(best.address.contains("/ip6/"));
    }

    #[test]
    fn dial_plan_has_expected_stages() {
        let plan = plan_dial_race(&TimingProfile::default());
        assert_eq!(plan[0].phase, DialPhase::Direct);
        assert_eq!(plan[1].phase, DialPhase::HolePunch);
        assert_eq!(plan[2].phase, DialPhase::Relay);
    }
}
