//! Plugin parameters for Hardwave Analyser

use nih_plug::prelude::*;
use std::sync::Arc;

/// Plugin parameters
#[derive(Params)]
pub struct HardwaveAnalyserParams {
    /// Enable/disable streaming
    #[id = "enabled"]
    pub enabled: BoolParam,

    /// WebSocket server port
    #[id = "port"]
    pub port: IntParam,

    /// FFT update rate sent to the analyser UI (60–144 Hz)
    #[id = "refresh_rate"]
    pub refresh_rate: IntParam,
}

impl Default for HardwaveAnalyserParams {
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
            refresh_rate: IntParam::new(
                "Refresh Rate",
                60,
                IntRange::Linear { min: 60, max: 144 },
            )
            .with_unit(" Hz")
            .with_value_to_string(Arc::new(|value| format!("{}", value)))
            .with_string_to_value(Arc::new(|string: &str| string.parse().ok())),
        }
    }
}
