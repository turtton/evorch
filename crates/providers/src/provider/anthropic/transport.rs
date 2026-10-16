//! Keep API and subscription transports distinct at the HTTP boundary.

use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ProviderError;
use crate::http::{map_request_error, map_response_error};
use crate::provider::claude::{oauth, transport as claude};

pub(super) enum PreparedRequest {
    Api(reqwest::Client, reqwest::Request),
    Claude(wreq::Client, wreq::Request),
}

impl PreparedRequest {
    pub fn api(builder: reqwest::RequestBuilder) -> Result<Self, ProviderError> {
        let (client, request) = builder.build_split();
        Ok(Self::Api(client, request.map_err(map_request_error)?))
    }

    pub fn claude(builder: wreq::RequestBuilder) -> Result<Self, ProviderError> {
        let (client, request) = builder.build_split();
        Ok(Self::Claude(
            client,
            request.map_err(claude::map_transport_error)?,
        ))
    }

    pub async fn execute(self) -> Result<HttpResponse, ProviderError> {
        match self {
            Self::Api(client, request) => client
                .execute(request)
                .await
                .map(HttpResponse::Api)
                .map_err(map_request_error),
            Self::Claude(client, request) => client
                .execute(request)
                .await
                .map(HttpResponse::Claude)
                .map_err(claude::map_transport_error),
        }
    }
}

pub(super) enum HttpResponse {
    Api(reqwest::Response),
    Claude(wreq::Response),
}

impl HttpResponse {
    pub fn request_id(&self) -> Option<&str> {
        let headers = match self {
            Self::Api(response) => response.headers(),
            Self::Claude(response) => response.headers(),
        };
        headers.get("request-id")?.to_str().ok()
    }

    pub fn is_success(&self) -> bool {
        match self {
            Self::Api(response) => response.status().is_success(),
            Self::Claude(response) => response.status().is_success(),
        }
    }

    pub async fn error(self) -> ProviderError {
        match self {
            Self::Api(response) => map_response_error(response).await,
            Self::Claude(response) => claude::response_error(response).await,
        }
    }

    pub async fn json<T: DeserializeOwned>(self) -> Result<T, ProviderError> {
        match self {
            Self::Api(response) => {
                response
                    .json()
                    .await
                    .map_err(|error| ProviderError::InvalidJson {
                        detail: error.to_string(),
                    })
            }
            Self::Claude(response) => {
                response
                    .json()
                    .await
                    .map_err(|_| ProviderError::InvalidJson {
                        detail: "invalid Claude response JSON".into(),
                    })
            }
        }
    }

    pub async fn bounded_json(self) -> Result<Value, ProviderError> {
        match self {
            Self::Api(response) => oauth::bounded_json(response)
                .await
                .map_err(|_| invalid_models()),
            Self::Claude(response) => claude::bounded_json(response)
                .await
                .map_err(|_| invalid_models()),
        }
    }

    pub fn bytes_stream(self) -> Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>> {
        match self {
            Self::Api(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(map_request_error)),
            ),
            Self::Claude(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(claude::map_transport_error)),
            ),
        }
    }
}

fn invalid_models() -> ProviderError {
    ProviderError::InvalidJson {
        detail: "invalid Anthropic models response".into(),
    }
}
