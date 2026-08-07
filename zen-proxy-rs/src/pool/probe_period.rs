use std::time::Duration;

use free_model_client_rs::kernel::{FreeModelKernel, KernelConfig};
use free_model_client_rs::protocol::types::{ChatRequest, Message};
use serde_json::Value;

use crate::pool::*;

/// Why a probe through a node failed. Lets the caller surface a concrete
/// reason instead of a bare boolean ("node offline", "rate limited by
/// upstream", "upstream 5xx", ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailReason {
    /// Upstream answered 429 (or an AppError flagged rate limiting).
    RateLimited,
    /// Upstream answered 5xx.
    UpstreamError,
    /// Transport-level failure: connection refused / reset, DNS, socks
    /// handshake — i.e. the exit node itself looks unreachable.
    NetworkError,
    /// Probe did not complete within `timeout_secs`.
    Timeout,
    /// Anything else (other HTTP status, malformed response, ...).
    Other,
}

impl ProbeFailReason {
    /// Stable machine-readable label used in the admin failed-nodes API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::UpstreamError => "upstream_error",
            Self::NetworkError => "network_error",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}

pub struct ProbePeriod;

impl ProbePeriod {
    /// Probe through `node` and report whether it succeeded (bool, no detail).
    pub async fn probe_node(
        client: &reqwest::Client,
        node: &NodeRef,
        upstream_base: &str,
        timeout_secs: u64,
        api_key: &str,
    ) -> bool {
        Self::probe_node_detailed(client, node, upstream_base, timeout_secs, api_key)
            .await
            .is_ok()
    }

    /// Probe through `node`, returning the concrete failure reason when the
    /// probe fails (after up to 3 attempts). Success yields `Ok(())`.
    pub async fn probe_node_detailed(
        client: &reqwest::Client,
        _node: &NodeRef,
        upstream_base: &str,
        timeout_secs: u64,
        api_key: &str,
    ) -> Result<(), ProbeFailReason> {
        let zen_chat_url = format!(
            "{}/v1/chat/completions",
            upstream_base.trim_end_matches('/')
        );
        let kernel = FreeModelKernel::new(KernelConfig {
            zen_chat_url,
            zen_api_key: api_key.to_string(),
            extra_headers: vec![("x-zen-proxy-probe".to_string(), "dead".to_string())],
            model_mappings: Vec::new(),
            true_first_token_frt: true,
            claude_code_stream_initial_fetch_timeout_secs: 30,
            claude_code_stream_slow_guard_min_input_tokens: 150_000,
            claude_code_stream_no_forwardable_retry_secs: 45,
            claude_code_stream_reasoning_stall_retry_secs: 15,
            claude_code_stream_reasoning_stall_window_secs: 5,
            claude_code_stream_max_wait_forwardable_secs: 60,
        });
        let mut last_failure = ProbeFailReason::Other;
        for i in 0..3 {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let request = ChatRequest {
                model: "deepseek-v4-flash-free".to_string(),
                messages: vec![Message {
                    role: "user".to_string(),
                    content: Value::String("1+1".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                }],
                stream: Some(false),
                max_tokens: Some(32),
                temperature: None,
                top_p: None,
                tools: None,
                tool_choice: None,
            };

            let result = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                kernel.openai_chat(client, request),
            )
            .await;

            match result {
                Ok(Ok(resp)) => {
                    if resp.status().is_success() {
                        return Ok(());
                    }
                    last_failure = match resp.status().as_u16() {
                        429 => ProbeFailReason::RateLimited,
                        500..=599 => ProbeFailReason::UpstreamError,
                        _ => ProbeFailReason::Other,
                    };
                }
                Ok(Err(e)) => {
                    if e.is_rate_limited() {
                        last_failure = ProbeFailReason::RateLimited;
                    } else {
                        let status = e.status.as_u16();
                        last_failure = match status {
                            429 => ProbeFailReason::RateLimited,
                            500..=599 => ProbeFailReason::UpstreamError,
                            _ => ProbeFailReason::Other,
                        };
                    }
                }
                Err(_) => {
                    // tokio timeout: the probe did not complete in time.
                    last_failure = ProbeFailReason::Timeout;
                }
            }
        }

        Err(last_failure)
    }
}
