//! Common request/notification shapes for upstream `codex app-server`.
//!
//! The protocol is modeled as JSON objects tagged by the `"method"` field.
//! - This module defines the top-level enums for those `"method"` tags.
//! - Version-specific parameter structs live in [`super::v1`] / [`super::v2`].

use serde::Deserialize;
use serde::Serialize;

use crate::app_server::upstream_protocol::JSONRPCRequest;
use crate::app_server::upstream_protocol::JSONRPCResponse;
use crate::app_server::upstream_protocol::RequestId;

use super::v1;
use super::v2;

/// Request from the client to the server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ClientRequest {
    Initialize {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v1::InitializeParams,
    },

    #[serde(rename = "configRequirements/read")]
    ConfigRequirementsRead {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<()>,
    },

    #[serde(rename = "account/read")]
    GetAccount {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::GetAccountParams,
    },

    #[serde(rename = "account/login/start")]
    LoginAccount {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::LoginAccountParams,
    },

    #[serde(rename = "account/login/cancel")]
    CancelLoginAccount {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::CancelLoginAccountParams,
    },

    #[serde(rename = "thread/start")]
    ThreadStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::ThreadStartParams,
    },

    #[serde(rename = "thread/resume")]
    ThreadResume {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::ThreadResumeParams,
    },

    #[serde(rename = "thread/rollback")]
    ThreadRollback {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::ThreadRollbackParams,
    },

    #[serde(rename = "thread/backgroundTerminals/clean")]
    ThreadBackgroundTerminalsClean {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::ThreadBackgroundTerminalsCleanParams,
    },

    #[serde(rename = "turn/start")]
    TurnStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::TurnStartParams,
    },

    #[serde(rename = "turn/interrupt")]
    TurnInterrupt {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: v2::TurnInterruptParams,
    },
}

/// Typed response from the server to a [`ClientRequest`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ClientResponse {
    Initialize {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v1::InitializeResponse,
    },

    #[serde(rename = "configRequirements/read")]
    ConfigRequirementsRead {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::ConfigRequirementsReadResponse,
    },

    #[serde(rename = "account/read")]
    GetAccount {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::GetAccountResponse,
    },

    #[serde(rename = "account/login/start")]
    LoginAccount {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::LoginAccountResponse,
    },

    #[serde(rename = "account/login/cancel")]
    CancelLoginAccount {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::CancelLoginAccountResponse,
    },

    #[serde(rename = "thread/start")]
    ThreadStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::ThreadStartResponse,
    },

    #[serde(rename = "thread/resume")]
    ThreadResume {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::ThreadResumeResponse,
    },

    #[serde(rename = "thread/rollback")]
    ThreadRollback {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::ThreadRollbackResponse,
    },

    #[serde(rename = "thread/backgroundTerminals/clean")]
    ThreadBackgroundTerminalsClean {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::ThreadBackgroundTerminalsCleanResponse,
    },

    #[serde(rename = "turn/start")]
    TurnStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::TurnStartResponse,
    },

    #[serde(rename = "turn/interrupt")]
    TurnInterrupt {
        #[serde(rename = "id")]
        request_id: RequestId,
        response: v2::TurnInterruptResponse,
    },
}

impl ClientRequest {
    pub fn id(&self) -> &RequestId {
        match self {
            Self::Initialize { request_id, .. }
            | Self::ConfigRequirementsRead { request_id, .. }
            | Self::GetAccount { request_id, .. }
            | Self::LoginAccount { request_id, .. }
            | Self::CancelLoginAccount { request_id, .. }
            | Self::ThreadStart { request_id, .. }
            | Self::ThreadResume { request_id, .. }
            | Self::ThreadRollback { request_id, .. }
            | Self::ThreadBackgroundTerminalsClean { request_id, .. }
            | Self::TurnStart { request_id, .. }
            | Self::TurnInterrupt { request_id, .. } => request_id,
        }
    }

    pub fn method(&self) -> &'static str {
        match self {
            Self::Initialize { .. } => "initialize",
            Self::ConfigRequirementsRead { .. } => "configRequirements/read",
            Self::GetAccount { .. } => "account/read",
            Self::LoginAccount { .. } => "account/login/start",
            Self::CancelLoginAccount { .. } => "account/login/cancel",
            Self::ThreadStart { .. } => "thread/start",
            Self::ThreadResume { .. } => "thread/resume",
            Self::ThreadRollback { .. } => "thread/rollback",
            Self::ThreadBackgroundTerminalsClean { .. } => "thread/backgroundTerminals/clean",
            Self::TurnStart { .. } => "turn/start",
            Self::TurnInterrupt { .. } => "turn/interrupt",
        }
    }

    pub fn decode_response(
        &self,
        response: JSONRPCResponse,
    ) -> Result<ClientResponse, serde_json::Error> {
        let request_id = response.id;
        let response = match self {
            Self::Initialize { .. } => ClientResponse::Initialize {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::ConfigRequirementsRead { .. } => ClientResponse::ConfigRequirementsRead {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::GetAccount { .. } => ClientResponse::GetAccount {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::LoginAccount { .. } => ClientResponse::LoginAccount {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::CancelLoginAccount { .. } => ClientResponse::CancelLoginAccount {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::ThreadStart { .. } => ClientResponse::ThreadStart {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::ThreadResume { .. } => ClientResponse::ThreadResume {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::ThreadRollback { .. } => ClientResponse::ThreadRollback {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::ThreadBackgroundTerminalsClean { .. } => {
                ClientResponse::ThreadBackgroundTerminalsClean {
                    request_id,
                    response: serde_json::from_value(response.result)?,
                }
            }
            Self::TurnStart { .. } => ClientResponse::TurnStart {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
            Self::TurnInterrupt { .. } => ClientResponse::TurnInterrupt {
                request_id,
                response: serde_json::from_value(response.result)?,
            },
        };
        Ok(response)
    }
}

/// Notification from the client to the server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ClientNotification {
    Initialized,
}

/// Request initiated from the server and sent to the client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ServerRequest {
    #[serde(rename = "item/commandExecution/requestApproval")]
    CommandExecution {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<serde_json::Value>,
    },

    #[serde(rename = "item/fileChange/requestApproval")]
    FileChange {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<serde_json::Value>,
    },

    #[serde(rename = "item/tool/requestUserInput")]
    ToolRequestUserInput {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<v2::ToolRequestUserInputParams>,
    },

    #[serde(rename = "mcpServer/elicitation/request")]
    McpServerElicitationRequest {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<v2::McpServerElicitationRequestParams>,
    },

    #[serde(rename = "item/permissions/requestApproval")]
    PermissionsRequestApproval {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<v2::PermissionsRequestApprovalParams>,
    },

    #[serde(rename = "item/tool/call")]
    DynamicToolCall {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<v2::DynamicToolCallParams>,
    },

    #[serde(rename = "account/chatgptAuthTokens/refresh")]
    ChatgptAuthTokensRefresh {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<v2::ChatgptAuthTokensRefreshParams>,
    },

    #[serde(rename = "applyPatchApproval")]
    ApplyPatchApproval {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<serde_json::Value>,
    },

    #[serde(rename = "execCommandApproval")]
    ExecCommandApproval {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: Option<serde_json::Value>,
    },
}

impl TryFrom<JSONRPCRequest> for ServerRequest {
    type Error = serde_json::Error;

    fn try_from(value: JSONRPCRequest) -> Result<Self, Self::Error> {
        serde_json::from_value(serde_json::to_value(value)?)
    }
}

#[cfg(test)]
mod tests {
    use codex_protocol::protocol::PlanType;

    use super::v1::ClientInfo;
    use super::v1::InitializeCapabilities;
    use super::v2::ThreadResumeParams;
    use super::v2::ThreadRollbackParams;
    use super::v2::ThreadStartParams;
    use super::v2::TurnInterruptParams;
    use super::v2::TurnStartParams;
    use super::*;

    #[test]
    fn serialize_initialized_notification_has_no_params_field() {
        let notification = ClientNotification::Initialized;
        let value = serde_json::to_value(&notification).expect("serialize notification");
        assert_eq!(value["method"], "initialized");
        assert!(
            value.get("params").is_none(),
            "Initialized should not include a params field"
        );
    }

    #[test]
    fn serialize_thread_requests_include_expected_keys() {
        {
            let request = ClientRequest::ThreadStart {
                request_id: RequestId::Integer(1),
                params: ThreadStartParams {
                    model: None,
                    model_provider: None,
                    service_tier: None,
                    cwd: None,
                    approval_policy: Some(
                        crate::app_server::upstream_protocol::AskForApproval::Never,
                    ),
                    approvals_reviewer: None,
                    sandbox: None,
                    permission_profile: None,
                    config: None,
                    service_name: None,
                    base_instructions: None,
                    developer_instructions: None,
                    personality: None,
                    ephemeral: None,
                    environments: None,
                    dynamic_tools: None,
                    mock_experimental_field: None,
                    experimental_raw_events: false,
                    persist_extended_history: false,
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "thread/start");
            assert_eq!(value["id"], 1);

            let params = value["params"].as_object().expect("params object");
            for key in [
                "model",
                "modelProvider",
                "serviceTier",
                "cwd",
                "approvalPolicy",
                "approvalsReviewer",
                "sandbox",
                "permissionProfile",
                "config",
                "serviceName",
                "baseInstructions",
                "developerInstructions",
                "personality",
                "ephemeral",
                "environments",
                "dynamicTools",
                "mockExperimentalField",
            ] {
                assert!(
                    params.contains_key(key),
                    "thread/start params must contain key {key}"
                );
            }
            assert_eq!(value["params"]["approvalPolicy"], "never");
            assert_eq!(value["params"]["experimentalRawEvents"], false);
            assert_eq!(value["params"]["persistExtendedHistory"], false);
        }

        {
            let request = ClientRequest::ThreadResume {
                request_id: RequestId::Integer(2),
                params: ThreadResumeParams {
                    thread_id: "thread-1".to_string(),
                    history: None,
                    path: None,
                    model: None,
                    model_provider: None,
                    service_tier: None,
                    cwd: None,
                    approval_policy: Some(
                        crate::app_server::upstream_protocol::AskForApproval::Never,
                    ),
                    approvals_reviewer: None,
                    sandbox: None,
                    permission_profile: None,
                    config: None,
                    base_instructions: None,
                    developer_instructions: None,
                    personality: None,
                    persist_extended_history: false,
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "thread/resume");
            assert_eq!(value["id"], 2);

            let params = value["params"].as_object().expect("params object");
            for key in [
                "threadId",
                "history",
                "path",
                "model",
                "modelProvider",
                "serviceTier",
                "cwd",
                "approvalPolicy",
                "approvalsReviewer",
                "sandbox",
                "permissionProfile",
                "config",
                "baseInstructions",
                "developerInstructions",
                "personality",
            ] {
                assert!(
                    params.contains_key(key),
                    "thread/resume params must contain key {key}"
                );
            }
            assert_eq!(value["params"]["threadId"], "thread-1");
            assert_eq!(value["params"]["approvalPolicy"], "never");
            assert_eq!(value["params"]["persistExtendedHistory"], false);
        }

        {
            let request = ClientRequest::ThreadRollback {
                request_id: RequestId::Integer(4),
                params: ThreadRollbackParams {
                    thread_id: "thread-1".to_string(),
                    num_turns: 1,
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "thread/rollback");
            assert_eq!(value["id"], 4);

            let params = value["params"].as_object().expect("params object");
            for key in ["threadId", "numTurns"] {
                assert!(
                    params.contains_key(key),
                    "thread/rollback params must contain key {key}"
                );
            }
            assert_eq!(value["params"]["numTurns"], 1);
        }
    }

    #[test]
    fn serialize_turn_requests_include_expected_keys() {
        {
            let request = ClientRequest::TurnStart {
                request_id: RequestId::Integer(3),
                params: TurnStartParams {
                    thread_id: "thread-1".to_string(),
                    input: Vec::new(),
                    environments: None,
                    cwd: None,
                    approval_policy: None,
                    approvals_reviewer: None,
                    sandbox_policy: None,
                    permission_profile: None,
                    model: None,
                    service_tier: None,
                    effort: None,
                    summary: None,
                    personality: None,
                    output_schema: None,
                    collaboration_mode: None,
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "turn/start");
            assert_eq!(value["id"], 3);

            let params = value["params"].as_object().expect("params object");
            for key in [
                "threadId",
                "input",
                "environments",
                "cwd",
                "approvalPolicy",
                "approvalsReviewer",
                "sandboxPolicy",
                "permissionProfile",
                "model",
                "serviceTier",
                "effort",
                "summary",
                "personality",
                "outputSchema",
                "collaborationMode",
            ] {
                assert!(
                    params.contains_key(key),
                    "turn/start params must contain key {key}"
                );
            }
        }

        {
            let request = ClientRequest::TurnInterrupt {
                request_id: RequestId::Integer(5),
                params: TurnInterruptParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "turn/interrupt");
            assert_eq!(value["id"], 5);

            let params = value["params"].as_object().expect("params object");
            for key in ["threadId", "turnId"] {
                assert!(
                    params.contains_key(key),
                    "turn/interrupt params must contain key {key}"
                );
            }

            assert_eq!(value["params"]["threadId"], "thread-1");
            assert_eq!(value["params"]["turnId"], "turn-1");
        }
    }

    #[test]
    fn serialize_initialize_request_uses_upstream_tui_client_identity() {
        let request = ClientRequest::Initialize {
            request_id: RequestId::Integer(4),
            params: v1::InitializeParams {
                client_info: ClientInfo {
                    name: "codex-tui".to_string(),
                    title: None,
                    version: "0.121.0".to_string(),
                },
                capabilities: Some(InitializeCapabilities {
                    experimental_api: true,
                    opt_out_notification_methods: None,
                }),
            },
        };

        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["id"], 4);
        assert_eq!(value["params"]["clientInfo"]["name"], "codex-tui");
        assert_eq!(
            value["params"]["clientInfo"]["title"],
            serde_json::Value::Null
        );
        assert_eq!(value["params"]["capabilities"]["experimentalApi"], true);
    }

    #[test]
    fn serialize_config_requirements_read_without_params() {
        let request = ClientRequest::ConfigRequirementsRead {
            request_id: RequestId::Integer(6),
            params: None,
        };

        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["method"], "configRequirements/read");
        assert_eq!(value["id"], 6);
        assert!(
            value.get("params").is_none(),
            "configRequirements/read should omit params when unset"
        );
    }

    #[test]
    fn serialize_account_requests_include_expected_keys() {
        {
            let request = ClientRequest::LoginAccount {
                request_id: RequestId::Integer(7),
                params: v2::LoginAccountParams::ApiKey {
                    api_key: "secret".to_string(),
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "account/login/start");
            assert_eq!(value["id"], 7);
            assert_eq!(value["params"]["type"], "apiKey");
            assert_eq!(value["params"]["apiKey"], "secret");
        }

        {
            let request = ClientRequest::GetAccount {
                request_id: RequestId::Integer(8),
                params: v2::GetAccountParams {
                    refresh_token: false,
                },
            };

            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(value["method"], "account/read");
            assert_eq!(value["id"], 8);
            assert_eq!(value["params"]["refreshToken"], false);
        }
    }

    #[test]
    fn account_serializes_chatgpt_plan_type_in_camel_case() {
        let account = v2::Account::Chatgpt {
            email: "user@example.com".to_string(),
            plan_type: PlanType::Go,
        };

        let value = serde_json::to_value(&account).expect("serialize account");
        assert_eq!(value["type"], "chatgpt");
        assert_eq!(value["email"], "user@example.com");
        assert_eq!(value["planType"], "go");
    }

    #[test]
    fn decode_initialize_response_into_typed_client_response() {
        let request = ClientRequest::Initialize {
            request_id: RequestId::Integer(9),
            params: v1::InitializeParams {
                client_info: ClientInfo {
                    name: "codex-tui".to_string(),
                    title: None,
                    version: "0.121.0".to_string(),
                },
                capabilities: None,
            },
        };

        let typed = request
            .decode_response(JSONRPCResponse {
                id: RequestId::Integer(9),
                result: serde_json::json!({
                    "userAgent": "codex-app-server",
                    "platformFamily": "unix",
                    "platformOs": "macos",
                }),
            })
            .expect("decode response");

        assert!(matches!(
            typed,
            ClientResponse::Initialize {
                request_id: RequestId::Integer(9),
                response: v1::InitializeResponse {
                    user_agent,
                    platform_family,
                    platform_os,
                    ..
                },
            } if user_agent.as_deref() == Some("codex-app-server")
                && platform_family.as_deref() == Some("unix")
                && platform_os.as_deref() == Some("macos")
        ));
    }
}
