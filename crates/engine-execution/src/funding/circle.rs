//! Circle Onramp Kit session creation.
//!
//! Wire format follows Circle's published Onramp Kit server SDK. The hosted
//! widget owns the live asset catalog and rejects unsupported destinations.

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const SESSION_PATH: &str = "/v1/stablecoinKits/sessions";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CircleEnvironment {
    Sandbox,
    Production,
}

impl CircleEnvironment {
    fn endpoints(self) -> (&'static str, &'static str) {
        match self {
            Self::Sandbox => (
                "https://api-test.circle.com",
                "https://onramp-sandbox.arc.io",
            ),
            Self::Production => ("https://api.circle.com", "https://onramp.arc.io"),
        }
    }
}

#[derive(Debug, Error)]
pub enum CircleError {
    #[error("Circle Onramp API key is missing")]
    MissingKey,
    #[error("Circle session input is missing: {0}")]
    MissingField(&'static str),
    #[error("Circle request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Circle rejected the session with HTTP {0}")]
    Rejected(reqwest::StatusCode),
    #[error("Circle returned an unusable session")]
    InvalidSession,
    #[error("Circle returned a widget URL outside its configured origin")]
    InvalidWidgetOrigin,
    #[error("Circle URL could not be parsed")]
    InvalidUrl(#[from] url::ParseError),
}

#[derive(Clone)]
pub struct CircleOnrampClient {
    http: Client,
    api_key: String,
    api_base: Url,
    widget_base: Url,
}

#[derive(Clone)]
pub struct CreateSession<'a> {
    pub app_user_id: &'a str,
    pub destination_address: &'a str,
    /// Circle validates this against its current catalog. "BASE" requests
    /// direct delivery to Base, with no bridge prescribed by this engine.
    pub destination_chain: &'a str,
    /// The web domain registered for the app in the Circle Console. Circle needs it before it
    /// offers debit cards, Apple Pay and Google Pay.
    pub referrer_domain: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionRequest<'a> {
    app_user_id: &'a str,
    wallet_address: &'a str,
    destination_chain: &'a str,
    trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    referrer_domain: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamSession {
    session_token: Option<String>,
    session_id: Option<String>,
    expires_at: Option<String>,
    widget_url: Option<String>,
    trace_id: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CircleSession {
    pub session_token: Option<String>,
    pub session_id: Option<String>,
    pub expires_at: Option<String>,
    pub widget_url: String,
    pub destination_wallet: String,
    pub trace_id: String,
}

impl CircleOnrampClient {
    pub fn new(
        api_key: impl Into<String>,
        environment: CircleEnvironment,
    ) -> Result<Self, CircleError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(CircleError::MissingKey);
        }
        let (api_base, widget_base) = environment.endpoints();
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            api_key,
            api_base: Url::parse(api_base)?,
            widget_base: Url::parse(widget_base)?,
        })
    }

    pub async fn create_session(
        &self,
        input: CreateSession<'_>,
    ) -> Result<CircleSession, CircleError> {
        if input.app_user_id.trim().is_empty() {
            return Err(CircleError::MissingField("app_user_id"));
        }
        if input.destination_address.trim().is_empty() {
            return Err(CircleError::MissingField("destination_address"));
        }
        if input.destination_chain.trim().is_empty() {
            return Err(CircleError::MissingField("destination_chain"));
        }

        let trace_id = Uuid::new_v4().to_string();
        let body = SessionRequest {
            app_user_id: input.app_user_id,
            wallet_address: input.destination_address,
            destination_chain: input.destination_chain,
            trace_id: trace_id.clone(),
            referrer_domain: input.referrer_domain,
        };
        let url = self.api_base.join(SESSION_PATH)?;
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .header("Cache-Control", "no-store")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(CircleError::Rejected(response.status()));
        }
        let value: serde_json::Value = response.json().await?;
        let payload = value.get("data").unwrap_or(&value).clone();
        let session: UpstreamSession =
            serde_json::from_value(payload).map_err(|_| CircleError::InvalidSession)?;
        let widget_url = match session.widget_url.as_deref() {
            Some(raw) => Url::parse(raw)?,
            None => {
                let token = session
                    .session_token
                    .as_deref()
                    .ok_or(CircleError::InvalidSession)?;
                let mut url = self.widget_base.clone();
                url.query_pairs_mut().append_pair("sessionToken", token);
                url
            }
        };
        if widget_url.origin() != self.widget_base.origin() {
            return Err(CircleError::InvalidWidgetOrigin);
        }
        let mut widget_url = widget_url;
        widget_url.query_pairs_mut().append_pair("tokens", "USDC");
        widget_url
            .query_pairs_mut()
            .append_pair("chains", &input.destination_chain.to_ascii_lowercase());
        Ok(CircleSession {
            session_token: session.session_token,
            session_id: session.session_id,
            expires_at: session.expires_at,
            widget_url: widget_url.into(),
            destination_wallet: input.destination_address.to_owned(),
            trace_id: session.trace_id.unwrap_or(trace_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_session_uses_circles_wallets_api_fields() {
        let body = SessionRequest {
            app_user_id: "user-1",
            wallet_address: "0x123",
            destination_chain: "BASE",
            trace_id: "trace-1".into(),
            referrer_domain: Some("atlas.example"),
        };
        let value = serde_json::to_value(body).unwrap();
        assert_eq!(value["destinationChain"], "BASE");
        assert_eq!(value["walletAddress"], "0x123");
        assert!(value.get("destinationAddress").is_none());
        assert_eq!(value["referrerDomain"], "atlas.example");
    }

    #[test]
    fn environments_keep_api_and_widget_on_matching_networks() {
        assert_eq!(
            CircleEnvironment::Sandbox.endpoints(),
            (
                "https://api-test.circle.com",
                "https://onramp-sandbox.arc.io"
            )
        );
        assert_eq!(
            CircleEnvironment::Production.endpoints(),
            ("https://api.circle.com", "https://onramp.arc.io")
        );
    }
}
