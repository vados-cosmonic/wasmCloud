//! SQL-powered database access provider implementing `wasmcloud:postgres` for connecting
//! to Postgres clusters.
//!
//! This implementation is multi-threaded and operations between different actors
//! use different connections and can run in parallel.
//!

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use deadpool_postgres::Pool;
use futures::stream::TryStreamExt;
use tokio::sync::RwLock;
use tokio_postgres::Statement;
use tracing::{debug, error, instrument};
use ulid::Ulid;

use wasmcloud_provider_sdk::{
    get_connection, run_provider, LinkConfig, Provider, ProviderInitConfig,
};

mod bindings;
use bindings::{
    into_result_row, serve, ConnectionCreateOptions, ConnectionOptions, ConnectionToken,
    CreateConnectionError, PgValue, PreparedStatementExecError, PreparedStatementToken, QueryError,
    ResultRow, StatementPrepareError,
};

mod config;
use config::{parse_managed_config, parse_profile_configs};

use wasmcloud_provider_sdk::Context;

#[derive(Clone, Default)]
struct PostgresProvider {
    create_options: Arc<RwLock<HashMap<ConnectionToken, ConnectionCreateOptions>>>,
    connections: Arc<RwLock<HashMap<ConnectionToken, Pool>>>,
    prepared_statements: Arc<RwLock<HashMap<PreparedStatementToken, (Statement, ConnectionToken)>>>,
}

/// Run [`PostgresProvider`] as a wasmCloud provider
pub async fn run() -> anyhow::Result<()> {
    let provider = PostgresProvider::default();
    let shutdown = run_provider(provider.clone(), "sqldb-postgres-provider")
        .await
        .context("failed to run provider")?;
    let connection = get_connection();
    serve(
        &connection.get_wrpc_client(connection.provider_key()),
        provider,
        shutdown,
    )
    .await
}

impl PostgresProvider {
    /// Create and store a connection pool, if not already present
    async fn ensure_pool(
        &self,
        token: impl AsRef<str>,
        create_opts: ConnectionCreateOptions,
    ) -> Result<ConnectionToken> {
        let token = token.as_ref();
        // Exit early if a connection with the given token is already present
        {
            let connections = self.connections.read().await;
            if connections.get(token).is_some() {
                return Ok(token.into());
            }
        }

        // Build the new connection pool
        let runtime = Some(deadpool_postgres::Runtime::Tokio1);
        let tls_required = create_opts.tls_required;
        let cfg: deadpool_postgres::Config = create_opts.try_into()?;
        let pool = if tls_required {
            create_tls_pool(cfg, runtime)
        } else {
            cfg.create_pool(runtime, tokio_postgres::NoTls)
                .context("failed to create non-TLS postgres pool")
        }?;

        // Save the newly created connection to the pool
        let mut connections = self.connections.write().await;
        connections.insert(token.into(), pool);
        Ok(token.into())
    }

    /// Perform a query
    async fn do_query(
        &self,
        connection_token: &str,
        query: &str,
        params: Vec<PgValue>,
    ) -> Result<Vec<ResultRow>, QueryError> {
        let connections = self.connections.read().await;
        let pool = connections.get(connection_token).ok_or_else(|| {
            QueryError::Unexpected(format!(
                "missing connection pool for token [{connection_token}] while querying"
            ))
        })?;

        let client = pool.get().await.map_err(|e| {
            QueryError::Unexpected(format!("failed to build client from pool: {e}"))
        })?;

        let rows = client
            .query_raw(query, params)
            .await
            .map_err(|e| QueryError::Unexpected(format!("failed to perform query: {e}")))?;

        // todo(fix): once async stream support is available & in contract
        // replace this with a mapped stream
        rows.map_ok(into_result_row)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| QueryError::Unexpected(format!("failed to evaluate full row: {e}")))
    }

    /// Prepare a statement
    async fn do_statement_prepare(
        &self,
        connection_token: &str,
        query: &str,
    ) -> Result<PreparedStatementToken, StatementPrepareError> {
        let connections = self.connections.read().await;
        let pool = connections.get(connection_token).ok_or_else(|| {
            StatementPrepareError::Unexpected(format!(
                "failed to find connection pool for token [{connection_token}]"
            ))
        })?;

        let client = pool.get().await.map_err(|e| {
            StatementPrepareError::Unexpected(format!("failed to build client from pool: {e}"))
        })?;

        let statement = client.prepare(query).await.map_err(|e| {
            StatementPrepareError::Unexpected(format!("failed to prepare query: {e}"))
        })?;

        let statement_token = format!("prepared-statement-{}", Ulid::new().to_string());

        let mut prepared_statements = self.prepared_statements.write().await;
        prepared_statements.insert(
            statement_token.clone(),
            (statement, connection_token.into()),
        );

        Ok(statement_token)
    }

    /// Execute a prepared statement, returning the number of rows affected
    async fn do_statement_execute(
        &self,
        statement_token: &str,
        params: Vec<PgValue>,
    ) -> Result<u64, PreparedStatementExecError> {
        let statements = self.prepared_statements.read().await;
        let (statement, connection_token) = statements.get(statement_token).ok_or_else(|| {
            PreparedStatementExecError::Unexpected(format!(
                "missing prepared statement with statement ID [{statement_token}]"
            ))
        })?;

        let connections = self.connections.read().await;
        let pool = connections.get(connection_token).ok_or_else(|| {
            PreparedStatementExecError::Unexpected(format!(
                "missing connection pool for token [{connection_token}], statement ID [{statement_token}]"
            ))
        })?;
        let client = pool.get().await.map_err(|e| {
            PreparedStatementExecError::Unexpected(format!("failed to build client from pool: {e}"))
        })?;

        let rows_affected = client.execute_raw(statement, params).await.map_err(|e| {
            PreparedStatementExecError::Unexpected(format!(
                "failed to execute prepared statement with token [{statement_token}]: {e}"
            ))
        })?;

        Ok(rows_affected)
    }

    /// Ingest all connection configs (managed/profile) that are specified in a given config map
    ///
    /// This method will populate `self.create_options` with configuration for connections that
    /// will be made (usually as late as possible, at query time)
    async fn ingest_connection_configs_from_map(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<()> {
        let managed_config = parse_managed_config(config);
        let profile_configs = parse_profile_configs(config);

        // We can return early if there were no configs to parse
        if managed_config.is_none() && profile_configs.is_empty() {
            return Ok(());
        }

        let mut create_options = self.create_options.write().await;
        if let Some(config) = managed_config {
            debug!("ingested managed config");
            create_options.insert(managed_connection_token_id(None), config);
        }
        for (profile_name, config) in profile_configs {
            debug!("ingested named profile config [{profile_name}]");
            create_options.insert(
                named_profile_connection_token_id(&profile_name, None),
                config,
            );
        }
        Ok(())
    }

    /// Create a managed connection config for a given source (usually a linked component)
    ///
    /// Usually this means making a copy of the existing managed connection, specialized for
    /// particular source.
    async fn create_managed_connection_config_for_source(&self, source_id: &str) -> Result<()> {
        let managed_token = managed_connection_token_id(None);
        // Get the options for the default managed connection
        let create_options = self
            .create_options
            .read()
            .await
            .get(&managed_token)
            .cloned()
            .with_context(|| {
                format!("failed to find connection options for token [{managed_token}]")
            })?;

        // Create a pool if one isn't already present for this particular source,
        // based on the managed one
        self.ensure_pool(managed_connection_token_id(Some(source_id)), create_options)
            .await?;

        Ok(())
    }

    /// Create a named connection config for a given source (usually a linked component)
    ///
    /// Normally this means making a copy of existing named profile connection configuration
    /// for a given source.
    async fn create_named_profile_connection_config_for_source(
        &self,
        profile_name: &str,
        source_id: &str,
    ) -> Result<()> {
        let existing = {
            let connections = self.connections.read().await;
            connections
                .get(&named_profile_connection_token_id(profile_name, None))
                .with_context(|| format!("no existing named profile [{profile_name}] connection (was initial config specified?)"))?
                .clone()
        };

        // todo(feat): enable overriding configuration
        let mut connections = self.connections.write().await;
        connections.insert(
            named_profile_connection_token_id(profile_name, Some(source_id)),
            existing,
        );
        Ok(())
    }
}

impl Provider for PostgresProvider {
    /// Initialize the provider
    ///
    /// This method is only run *once* at provider startup, and has access to configuration
    /// that was provided to the provider, prior to any links being established.
    ///
    /// Since this provider is expected to receive configuration for managed and named profile
    /// connections *before* connections are made, gathering of those configurations should be done here.
    #[instrument(level = "debug", skip_all)]
    async fn init(&self, init_config: impl ProviderInitConfig) -> anyhow::Result<()> {
        let cfg = init_config.get_config();
        self.ingest_connection_configs_from_map(cfg)
            .await
            .context("failed to ingest configurations from startup config")?;
        Ok(())
    }

    /// Handle being linked to a source (likely a component) as a target
    ///
    /// Managed and named profile connections are built at provider initialization, but if per-component
    /// connections are enabled (the default), then each component will get it's own pool, *based* on the
    /// managed profile selected.
    ///
    /// Completely unmanaged connections (where the component must specify all the connection options)
    /// are handled as connections are made/destroyed by clients.
    #[instrument(level = "debug", skip_all, fields(source_id))]
    async fn receive_link_config_as_target(
        &self,
        LinkConfig {
            source_id, config, ..
        }: LinkConfig<'_>,
    ) -> anyhow::Result<()> {
        // If the component specified a named profile, use it
        if let Some(profile_name) = config.get("CONNECTION_PROFILE") {
            if let Err(error) = self
                .create_named_profile_connection_config_for_source(profile_name, source_id)
                .await
            {
                error!(
                    ?error,
                    source_id, profile_name, "failed to create named connection config"
                );
            }
        }

        // Create an instance of the managed connection
        //
        // todo(feat): we might be able to check for the interface here (`managed-query`?)
        if let Err(error) = self
            .create_managed_connection_config_for_source(source_id)
            .await
        {
            error!(
                ?error,
                source_id, "failed to create managed connection config",
            );
        };
        Ok(())
    }

    /// Handle notification that a link is dropped
    ///
    /// Generally we can release the resources (connections) associated with the source
    #[instrument(level = "debug", skip(self))]
    async fn delete_link(&self, source_id: &str) -> anyhow::Result<()> {
        let token = managed_connection_token_id(Some(source_id));
        let mut prepared_statements = self.prepared_statements.write().await;
        prepared_statements
            .retain(|_stmt_token, (_conn, connection_token)| *connection_token != token);
        drop(prepared_statements);
        let mut connections = self.connections.write().await;
        connections.remove(&token);
        drop(connections);
        Ok(())
    }

    /// Handle shutdown request by closing all connections
    #[instrument(level = "debug", skip(self))]
    async fn shutdown(&self) -> anyhow::Result<()> {
        let mut prepared_statements = self.prepared_statements.write().await;
        prepared_statements.drain();
        let mut connections = self.connections.write().await;
        connections.drain();
        Ok(())
    }
}

/// Implement the `wasmcloud:postgres/connection` interface for [`PostgresProvider`]
impl bindings::connection::Handler<Option<Context>> for PostgresProvider {
    #[instrument(level = "debug", skip_all, fields(connection_token, query))]
    async fn create_connection(
        &self,
        ctx: Option<Context>,
        opts: ConnectionOptions,
    ) -> Result<Result<ConnectionToken, CreateConnectionError>> {
        // Get the source ID of the incoming connection
        let source_id = ctx.and_then(|c| c.component).ok_or_else(|| {
            CreateConnectionError::Unexpected("failed to get source ID from context".into())
        })?;

        // Generate the connection token for the connection we'll create
        let token = opts.connection_token(&source_id);

        // Return early if a pool already exists
        {
            let connections = self.connections.read().await;
            if connections.get(&token).is_some() {
                return Ok(Ok(token));
            }
        }

        let lookup = self.create_options.read().await;
        let create_connection_opts = match opts {
            // Managed connections should *already* have config specified, via MANAGED_* named config values
            ConnectionOptions::Managed => {
                lookup
                    .get(&token)
                    .ok_or_else(|| CreateConnectionError::Unexpected("missing managed connection details, were MANAGED_* named config values specified?".into()))?
            }
            // Managed connections should *already* have config specified, via PROFILE_<name>_* named config values
            ConnectionOptions::NamedProfile(name) => {
                lookup
                    .get(&token)
                    .ok_or_else(|| CreateConnectionError::Unexpected(format!("missing profile connection details, were PROFILE_{name}_* named config values specified?")))?
            }
            // Manual connections must be made on the spot (if they haven't already been),
            // so we can just pass through what we've been given
            ConnectionOptions::Manual(ref opts) => opts,
        };

        // Ensure the pool exists
        let token = self
            .ensure_pool(token, create_connection_opts.clone())
            .await
            .map_err(|e| {
                CreateConnectionError::Unexpected(format!("failed to create pool: {e}"))
            })?;

        Ok(Ok(token))
    }
}

/// Implement the `wasmcloud:postgres/managed-query` interface for [`PostgresProvider`]
impl bindings::managed_query::Handler<Option<Context>> for PostgresProvider {
    #[instrument(level = "debug", skip_all, fields(connection_token, query))]
    async fn query(
        &self,
        ctx: Option<Context>,
        query: String,
        params: Vec<PgValue>,
    ) -> Result<Result<Vec<ResultRow>, QueryError>> {
        let source_id = ctx
            .and_then(|c| c.component)
            .ok_or_else(|| QueryError::Unexpected("failed to get source ID from context".into()))?;
        let token = managed_connection_token_id(Some(&source_id));
        Ok(self.do_query(&token, &query, params).await)
    }
}

/// Implement the `wasmcloud:postgres/query` interface for [`PostgresProvider`]
impl bindings::query::Handler<Option<Context>> for PostgresProvider {
    #[instrument(level = "debug", skip_all, fields(connection_token, query))]
    async fn query(
        &self,
        _ctx: Option<Context>,
        connection_token: ConnectionToken,
        query: String,
        params: Vec<PgValue>,
    ) -> Result<Result<Vec<ResultRow>, QueryError>> {
        Ok(self.do_query(&connection_token, &query, params).await)
    }
}

/// Implement the `wasmcloud:postgres/prepared` interface for [`PostgresProvider`]
impl bindings::prepared::Handler<Option<Context>> for PostgresProvider {
    #[instrument(level = "debug", skip_all, fields(connection_token, query))]
    async fn prepare(
        &self,
        _ctx: Option<Context>,
        connection_token: ConnectionToken,
        query: String,
    ) -> Result<Result<PreparedStatementToken, StatementPrepareError>> {
        Ok(self.do_statement_prepare(&connection_token, &query).await)
    }

    async fn exec(
        &self,
        _ctx: Option<Context>,
        statement_token: PreparedStatementToken,
        params: Vec<PgValue>,
    ) -> Result<Result<u64, PreparedStatementExecError>> {
        Ok(self.do_statement_execute(&statement_token, params).await)
    }
}

/// Prefix used for managed connections, normally combined with the source-id of the connection
const MANAGED_CONNECTION_TOKEN_PREFIX: &str = "managed-";
/// Generate the token for a managed connection, optionally specialized with a source_id
pub(crate) fn managed_connection_token_id(source_id: Option<&str>) -> String {
    match source_id {
        None => "managed".into(),
        Some(source_id) => {
            let source_id = source_id.trim().to_lowercase();
            format!("{MANAGED_CONNECTION_TOKEN_PREFIX}{source_id}")
        }
    }
}

/// Prefix used for named profile connections, normally combined with the source-id and the profile of the
/// connection
const NAMED_PROFILE_CONNECTION_TOKEN_PREFIX: &str = "profile-";

/// Generate the token for a named profile conection, optionally specialized with a source_id
pub(crate) fn named_profile_connection_token_id(name: &str, source_id: Option<&str>) -> String {
    let name = name.trim().to_lowercase();
    match source_id {
        None => format!("{NAMED_PROFILE_CONNECTION_TOKEN_PREFIX}{name}"),
        Some(source_id) => {
            let source_id = source_id.trim().to_lowercase();
            format!("{NAMED_PROFILE_CONNECTION_TOKEN_PREFIX}{name}-{source_id}")
        }
    }
}

#[cfg(feature = "rustls")]
fn create_tls_pool(
    cfg: deadpool_postgres::Config,
    runtime: Option<deadpool_postgres::Runtime>,
) -> Result<Pool> {
    cfg.create_pool(
        runtime,
        tokio_postgres_rustls::MakeRustlsConnect::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        ),
    )
    .context("failed to create TLS-enabled connection pool")
}

#[cfg(not(feature = "rustls"))]
fn create_tls_pool(
    _cfg: deadpool_postgres::Config,
    _runtime: Option<deadpool_postgres::Runtime>,
) -> Result<Pool> {
    anyhow::bail!("cannot build TLS connections without rustls feature")
}
