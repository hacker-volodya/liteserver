use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer};
use tracing_subscriber::fmt::format::FmtSpan;


pub fn init() {
    let jaeger_layer = if let Ok(agent_endpoint) = std::env::var("JAEGER_AGENT_ENDPOINT") {
        Some(tracing_opentelemetry::layer().with_tracer(opentelemetry_jaeger::new_pipeline()
        .with_service_name("liteserver")
        .with_agent_endpoint(agent_endpoint)
        .install_simple().expect("jaeger tracer must initialize")))
    } else { None };

    let stdout_layer = tracing_subscriber::fmt::layer().with_span_events(FmtSpan::CLOSE).with_filter(
        EnvFilter::builder()
            .with_default_directive(tracing::Level::INFO.into())
            .from_env_lossy(),
    );

    if let Some(jaeger_layer) = jaeger_layer {
        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(jaeger_layer)
            .try_init().expect("Failed to register tracer with registry");
    } else {
        tracing_subscriber::registry()
            .with(stdout_layer)
            .try_init().expect("Failed to register tracer with registry");
    }
}

pub fn shutdown() {
    opentelemetry::global::shutdown_tracer_provider();
}