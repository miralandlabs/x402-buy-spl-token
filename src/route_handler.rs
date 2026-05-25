use {
    crate::AppState,
    http::HeaderMap,
    std::sync::Arc,
    vercel_runtime::{run, Body, Request, Response},
};

pub async fn run_server<F>(
    state: Arc<AppState>,
    route_handler: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(
            HeaderMap,
            http::Method,
            String,
            String,
            Body,
            Arc<AppState>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response<Body>> + Send>>
        + Send
        + Sync
        + Clone
        + 'static,
{
    let handler = move |req: Request| {
        let state = state.clone();
        let route_handler = route_handler.clone();

        Box::pin(async move {
            let headers = req.headers().clone();
            let method = req.method().clone();
            let uri = req.uri().clone();
            let path = uri.path().to_string();
            let query = uri.query().unwrap_or("").to_string();
            let body = req.into_body();

            let response = route_handler(headers, method, path, query, body, state).await;
            Ok(response)
        })
    };

    run(handler).await
}
