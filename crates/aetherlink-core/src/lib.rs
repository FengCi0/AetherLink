#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingProfile {
    pub discovery_timeout_ms: u64,
    pub direct_dial_budget_ms: u64,
    pub punch_budget_ms: u64,
    pub relay_dial_timeout_ms: u64,
    pub handshake_timeout_ms: u64,
    pub ping_interval_ms: u64,
    pub path_lost_threshold: u32,
    pub reconnect_budget_ms: u64,
    pub reconnect_backoff_start_ms: u64,
    pub reconnect_backoff_max_ms: u64,
}

impl Default for TimingProfile {
    fn default() -> Self {
        Self {
            discovery_timeout_ms: 2_500,
            direct_dial_budget_ms: 1_500,
            punch_budget_ms: 2_200,
            relay_dial_timeout_ms: 2_500,
            handshake_timeout_ms: 1_200,
            ping_interval_ms: 1_000,
            path_lost_threshold: 3,
            reconnect_budget_ms: 15_000,
            reconnect_backoff_start_ms: 200,
            reconnect_backoff_max_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    DiscoveryTimeout,
    RelayTimeout,
    AuthFailed,
    VersionMismatch,
    RetryBudgetExhausted,
    UserAbort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Discovering,
    DialingDirect,
    HolePunching,
    RelayDialing,
    SecureHandshake,
    Active,
    Reconnecting,
    Failed(FailureReason),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    StartConnect,
    CandidatesFound,
    DiscoveryTimeout,
    DirectConnected,
    DirectNoSuccess,
    PunchConnected,
    PunchTimeout,
    RelayConnected,
    RelayTimeout,
    HandshakeOk,
    AuthFailed,
    VersionMismatch,
    PathLost,
    RetryBudgetAvailable,
    RetryBudgetExhausted,
    UserRetry,
    UserHangup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    Discovery,
    DirectDial,
    HolePunch,
    RelayDial,
    Handshake,
    ReconnectBackoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from: ConnectionState,
    pub to: ConnectionState,
    pub arm_timer: Option<(TimerKind, u64)>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StateMachineError {
    #[error("invalid transition from {from:?} with trigger {trigger:?}")]
    InvalidTransition {
        from: ConnectionState,
        trigger: Trigger,
    },
}

#[derive(Debug, Clone)]
pub struct ConnectionStateMachine {
    state: ConnectionState,
    timing: TimingProfile,
    reconnect_elapsed_ms: u64,
    reconnect_attempts: u32,
}

impl Default for ConnectionStateMachine {
    fn default() -> Self {
        Self {
            state: ConnectionState::Idle,
            timing: TimingProfile::default(),
            reconnect_elapsed_ms: 0,
            reconnect_attempts: 0,
        }
    }
}

impl ConnectionStateMachine {
    pub fn new(timing: TimingProfile) -> Self {
        Self {
            timing,
            ..Self::default()
        }
    }

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    pub fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts
    }

    pub fn reconnect_elapsed_ms(&self) -> u64 {
        self.reconnect_elapsed_ms
    }

    pub fn register_backoff_wait(&mut self, backoff_ms: u64) {
        self.reconnect_attempts = self.reconnect_attempts.saturating_add(1);
        self.reconnect_elapsed_ms = self.reconnect_elapsed_ms.saturating_add(backoff_ms);
    }

    pub fn has_reconnect_budget(&self) -> bool {
        self.reconnect_elapsed_ms < self.timing.reconnect_budget_ms
    }

    pub fn next_backoff_ms(&self) -> u64 {
        let shift = self.reconnect_attempts.min(31);
        let base = self
            .timing
            .reconnect_backoff_start_ms
            .saturating_mul(1_u64 << shift);
        base.min(self.timing.reconnect_backoff_max_ms)
    }

    pub fn apply(&mut self, trigger: Trigger) -> Result<Transition, StateMachineError> {
        let from = self.state.clone();
        let (to, arm_timer) = match (&self.state, &trigger) {
            (ConnectionState::Idle, Trigger::StartConnect) => (
                ConnectionState::Discovering,
                Some((TimerKind::Discovery, self.timing.discovery_timeout_ms)),
            ),
            (ConnectionState::Discovering, Trigger::CandidatesFound) => (
                ConnectionState::DialingDirect,
                Some((TimerKind::DirectDial, self.timing.direct_dial_budget_ms)),
            ),
            (ConnectionState::Discovering, Trigger::DiscoveryTimeout) => {
                (ConnectionState::Failed(FailureReason::DiscoveryTimeout), None)
            }
            (ConnectionState::DialingDirect, Trigger::DirectConnected) => (
                ConnectionState::SecureHandshake,
                Some((TimerKind::Handshake, self.timing.handshake_timeout_ms)),
            ),
            (ConnectionState::DialingDirect, Trigger::DirectNoSuccess) => (
                ConnectionState::HolePunching,
                Some((TimerKind::HolePunch, self.timing.punch_budget_ms)),
            ),
            (ConnectionState::HolePunching, Trigger::PunchConnected) => (
                ConnectionState::SecureHandshake,
                Some((TimerKind::Handshake, self.timing.handshake_timeout_ms)),
            ),
            (ConnectionState::HolePunching, Trigger::PunchTimeout) => (
                ConnectionState::RelayDialing,
                Some((TimerKind::RelayDial, self.timing.relay_dial_timeout_ms)),
            ),
            (ConnectionState::RelayDialing, Trigger::RelayConnected) => (
                ConnectionState::SecureHandshake,
                Some((TimerKind::Handshake, self.timing.handshake_timeout_ms)),
            ),
            (ConnectionState::RelayDialing, Trigger::RelayTimeout) => {
                (ConnectionState::Failed(FailureReason::RelayTimeout), None)
            }
            (ConnectionState::SecureHandshake, Trigger::HandshakeOk) => {
                self.reconnect_attempts = 0;
                self.reconnect_elapsed_ms = 0;
                (ConnectionState::Active, None)
            }
            (ConnectionState::SecureHandshake, Trigger::AuthFailed) => {
                (ConnectionState::Failed(FailureReason::AuthFailed), None)
            }
            (ConnectionState::SecureHandshake, Trigger::VersionMismatch) => {
                (ConnectionState::Failed(FailureReason::VersionMismatch), None)
            }
            (ConnectionState::Active, Trigger::PathLost) => {
                let wait = self.next_backoff_ms();
                self.register_backoff_wait(wait);
                (
                    ConnectionState::Reconnecting,
                    Some((TimerKind::ReconnectBackoff, wait)),
                )
            }
            (ConnectionState::Reconnecting, Trigger::RetryBudgetAvailable)
                if self.has_reconnect_budget() =>
            {
                (
                    ConnectionState::DialingDirect,
                    Some((TimerKind::DirectDial, self.timing.direct_dial_budget_ms)),
                )
            }
            (ConnectionState::Reconnecting, Trigger::RetryBudgetExhausted)
            | (ConnectionState::Reconnecting, Trigger::RetryBudgetAvailable)
                if !self.has_reconnect_budget() =>
            {
                (
                    ConnectionState::Failed(FailureReason::RetryBudgetExhausted),
                    None,
                )
            }
            (ConnectionState::Failed(_), Trigger::UserRetry) => (
                ConnectionState::Idle,
                // Retry from Idle keeps old telemetry but resets active attempt.
                None,
            ),
            (ConnectionState::Active, Trigger::UserHangup)
            | (ConnectionState::Reconnecting, Trigger::UserHangup)
            | (ConnectionState::SecureHandshake, Trigger::UserHangup)
            | (ConnectionState::DialingDirect, Trigger::UserHangup)
            | (ConnectionState::HolePunching, Trigger::UserHangup)
            | (ConnectionState::RelayDialing, Trigger::UserHangup)
            | (ConnectionState::Discovering, Trigger::UserHangup) => (ConnectionState::Closed, None),
            _ => {
                return Err(StateMachineError::InvalidTransition {
                    from,
                    trigger: trigger.clone(),
                });
            }
        };

        self.state = to.clone();
        Ok(Transition { from, to, arm_timer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_direct_to_active() {
        let mut sm = ConnectionStateMachine::default();

        sm.apply(Trigger::StartConnect).unwrap();
        sm.apply(Trigger::CandidatesFound).unwrap();
        sm.apply(Trigger::DirectConnected).unwrap();
        sm.apply(Trigger::HandshakeOk).unwrap();

        assert_eq!(sm.state(), &ConnectionState::Active);
        assert_eq!(sm.reconnect_attempts(), 0);
        assert_eq!(sm.reconnect_elapsed_ms(), 0);
    }

    #[test]
    fn reconnect_budget_resets_after_successful_handshake() {
        let mut sm = ConnectionStateMachine::new(TimingProfile {
            reconnect_budget_ms: 500,
            reconnect_backoff_start_ms: 200,
            reconnect_backoff_max_ms: 300,
            ..TimingProfile::default()
        });

        sm.apply(Trigger::StartConnect).unwrap();
        sm.apply(Trigger::CandidatesFound).unwrap();
        sm.apply(Trigger::DirectConnected).unwrap();
        sm.apply(Trigger::HandshakeOk).unwrap();
        assert_eq!(sm.state(), &ConnectionState::Active);

        sm.apply(Trigger::PathLost).unwrap();
        assert_eq!(sm.state(), &ConnectionState::Reconnecting);
        assert!(sm.has_reconnect_budget());
        assert_eq!(sm.reconnect_elapsed_ms(), 200);

        sm.apply(Trigger::RetryBudgetAvailable).unwrap();
        assert_eq!(sm.state(), &ConnectionState::DialingDirect);

        // Successful handshake resets reconnect budget counters.
        sm.apply(Trigger::DirectConnected).unwrap();
        sm.apply(Trigger::HandshakeOk).unwrap();
        assert_eq!(sm.reconnect_attempts(), 0);
        assert_eq!(sm.reconnect_elapsed_ms(), 0);
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let mut sm = ConnectionStateMachine::default();
        let err = sm.apply(Trigger::HandshakeOk).unwrap_err();
        assert!(matches!(
            err,
            StateMachineError::InvalidTransition {
                from: ConnectionState::Idle,
                trigger: Trigger::HandshakeOk
            }
        ));
    }
}
