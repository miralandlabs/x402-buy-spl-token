use {
    axum::{
        extract::{Query, State},
        http::{HeaderMap, Method, StatusCode},
        response::IntoResponse,
        routing::get,
        Router,
    },
    std::{collections::HashMap, sync::Arc},
    tower_http::cors::{Any, CorsLayer},
    tower_http::trace::TraceLayer,
    x402_buy_spl_token::{
        axum_adapter::into_axum,
        config::Config,
        cors::ALLOW_HEADERS,
        handler,
        init::{cold_start, init_tracing},
        AppState,
    },
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();

    let config = Config::from_env()?;
    let listen = config.listen_addr.clone();

    let state = cold_start(&config).await.map_err(|e| {
        format!("cold-start failed: {}", e)
    })?;
    let shared = Arc::new(state);

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::OPTIONS])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::HeaderName::from_static("payment-signature"),
            axum::http::header::HeaderName::from_static("payment-required"),
            axum::http::header::HeaderName::from_static("payment-response"),
            axum::http::header::HeaderName::from_static("x-api-version"),
            axum::http::header::HeaderName::from_static("x-correlation-id"),
        ]);

    let app = Router::new()
        .route("/api/v1/buy-spl-token", get(buy_spl_token).options(cors_options))
        .route(
            "/api/v1/buy-spl-token/intent-contract",
            get(intent_contract).options(cors_options),
        )
        .route("/health", get(|| async { (StatusCode::OK, "ok") }))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(shared);

    tracing::info!(%listen, "x402-buy-spl-token listening");
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn buy_spl_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let query = serde_qs::to_string(&params).unwrap_or_default();
    let resp = handler::handle(&headers, "/api/v1/buy-spl-token", &query, state).await;
    into_axum(resp)
}

async fn intent_contract() -> impl IntoResponse {
    let body = serde_json::to_string(&x402_buy_spl_token::intent_contract::intent_contract_document())
        .unwrap_or_else(|_| "{}".to_string());
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
}

async fn cors_options() -> impl IntoResponse {
    (
        StatusCode::NO_CONTENT,
        [
            (axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (axum::http::header::ACCESS_CONTROL_ALLOW_METHODS, "GET, OPTIONS"),
            (
                axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
                ALLOW_HEADERS,
            ),
            (axum::http::header::HeaderName::from_static("access-control-max-age"), "86400"),
        ],
    )
}
