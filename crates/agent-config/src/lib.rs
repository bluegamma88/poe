//! Configuration loading for poe.
//!
//! poe is named after Poe, the AI from Altered Carbon.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
};

pub const DEFAULT_MODEL: &str = "openai/gpt-oss-120b";
pub const DEFAULT_OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
pub const POE_DIR_NAME: &str = ".poe";
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const SESSIONS_DIR_NAME: &str = "sessions";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub model: String,
    /// Name of the environment variable holding the OpenRouter API key. Used
    /// when no literal `api_key` is configured.
    pub api_key_env: String,
    /// Literal OpenRouter API key read directly from config. When set, it takes
    /// precedence over `api_key_env`.
    pub api_key: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let home_dir = env::var_os("HOME").ok_or(ConfigError::MissingHomeDirectory)?;
        Self::load_from_home_dir(home_dir)
    }

    pub fn load_from_home_dir(home_dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = config_path_from_home_dir(home_dir);
        Self::load_from_config_path(path)
    }

    pub fn load_from_config_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path).map_err(|error| ConfigError::Io {
            path: path.to_path_buf(),
            source: error,
        })?;
        let file_config = parse_config_file(&contents).map_err(|message| ConfigError::Parse {
            path: path.to_path_buf(),
            message,
        })?;

        Ok(file_config.into_config())
    }

    pub fn resolve_model_config(&self) -> Result<ModelConfig, ConfigError> {
        self.resolve_model_config_with(|name| env::var_os(name))
    }

    pub fn resolve_model_config_with<F>(&self, lookup: F) -> Result<ModelConfig, ConfigError>
    where
        F: FnOnce(&str) -> Option<OsString>,
    {
        Ok(ModelConfig {
            model: self.model.clone(),
            api_key: self.openrouter_api_key_with(lookup)?,
        })
    }

    pub fn validate_openrouter_api_key(&self) -> Result<(), ConfigError> {
        self.validate_openrouter_api_key_with(|name| env::var_os(name))
    }

    pub fn validate_openrouter_api_key_with<F>(&self, lookup: F) -> Result<(), ConfigError>
    where
        F: FnOnce(&str) -> Option<OsString>,
    {
        self.openrouter_api_key_with(lookup).map(|_| ())
    }

    pub fn openrouter_api_key(&self) -> Result<String, ConfigError> {
        self.openrouter_api_key_with(|name| env::var_os(name))
    }

    pub fn openrouter_api_key_with<F>(&self, lookup: F) -> Result<String, ConfigError>
    where
        F: FnOnce(&str) -> Option<OsString>,
    {
        // A literal key configured in config.toml wins over the environment
        // variable. An empty literal is treated as unset and falls through.
        if let Some(api_key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            return Ok(api_key.to_string());
        }

        let value =
            lookup(&self.api_key_env).ok_or_else(|| ConfigError::MissingOpenRouterApiKey {
                env_var: self.api_key_env.clone(),
            })?;

        if value.is_empty() {
            return Err(ConfigError::MissingOpenRouterApiKey {
                env_var: self.api_key_env.clone(),
            });
        }

        value
            .into_string()
            .map_err(|_| ConfigError::InvalidOpenRouterApiKey {
                env_var: self.api_key_env.clone(),
            })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            api_key_env: DEFAULT_OPENROUTER_API_KEY_ENV.to_string(),
            api_key: None,
        }
    }
}

pub fn config_path_from_home_dir(home_dir: impl AsRef<Path>) -> PathBuf {
    poe_home_from_home_dir(home_dir).join(CONFIG_FILE_NAME)
}

pub fn sessions_dir_from_home_dir(home_dir: impl AsRef<Path>) -> PathBuf {
    poe_home_from_home_dir(home_dir).join(SESSIONS_DIR_NAME)
}

pub fn poe_home_from_home_dir(home_dir: impl AsRef<Path>) -> PathBuf {
    home_dir.as_ref().join(POE_DIR_NAME)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub model: String,
    pub api_key: String,
}

#[derive(Debug)]
pub enum ConfigError {
    MissingHomeDirectory,
    Io { path: PathBuf, source: io::Error },
    Parse { path: PathBuf, message: String },
    MissingOpenRouterApiKey { env_var: String },
    InvalidOpenRouterApiKey { env_var: String },
}

// `io::Error` is neither `Clone` nor `PartialEq`, so `ConfigError` cannot derive
// them. Equality compares the `Io` source by `ErrorKind`, which is enough to keep
// errors meaningfully comparable in tests and callers.
impl PartialEq for ConfigError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::MissingHomeDirectory, Self::MissingHomeDirectory) => true,
            (
                Self::Io {
                    path: left_path,
                    source: left_source,
                },
                Self::Io {
                    path: right_path,
                    source: right_source,
                },
            ) => left_path == right_path && left_source.kind() == right_source.kind(),
            (
                Self::Parse {
                    path: left_path,
                    message: left_message,
                },
                Self::Parse {
                    path: right_path,
                    message: right_message,
                },
            ) => left_path == right_path && left_message == right_message,
            (
                Self::MissingOpenRouterApiKey { env_var: left },
                Self::MissingOpenRouterApiKey { env_var: right },
            ) => left == right,
            (
                Self::InvalidOpenRouterApiKey { env_var: left },
                Self::InvalidOpenRouterApiKey { env_var: right },
            ) => left == right,
            _ => false,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHomeDirectory => write!(
                formatter,
                "HOME is not configured.\nSet HOME so poe can find ~/.poe/config.toml."
            ),
            Self::Io { path, source } => write!(
                formatter,
                "failed to read config file {}: {source}",
                path.display()
            ),
            Self::Parse { path, message } => write!(
                formatter,
                "failed to parse config file {}: {message}",
                path.display()
            ),
            Self::MissingOpenRouterApiKey { env_var } => write!(
                formatter,
                "OpenRouter API key is not configured.\nSet [openrouter].api_key in config.toml, set the {env_var} environment variable, or point [openrouter].api_key_env at a different variable."
            ),
            Self::InvalidOpenRouterApiKey { env_var } => {
                write!(
                    formatter,
                    "OpenRouter API key in {env_var} is not valid UTF-8."
                )
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FileConfig {
    model: Option<String>,
    openrouter: OpenRouterConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpenRouterConfig {
    api_key: Option<String>,
    api_key_env: Option<String>,
}

impl FileConfig {
    fn into_config(self) -> Config {
        Config {
            model: self.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            api_key_env: self
                .openrouter
                .api_key_env
                .unwrap_or_else(|| DEFAULT_OPENROUTER_API_KEY_ENV.to_string()),
            api_key: self.openrouter.api_key,
        }
    }
}

fn parse_config_file(contents: &str) -> Result<FileConfig, String> {
    let mut config = FileConfig::default();
    let mut section: Option<String> = None;

    for (line_number, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') {
            if !line.ends_with(']') {
                return Err(format!(
                    "line {}: unterminated section header",
                    line_number + 1
                ));
            }

            let section_name = line[1..line.len() - 1].trim();
            if section_name != "openrouter" {
                return Err(format!(
                    "line {}: unsupported section [{section_name}]",
                    line_number + 1
                ));
            }

            section = Some(section_name.to_string());
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", line_number + 1));
        };

        let key = key.trim();
        let value = parse_string_value(value.trim())
            .map_err(|message| format!("line {}: {message}", line_number + 1))?;

        match section.as_deref() {
            None => match key {
                "model" => config.model = Some(value),
                other => {
                    return Err(format!(
                        "line {}: unsupported top-level key {other}",
                        line_number + 1
                    ));
                }
            },
            Some("openrouter") => match key {
                "api_key" => config.openrouter.api_key = Some(value),
                "api_key_env" => config.openrouter.api_key_env = Some(value),
                other => {
                    return Err(format!(
                        "line {}: unsupported [openrouter] key {other}",
                        line_number + 1
                    ));
                }
            },
            Some(other) => {
                return Err(format!(
                    "line {}: unsupported section [{other}]",
                    line_number + 1
                ));
            }
        }
    }

    Ok(config)
}

fn parse_string_value(value: &str) -> Result<String, String> {
    if value.len() < 2 {
        return Err("expected a quoted string".to_string());
    }

    let quote = value.as_bytes()[0];
    if quote != b'"' && quote != b'\'' {
        return Err("expected a quoted string".to_string());
    }

    if value.as_bytes()[value.len() - 1] != quote {
        return Err("expected a quoted string".to_string());
    }

    Ok(value[1..value.len() - 1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_openrouter_defaults() {
        let config = Config::default();

        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.api_key_env, DEFAULT_OPENROUTER_API_KEY_ENV);
    }

    #[test]
    fn load_from_home_dir_uses_default_when_config_is_missing() {
        let config = Config::load_from_home_dir("/tmp/poe-agent-test-home").expect("load config");

        assert_eq!(config, Config::default());
    }

    #[test]
    fn load_from_home_dir_reads_model_and_openrouter_settings() {
        let config = Config::load_from_config_path_from_contents(
            "/tmp/poe-agent-test-home/.poe/config.toml",
            r#"
model = "anthropic/claude-sonnet-4"

[openrouter]
api_key_env = "CUSTOM_OPENROUTER_KEY"
"#,
        )
        .expect("load config");

        assert_eq!(
            config,
            Config {
                model: "anthropic/claude-sonnet-4".to_string(),
                api_key_env: "CUSTOM_OPENROUTER_KEY".to_string(),
                api_key: None,
            }
        );
    }

    #[test]
    fn load_reads_literal_api_key_and_prefers_it_over_env() {
        let config = Config::load_from_config_path_from_contents(
            "/tmp/poe-agent-test-home/.poe/config.toml",
            r#"
[openrouter]
api_key = "sk-or-from-file"
"#,
        )
        .expect("load config");

        assert_eq!(config.api_key.as_deref(), Some("sk-or-from-file"));
        // The env lookup must never be consulted when a literal key is present.
        assert_eq!(
            config.openrouter_api_key_with(|_| panic!("env lookup should not run")),
            Ok("sk-or-from-file".to_string())
        );
    }

    #[test]
    fn empty_literal_api_key_falls_back_to_env() {
        let config = Config {
            model: DEFAULT_MODEL.to_string(),
            api_key_env: DEFAULT_OPENROUTER_API_KEY_ENV.to_string(),
            api_key: Some(String::new()),
        };

        assert_eq!(
            config.openrouter_api_key_with(|_| Some(OsString::from("from-env"))),
            Ok("from-env".to_string())
        );
    }

    #[test]
    fn load_from_home_dir_rejects_invalid_config_shape() {
        let error = Config::load_from_config_path_from_contents(
            "/tmp/poe-agent-test-home/.poe/config.toml",
            r#"
[other]
value = "nope"
"#,
        )
        .expect_err("invalid config");

        assert!(error.to_string().contains("unsupported section [other]"));
    }

    #[test]
    fn home_helpers_use_single_poe_root() {
        let home = PathBuf::from("/tmp/home");

        assert_eq!(
            poe_home_from_home_dir(&home),
            PathBuf::from("/tmp/home/.poe")
        );
        assert_eq!(
            config_path_from_home_dir(&home),
            PathBuf::from("/tmp/home/.poe/config.toml")
        );
        assert_eq!(
            sessions_dir_from_home_dir(&home),
            PathBuf::from("/tmp/home/.poe/sessions")
        );
    }

    #[test]
    fn openrouter_validation_accepts_non_empty_api_key() {
        let config = Config::default();

        assert_eq!(
            config.validate_openrouter_api_key_with(|name| {
                assert_eq!(name, DEFAULT_OPENROUTER_API_KEY_ENV);
                Some(OsString::from("test-key"))
            }),
            Ok(())
        );
    }

    #[test]
    fn openrouter_api_key_returns_secret_value() {
        let config = Config::default();

        assert_eq!(
            config.openrouter_api_key_with(|name| {
                assert_eq!(name, DEFAULT_OPENROUTER_API_KEY_ENV);
                Some(OsString::from("test-key"))
            }),
            Ok("test-key".to_string())
        );
    }

    #[test]
    fn model_config_returns_model_and_secret_value() {
        let config = Config {
            model: "openai/gpt-4.1-mini".to_string(),
            api_key_env: "CUSTOM_OPENROUTER_KEY".to_string(),
            api_key: None,
        };

        assert_eq!(
            config.resolve_model_config_with(|name| {
                assert_eq!(name, "CUSTOM_OPENROUTER_KEY");
                Some(OsString::from("custom-key"))
            }),
            Ok(ModelConfig {
                model: "openai/gpt-4.1-mini".to_string(),
                api_key: "custom-key".to_string(),
            })
        );
    }

    #[test]
    fn openrouter_validation_rejects_missing_api_key() {
        let config = Config::default();

        assert_eq!(
            config.validate_openrouter_api_key_with(|_| None),
            Err(ConfigError::MissingOpenRouterApiKey {
                env_var: DEFAULT_OPENROUTER_API_KEY_ENV.to_string()
            })
        );
    }

    #[test]
    fn openrouter_validation_rejects_empty_api_key() {
        let config = Config::default();

        assert_eq!(
            config.validate_openrouter_api_key_with(|_| Some(OsString::new())),
            Err(ConfigError::MissingOpenRouterApiKey {
                env_var: DEFAULT_OPENROUTER_API_KEY_ENV.to_string()
            })
        );
    }

    #[test]
    fn missing_key_error_does_not_include_secret_values() {
        let error = ConfigError::MissingOpenRouterApiKey {
            env_var: DEFAULT_OPENROUTER_API_KEY_ENV.to_string(),
        };

        assert!(!error.to_string().contains("sk-or-"));
        assert!(error.to_string().contains(DEFAULT_OPENROUTER_API_KEY_ENV));
    }

    impl Config {
        fn load_from_config_path_from_contents(
            path: impl AsRef<Path>,
            contents: &str,
        ) -> Result<Self, ConfigError> {
            let path = path.as_ref();
            let file_config =
                parse_config_file(contents).map_err(|message| ConfigError::Parse {
                    path: path.to_path_buf(),
                    message,
                })?;

            Ok(file_config.into_config())
        }
    }
}
