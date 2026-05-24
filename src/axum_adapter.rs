use {
    http::{header, HeaderMap, StatusCode},
    vercel_runtime::{Body, Response},
};

/// Convert a handler [`Response`] into an Axum-compatible triple.
pub fn into_axum(resp: Response<Body>) -> (StatusCode, HeaderMap, String) {
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    for (name, value) in resp.headers().iter() {
        if let Ok(v) = value.to_str() {
            if let (Ok(n), Ok(val)) = (
                header::HeaderName::from_bytes(name.as_str().as_bytes()),
                header::HeaderValue::from_str(v),
            ) {
                headers.insert(n, val);
            }
        }
    }
    let body = match resp.into_body() {
        Body::Text(s) => s,
        Body::Empty => String::new(),
        other => format!("{:?}", other),
    };
    (status, headers, body)
}
