use agentfs_sdk::{
    AdapterControlActionV1, AdapterControlOutcomeV1, AdapterErrorV1, AdapterExecutionModeV1,
    AdapterInputModeV1, AdapterSubmitOutcomeV1, AppAdapterV1, RequestContextV1,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

pub(super) struct HttpBridgeAdapterV1 {
    app_id: String,
    endpoint: String,
    timeout: Duration,
}

#[derive(Debug, Serialize)]
struct SubmitActionRequest {
    app_id: String,
    path: String,
    payload: String,
    input_mode: AdapterInputModeV1,
    execution_mode: AdapterExecutionModeV1,
    context: RequestContextV1,
}

#[derive(Debug, Serialize)]
struct SubmitControlRequest {
    app_id: String,
    path: String,
    action: AdapterControlActionV1,
    context: RequestContextV1,
}

#[derive(Debug, Deserialize)]
struct BridgeErrorPayload {
    code: String,
    message: String,
    #[serde(default)]
    retryable: bool,
}

impl HttpBridgeAdapterV1 {
    pub(super) fn new(app_id: String, endpoint: String, timeout: Duration) -> Self {
        Self {
            app_id,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            timeout,
        }
    }

    fn post_json<Req, Resp>(
        &self,
        route: &str,
        req: &Req,
    ) -> std::result::Result<Resp, AdapterErrorV1>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let url = format!("{}/{}", self.endpoint, route.trim_start_matches('/'));
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let request = agent.post(&url);

        match request.send_json(req) {
            Ok(response) => response
                .into_json::<Resp>()
                .map_err(|err| AdapterErrorV1::Internal {
                    message: format!("bridge decode error for {url}: {err}"),
                }),
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(map_status_error(status, &body))
            }
            Err(ureq::Error::Transport(err)) => Err(AdapterErrorV1::Internal {
                message: format!("bridge transport error for {url}: {err}"),
            }),
        }
    }
}

impl AppAdapterV1 for HttpBridgeAdapterV1 {
    fn app_id(&self) -> &str {
        &self.app_id
    }

    fn submit_action(
        &mut self,
        path: &str,
        payload: &str,
        input_mode: AdapterInputModeV1,
        execution_mode: AdapterExecutionModeV1,
        ctx: &RequestContextV1,
    ) -> std::result::Result<AdapterSubmitOutcomeV1, AdapterErrorV1> {
        let request = SubmitActionRequest {
            app_id: self.app_id.clone(),
            path: path.to_string(),
            payload: payload.to_string(),
            input_mode,
            execution_mode,
            context: ctx.clone(),
        };
        self.post_json("v1/submit-action", &request)
    }

    fn submit_control_action(
        &mut self,
        path: &str,
        action: AdapterControlActionV1,
        ctx: &RequestContextV1,
    ) -> std::result::Result<AdapterControlOutcomeV1, AdapterErrorV1> {
        let request = SubmitControlRequest {
            app_id: self.app_id.clone(),
            path: path.to_string(),
            action,
            context: ctx.clone(),
        };
        self.post_json("v1/submit-control-action", &request)
    }
}

fn map_status_error(status: u16, body: &str) -> AdapterErrorV1 {
    if let Ok(adapter_error) = serde_json::from_str::<AdapterErrorV1>(body) {
        return adapter_error;
    }
    if let Ok(payload) = serde_json::from_str::<BridgeErrorPayload>(body) {
        return AdapterErrorV1::Rejected {
            code: payload.code,
            message: payload.message,
            retryable: payload.retryable,
        };
    }
    AdapterErrorV1::Internal {
        message: if body.trim().is_empty() {
            format!("bridge http status {status}")
        } else {
            format!("bridge http status {status}: {body}")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::map_status_error;
    use agentfs_sdk::AdapterErrorV1;

    #[test]
    fn map_status_error_accepts_adapter_error_shape() {
        let err = map_status_error(
            400,
            r#"{"kind":"rejected","code":"INVALID_ARGUMENT","message":"bad payload","retryable":false}"#,
        );
        match err {
            AdapterErrorV1::Rejected {
                code,
                message,
                retryable,
            } => {
                assert_eq!(code, "INVALID_ARGUMENT");
                assert_eq!(message, "bad payload");
                assert!(!retryable);
            }
            _ => panic!("expected rejected error"),
        }
    }

    #[test]
    fn map_status_error_accepts_simple_error_shape() {
        let err = map_status_error(
            403,
            r#"{"code":"PERMISSION_DENIED","message":"forbidden","retryable":false}"#,
        );
        match err {
            AdapterErrorV1::Rejected {
                code,
                message,
                retryable,
            } => {
                assert_eq!(code, "PERMISSION_DENIED");
                assert_eq!(message, "forbidden");
                assert!(!retryable);
            }
            _ => panic!("expected rejected error"),
        }
    }
}
