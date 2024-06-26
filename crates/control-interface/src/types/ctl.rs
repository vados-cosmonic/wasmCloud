//! Data types used when interacting with the control interface of a wasmCloud lattice

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ComponentId;

/// A control interface response that wraps a response payload, a success flag, and a message
/// with additional context if necessary.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CtlResponse<T> {
    /// Whether the request succeeded
    pub success: bool,
    /// A message with additional context about the response
    pub message: String,
    /// The response data, if any
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<T>,
}

impl<T> CtlResponse<T> {
    pub fn ok(response: T) -> Self {
        CtlResponse {
            success: true,
            message: String::new(),
            response: Some(response),
        }
    }
}

impl CtlResponse<()> {
    /// Helper function to return a successful response without
    /// a message or a payload.
    #[must_use]
    pub fn success() -> Self {
        CtlResponse {
            success: true,
            message: String::new(),
            response: None,
        }
    }

    /// Helper function to return an unsuccessful response with
    /// a message but no payload. Note that this implicitly is
    /// typing the inner payload as `()` for efficiency.
    #[must_use]
    pub fn error(message: &str) -> Self {
        CtlResponse {
            success: false,
            message: message.to_string(),
            response: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScaleComponentCommand {
    /// Image reference for the component.
    #[serde(default)]
    pub component_ref: String,
    /// Unique identifier of the component to scale.
    pub component_id: ComponentId,
    /// Optional set of annotations used to describe the nature of this component scale command. For
    /// example, autonomous agents may wish to "tag" scale requests as part of a given deployment
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
    /// The maximum number of concurrent executing instances of this component. Setting this to `0` will
    /// stop the component.
    // NOTE: renaming to `count` lets us remain backwards compatible for a few minor versions
    #[serde(default, alias = "count", rename = "count")]
    pub max_instances: u32,
    /// Host ID on which to scale this component
    #[serde(default)]
    pub host_id: String,
    /// A list of named configs to use for this component. It is not required to specify a config.
    /// Configs are merged together before being given to the component, with values from the right-most
    /// config in the list taking precedence. For example, given ordered configs foo {a: 1, b: 2},
    /// bar {b: 3, c: 4}, and baz {c: 5, d: 6}, the resulting config will be: {a: 1, b: 3, c: 5, d:
    /// 6}
    #[serde(default)]
    pub config: Vec<String>,
    #[serde(default)]
    /// Whether to perform an update if the details of the component (ex. component ID) change as
    /// part of the scale request.
    ///
    /// Normally this is implemented by the receiver (ex. wasmcloud host) as a *separate* update component call
    /// being made shortly after this command (scale) is processed.
    pub allow_update: bool,
}

/// A command sent to a host requesting a capability provider be started with the
/// given link name and optional configuration.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StartProviderCommand {
    /// Optional set of annotations used to describe the nature of this provider start command. For
    /// example, autonomous agents may wish to "tag" start requests as part of a given deployment
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
    /// Unique identifier of the provider to start.
    pub provider_id: ComponentId,
    /// A list of named configs to use for this provider. It is not required to specify a config.
    /// Configs are merged together before being given to the provider, with values from the right-most
    /// config in the list taking precedence. For example, given ordered configs foo {a: 1, b: 2},
    /// bar {b: 3, c: 4}, and baz {c: 5, d: 6}, the resulting config will be: {a: 1, b: 3, c: 5, d:
    /// 6}
    #[serde(default)]
    pub config: Vec<String>,
    /// The host ID on which to start the provider
    #[serde(default)]
    pub host_id: String,
    /// The image reference of the provider to be started
    #[serde(default)]
    pub provider_ref: String,
}

/// A command sent to request that the given host purge and stop
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StopHostCommand {
    /// The ID of the target host
    #[serde(default)]
    pub host_id: String,
    /// An optional timeout, in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// A request to stop the given provider on the indicated host
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StopProviderCommand {
    /// Host ID on which to stop the provider
    #[serde(default)]
    pub host_id: String,
    /// Unique identifier for the provider to stop.
    #[serde(default, alias = "provider_ref")]
    pub provider_id: ComponentId,
}

/// A command instructing a specific host to perform a live update
/// on the indicated component by supplying a new image reference. Note that
/// live updates are only possible through image references
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateComponentCommand {
    /// The component's 56-character unique ID
    #[serde(default)]
    pub component_id: ComponentId,
    /// Optional set of annotations used to describe the nature of this
    /// update request. Only component instances that have matching annotations
    /// will be upgraded, allowing for instance isolation by
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
    /// The host ID of the host to perform the live update
    #[serde(default)]
    pub host_id: String,
    /// The new image reference of the upgraded version of this component
    #[serde(default)]
    pub new_component_ref: String,
}
