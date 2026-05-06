//! `auth.v2.AuthService` client — currently used only for the one-shot
//! `ClaimHubToken` exchange that turns hub-generated `(device_id, hub_secret)`
//! into a long-lived hub JWT.
//!
//! The RPC is **claim-once** server-side: a successful claim is recorded in the
//! `hub_tokens` row, and any retry returns `FAILED_PRECONDITION`. We surface
//! that as a distinct error variant so `main()` can halt with a useful message
//! (recovery requires the user to call `auth.v2.RevokeHub` from the mobile app).

use anyhow::{Context, Result};
use protos_rust::auth::v2::{auth_service_client::AuthServiceClient, ClaimHubTokenRequest};
use tonic::transport::Endpoint;
use tonic::Code;

const AUTH_CHANNEL_BUFFER_SIZE: usize = 4;
const AUTH_CONCURRENCY_LIMIT: usize = 1;
const AUTH_MAX_ENCODING_MESSAGE_SIZE: usize = 512;
const AUTH_MAX_DECODING_MESSAGE_SIZE: usize = 4096;

/// Distinct error: the device's claim slot is already consumed. Caller must
/// instruct the user to revoke the hub from the mobile app before retrying.
#[derive(Debug)]
pub struct AlreadyClaimed;

impl core::fmt::Display for AlreadyClaimed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(
            "ClaimHubToken returned FAILED_PRECONDITION: this device has already been claimed. \
             Have the user call auth.v2.RevokeHub from the mobile app, then reboot.",
        )
    }
}

impl std::error::Error for AlreadyClaimed {}

/// Exchange `(device_id, hub_secret)` for a hub JWT. Idempotent only on the
/// HAPPY first call — subsequent calls return [`AlreadyClaimed`].
pub async fn claim_hub_token(
    api_url: &'static str,
    device_id: &str,
    hub_secret: &str,
) -> Result<String> {
    let channel = Endpoint::from_static(api_url)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(20))
        .buffer_size(AUTH_CHANNEL_BUFFER_SIZE)
        .concurrency_limit(AUTH_CONCURRENCY_LIMIT)
        .connect()
        .await
        .context("connect to auth.v2.AuthService")?;

    let mut client = AuthServiceClient::new(channel)
        .max_encoding_message_size(AUTH_MAX_ENCODING_MESSAGE_SIZE)
        .max_decoding_message_size(AUTH_MAX_DECODING_MESSAGE_SIZE);

    let resp = client
        .claim_hub_token(ClaimHubTokenRequest {
            device_id: device_id.to_string(),
            hub_secret: hub_secret.to_string(),
        })
        .await;

    match resp {
        Ok(ok) => Ok(ok.into_inner().token),
        Err(status) if status.code() == Code::FailedPrecondition => Err(AlreadyClaimed.into()),
        Err(status) => Err(anyhow::anyhow!(status).context("auth.v2.ClaimHubToken RPC failed")),
    }
}
