//! Authenticated main-API controls; the isolated preview listener is a separate service.

use axum::body::Body;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use serde::Serialize;

use super::respond::match_param;
use super::{error_body, json, read_body, MAX_BODY};
use crate::artifacts::{ArtifactError, CreateArtifactPreviewRequest};
use crate::contract::AppState;

const COLLECTION: &str = "/api/artifact-previews";
const PREFIX: &str = "/api/artifact-previews/";

pub enum Target {
    Create,
    Source(String),
    Close(String),
}

impl Target {
    pub fn parse(method: &Method, path: &str) -> Option<Self> {
        match *method {
            Method::POST if path == COLLECTION => Some(Self::Create),
            Method::GET => match_param(path, PREFIX, "/source").map(Self::Source),
            Method::DELETE => match_param(path, PREFIX, "").map(Self::Close),
            _ => None,
        }
    }
}

/// Called only after the main API token gate. Only creates read a body.
pub async fn route(state: &AppState, target: Target, body: Body) -> Response {
    let request = if matches!(target, Target::Create) {
        let bytes = match read_body(body, MAX_BODY).await {
            Ok(bytes) => bytes,
            Err(response) => return response,
        };
        match serde_json::from_slice::<CreateArtifactPreviewRequest>(&bytes) {
            Ok(request) => Some(request),
            Err(_) => {
                return respond::<()>(Err(ArtifactError::Invalid(
                    "invalid artifact preview request".into(),
                )));
            }
        }
    } else {
        None
    };
    let Some(service) = &state.services.artifacts else {
        return respond::<()>(Err(ArtifactError::Unavailable(
            "artifact previews are unavailable".into(),
        )));
    };
    match target {
        Target::Create => respond(service.create(request.expect("create body parsed")).await),
        Target::Source(id) => respond(service.source(id).await),
        Target::Close(id) => respond(service.close(id).await),
    }
}

fn respond<T: Serialize>(result: Result<T, ArtifactError>) -> Response {
    match result {
        Ok(body) => json(StatusCode::OK, &body),
        Err(error) => json(
            StatusCode::from_u16(error.status_code()).expect("artifact error status"),
            &error_body(&error.to_string()),
        ),
    }
}
