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

    /// Window function: 0=Hann, 1=Blackman-Harris, 2=Kaiser
    #[id = "window_fn"]
    pub window_fn: IntParam,
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
                144,
                IntRange::Linear { min: 60, max: 144 },
            )
            .with_unit(" Hz")
            .with_value_to_string(Arc::new(|value| format!("{}", value)))
            .with_string_to_value(Arc::new(|string: &str| string.parse().ok())),
            window_fn: IntParam::new(
                "Window",
                0,
                IntRange::Linear { min: 0, max: 2 },
            )
            .with_unit("")
            .with_value_to_string(Arc::new(|value| {
                match value {
                    0 => "Hann".to_string(),
                    1 => "Blackman-Harris".to_string(),
                    2 => "Kaiser".to_string(),
                    _ => "Hann".to_string(),
                }
            }))
            .with_string_to_value(Arc::new(|string: &str| {
                match string.to_lowercase().as_str() {
                    "hann" => Some(0),
                    "blackman-harris" | "blackman" => Some(1),
                    "kaiser" => Some(2),
                    _ => None,
                }
            })),
        }
    }
}
