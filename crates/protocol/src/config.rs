//! The user's Argon configuration.

use serde::{Deserialize, Serialize};

/// The log level used when the configuration names none.
pub const DEFAULT_LOG_LEVEL: &str = "error";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArgonConfig {
    pub analyzer: AnalyzerConfig,
    pub agent: AgentConfig,
    pub gui: GuiConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub approval: ApprovalMode,
}

/// Whether the user confirms each agent edit in the editor before it is applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Agent edits apply immediately and the editor is never asked to confirm.
    #[default]
    Never,
    /// The editor shows each agent edit and applies it only if the user accepts.
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzerConfig {
    pub compile_debounce_ms: u64,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            compile_debounce_ms: 150,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuiConfig {
    pub dark_mode: bool,
    /// Maximum rendered hierarchy depth. Omitted means unlimited.
    pub hierarchy_depth: Option<usize>,
    /// Global GUI icon-size override in logical pixels.
    #[serde(default, deserialize_with = "deserialize_gui_size")]
    pub icon_size: Option<f32>,
    /// Global GUI font-size override in logical pixels.
    #[serde(default, deserialize_with = "deserialize_gui_size")]
    pub font_size: Option<f32>,
    /// Whether Option acts as Meta in the GUI's Neovim pane on macOS.
    pub option_as_meta: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            dark_mode: true,
            hierarchy_depth: None,
            icon_size: None,
            font_size: None,
            option_as_meta: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum HumanReadableGuiSize {
    Integer(u16),
    Float(f32),
    None(()),
}

fn deserialize_gui_size<'de, D>(deserializer: D) -> std::result::Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let size = if deserializer.is_human_readable() {
        match HumanReadableGuiSize::deserialize(deserializer)? {
            HumanReadableGuiSize::Integer(size) => Some(f32::from(size)),
            HumanReadableGuiSize::Float(size) => Some(size),
            HumanReadableGuiSize::None(()) => None,
        }
    } else {
        Option::<f32>::deserialize(deserializer)?
    };
    if size.is_some_and(|size| !size.is_finite() || !(1.0..=256.0).contains(&size)) {
        return Err(<D::Error as serde::de::Error>::custom(
            "GUI sizes must be between 1 and 256 logical pixels",
        ));
    }
    Ok(size)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL.to_owned(),
        }
    }
}
