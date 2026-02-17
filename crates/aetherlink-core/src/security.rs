use std::collections::HashMap;

use aetherlink_proto::v1::SessionRequest;
use libp2p::{PeerId, identity};
use prost::Message;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MIN_NONCE_BYTES: usize = 12;
pub const DEFAULT_ALLOWED_SKEW_MS: i64 = 30_000;
pub const DEFAULT_REPLAY_RETENTION_MS: i64 = 60_000;
pub const LAST_SEEN_PERSIST_INTERVAL_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSessionPeer {
    pub peer_id: PeerId,
    pub device_code: String,
    pub trust_store_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeerRecord {
    pub device_code: String,
    pub peer_id: String,
    pub identity_pubkey_hex: String,
    pub first_seen_unix_ms: i64,
    pub last_seen_unix_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct TrustedPeers {
    by_device_code: HashMap<String, TrustedPeerRecord>,
}

impl TrustedPeers {
    pub fn from_records(records: Vec<TrustedPeerRecord>) -> Result<Self, SessionAuthError> {
        let mut by_device_code = HashMap::new();
        for record in records {
            if record.device_code.trim().is_empty() {
                return Err(SessionAuthError::TrustStoreCorrupt(
                    "empty device_code in trust record".to_string(),
                ));
            }
            parse_peer_id(&record.peer_id)?;
            decode_hex(&record.identity_pubkey_hex)?;
            by_device_code.insert(record.device_code.clone(), record);
        }
        Ok(Self { by_device_code })
    }

    pub fn to_records(&self) -> Vec<TrustedPeerRecord> {
        let mut records: Vec<_> = self.by_device_code.values().cloned().collect();
        records.sort_by(|a, b| a.device_code.cmp(&b.device_code));
        records
    }

    pub fn len(&self) -> usize {
        self.by_device_code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_device_code.is_empty()
    }

    fn ensure_trusted(
        &mut self,
        device_code: &str,
        peer_id: &PeerId,
        identity_pubkey: &[u8],
        now_unix_ms: i64,
        trust_on_first_use: bool,
    ) -> Result<bool, SessionAuthError> {
        let current_peer_id = peer_id.to_string();
        let current_pubkey_hex = encode_hex(identity_pubkey);

        if let Some(existing) = self.by_device_code.get_mut(device_code) {
            if existing.peer_id != current_peer_id
                || existing.identity_pubkey_hex != current_pubkey_hex
            {
                return Err(SessionAuthError::TrustedPeerMismatch {
                    device_code: device_code.to_string(),
                });
            }
            let changed = now_unix_ms.saturating_sub(existing.last_seen_unix_ms)
                >= LAST_SEEN_PERSIST_INTERVAL_MS;
            if changed {
                existing.last_seen_unix_ms = now_unix_ms;
            }
            return Ok(changed);
        }

        if !trust_on_first_use {
            return Err(SessionAuthError::UntrustedPeer {
                device_code: device_code.to_string(),
            });
        }

        self.by_device_code.insert(
            device_code.to_string(),
            TrustedPeerRecord {
                device_code: device_code.to_string(),
                peer_id: current_peer_id,
                identity_pubkey_hex: current_pubkey_hex,
                first_seen_unix_ms: now_unix_ms,
                last_seen_unix_ms: now_unix_ms,
            },
        );
        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct NonceReplayCache {
    retention_ms: i64,
    seen: HashMap<Vec<u8>, i64>,
}

impl Default for NonceReplayCache {
    fn default() -> Self {
        Self::new(DEFAULT_REPLAY_RETENTION_MS)
    }
}

impl NonceReplayCache {
    pub fn new(retention_ms: i64) -> Self {
        Self {
            retention_ms: retention_ms.max(1),
            seen: HashMap::new(),
        }
    }

    pub fn check_and_store(
        &mut self,
        nonce: &[u8],
        now_unix_ms: i64,
    ) -> Result<(), SessionAuthError> {
        self.evict_expired(now_unix_ms);
        if self.seen.contains_key(nonce) {
            return Err(SessionAuthError::ReplayDetected);
        }
        self.seen.insert(nonce.to_vec(), now_unix_ms);
        Ok(())
    }

    fn evict_expired(&mut self, now_unix_ms: i64) {
        let retention = self.retention_ms;
        self.seen.retain(|_, seen_unix_ms| {
            if *seen_unix_ms > now_unix_ms {
                true
            } else {
                now_unix_ms - *seen_unix_ms <= retention
            }
        });
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionAuthError {
    #[error("missing sender identity in SessionRequest.from")]
    MissingSenderIdentity,
    #[error("missing device_code in sender identity")]
    MissingDeviceCode,
    #[error("missing nonce")]
    MissingNonce,
    #[error("nonce too short: expected at least {min_bytes} bytes")]
    NonceTooShort { min_bytes: usize },
    #[error("invalid target device code: expected {expected}, got {got}")]
    InvalidTargetDeviceCode { expected: String, got: String },
    #[error(
        "request timestamp outside allowed skew ({allowed_skew_ms} ms): request={request_unix_ms}, now={now_unix_ms}"
    )]
    TimestampSkew {
        request_unix_ms: i64,
        now_unix_ms: i64,
        allowed_skew_ms: i64,
    },
    #[error("replay detected for nonce")]
    ReplayDetected,
    #[error("invalid sender public key encoding")]
    InvalidSenderPublicKey,
    #[error("invalid sender peer id encoding")]
    InvalidSenderPeerId,
    #[error("sender peer id does not match public key")]
    PeerIdMismatch,
    #[error("transport peer id does not match signed identity")]
    TransportPeerIdMismatch,
    #[error("invalid session request signature")]
    InvalidSignature,
    #[error("signing session request failed")]
    SigningFailed,
    #[error("peer is not trusted and trust-on-first-use is disabled: {device_code}")]
    UntrustedPeer { device_code: String },
    #[error("trusted peer mismatch for device code {device_code}")]
    TrustedPeerMismatch { device_code: String },
    #[error("trust store is corrupted: {0}")]
    TrustStoreCorrupt(String),
}

pub fn sign_session_request(
    request: &mut SessionRequest,
    keypair: &identity::Keypair,
) -> Result<(), SessionAuthError> {
    request.signature.clear();
    let signing_payload = canonical_session_request_payload(request);
    request.signature = keypair
        .sign(&signing_payload)
        .map_err(|_| SessionAuthError::SigningFailed)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn verify_session_request(
    request: &SessionRequest,
    transport_peer_id: Option<&PeerId>,
    expected_target_device_code: Option<&str>,
    now_unix_ms: i64,
    allowed_skew_ms: i64,
    replay_cache: &mut NonceReplayCache,
    trusted_peers: &mut TrustedPeers,
    trust_on_first_use: bool,
) -> Result<VerifiedSessionPeer, SessionAuthError> {
    let from = request
        .from
        .as_ref()
        .ok_or(SessionAuthError::MissingSenderIdentity)?;

    if from.device_code.trim().is_empty() {
        return Err(SessionAuthError::MissingDeviceCode);
    }

    if let Some(expected_target) = expected_target_device_code
        && request.target_device_code != expected_target
    {
        return Err(SessionAuthError::InvalidTargetDeviceCode {
            expected: expected_target.to_string(),
            got: request.target_device_code.clone(),
        });
    }

    if request.nonce.is_empty() {
        return Err(SessionAuthError::MissingNonce);
    }
    if request.nonce.len() < MIN_NONCE_BYTES {
        return Err(SessionAuthError::NonceTooShort {
            min_bytes: MIN_NONCE_BYTES,
        });
    }

    if (now_unix_ms - request.unix_ms).abs() > allowed_skew_ms {
        return Err(SessionAuthError::TimestampSkew {
            request_unix_ms: request.unix_ms,
            now_unix_ms,
            allowed_skew_ms,
        });
    }
    replay_cache.check_and_store(&request.nonce, now_unix_ms)?;

    let sender_public_key = identity::PublicKey::try_decode_protobuf(&from.identity_pubkey)
        .map_err(|_| SessionAuthError::InvalidSenderPublicKey)?;

    let signing_payload = canonical_session_request_payload(request);
    if !sender_public_key.verify(&signing_payload, &request.signature) {
        return Err(SessionAuthError::InvalidSignature);
    }

    let claimed_peer_id =
        PeerId::from_bytes(&from.peer_id).map_err(|_| SessionAuthError::InvalidSenderPeerId)?;
    let derived_peer_id = PeerId::from_public_key(&sender_public_key);
    if claimed_peer_id != derived_peer_id {
        return Err(SessionAuthError::PeerIdMismatch);
    }

    if let Some(bound_peer_id) = transport_peer_id
        && *bound_peer_id != derived_peer_id
    {
        return Err(SessionAuthError::TransportPeerIdMismatch);
    }

    let trust_store_changed = trusted_peers.ensure_trusted(
        &from.device_code,
        &derived_peer_id,
        &from.identity_pubkey,
        now_unix_ms,
        trust_on_first_use,
    )?;

    Ok(VerifiedSessionPeer {
        peer_id: derived_peer_id,
        device_code: from.device_code.clone(),
        trust_store_changed,
    })
}

fn canonical_session_request_payload(request: &SessionRequest) -> Vec<u8> {
    let mut stripped = request.clone();
    stripped.signature.clear();
    stripped.encode_to_vec()
}

fn parse_peer_id(text: &str) -> Result<PeerId, SessionAuthError> {
    text.parse::<PeerId>()
        .map_err(|_| SessionAuthError::TrustStoreCorrupt(format!("invalid peer id: {text}")))
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(hex_nibble_to_char(b >> 4));
        out.push(hex_nibble_to_char(b & 0x0f));
    }
    out
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, SessionAuthError> {
    if !hex.len().is_multiple_of(2) {
        return Err(SessionAuthError::TrustStoreCorrupt(
            "hex string has odd length".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = char_to_hex_nibble(bytes[i] as char)?;
        let lo = char_to_hex_nibble(bytes[i + 1] as char)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble_to_char(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        10..=15 => (b'a' + (v - 10)) as char,
        _ => '0',
    }
}

fn char_to_hex_nibble(c: char) -> Result<u8, SessionAuthError> {
    match c {
        '0'..='9' => Ok((c as u8) - b'0'),
        'a'..='f' => Ok((c as u8) - b'a' + 10),
        'A'..='F' => Ok((c as u8) - b'A' + 10),
        _ => Err(SessionAuthError::TrustStoreCorrupt(format!(
            "invalid hex char {c}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aetherlink_proto::v1::{DeviceIdentity, ProtocolVersion, SessionRole, VideoCodec};

    fn make_signed_request(
        keypair: &identity::Keypair,
        target_device_code: &str,
        nonce: &[u8],
        unix_ms: i64,
    ) -> SessionRequest {
        let mut req = SessionRequest {
            session_id: "session-test".to_string(),
            from: Some(DeviceIdentity {
                peer_id: PeerId::from(keypair.public()).to_bytes(),
                identity_pubkey: keypair.public().encode_protobuf(),
                device_code: PeerId::from(keypair.public()).to_string(),
            }),
            requested_role: SessionRole::Controller as i32,
            target_device_code: target_device_code.to_string(),
            supported_video_codecs: vec![VideoCodec::H264 as i32],
            allow_relay: true,
            preferred_max_fps: 30,
            preferred_max_width: 1280,
            preferred_max_height: 720,
            nonce: nonce.to_vec(),
            unix_ms,
            signature: Vec::new(),
            version: Some(ProtocolVersion {
                major: 1,
                minor: 0,
                patch: 0,
            }),
        };
        sign_session_request(&mut req, keypair).unwrap();
        req
    }

    #[test]
    fn sign_and_verify_success_with_tofu() {
        let key = identity::Keypair::generate_ed25519();
        let peer_id = PeerId::from(key.public());
        let mut req = make_signed_request(&key, "target-a", b"0123456789abcdef", 1_000_000);

        let mut replay = NonceReplayCache::default();
        let mut trust = TrustedPeers::default();
        let verified = verify_session_request(
            &req,
            Some(&peer_id),
            Some("target-a"),
            1_000_100,
            DEFAULT_ALLOWED_SKEW_MS,
            &mut replay,
            &mut trust,
            true,
        )
        .unwrap();
        assert_eq!(verified.peer_id, peer_id);
        assert!(verified.trust_store_changed);
        assert_eq!(trust.len(), 1);

        req.nonce = b"abcdef0123456789".to_vec();
        req.unix_ms = 1_000_200;
        sign_session_request(&mut req, &key).unwrap();
        let verified = verify_session_request(
            &req,
            Some(&peer_id),
            Some("target-a"),
            1_000_250,
            DEFAULT_ALLOWED_SKEW_MS,
            &mut replay,
            &mut trust,
            true,
        )
        .unwrap();
        assert!(!verified.trust_store_changed);
    }

    #[test]
    fn replay_is_rejected() {
        let key = identity::Keypair::generate_ed25519();
        let peer_id = PeerId::from(key.public());
        let req = make_signed_request(&key, "target-a", b"0123456789abcdef", 1_000_000);
        let mut replay = NonceReplayCache::default();
        let mut trust = TrustedPeers::default();

        verify_session_request(
            &req,
            Some(&peer_id),
            Some("target-a"),
            1_000_100,
            DEFAULT_ALLOWED_SKEW_MS,
            &mut replay,
            &mut trust,
            true,
        )
        .unwrap();

        let err = verify_session_request(
            &req,
            Some(&peer_id),
            Some("target-a"),
            1_000_200,
            DEFAULT_ALLOWED_SKEW_MS,
            &mut replay,
            &mut trust,
            true,
        )
        .unwrap_err();
        assert_eq!(err, SessionAuthError::ReplayDetected);
    }

    #[test]
    fn untrusted_peer_rejected_when_tofu_disabled() {
        let key = identity::Keypair::generate_ed25519();
        let peer_id = PeerId::from(key.public());
        let req = make_signed_request(&key, "target-a", b"0123456789abcdef", 1_000_000);
        let mut replay = NonceReplayCache::default();
        let mut trust = TrustedPeers::default();

        let err = verify_session_request(
            &req,
            Some(&peer_id),
            Some("target-a"),
            1_000_100,
            DEFAULT_ALLOWED_SKEW_MS,
            &mut replay,
            &mut trust,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, SessionAuthError::UntrustedPeer { .. }));
    }
}
