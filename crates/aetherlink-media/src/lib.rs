#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for VideoProfile {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 2500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkFeedback {
    pub rtt_ms: u32,
    pub packet_loss_x10000: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateLimits {
    pub floor_kbps: u32,
    pub ceil_kbps: u32,
}

impl Default for BitrateLimits {
    fn default() -> Self {
        Self {
            floor_kbps: 600,
            ceil_kbps: 8_000,
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("invalid bitrate limits")]
    InvalidLimits,
}

pub fn adaptive_bitrate_step(
    current_kbps: u32,
    feedback: NetworkFeedback,
    limits: BitrateLimits,
) -> Result<u32, MediaError> {
    if limits.floor_kbps == 0 || limits.floor_kbps > limits.ceil_kbps {
        return Err(MediaError::InvalidLimits);
    }
    let mut next = current_kbps.clamp(limits.floor_kbps, limits.ceil_kbps);
    if feedback.packet_loss_x10000 > 800 || feedback.rtt_ms > 200 {
        next = next.saturating_mul(85) / 100;
    } else if feedback.packet_loss_x10000 < 150 && feedback.rtt_ms < 80 {
        next = next.saturating_mul(108) / 100;
    }
    Ok(next.clamp(limits.floor_kbps, limits.ceil_kbps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decreases_bitrate_on_bad_network() {
        let next = adaptive_bitrate_step(
            3_000,
            NetworkFeedback {
                rtt_ms: 260,
                packet_loss_x10000: 1200,
            },
            BitrateLimits::default(),
        )
        .unwrap();
        assert!(next < 3_000);
    }

    #[test]
    fn increases_bitrate_on_good_network() {
        let next = adaptive_bitrate_step(
            1_200,
            NetworkFeedback {
                rtt_ms: 30,
                packet_loss_x10000: 20,
            },
            BitrateLimits::default(),
        )
        .unwrap();
        assert!(next > 1_200);
    }
}
