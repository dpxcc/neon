use std::{io, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::Full;
use hyper1::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{lookup_host, TcpStream};
use tracing::{field::display, info};

use crate::{
    auth::{
        backend::{local::StaticAuthRules, ComputeCredentials, ComputeUserInfo},
        check_peer_addr_is_in_list, AuthError,
    },
    compute,
    config::{AuthenticationConfig, ProxyConfig},
    console::{
        errors::{GetAuthInfoError, WakeComputeError},
        locks::ApiLocks,
        provider::ApiLockError,
        CachedNodeInfo,
    },
    context::RequestMonitoring,
    error::{ErrorKind, ReportableError, UserFacingError},
    intern::EndpointIdInt,
    proxy::{
        connect_compute::ConnectMechanism,
        retry::{CouldRetry, ShouldRetryWakeCompute},
    },
    rate_limiter::EndpointRateLimiter,
    Host,
};

use super::{
    conn_pool::{poll_client, Client, ConnInfo, GlobalConnPool},
    http_conn_pool::{self, poll_http2_client},
};

pub(crate) struct PoolingBackend {
    pub(crate) http_conn_pool: Arc<super::http_conn_pool::GlobalConnPool>,
    pub(crate) pool: Arc<GlobalConnPool<tokio_postgres::Client>>,
    pub(crate) config: &'static ProxyConfig,
    pub(crate) endpoint_rate_limiter: Arc<EndpointRateLimiter>,
}

impl PoolingBackend {
    pub(crate) async fn authenticate_with_password(
        &self,
        ctx: &RequestMonitoring,
        config: &AuthenticationConfig,
        user_info: &ComputeUserInfo,
        password: &[u8],
    ) -> Result<ComputeCredentials, AuthError> {
        let user_info = user_info.clone();
        let backend = self
            .config
            .auth_backend
            .as_ref()
            .map(|()| user_info.clone());
        let (allowed_ips, maybe_secret) = backend.get_allowed_ips_and_secret(ctx).await?;
        if config.ip_allowlist_check_enabled
            && !check_peer_addr_is_in_list(&ctx.peer_addr(), &allowed_ips)
        {
            return Err(AuthError::ip_address_not_allowed(ctx.peer_addr()));
        }
        if !self
            .endpoint_rate_limiter
            .check(user_info.endpoint.clone().into(), 1)
        {
            return Err(AuthError::too_many_connections());
        }
        let cached_secret = match maybe_secret {
            Some(secret) => secret,
            None => backend.get_role_secret(ctx).await?,
        };

        let secret = match cached_secret.value.clone() {
            Some(secret) => self.config.authentication_config.check_rate_limit(
                ctx,
                config,
                secret,
                &user_info.endpoint,
                true,
            )?,
            None => {
                // If we don't have an authentication secret, for the http flow we can just return an error.
                info!("authentication info not found");
                return Err(AuthError::auth_failed(&*user_info.user));
            }
        };
        let ep = EndpointIdInt::from(&user_info.endpoint);
        let auth_outcome =
            crate::auth::validate_password_and_exchange(&config.thread_pool, ep, password, secret)
                .await?;
        let res = match auth_outcome {
            crate::sasl::Outcome::Success(key) => {
                info!("user successfully authenticated");
                Ok(key)
            }
            crate::sasl::Outcome::Failure(reason) => {
                info!("auth backend failed with an error: {reason}");
                Err(AuthError::auth_failed(&*user_info.user))
            }
        };
        res.map(|key| ComputeCredentials {
            info: user_info,
            keys: key,
        })
    }

    pub(crate) async fn authenticate_with_jwt(
        &self,
        ctx: &RequestMonitoring,
        config: &AuthenticationConfig,
        user_info: &ComputeUserInfo,
        jwt: String,
    ) -> Result<ComputeCredentials, AuthError> {
        match &self.config.auth_backend {
            crate::auth::Backend::Console(console, ()) => {
                config
                    .jwks_cache
                    .check_jwt(
                        ctx,
                        user_info.endpoint.clone(),
                        &user_info.user,
                        &**console,
                        &jwt,
                    )
                    .await
                    .map_err(|e| AuthError::auth_failed(e.to_string()))?;
                Ok(ComputeCredentials {
                    info: user_info.clone(),
                    keys: crate::auth::backend::ComputeCredentialKeys::Jwt(jwt),
                })
            }
            crate::auth::Backend::Web(_, ()) => Err(AuthError::auth_failed(
                "JWT login over web auth proxy is not supported",
            )),
            crate::auth::Backend::Local(_) => {
                config
                    .jwks_cache
                    .check_jwt(
                        ctx,
                        user_info.endpoint.clone(),
                        &user_info.user,
                        &StaticAuthRules,
                        &jwt,
                    )
                    .await
                    .map_err(|e| AuthError::auth_failed(e.to_string()))?;
                Ok(ComputeCredentials {
                    info: user_info.clone(),
                    // todo: rewrite JWT signature with key shared somehow between local proxy and postgres
                    keys: crate::auth::backend::ComputeCredentialKeys::None,
                })
            }
        }
    }

    // Wake up the destination if needed. Code here is a bit involved because
    // we reuse the code from the usual proxy and we need to prepare few structures
    // that this code expects.
    #[tracing::instrument(fields(pid = tracing::field::Empty), skip_all)]
    pub(crate) async fn connect_to_compute(
        &self,
        ctx: &RequestMonitoring,
        conn_info: ConnInfo,
        keys: ComputeCredentials,
        force_new: bool,
    ) -> Result<Client<tokio_postgres::Client>, HttpConnError> {
        let maybe_client = if force_new {
            info!("pool: pool is disabled");
            None
        } else {
            info!("pool: looking for an existing connection");
            self.pool.get(ctx, &conn_info)?
        };

        if let Some(client) = maybe_client {
            return Ok(client);
        }
        let conn_id = uuid::Uuid::new_v4();
        tracing::Span::current().record("conn_id", display(conn_id));
        info!(%conn_id, "pool: opening a new connection '{conn_info}'");
        let backend = self.config.auth_backend.as_ref().map(|()| keys);
        crate::proxy::connect_compute::connect_to_compute(
            ctx,
            &TokioMechanism {
                conn_id,
                conn_info,
                pool: self.pool.clone(),
                locks: &self.config.connect_compute_locks,
            },
            &backend,
            false, // do not allow self signed compute for http flow
            self.config.wake_compute_retry_config,
            self.config.connect_to_compute_retry_config,
        )
        .await
    }

    // Wake up the destination if needed
    #[tracing::instrument(fields(pid = tracing::field::Empty), skip_all)]
    pub(crate) async fn connect_to_local_proxy(
        &self,
        ctx: &RequestMonitoring,
        conn_info: ConnInfo,
        keys: ComputeCredentials,
    ) -> Result<http_conn_pool::Client, HttpConnError> {
        info!("pool: looking for an existing connection");
        if let Some(client) = self.http_conn_pool.get(ctx, &conn_info) {
            return Ok(client);
        }

        let conn_id = uuid::Uuid::new_v4();
        tracing::Span::current().record("conn_id", display(conn_id));
        info!(%conn_id, "pool: opening a new connection '{conn_info}'");
        let backend = self.config.auth_backend.as_ref().map(|()| keys);
        crate::proxy::connect_compute::connect_to_compute(
            ctx,
            &HyperMechanism {
                conn_id,
                conn_info,
                pool: self.http_conn_pool.clone(),
                locks: &self.config.connect_compute_locks,
            },
            &backend,
            false, // do not allow self signed compute for http flow
            self.config.wake_compute_retry_config,
            self.config.connect_to_compute_retry_config,
        )
        .await
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpConnError {
    #[error("pooled connection closed at inconsistent state")]
    ConnectionClosedAbruptly(#[from] tokio::sync::watch::error::SendError<uuid::Uuid>),
    #[error("could not connection to compute")]
    ConnectionError(#[from] tokio_postgres::Error),
    #[error("could not connection to compute")]
    IoConnectionError(#[from] std::io::Error),
    #[error("could not establish h2 connection to compute")]
    H2ConnectionError(#[from] hyper1::Error),

    #[error("could not get auth info")]
    GetAuthInfo(#[from] GetAuthInfoError),
    #[error("user not authenticated")]
    AuthError(#[from] AuthError),
    #[error("wake_compute returned error")]
    WakeCompute(#[from] WakeComputeError),
    #[error("error acquiring resource permit: {0}")]
    TooManyConnectionAttempts(#[from] ApiLockError),
}

impl ReportableError for HttpConnError {
    fn get_error_kind(&self) -> ErrorKind {
        match self {
            HttpConnError::ConnectionClosedAbruptly(_) => ErrorKind::Compute,
            HttpConnError::ConnectionError(p) => p.get_error_kind(),
            HttpConnError::IoConnectionError(_) => ErrorKind::Compute,
            HttpConnError::H2ConnectionError(_) => ErrorKind::Compute,
            HttpConnError::GetAuthInfo(a) => a.get_error_kind(),
            HttpConnError::AuthError(a) => a.get_error_kind(),
            HttpConnError::WakeCompute(w) => w.get_error_kind(),
            HttpConnError::TooManyConnectionAttempts(w) => w.get_error_kind(),
        }
    }
}

impl UserFacingError for HttpConnError {
    fn to_string_client(&self) -> String {
        match self {
            HttpConnError::ConnectionClosedAbruptly(_) => self.to_string(),
            HttpConnError::ConnectionError(p) => p.to_string(),
            HttpConnError::IoConnectionError(p) => p.to_string(),
            HttpConnError::H2ConnectionError(_) => "Could not establish HTTP connection to the database".to_string(),
            HttpConnError::GetAuthInfo(c) => c.to_string_client(),
            HttpConnError::AuthError(c) => c.to_string_client(),
            HttpConnError::WakeCompute(c) => c.to_string_client(),
            HttpConnError::TooManyConnectionAttempts(_) => {
                "Failed to acquire permit to connect to the database. Too many database connection attempts are currently ongoing.".to_owned()
            }
        }
    }
}

impl CouldRetry for HttpConnError {
    fn could_retry(&self) -> bool {
        match self {
            HttpConnError::ConnectionError(e) => e.could_retry(),
            HttpConnError::IoConnectionError(e) => e.could_retry(),
            HttpConnError::H2ConnectionError(_) => false,
            HttpConnError::ConnectionClosedAbruptly(_) => false,
            HttpConnError::GetAuthInfo(_) => false,
            HttpConnError::AuthError(_) => false,
            HttpConnError::WakeCompute(_) => false,
            HttpConnError::TooManyConnectionAttempts(_) => false,
        }
    }
}
impl ShouldRetryWakeCompute for HttpConnError {
    fn should_retry_wake_compute(&self) -> bool {
        match self {
            HttpConnError::ConnectionError(e) => e.should_retry_wake_compute(),
            // we never checked cache validity
            HttpConnError::TooManyConnectionAttempts(_) => false,
            _ => true,
        }
    }
}

struct TokioMechanism {
    pool: Arc<GlobalConnPool<tokio_postgres::Client>>,
    conn_info: ConnInfo,
    conn_id: uuid::Uuid,

    /// connect_to_compute concurrency lock
    locks: &'static ApiLocks<Host>,
}

#[async_trait]
impl ConnectMechanism for TokioMechanism {
    type Connection = Client<tokio_postgres::Client>;
    type ConnectError = HttpConnError;
    type Error = HttpConnError;

    async fn connect_once(
        &self,
        ctx: &RequestMonitoring,
        node_info: &CachedNodeInfo,
        timeout: Duration,
    ) -> Result<Self::Connection, Self::ConnectError> {
        let host = node_info.config.get_host()?;
        let permit = self.locks.get_permit(&host).await?;

        let mut config = (*node_info.config).clone();
        let config = config
            .user(&self.conn_info.user_info.user)
            .dbname(&self.conn_info.dbname)
            .connect_timeout(timeout);

        let pause = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);
        let res = config.connect(tokio_postgres::NoTls).await;
        drop(pause);
        let (client, connection) = permit.release_result(res)?;

        tracing::Span::current().record("pid", tracing::field::display(client.get_process_id()));
        Ok(poll_client(
            self.pool.clone(),
            ctx,
            self.conn_info.clone(),
            client,
            connection,
            self.conn_id,
            node_info.aux.clone(),
        ))
    }

    fn update_connect_config(&self, _config: &mut compute::ConnCfg) {}
}

struct HyperMechanism {
    pool: Arc<http_conn_pool::GlobalConnPool>,
    conn_info: ConnInfo,
    conn_id: uuid::Uuid,

    /// connect_to_compute concurrency lock
    locks: &'static ApiLocks<Host>,
}

#[async_trait]
impl ConnectMechanism for HyperMechanism {
    type Connection = http_conn_pool::Client;
    type ConnectError = HttpConnError;
    type Error = HttpConnError;

    async fn connect_once(
        &self,
        ctx: &RequestMonitoring,
        node_info: &CachedNodeInfo,
        timeout: Duration,
    ) -> Result<Self::Connection, Self::ConnectError> {
        let host = node_info.config.get_host()?;
        let permit = self.locks.get_permit(&host).await?;

        let pause = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);

        // let port = node_info.config.get_ports().first().unwrap_or_else(10432);
        let res = connect_http2(&host, 10432, timeout).await;
        drop(pause);
        let (client, connection) = permit.release_result(res)?;

        Ok(poll_http2_client(
            self.pool.clone(),
            ctx,
            self.conn_info.clone(),
            client,
            connection,
            self.conn_id,
            node_info.aux.clone(),
        ))
    }

    fn update_connect_config(&self, _config: &mut compute::ConnCfg) {}
}

async fn connect_http2(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<
    (
        http2::SendRequest<Full<Bytes>>,
        http2::Connection<TokioIo<TcpStream>, Full<Bytes>, TokioExecutor>,
    ),
    HttpConnError,
> {
    let mut addrs = lookup_host((host, port)).await?;

    let mut last_err = None;

    let stream = loop {
        let Some(addr) = addrs.next() else {
            return Err(last_err.unwrap_or_else(|| {
                HttpConnError::IoConnectionError(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not resolve any addresses",
                ))
            }));
        };

        let stream = match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                last_err = Some(HttpConnError::IoConnectionError(e));
                continue;
            }
            Err(e) => {
                last_err = Some(HttpConnError::IoConnectionError(io::Error::new(
                    io::ErrorKind::TimedOut,
                    e,
                )));
                continue;
            }
        };

        stream.set_nodelay(true)?;

        break stream;
    };

    let (client, connection) = hyper1::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await?;

    Ok((client, connection))
}
