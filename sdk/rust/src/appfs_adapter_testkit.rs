use crate::{
    AdapterControlActionV1, AdapterErrorV1, AdapterExecutionModeV1, AdapterInputModeV1,
    AdapterSubmitOutcomeV1, AppAdapterV1, RequestContextV1,
};
use thiserror::Error;

/// Reusable required-case matrix for adapter implementations targeting AppFS v0.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCaseMatrixV1 {
    pub inline_path: String,
    pub inline_payload: String,
    pub inline_input_mode: AdapterInputModeV1,
    pub streaming_path: String,
    pub streaming_payload: String,
    pub streaming_input_mode: AdapterInputModeV1,
    pub fetch_next_path: String,
    pub close_path: String,
    pub fetch_handle_id: String,
}

impl Default for RequiredCaseMatrixV1 {
    fn default() -> Self {
        Self {
            inline_path: "/contacts/zhangsan/send_message.act".to_string(),
            inline_payload: "hello\n".to_string(),
            inline_input_mode: AdapterInputModeV1::Text,
            streaming_path: "/files/file-001/download.act".to_string(),
            streaming_payload: r#"{"target":"/tmp/a.bin"}"#.to_string(),
            streaming_input_mode: AdapterInputModeV1::Json,
            fetch_next_path: "/_paging/fetch_next.act".to_string(),
            close_path: "/_paging/close.act".to_string(),
            fetch_handle_id: "ph_abc".to_string(),
        }
    }
}

/// Reusable error-case matrix for adapter implementations targeting AppFS v0.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorCaseMatrixV1 {
    pub rejected_path: String,
    pub rejected_payload: String,
    pub rejected_input_mode: AdapterInputModeV1,
    pub expected_rejected_code: String,
    pub internal_path: String,
    pub internal_payload: String,
    pub internal_input_mode: AdapterInputModeV1,
}

impl Default for ErrorCaseMatrixV1 {
    fn default() -> Self {
        Self {
            rejected_path: "/actions/reject.act".to_string(),
            rejected_payload: "{}".to_string(),
            rejected_input_mode: AdapterInputModeV1::Json,
            expected_rejected_code: "INVALID_ARGUMENT".to_string(),
            internal_path: "/actions/internal.act".to_string(),
            internal_payload: "{}".to_string(),
            internal_input_mode: AdapterInputModeV1::Json,
        }
    }
}

/// Failure surface returned by the adapter testkit matrix runners.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("adapter case '{case}': {message}")]
pub struct AdapterCaseErrorV1 {
    pub case: &'static str,
    pub message: String,
}

fn case_error(case: &'static str, message: impl Into<String>) -> AdapterCaseErrorV1 {
    AdapterCaseErrorV1 {
        case,
        message: message.into(),
    }
}

/// Creates a deterministic default request context for SDK-level matrix tests.
pub fn default_request_context_v1(app_id: &str) -> RequestContextV1 {
    RequestContextV1 {
        app_id: app_id.to_string(),
        session_id: "sess-test".to_string(),
        request_id: "req-test".to_string(),
        client_token: Some("tok-1".to_string()),
    }
}

/// Runs the required-case matrix against an adapter implementation.
pub fn run_required_case_matrix_v1(
    adapter: &mut dyn AppAdapterV1,
    ctx: &RequestContextV1,
    cases: &RequiredCaseMatrixV1,
) -> Result<(), AdapterCaseErrorV1> {
    if adapter.app_id() != ctx.app_id {
        return Err(case_error(
            "context.app_id",
            format!(
                "adapter app_id '{}' does not match context app_id '{}'",
                adapter.app_id(),
                ctx.app_id
            ),
        ));
    }

    let inline = adapter
        .submit_action(
            &cases.inline_path,
            &cases.inline_payload,
            cases.inline_input_mode,
            AdapterExecutionModeV1::Inline,
            ctx,
        )
        .map_err(|err| case_error("inline.submit", format!("unexpected error: {err}")))?;
    if !matches!(inline, AdapterSubmitOutcomeV1::Completed { .. }) {
        return Err(case_error(
            "inline.submit",
            "expected AdapterSubmitOutcomeV1::Completed",
        ));
    }

    let streaming = adapter
        .submit_action(
            &cases.streaming_path,
            &cases.streaming_payload,
            cases.streaming_input_mode,
            AdapterExecutionModeV1::Streaming,
            ctx,
        )
        .map_err(|err| case_error("streaming.submit", format!("unexpected error: {err}")))?;
    match streaming {
        AdapterSubmitOutcomeV1::Streaming { plan } => {
            if plan.terminal_content.is_null() {
                return Err(case_error(
                    "streaming.submit",
                    "terminal_content must not be null",
                ));
            }
        }
        _ => {
            return Err(case_error(
                "streaming.submit",
                "expected AdapterSubmitOutcomeV1::Streaming",
            ));
        }
    }

    adapter
        .submit_control_action(
            &cases.fetch_next_path,
            AdapterControlActionV1::PagingFetchNext {
                handle_id: cases.fetch_handle_id.clone(),
                page_no: 1,
                has_more: true,
            },
            ctx,
        )
        .map_err(|err| case_error("paging.fetch_next", format!("unexpected error: {err}")))?;

    adapter
        .submit_control_action(
            &cases.close_path,
            AdapterControlActionV1::PagingClose {
                handle_id: cases.fetch_handle_id.clone(),
            },
            ctx,
        )
        .map_err(|err| case_error("paging.close", format!("unexpected error: {err}")))?;

    Ok(())
}

/// Runs the error-case matrix against an adapter implementation.
pub fn run_error_case_matrix_v1(
    adapter: &mut dyn AppAdapterV1,
    ctx: &RequestContextV1,
    cases: &ErrorCaseMatrixV1,
) -> Result<(), AdapterCaseErrorV1> {
    let rejected = match adapter.submit_action(
        &cases.rejected_path,
        &cases.rejected_payload,
        cases.rejected_input_mode,
        AdapterExecutionModeV1::Inline,
        ctx,
    ) {
        Ok(_) => {
            return Err(case_error(
                "error.rejected",
                "expected rejected error, got successful outcome",
            ));
        }
        Err(err) => err,
    };
    match rejected {
        AdapterErrorV1::Rejected { code, .. } => {
            if code != cases.expected_rejected_code {
                return Err(case_error(
                    "error.rejected",
                    format!(
                        "expected rejected code '{}' got '{}'",
                        cases.expected_rejected_code, code
                    ),
                ));
            }
        }
        other => {
            return Err(case_error(
                "error.rejected",
                format!("expected rejected error, got '{other}'"),
            ));
        }
    }

    let internal = match adapter.submit_action(
        &cases.internal_path,
        &cases.internal_payload,
        cases.internal_input_mode,
        AdapterExecutionModeV1::Inline,
        ctx,
    ) {
        Ok(_) => {
            return Err(case_error(
                "error.internal",
                "expected internal error, got successful outcome",
            ));
        }
        Err(err) => err,
    };
    if !matches!(internal, AdapterErrorV1::Internal { .. }) {
        return Err(case_error(
            "error.internal",
            format!("expected internal error, got '{internal}'"),
        ));
    }

    Ok(())
}
