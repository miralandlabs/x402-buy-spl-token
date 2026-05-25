use {
    std::{future::Future, pin::Pin, sync::Arc},
    vercel_runtime::{Body, Response, StatusCode as VercelStatusCode},
    x402_buy_spl_token::{
        config::Config,
        cors::ALLOW_HEADERS,
        handler,
        init::{cold_start, init_tracing},
        route_handler::run_server,
        AppState,
    },
};

fn cors_options() -> Response<Body> {
    Response::builder()
        .status(VercelStatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, OPTIONS")
        .header("Access-Control-Allow-Headers", ALLOW_HEADERS)
        .header("Access-Control-Max-Age", "86400")
        .body(Body::Empty)
        .unwrap()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();

    let config = Config::from_env()?;
    let state = cold_start(&config)
        .await
        .map_err(|e| format!("cold-start failed: {}", e))?;
    let shared_state = Arc::new(state);

    let routes = |headers: http::HeaderMap,
                  method: http::Method,
                  path: String,
                  query: String,
                  _body: Body,
                  state: Arc<AppState>|
     -> Pin<Box<dyn Future<Output = Response<Body>> + Send>> {
        Box::pin(async move {
            let effective_method = if method == http::Method::HEAD {
                http::Method::GET
            } else {
                method
            };
            match (&effective_method, path.as_str()) {
                (&http::Method::GET, "/health") => Response::builder()
                    .status(VercelStatusCode::OK)
                    .header("Content-Type", "text/plain; charset=utf-8")
                    .body(Body::Text("ok".to_string()))
                    .unwrap(),

                (&http::Method::OPTIONS, "/api/v1/buy-spl-token")
                | (&http::Method::OPTIONS, "/api/v1/buy-spl-token/intent-contract")
                | (&http::Method::OPTIONS, "/api/v1/buy-spl-token/catalog") => {
                    cors_options()
                }

                (&http::Method::GET, "/api/v1/buy-spl-token") => {
                    handler::handle(&headers, path.as_str(), &query, state).await
                }

                (&http::Method::GET, "/api/v1/buy-spl-token/catalog") => {
                    x402_buy_spl_token::catalog_api::handle_catalog(state).await
                }

                (&http::Method::GET, "/api/v1/buy-spl-token/intent-contract") => {
                    let body = serde_json::to_string(
                        &x402_buy_spl_token::intent_contract::intent_contract_document(),
                    )
                    .unwrap_or_else(|_| "{}".to_string());
                    Response::builder()
                        .status(VercelStatusCode::OK)
                        .header("Content-Type", "application/json; charset=utf-8")
                        .body(Body::Text(body))
                        .unwrap()
                }

                _ => Response::builder()
                    .status(VercelStatusCode::NOT_FOUND)
                    .body(Body::Text("Not found".to_string()))
                    .unwrap(),
            }
        })
    };

    run_server(shared_state, routes).await
}
