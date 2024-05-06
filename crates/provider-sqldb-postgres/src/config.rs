use std::collections::HashMap;

use tracing::{debug, warn};

use crate::bindings::ConnectionCreateOptions;

const POSTGRES_DEFAULT_PORT: u16 = 5432;

fn parse_prefixed_config_from_map(
    prefix: &str,
    config: &HashMap<String, String>,
) -> Option<ConnectionCreateOptions> {
    let keys = [
        format!("{prefix}HOST"),
        format!("{prefix}PORT"),
        format!("{prefix}USERNAME"),
        format!("{prefix}PASSWORD"),
        format!("{prefix}DATABASE"),
        format!("{prefix}TLS_REQUIRED"),
    ];
    // todo(refactor): once host can standardize keys, use it (likely lowercase)
    match keys
        .iter()
        .map(|k| config.get(k))
        .collect::<Vec<Option<&String>>>()[..]
    {
        [Some(host), Some(port), Some(username), Some(password), Some(database), Some(tls_required)] => {
            Some(ConnectionCreateOptions {
                host: host.to_string(),
                port: port.parse::<u16>().unwrap_or_else(|_e| {
                    warn!("invalid port value [{port}], using {POSTGRES_DEFAULT_PORT}");
                    POSTGRES_DEFAULT_PORT
                }),
                username: username.to_string(),
                password: password.to_string(),
                tls_required: matches!(tls_required.to_lowercase().as_str(), "true" | "yes"),
                database: database.to_string(),
            })
        }
        _ => {
            warn!("failed to find keys in configuration: [{:?}]", keys);
            None
        }
    }
}

/// Attempt to parse the only managed from a given config map
pub(crate) fn parse_managed_config(
    config: &HashMap<String, String>,
) -> Option<ConnectionCreateOptions> {
    debug!("parsing managed config");
    parse_prefixed_config_from_map("MANAGED_", config)
}

/// Attempt to parse profile configurations from a config map,
/// Returning the profiles names along with the create options
pub(crate) fn parse_profile_configs(
    config: &HashMap<String, String>,
) -> Vec<(String, ConnectionCreateOptions)> {
    config
        .keys()
        .filter_map(|s| match s.split('_').collect::<Vec<_>>()[..] {
            ["PROFILE", name, ..] => {
                debug!("parsing named profile config [{name}]");
                parse_prefixed_config_from_map(format!("PROFILE_{name}_").as_str(), config)
                    .map(|cfg| (name.to_string(), cfg))
            }
            _ => None,
        })
        .collect::<Vec<(String, ConnectionCreateOptions)>>()
}
