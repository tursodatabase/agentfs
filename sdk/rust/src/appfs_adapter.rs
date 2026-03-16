use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;

/// Frozen AppFS adapter SDK surface version for v0.1.
pub const APPFS_ADAPTER_SDK_VERSION: &str = "0.1.0";

/// v0.1 frozen input payload mode model for adapter dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterInputModeV1 {
    Text,
    Json,
    TextOrJson,
}

/// v0.1 frozen execution mode model for adapter dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterExecutionModeV1 {
    Inline,
    Streaming,
}

/// Runtime correlation and principal context passed to adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContextV1 {
    pub app_id: String,
    pub session_id: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_token: Option<String>,
}

/// Streaming lifecycle payload plan emitted by runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdapterStreamingPlanV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_content: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_content: Option<JsonValue>,
    pub terminal_content: JsonValue,
}

/// v0.1 control-path action model (non-resource action channels such as paging control).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdapterControlActionV1 {
    PagingFetchNext {
        handle_id: String,
        page_no: u64,
        has_more: bool,
    },
    PagingClose {
        handle_id: String,
    },
}

/// v0.1 frozen adapter submit outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdapterSubmitOutcomeV1 {
    Completed { content: JsonValue },
    Streaming { plan: AdapterStreamingPlanV1 },
}

/// v0.1 frozen adapter control-path outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdapterControlOutcomeV1 {
    Completed { content: JsonValue },
}

/// v0.1 frozen adapter error contract.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdapterErrorV1 {
    #[error("{code}: {message}")]
    Rejected {
        code: String,
        message: String,
        #[serde(default)]
        retryable: bool,
    },
    #[error("adapter internal error: {message}")]
    Internal { message: String },
}

/// Returns true when `version` is compatible with the frozen v0.1 adapter SDK surface.
///
/// Compatibility rule:
/// - any `0.1.x` is considered compatible.
pub fn is_appfs_adapter_sdk_v01_compatible(version: &str) -> bool {
    let core = version.trim().split('-').next().unwrap_or("").trim();
    let mut parts = core.split('.');
    let major = parts.next().and_then(|v| v.parse::<u64>().ok());
    let minor = parts.next().and_then(|v| v.parse::<u64>().ok());
    matches!((major, minor), (Some(0), Some(1)))
}

/// AppFS adapter SDK v0.1 frozen trait surface.
///
/// Compatibility:
/// 1. `v0.1.x` allows additive-only, backward-compatible changes.
/// 2. Breaking method/behavior changes require a `v0.2` trait surface.
pub trait AppAdapterV1: Send {
    fn app_id(&self) -> &str;

    fn submit_action(
        &mut self,
        path: &str,
        payload: &str,
        input_mode: AdapterInputModeV1,
        execution_mode: AdapterExecutionModeV1,
        ctx: &RequestContextV1,
    ) -> std::result::Result<AdapterSubmitOutcomeV1, AdapterErrorV1>;

    fn submit_control_action(
        &mut self,
        path: &str,
        action: AdapterControlActionV1,
        _ctx: &RequestContextV1,
    ) -> std::result::Result<AdapterControlOutcomeV1, AdapterErrorV1> {
        let _ = path;
        let _ = action;
        Err(AdapterErrorV1::Rejected {
            code: "NOT_SUPPORTED".to_string(),
            message: "control action is not supported by this adapter".to_string(),
            retryable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_appfs_adapter_sdk_v01_compatible, AdapterControlActionV1, AdapterControlOutcomeV1,
        AdapterErrorV1, AdapterExecutionModeV1, AdapterInputModeV1, AdapterStreamingPlanV1,
        AdapterSubmitOutcomeV1, AppAdapterV1, RequestContextV1, APPFS_ADAPTER_SDK_VERSION,
    };
    use crate::appfs_adapter_testkit::{
        default_request_context_v1, run_error_case_matrix_v1, run_required_case_matrix_v1,
        ErrorCaseMatrixV1, RequiredCaseMatrixV1,
    };
    use serde_json::json;

    struct RequiredMatrixAdapterA;

    impl AppAdapterV1 for RequiredMatrixAdapterA {
        fn app_id(&self) -> &str {
            "aiim"
        }

        fn submit_action(
            &mut self,
            path: &str,
            _payload: &str,
            _input_mode: AdapterInputModeV1,
            execution_mode: AdapterExecutionModeV1,
            _ctx: &RequestContextV1,
        ) -> std::result::Result<AdapterSubmitOutcomeV1, AdapterErrorV1> {
            if execution_mode == AdapterExecutionModeV1::Inline {
                return Ok(AdapterSubmitOutcomeV1::Completed {
                    content: json!({ "path": path, "status": "ok", "adapter": "a" }),
                });
            }
            Ok(AdapterSubmitOutcomeV1::Streaming {
                plan: AdapterStreamingPlanV1 {
                    accepted_content: Some(json!({ "state": "accepted" })),
                    progress_content: Some(json!({ "percent": 50 })),
                    terminal_content: json!({ "ok": true }),
                },
            })
        }

        fn submit_control_action(
            &mut self,
            _path: &str,
            action: AdapterControlActionV1,
            _ctx: &RequestContextV1,
        ) -> std::result::Result<AdapterControlOutcomeV1, AdapterErrorV1> {
            match action {
                AdapterControlActionV1::PagingFetchNext {
                    handle_id,
                    page_no,
                    has_more,
                } => Ok(AdapterControlOutcomeV1::Completed {
                    content: json!({
                        "page": { "handle_id": handle_id, "page_no": page_no, "has_more": has_more }
                    }),
                }),
                AdapterControlActionV1::PagingClose { handle_id } => {
                    Ok(AdapterControlOutcomeV1::Completed {
                        content: json!({ "closed": true, "handle_id": handle_id }),
                    })
                }
            }
        }
    }

    struct RequiredMatrixAdapterB;

    impl AppAdapterV1 for RequiredMatrixAdapterB {
        fn app_id(&self) -> &str {
            "aiim"
        }

        fn submit_action(
            &mut self,
            path: &str,
            _payload: &str,
            _input_mode: AdapterInputModeV1,
            execution_mode: AdapterExecutionModeV1,
            _ctx: &RequestContextV1,
        ) -> std::result::Result<AdapterSubmitOutcomeV1, AdapterErrorV1> {
            if execution_mode == AdapterExecutionModeV1::Inline {
                return Ok(AdapterSubmitOutcomeV1::Completed {
                    content: json!({ "path": path, "status": "ok", "adapter": "b", "ts": 1 }),
                });
            }
            Ok(AdapterSubmitOutcomeV1::Streaming {
                plan: AdapterStreamingPlanV1 {
                    accepted_content: Some(json!({ "state": "accepted", "queue": "default" })),
                    progress_content: Some(json!({ "percent": 50, "phase": "download" })),
                    terminal_content: json!({ "ok": true, "saved_to": "/tmp/out.bin" }),
                },
            })
        }

        fn submit_control_action(
            &mut self,
            _path: &str,
            action: AdapterControlActionV1,
            _ctx: &RequestContextV1,
        ) -> std::result::Result<AdapterControlOutcomeV1, AdapterErrorV1> {
            match action {
                AdapterControlActionV1::PagingFetchNext {
                    handle_id,
                    page_no,
                    has_more,
                } => Ok(AdapterControlOutcomeV1::Completed {
                    content: json!({
                        "items": [{ "id": "m-1" }],
                        "page": { "handle_id": handle_id, "page_no": page_no, "has_more": has_more, "mode": "snapshot" }
                    }),
                }),
                AdapterControlActionV1::PagingClose { handle_id } => {
                    Ok(AdapterControlOutcomeV1::Completed {
                        content: json!({ "closed": true, "handle_id": handle_id }),
                    })
                }
            }
        }
    }

    struct ErrorMatrixAdapter;

    impl AppAdapterV1 for ErrorMatrixAdapter {
        fn app_id(&self) -> &str {
            "aiim"
        }

        fn submit_action(
            &mut self,
            path: &str,
            _payload: &str,
            _input_mode: AdapterInputModeV1,
            _execution_mode: AdapterExecutionModeV1,
            _ctx: &RequestContextV1,
        ) -> std::result::Result<AdapterSubmitOutcomeV1, AdapterErrorV1> {
            if path.ends_with("/reject.act") {
                return Err(AdapterErrorV1::Rejected {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: "bad payload".to_string(),
                    retryable: false,
                });
            }
            Err(AdapterErrorV1::Internal {
                message: "backend unavailable".to_string(),
            })
        }
    }

    #[test]
    fn sdk_trait_smoke_submit_and_control() {
        let mut adapter = RequiredMatrixAdapterA;
        run_required_case_matrix_v1(
            &mut adapter,
            &default_request_context_v1("aiim"),
            &RequiredCaseMatrixV1::default(),
        )
        .expect("required matrix should pass");
    }

    #[test]
    fn sdk_trait_required_case_matrix_is_adapter_pluggable() {
        let cases = RequiredCaseMatrixV1::default();
        let ctx = default_request_context_v1("aiim");

        let mut adapter_a = RequiredMatrixAdapterA;
        run_required_case_matrix_v1(&mut adapter_a, &ctx, &cases)
            .expect("adapter A should pass required matrix");

        let mut adapter_b = RequiredMatrixAdapterB;
        run_required_case_matrix_v1(&mut adapter_b, &ctx, &cases)
            .expect("adapter B should pass required matrix");
    }

    #[test]
    fn sdk_trait_error_case_matrix() {
        let mut adapter = ErrorMatrixAdapter;
        run_error_case_matrix_v1(
            &mut adapter,
            &default_request_context_v1("aiim"),
            &ErrorCaseMatrixV1::default(),
        )
        .expect("error matrix should pass");
    }

    #[test]
    fn sdk_trait_default_control_action_not_supported() {
        let mut adapter = ErrorMatrixAdapter;
        let err = adapter
            .submit_control_action(
                "/_paging/fetch_next.act",
                AdapterControlActionV1::PagingFetchNext {
                    handle_id: "ph_abc".to_string(),
                    page_no: 1,
                    has_more: true,
                },
                &default_request_context_v1("aiim"),
            )
            .expect_err("default control path should be unsupported");
        match err {
            AdapterErrorV1::Rejected {
                code,
                message,
                retryable,
            } => {
                assert_eq!(code, "NOT_SUPPORTED");
                assert!(
                    message.contains("control action"),
                    "unexpected message: {}",
                    message
                );
                assert!(!retryable);
            }
            _ => panic!("expected rejected not-supported error"),
        }
    }

    #[test]
    fn sdk_version_reports_v01_compatibility() {
        assert!(is_appfs_adapter_sdk_v01_compatible(
            APPFS_ADAPTER_SDK_VERSION
        ));
        assert!(is_appfs_adapter_sdk_v01_compatible("0.1.9"));
        assert!(!is_appfs_adapter_sdk_v01_compatible("0.2.0"));
        assert!(!is_appfs_adapter_sdk_v01_compatible("1.0.0"));
        assert!(!is_appfs_adapter_sdk_v01_compatible("invalid"));
    }
}
