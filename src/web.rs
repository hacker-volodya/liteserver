use std::sync::Arc;

use anyhow::Result;
use axum::{
    body::Body, extract::State, http::{Response, StatusCode}, response::IntoResponse, routing::get, Router
};
use metrics_exporter_prometheus::{PrometheusHandle, PrometheusRecorder};
use prometheus::{Encoder, TextEncoder};
use ton_indexer::Engine;

pub async fn run(engine: Arc<Engine>, recorder: PrometheusHandle) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(get_metrics))
        .with_state(Arc::new(AppState { engine, recorder }));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ok(())
}

struct AppState {
    engine: Arc<Engine>,
    recorder: PrometheusHandle,
}

async fn get_metrics(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    let metrics = prometheus::gather();
    encoder.encode(&metrics, &mut buffer)?;
    let mut response = String::from_utf8(buffer)?;
    response += "\n\n# engine metrics\n\n";
    response += &state.recorder.render();
    Ok(response)
}

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response<Body> {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
