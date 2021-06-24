use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::{event, span, Level};

use crate::policy::Policy;

#[derive(Debug, Serialize)]
enum PolicyLogEntryLevel {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
}

impl<'de> Deserialize<'de> for PolicyLogEntryLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_uppercase().as_str() {
            "TRACE" => Ok(PolicyLogEntryLevel::Trace),
            "DEBUG" => Ok(PolicyLogEntryLevel::Debug),
            "INFO" => Ok(PolicyLogEntryLevel::Info),
            "WARNING" => Ok(PolicyLogEntryLevel::Warning),
            "ERROR" => Ok(PolicyLogEntryLevel::Error),
            _ => Err(anyhow!("unknown log level {}", s)).map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct PolicyLogEntry {
    level: PolicyLogEntryLevel,
    message: Option<String>,
    #[serde(flatten)]
    data: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Policy {
    pub(crate) fn log(&self, contents: &[u8]) -> Result<()> {
        let log_entry: PolicyLogEntry = serde_json::from_slice(&contents)?;
        let span = span!(
            parent: &self.span,
            Level::INFO,
            "policy",
            request_uid = tracing::field::Empty,
            data = %&serde_json::to_string(&log_entry.data.clone().unwrap())?.as_str(),
        );

        if let Some(request_uid) = &self.request_uid {
            span.record("request_uid", &request_uid.as_str());
        }

        macro_rules! log {
            ($level:path) => {
                event!(
                    target: "policy_log",
                    parent: &span,
                    $level,
                    "{}",
                    log_entry.message.clone().unwrap_or_default()
                );
            };
        }

        match log_entry.level {
            PolicyLogEntryLevel::Trace => {
                log!(Level::TRACE);
            }
            PolicyLogEntryLevel::Debug => {
                log!(Level::DEBUG);
            }
            PolicyLogEntryLevel::Info => {
                log!(Level::INFO);
            }
            PolicyLogEntryLevel::Warning => {
                log!(Level::WARN);
            }
            PolicyLogEntryLevel::Error => {
                log!(Level::ERROR);
            }
        };

        Ok(())
    }
}
