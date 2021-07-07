use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use serde::Serialize;
use serde_json::{json, value};
use std::{collections::HashMap, convert::TryFrom, fmt, fs, path::Path, sync::RwLock};
use tracing::{error, span, Level};

use wapc::WapcHost;
use wasmtime_provider::WasmtimeEngineProvider;

use kubewarden_policy_sdk::metadata::ProtocolVersion;
use kubewarden_policy_sdk::response::ValidationResponse as PolicyValidationResponse;
use kubewarden_policy_sdk::settings::SettingsValidationResponse;

use crate::cluster_context::ClusterContext;
use crate::policy::Policy;
use crate::validation_response::ValidationResponse;

lazy_static! {
    static ref POLICY_MAPPING: RwLock<HashMap<u64, Policy>> =
        RwLock::new(HashMap::with_capacity(64));
}

#[derive(Serialize)]
pub struct ValidateRequest(serde_json::Value);

impl ValidateRequest {
    pub fn new(request: serde_json::Value) -> Self {
        ValidateRequest(request)
    }

    fn uid(&self) -> &str {
        if let Some(uid) = self.0.get("uid").and_then(value::Value::as_str) {
            uid
        } else {
            ""
        }
    }
}

pub(crate) fn host_callback(
    policy_id: u64,
    binding: &str,
    namespace: &str,
    operation: &str,
    payload: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    match binding {
        "kubewarden" => match namespace {
            "tracing" => match operation {
                "log" => {
                    let policy_mapping = POLICY_MAPPING.read().unwrap();
                    let policy = policy_mapping.get(&policy_id).unwrap();
                    if let Err(e) = policy.log(payload) {
                        let p =
                            String::from_utf8(payload.to_vec()).unwrap_or_else(|e| e.to_string());
                        error!(
                            payload = p.as_str(),
                            error = e.to_string().as_str(),
                            "Cannot log event"
                        );
                    }
                    Ok(Vec::new())
                }
                _ => {
                    error!("unknown operation: {}", operation);
                    Err(format!("unknown operation: {}", operation).into())
                }
            },
            _ => {
                error!("unknown namespace: {}", namespace);
                Err(format!("unknown namespace: {}", namespace).into())
            }
        },
        "kubernetes" => {
            let cluster_context = ClusterContext::get();
            match namespace {
                "ingresses" => Ok(cluster_context.ingresses().into()),
                "namespaces" => Ok(cluster_context.namespaces().into()),
                "services" => Ok(cluster_context.services().into()),
                _ => {
                    error!("unknown namespace: {}", namespace);
                    Err(format!("unknown namespace: {}", namespace).into())
                }
            }
        }
        _ => {
            error!("unknown binding: {}", binding);
            Err(format!("unknown binding: {}", binding).into())
        }
    }
}

pub struct PolicyEvaluator {
    wapc_host: WapcHost,
    policy: Policy,
}

impl fmt::Debug for PolicyEvaluator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyEvaluator")
            .field("settings", &self.policy.settings)
            .finish()
    }
}

impl PolicyEvaluator {
    pub fn from_file(
        policy_file: &Path,
        settings: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<PolicyEvaluator> {
        PolicyEvaluator::from_contents(fs::read(policy_file)?, settings)
    }

    pub fn from_contents(
        policy_contents: Vec<u8>,
        settings: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<PolicyEvaluator> {
        let engine = WasmtimeEngineProvider::new(&policy_contents, None);
        let wapc_host = WapcHost::new(Box::new(engine), host_callback)?;
        let policy = PolicyEvaluator::from_contents_internal(
            policy_contents,
            || Ok(wapc_host.id()),
            |policy_contents, wapc_policy_id, span, settings| {
                Policy::from_contents(policy_contents, wapc_policy_id, span, settings)
            },
            settings,
        )?;

        Ok(PolicyEvaluator { wapc_host, policy })
    }

    fn from_contents_internal<E, P>(
        policy_contents: Vec<u8>,
        engine_initializer: E,
        policy_from_contents: P,
        settings: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Policy>
    where
        E: Fn() -> Result<u64>,
        P: Fn(
            Vec<u8>,
            u64,
            span::Span,
            Option<serde_json::Map<String, serde_json::Value>>,
        ) -> Result<Policy>,
    {
        let wapc_policy_id = engine_initializer()?;
        let span = span!(
            Level::INFO,
            "kubewarden",
            policy_id = wapc_policy_id,
            policy_name = tracing::field::Empty,
        );

        let policy = policy_from_contents(policy_contents, wapc_policy_id, span, settings)?;
        POLICY_MAPPING
            .write()
            .unwrap()
            .insert(wapc_policy_id, policy.clone());

        Ok(policy)
    }

    pub fn validate(&self, request: ValidateRequest) -> ValidationResponse {
        self.policy.span.in_scope(|| {
            let uid = request.uid();
            let policy = Policy {
                request_uid: Some(uid.to_string()),
                ..self.policy.clone()
            };

            POLICY_MAPPING
                .write()
                .unwrap()
                .insert(self.policy.wapc_policy_id, policy.clone());

            let req_obj = match request.0.get("object") {
                Some(req_obj) => req_obj,
                None => {
                    return ValidationResponse::reject(
                        uid.to_string(),
                        "request doesn't have an 'object' value".to_string(),
                        hyper::StatusCode::BAD_REQUEST.as_u16(),
                    );
                }
            };
            let validate_params = json!({
                "request": request,
                "settings": policy.settings.unwrap_or_default(),
            });
            let validate_str = match serde_json::to_string(&validate_params) {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        error = e.to_string().as_str(),
                        "cannot serialize validation params"
                    );
                    return ValidationResponse::reject_internal_server_error(
                        uid.to_string(),
                        e.to_string(),
                    );
                }
            };
            match self.wapc_host.call("validate", validate_str.as_bytes()) {
                Ok(res) => {
                    let pol_val_resp: Result<PolicyValidationResponse> =
                        serde_json::from_slice(&res).map_err(|e| {
                            anyhow!("cannot deserialize policy validation response: {:?}", e)
                        });
                    pol_val_resp
                        .and_then(|pol_val_resp| {
                            ValidationResponse::from_policy_validation_response(
                                uid.to_string(),
                                &req_obj,
                                &pol_val_resp,
                            )
                        })
                        .unwrap_or_else(|e| {
                            error!(
                                error = e.to_string().as_str(),
                                "cannot build validation response from policy result"
                            );
                            ValidationResponse::reject_internal_server_error(
                                uid.to_string(),
                                e.to_string(),
                            )
                        })
                }
                Err(e) => {
                    error!(error = e.to_string().as_str(), "waPC communication error");
                    ValidationResponse::reject_internal_server_error(uid.to_string(), e.to_string())
                }
            }
        })
    }

    pub fn validate_settings(&self) -> SettingsValidationResponse {
        let settings_str = match &self.policy.settings {
            Some(settings) => match serde_json::to_string(settings) {
                Ok(s) => s,
                Err(e) => {
                    return SettingsValidationResponse {
                        valid: false,
                        message: Some(format!("Cannot serialize validation params: {}", e)),
                    }
                }
            },
            // By sending an empty object we allow the logic of the
            // guest SDK or the guest program to default inner
            // attributes as desired.
            None => "{}".to_string(),
        };

        match self
            .wapc_host
            .call("validate_settings", settings_str.as_bytes())
        {
            Ok(res) => {
                let vr: Result<SettingsValidationResponse> = serde_json::from_slice(&res)
                    .map_err(|e| anyhow!("cannot convert response: {:?}", e));
                vr.unwrap_or_else(|e| SettingsValidationResponse {
                    valid: false,
                    message: Some(format!("error: {:?}", e)),
                })
            }
            Err(err) => SettingsValidationResponse {
                valid: false,
                message: Some(format!(
                    "Error invoking settings validation callback: {:?}",
                    err
                )),
            },
        }
    }

    pub fn protocol_version(&self) -> Result<ProtocolVersion> {
        match self.wapc_host.call("protocol_version", &[0; 0]) {
            Ok(res) => ProtocolVersion::try_from(res.clone()).map_err(|e| {
                anyhow!(
                    "Cannot create ProtocolVersion object from '{:?}': {:?}",
                    res,
                    e
                )
            }),
            Err(err) => Err(anyhow!(
                "Cannot invoke 'protocol_version' waPC function: {:?}",
                err
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_registered_in_the_mapping() -> Result<()> {
        let policy = Policy::default();
        let policy_id = 1;

        assert!(!POLICY_MAPPING.read().unwrap().contains_key(&policy_id));

        PolicyEvaluator::from_contents_internal(
            Vec::new(),
            || Ok(policy_id),
            |_, _, _, _| Ok(policy.clone()),
            None,
        )?;

        let policy_mapping = POLICY_MAPPING.read().unwrap();

        assert!(policy_mapping.contains_key(&policy_id));
        assert_eq!(policy_mapping[&policy_id], policy);

        Ok(())
    }
}
