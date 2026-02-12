//! Plugin parameters for Hardwave Bridge

use nih_plug::prelude::*;
use std::sync::Arc;

/// Plugin parameters
#[derive(Params)]
pub struct HardwaveBridgeParams {
    /// Enable/disable streaming
    #[id = "enabled"]
    pub enabled: BoolParam,

    /// WebSocket server port
    #[id = "port"]
    pub port: IntParam,
}

impl Default for HardwaveBridgeParams {
    fn default() -> Self {
        Self {
            enabled: BoolParam::new("Enabled", true),
            port: IntParam::new(
                "Port",
                9847,
                IntRange::Linear {
                    min: 1024,
                    max: 65535,
                },
            )
            .with_unit(" ")
            .with_value_to_string(Arc::new(|value| format!("{}", value)))
            .with_string_to_value(Arc::new(|string: &str| string.parse().ok())),
        }
    }
}
