//! Evidence registry HTTP client.
//!
//! The Buy SPL Token endpoint talks to an evidence registry twice per request:
//!
//! 1. On the unpaid `GET` path it uploads the **byte-exact canonical
//!    TransferSla JSON** so the buyer can fetch the same bytes from
//!    `slaUrl` and re-derive the same `slaHash`.
//! 2. On the paid `GET` path, after the SPL `TransferChecked` is confirmed,
//!    it uploads a **TransferEvidence** document so the oracle's
//!    `SubmitDelivery` flow has a content-addressed record of what was
//!    delivered.
//!
//! Both endpoints are part of the canonical `/v1/registry/...` HTTP API
//! exposed by the `oracle-onchain-transfer` binary. Per the registry
//! contract:
//!
//! - `POST /v1/registry/sla` — body is the SLA JSON, returns
//!   `{ "sha256": "<hex>", "url": "/v1/registry/<sha256>", ... }`.
//! - `POST /v1/registry/delivery` — body is the evidence JSON, returns the
//!   same response shape with `kind: "delivery"`.
//!
//! # Why "byte-exact" matters
//!
//! Hashing the *serialized bytes* (not a re-parsed JSON value) is what lets
//! the buyer, seller, and oracle all agree on what `sla_hash` and
//! `delivery_hash` commit to. This module never round-trips the SLA payload
//! through `serde_json::from_slice`/`serde_json::to_vec` — `upload_sla`
//! takes `&[u8]` and forwards the same bytes verbatim.
//!
//! # Schema-before-upload
//!
//! The `TransferEvidence` Rust type is a 1:1 mirror of the published
//! evidence schema (`x402/oracles/onchain-transfer/v1`). Even with a
//! strongly-typed builder we still validate the serialized JSON against the
//! embedded schema *before* upload, so a future schema tightening (e.g. a
//! new required field) surfaces as a `SchemaValidationError` returned to
//! the caller without ever reaching the registry.
//!
//! # Retry policy
//!
//! Transient HTTP failures (connect / timeout / body / decode / HTTP 5xx /
//! 429) are retried per [`RetryPolicy`]. The retry budget mirrors
//! [`crate::rpc_retry::RetryPolicy`] semantics so operators have a single
//! mental model for "how many times do we retry network blips."
//!
//! Permanent failures (HTTP 4xx other than 429) are returned immediately.
//!
//! # Auth
//!
//! Every `POST` requires an `Authorization: Bearer <token>` header. The
//! token is supplied at construction time via
//! `REGISTRY_BEARER_TOKEN` (env) or
//! [`RegistryClient::with_bearer`] in tests.

use {
    crate::rpc_retry::RetryPolicy,
    serde::{Deserialize, Serialize},
    std::{fmt, time::Duration},
    tokio::time::Instant,
    tracing::{debug, warn},
};

/// Process env var carrying the bearer token used for `POST /v1/registry/...`.
pub const REGISTRY_BEARER_TOKEN: &str = "REGISTRY_BEARER_TOKEN";

/// Process env var carrying the registry base URL
/// (e.g. `https://oracle.example.com`).
pub const REGISTRY_BASE_URL: &str = "REGISTRY_BASE_URL";

/// Profile id every uploaded evidence document MUST carry.
pub const EVIDENCE_PROFILE_ID: &str = "x402/oracles/onchain-transfer/v1";

/// Embedded JSON schema for `x402/oracles/onchain-transfer/v1` delivery
/// evidence. Mirrored verbatim from
/// `oracles/oracle-onchain-transfer/spec/onchain-transfer-v1/schema/delivery-evidence.schema.json`.
///
/// We embed rather than `include_str!` from the other crate to keep the
/// `spl-token-balance` crate self-contained: this crate is shipped as a
/// Vercel serverless function and must not depend on adjacent workspace
/// directories at build time.
pub const TRANSFER_EVIDENCE_SCHEMA: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "x402/oracles/onchain-transfer/v1 delivery evidence",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "version",
    "profile_id",
    "tx_signature",
    "asserted_transfers",
    "submitted_at",
    "payment_uid"
  ],
  "properties": {
    "version": { "const": 1 },
    "profile_id": { "const": "x402/oracles/onchain-transfer/v1" },
    "tx_signature": { "type": "string", "minLength": 64, "maxLength": 100 },
    "asserted_transfers": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["mint", "recipient_owner", "claimed_delta"],
        "properties": {
          "mint": { "type": "string" },
          "recipient_owner": { "type": "string" },
          "claimed_delta": { "type": "string", "pattern": "^[0-9]+$" }
        }
      }
    },
    "submitted_at": { "type": "integer" },
    "payment_uid": {
      "type": "string",
      "pattern": "^[0-9a-fA-F]{64}$",
      "description": "Hex-encoded 32-byte payment_uid echoed verbatim from the SLA the seller is fulfilling."
    },
    "buyer_nonce": {
      "type": "string",
      "pattern": "^[0-9a-fA-F]{64}$",
      "description": "Optional hex-encoded 32-byte buyer_nonce echoed from the SLA when present."
    }
  }
}"#;

// =====================================================================
// Newtypes
// =====================================================================

/// Absolute URL at which the canonical SLA JSON is retrievable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlaUrl(pub String);

impl SlaUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SlaUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Absolute URL at which an uploaded TransferEvidence JSON is retrievable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceUrl(pub String);

impl EvidenceUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EvidenceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// =====================================================================
// Strongly-typed evidence
// =====================================================================

/// One row of the evidence document's `asserted_transfers` array.
///
/// Field order and naming match
/// [`TRANSFER_EVIDENCE_SCHEMA`](TRANSFER_EVIDENCE_SCHEMA) field-for-field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertedTransfer {
    /// Base58 SPL mint pubkey.
    pub mint: String,
    /// Base58 owner pubkey of the destination ATA (NOT the ATA itself).
    pub recipient_owner: String,
    /// Decimal-string raw token amount the seller claims to have transferred.
    pub claimed_delta: String,
}

/// Validated TransferEvidence document.
///
/// Construct via [`TransferEvidenceBuilder`] — the builder pins
/// `version` and `profile_id` to the values the schema requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferEvidence {
    /// MUST be `1`.
    pub version: u32,
    /// MUST be `x402/oracles/onchain-transfer/v1`.
    pub profile_id: String,
    /// Solana transaction signature of the SPL `TransferChecked`.
    pub tx_signature: String,
    /// Seller's claim of `(mint, recipient_owner, claimed_delta)` rows.
    pub asserted_transfers: Vec<AssertedTransfer>,
    /// Unix epoch seconds at which the evidence was recorded.
    pub submitted_at: i64,
    /// Hex-encoded 32-byte `payment_uid` echoed from the SLA.
    pub payment_uid: String,
    /// Optional hex-encoded 32-byte `buyer_nonce` echoed from the SLA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buyer_nonce: Option<String>,
}

/// Strongly-typed builder for [`TransferEvidence`].
///
/// `version` and `profile_id` are pinned by the builder so callers cannot
/// produce a document that fails the schema's `const` check by mistake.
#[derive(Debug, Clone, Default)]
pub struct TransferEvidenceBuilder {
    tx_signature: Option<String>,
    asserted_transfers: Vec<AssertedTransfer>,
    submitted_at: Option<i64>,
    payment_uid: Option<String>,
    buyer_nonce: Option<String>,
}

impl TransferEvidenceBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tx_signature(mut self, sig: impl Into<String>) -> Self {
        self.tx_signature = Some(sig.into());
        self
    }

    pub fn add_asserted_transfer(mut self, t: AssertedTransfer) -> Self {
        self.asserted_transfers.push(t);
        self
    }

    pub fn asserted_transfers(mut self, ts: Vec<AssertedTransfer>) -> Self {
        self.asserted_transfers = ts;
        self
    }

    pub fn submitted_at(mut self, ts: i64) -> Self {
        self.submitted_at = Some(ts);
        self
    }

    pub fn payment_uid(mut self, uid: impl Into<String>) -> Self {
        self.payment_uid = Some(uid.into());
        self
    }

    pub fn buyer_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.buyer_nonce = Some(nonce.into());
        self
    }

    /// Finalize into a [`TransferEvidence`]. Returns
    /// [`RegistryClientError::MissingField`] when a required builder slot
    /// was never populated.
    pub fn build(self) -> Result<TransferEvidence, RegistryClientError> {
        let tx_signature = self.tx_signature.ok_or(RegistryClientError::MissingField {
            name: "tx_signature",
        })?;
        let submitted_at = self.submitted_at.ok_or(RegistryClientError::MissingField {
            name: "submitted_at",
        })?;
        let payment_uid = self.payment_uid.ok_or(RegistryClientError::MissingField {
            name: "payment_uid",
        })?;
        if self.asserted_transfers.is_empty() {
            return Err(RegistryClientError::MissingField {
                name: "asserted_transfers",
            });
        }
        Ok(TransferEvidence {
            version: 1,
            profile_id: EVIDENCE_PROFILE_ID.to_string(),
            tx_signature,
            asserted_transfers: self.asserted_transfers,
            submitted_at,
            payment_uid,
            buyer_nonce: self.buyer_nonce,
        })
    }
}

// =====================================================================
// Errors
// =====================================================================

/// All registry-client failures.
#[derive(Debug)]
pub enum RegistryClientError {
    /// The strongly-typed builder was finalized with a required field unset.
    MissingField { name: &'static str },
    /// Serializing the evidence document for upload failed.
    SerializeFailed(serde_json::Error),
    /// JSON-Schema validation failed before upload — the document is NOT
    /// uploaded. The `errors` field carries one human-readable message per
    /// schema violation.
    SchemaValidation { errors: Vec<String> },
    /// The embedded schema itself failed to compile. Indicates a bug in
    /// this module; promoted to a typed variant rather than a panic so
    /// callers can surface it as a 500.
    SchemaCompilation(String),
    /// A transport-layer failure (timeout, connect refused, body decode)
    /// after the [`RetryPolicy`] budget was exhausted.
    Transport { step: &'static str, source: String },
    /// The registry returned a non-success HTTP status. The body, when
    /// present and short, is included for diagnostics.
    HttpStatus {
        step: &'static str,
        status: u16,
        body: String,
    },
    /// The registry's success body was missing the `url` field or could
    /// not be decoded as the expected shape.
    MalformedResponse { step: &'static str, reason: String },
}

impl fmt::Display for RegistryClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField { name } => {
                write!(f, "TransferEvidenceBuilder missing field: {}", name)
            }
            Self::SerializeFailed(e) => {
                write!(f, "evidence serialization failed: {}", e)
            }
            Self::SchemaValidation { errors } => write!(
                f,
                "evidence document failed JSON Schema validation ({} errors): {}",
                errors.len(),
                errors.join("; ")
            ),
            Self::SchemaCompilation(e) => {
                write!(f, "embedded evidence schema failed to compile: {}", e)
            }
            Self::Transport { step, source } => {
                write!(f, "registry {} transport failure: {}", step, source)
            }
            Self::HttpStatus { step, status, body } => write!(
                f,
                "registry {} HTTP {}: {}",
                step,
                status,
                truncate(body, 240)
            ),
            Self::MalformedResponse { step, reason } => {
                write!(f, "registry {} malformed response: {}", step, reason)
            }
        }
    }
}

impl std::error::Error for RegistryClientError {}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// =====================================================================
// Client
// =====================================================================

/// HTTP client for the evidence registry.
///
/// Cheap to clone (the inner `reqwest::Client` is `Arc`-backed).
#[derive(Clone)]
pub struct RegistryClient {
    base_url: String,
    bearer_token: Option<String>,
    http: reqwest::Client,
    retry: RetryPolicy,
}

impl RegistryClient {
    /// Construct a client. `base_url` should be the schema-and-host portion
    /// of the registry (e.g. `https://oracle.example.com`); upload paths
    /// are appended internally.
    pub fn new(base_url: impl Into<String>, retry: RetryPolicy) -> Self {
        let mut base = base_url.into();
        // Normalize trailing slash so path concatenation is unambiguous.
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            base_url: base,
            bearer_token: None,
            http: reqwest::Client::builder()
                // Keep timeouts tighter than the serverless wall budget so
                // we leave headroom for retries within the same request.
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest::Client::builder must not fail with default settings"),
            retry,
        }
    }

    /// Configure the bearer token sent with every upload.
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    /// Upload the **byte-exact** canonical SLA JSON.
    ///
    /// The `canonical_json` slice is forwarded verbatim to the registry —
    /// no re-parsing, no re-serialization. This is the contract the SLA
    /// hash relies on: whatever bytes were SHA-256'd to produce
    /// `sla_hash` are the bytes the registry stores and serves.
    pub async fn upload_sla(&self, canonical_json: &[u8]) -> Result<SlaUrl, RegistryClientError> {
        let path = "/v1/registry/sla";
        let url = self.absolute_url(path);
        let body = canonical_json.to_vec();
        let resp = self
            .post_with_retry("upload_sla", &url, body, "application/json")
            .await?;
        let path = parse_upload_response("upload_sla", &resp)?;
        Ok(SlaUrl(self.absolute_url(&path)))
    }

    /// Validate `evidence` against the embedded JSON Schema, then upload it.
    ///
    /// Returns [`RegistryClientError::SchemaValidation`] **before** issuing
    /// any HTTP request when validation fails.
    pub async fn upload_evidence(
        &self,
        evidence: TransferEvidence,
    ) -> Result<EvidenceUrl, RegistryClientError> {
        let body = serde_json::to_vec(&evidence).map_err(RegistryClientError::SerializeFailed)?;
        validate_evidence_bytes(&body)?;
        let url = self.absolute_url("/v1/registry/delivery");
        let resp = self
            .post_with_retry("upload_evidence", &url, body, "application/json")
            .await?;
        let path = parse_upload_response("upload_evidence", &resp)?;
        Ok(EvidenceUrl(self.absolute_url(&path)))
    }

    fn absolute_url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            // Registry returned an already-absolute URL — pass through.
            return path.to_string();
        }
        let sep = if path.starts_with('/') { "" } else { "/" };
        format!("{}{}{}", self.base_url, sep, path)
    }

    /// Issue a `POST` with the configured retry policy.
    ///
    /// Returns the response body as bytes for the caller to decode.
    async fn post_with_retry(
        &self,
        step: &'static str,
        url: &str,
        body: Vec<u8>,
        content_type: &'static str,
    ) -> Result<Vec<u8>, RegistryClientError> {
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let op_started = Instant::now();
            let req = self
                .http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body.clone());
            let req = match self.bearer_token.as_deref() {
                Some(t) => req.bearer_auth(t),
                None => req,
            };
            let outcome = req.send().await;
            let (transient_reason, terminal): (Option<&'static str>, _) = match &outcome {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) {
                        (None, None)
                    } else if status == 429 || (500..600).contains(&status) {
                        (Some("http-status-retryable"), None)
                    } else {
                        (None, Some(status))
                    }
                }
                Err(e) => {
                    if e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() {
                        (Some("transport"), None)
                    } else {
                        (None, Some(0u16))
                    }
                }
            };

            // Success path (2xx).
            if transient_reason.is_none() && terminal.is_none() {
                let resp = outcome.expect("Ok branch above implies Ok");
                return resp.bytes().await.map(|b| b.to_vec()).map_err(|e| {
                    RegistryClientError::Transport {
                        step,
                        source: format!("read body: {}", e),
                    }
                });
            }

            // Compute remaining budget BEFORE we sleep — used to decide
            // whether one more attempt fits.
            let elapsed = started.elapsed();
            let budget_left = self.retry.total_budget.saturating_sub(elapsed);
            let attempts_left = self.retry.max_attempts.saturating_sub(attempt);

            // Terminal HTTP / non-retryable client error: surface now.
            if let Some(status) = terminal {
                if status == 0 {
                    let err = outcome.unwrap_err();
                    return Err(RegistryClientError::Transport {
                        step,
                        source: err.to_string(),
                    });
                }
                let resp = outcome.expect("terminal HTTP path implies Ok");
                let status_u16 = resp.status().as_u16();
                let body = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(RegistryClientError::HttpStatus {
                    step,
                    status: status_u16,
                    body,
                });
            }

            // Retryable: log the attempt and decide.
            let will_retry = attempts_left > 0 && budget_left > Duration::from_millis(1);
            warn!(
                target: "server_log",
                op = step,
                attempt,
                attempts_left,
                budget_left_ms = budget_left.as_millis() as u64,
                attempt_ms = op_started.elapsed().as_millis() as u64,
                retry_reason = transient_reason.unwrap_or("transient"),
                retrying = will_retry,
                "registry upload transient failure",
            );
            if !will_retry {
                let source = match outcome {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp
                            .text()
                            .await
                            .unwrap_or_else(|_| "<unreadable body>".to_string());
                        return Err(RegistryClientError::HttpStatus { step, status, body });
                    }
                    Err(e) => e.to_string(),
                };
                return Err(RegistryClientError::Transport { step, source });
            }
            // Exponential backoff with full jitter, capped by budget_left.
            let base = self
                .retry
                .initial_backoff
                .saturating_mul(1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX));
            let base_ms = base.as_millis().min(budget_left.as_millis()) as u64;
            let sleep_ms = if base_ms > 0 {
                use rand::RngExt;
                rand::rng().random_range(0..=base_ms)
            } else {
                0
            };
            if Duration::from_millis(sleep_ms) >= budget_left {
                return Err(RegistryClientError::Transport {
                    step,
                    source: "retry budget exhausted".into(),
                });
            }
            debug!(
                target: "server_log",
                op = step,
                sleep_ms,
                "registry retry sleep"
            );
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        }
    }
}

// =====================================================================
// Helpers
// =====================================================================

/// Validate a serialized evidence JSON byte slice against the embedded schema.
///
/// Pure function — exposed (and used by [`RegistryClient::upload_evidence`])
/// so it can be tested directly.
pub fn validate_evidence_bytes(bytes: &[u8]) -> Result<(), RegistryClientError> {
    let instance: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| RegistryClientError::SchemaValidation {
            errors: vec![format!("body is not JSON: {}", e)],
        })?;
    let schema_value: serde_json::Value =
        serde_json::from_str(TRANSFER_EVIDENCE_SCHEMA).map_err(|e| {
            RegistryClientError::SchemaCompilation(format!(
                "schema literal is not valid JSON: {}",
                e
            ))
        })?;
    let validator = jsonschema::validator_for(&schema_value)
        .map_err(|e| RegistryClientError::SchemaCompilation(e.to_string()))?;
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(RegistryClientError::SchemaValidation { errors })
    }
}

/// Decode the registry's `UploadResponse` body and pull out the `url` field.
///
/// The registry returns a relative path like `/v1/registry/<sha256>`; the
/// caller composes the absolute URL by joining with `base_url`.
fn parse_upload_response(step: &'static str, body: &[u8]) -> Result<String, RegistryClientError> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| RegistryClientError::MalformedResponse {
            step,
            reason: format!("not JSON: {}", e),
        })?;
    let url = v.get("url").and_then(|u| u.as_str()).ok_or_else(|| {
        RegistryClientError::MalformedResponse {
            step,
            reason: "missing string field `url`".into(),
        }
    })?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use wiremock::matchers::{body_bytes, body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fast_retry() -> RetryPolicy {
        // Tight budget so transient-failure tests finish in well under a
        // second even if every retry sleeps through its full window.
        RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            total_budget: Duration::from_millis(200),
        }
    }

    fn upload_response_for(sha: &str, kind: &str) -> serde_json::Value {
        json!({
            "sha256": sha,
            "url": format!("/v1/registry/{}", sha),
            "size_bytes": 0,
            "kind": kind,
            "stored_at": "2024-12-25T00:00:00Z",
        })
    }

    /// Sample SHA-256 hex (64 chars) for response bodies. The value isn't
    /// re-derived from the request — the registry would normally compute it,
    /// but the client only consumes the `url` field of the response.
    const FAKE_SHA: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    fn valid_evidence() -> TransferEvidence {
        TransferEvidenceBuilder::new()
            .tx_signature("5wJfx6S5LMJrEdMnL9ks8sJZQrL8YZk7Vb5T9Yyy9R3K7vfqWYZK1bM3pX1ZqA9k")
            .add_asserted_transfer(AssertedTransfer {
                mint: "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP".into(),
                recipient_owner: "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU".into(),
                claimed_delta: "1000000".into(),
            })
            .submitted_at(1_770_000_000)
            .payment_uid("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .build()
            .expect("happy-path builder")
    }

    // --- Pure helpers (no HTTP) -----------------------------------------

    /// Validates: Requirements 7.1, 7.2, 7.3, 9.5
    /// The strongly-typed builder serializes to a schema-conformant document.
    #[test]
    fn evidence_builder_serializes_to_schema_conformant_document() {
        let evidence = valid_evidence();
        let bytes = serde_json::to_vec(&evidence).expect("serialize");
        validate_evidence_bytes(&bytes).expect("schema-conformant evidence must validate");

        // Spot-check the constant-pinned fields the schema requires.
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["version"], json!(1));
        assert_eq!(v["profile_id"], json!(EVIDENCE_PROFILE_ID));
    }

    /// Validates: Requirements 7.3
    /// Schema-validation failure short-circuits before upload.
    #[test]
    fn schema_validation_failure_is_caught_before_upload() {
        // Tamper with a serialized document so it violates the schema:
        // payment_uid no longer 64 hex chars, claimed_delta has letters.
        let mut tampered = json!({
            "version": 1,
            "profile_id": EVIDENCE_PROFILE_ID,
            "tx_signature": "5wJfx6S5LMJrEdMnL9ks8sJZQrL8YZk7Vb5T9Yyy9R3K7vfqWYZK1bM3pX1ZqA9k",
            "asserted_transfers": [{
                "mint": "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP",
                "recipient_owner": "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
                "claimed_delta": "abc",   // schema requires ^[0-9]+$
            }],
            "submitted_at": 1_770_000_000,
            "payment_uid": "tooshort",     // schema requires 64 hex chars
        });
        // Should fail schema validation.
        let bytes = serde_json::to_vec(&tampered).unwrap();
        let err = validate_evidence_bytes(&bytes).unwrap_err();
        match err {
            RegistryClientError::SchemaValidation { errors } => {
                assert!(!errors.is_empty(), "expected at least one schema error");
            }
            other => panic!("expected SchemaValidation, got {:?}", other),
        }
        // And remove a required field — also caught.
        tampered.as_object_mut().unwrap().remove("tx_signature");
        let bytes = serde_json::to_vec(&tampered).unwrap();
        assert!(matches!(
            validate_evidence_bytes(&bytes).unwrap_err(),
            RegistryClientError::SchemaValidation { .. }
        ));
    }

    #[test]
    fn builder_reports_missing_required_fields() {
        let err = TransferEvidenceBuilder::new().build().unwrap_err();
        assert!(
            matches!(err, RegistryClientError::MissingField { name } if name == "tx_signature")
        );

        let err = TransferEvidenceBuilder::new()
            .tx_signature("sig")
            .build()
            .unwrap_err();
        assert!(
            matches!(err, RegistryClientError::MissingField { name } if name == "submitted_at")
        );

        let err = TransferEvidenceBuilder::new()
            .tx_signature("sig")
            .submitted_at(0)
            .build()
            .unwrap_err();
        assert!(matches!(err, RegistryClientError::MissingField { name } if name == "payment_uid"));

        let err = TransferEvidenceBuilder::new()
            .tx_signature("sig")
            .submitted_at(0)
            .payment_uid("uid")
            .build()
            .unwrap_err();
        assert!(
            matches!(err, RegistryClientError::MissingField { name } if name == "asserted_transfers")
        );
    }

    // --- HTTP integration with wiremock ---------------------------------

    /// Validates: Requirements 3.5, 7.5
    /// `upload_sla` forwards the canonical SLA bytes verbatim.
    #[tokio::test]
    async fn upload_sla_uploads_byte_exact_canonical_json() {
        let server = MockServer::start().await;
        // Build a deliberately ordered JSON byte sequence — the registry
        // must see the same bytes the caller passed in. Trailing whitespace,
        // odd key order, etc. all matter for hash determinism.
        let canonical: &[u8] =
            br#"{"profile_id":"x402/oracles/onchain-transfer/v1","payment_uid":"a","cluster":"devnet"}"#;

        Mock::given(method("POST"))
            .and(path("/v1/registry/sla"))
            .and(header("authorization", "Bearer test-token"))
            .and(header("content-type", "application/json"))
            .and(body_bytes(canonical))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(upload_response_for(FAKE_SHA, "sla")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = RegistryClient::new(server.uri(), fast_retry()).with_bearer("test-token");
        let url = client
            .upload_sla(canonical)
            .await
            .expect("byte-exact upload must succeed");
        assert_eq!(
            url.as_str(),
            format!("{}/v1/registry/{}", server.uri(), FAKE_SHA)
        );
    }

    /// Validates: Requirements 7.1, 7.2
    /// The strongly-typed evidence builder produces a schema-conformant body
    /// and the client uploads it.
    #[tokio::test]
    async fn upload_evidence_validates_then_uploads() {
        let server = MockServer::start().await;
        let evidence = valid_evidence();
        let expected_body = serde_json::to_value(&evidence).unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/registry/delivery"))
            .and(header("authorization", "Bearer test-token"))
            // body_json asserts the request body parses to the expected
            // JSON value; equivalent to body equality for serde
            // serialization.
            .and(body_json(&expected_body))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(upload_response_for(FAKE_SHA, "delivery")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = RegistryClient::new(server.uri(), fast_retry()).with_bearer("test-token");
        let url = client
            .upload_evidence(evidence)
            .await
            .expect("schema-conformant evidence must upload");
        assert_eq!(
            url.as_str(),
            format!("{}/v1/registry/{}", server.uri(), FAKE_SHA)
        );
    }

    /// Validates: Requirements 7.3
    /// Schema-validation failure short-circuits before any HTTP request.
    #[tokio::test]
    async fn upload_evidence_short_circuits_on_schema_failure() {
        let server = MockServer::start().await;
        // No mock registered: any HTTP call would return wiremock's default
        // 404 (and fail the test by routing through the retry path), so the
        // expectation here is that we *never* call the server.

        // Construct an evidence document that bypasses the builder and
        // therefore can violate the schema (`payment_uid` not 64 hex chars).
        let bad = TransferEvidence {
            version: 1,
            profile_id: EVIDENCE_PROFILE_ID.to_string(),
            tx_signature: "5wJfx6S5LMJrEdMnL9ks8sJZQrL8YZk7Vb5T9Yyy9R3K7vfqWYZK1bM3pX1ZqA9k"
                .to_string(),
            asserted_transfers: vec![AssertedTransfer {
                mint: "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP".into(),
                recipient_owner: "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU".into(),
                claimed_delta: "1000000".into(),
            }],
            submitted_at: 0,
            payment_uid: "tooshort".to_string(),
            buyer_nonce: None,
        };

        let client = RegistryClient::new(server.uri(), fast_retry()).with_bearer("test-token");
        let err = client
            .upload_evidence(bad)
            .await
            .expect_err("schema violation must short-circuit");
        match err {
            RegistryClientError::SchemaValidation { errors } => {
                assert!(!errors.is_empty());
            }
            other => panic!("expected SchemaValidation, got {:?}", other),
        }

        // Wiremock will assert at drop time that no expectations were
        // violated; an unexpected call would surface as a panic.
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// Validates: Requirements 9.5
    /// Transient HTTP 5xx is retried per [`RetryPolicy`].
    #[tokio::test]
    async fn upload_evidence_retries_transient_5xx_then_succeeds() {
        let server = MockServer::start().await;
        let evidence = valid_evidence();

        // First two attempts fail with 503; third attempt succeeds.
        Mock::given(method("POST"))
            .and(path("/v1/registry/delivery"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/registry/delivery"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(upload_response_for(FAKE_SHA, "delivery")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = RegistryClient::new(server.uri(), fast_retry()).with_bearer("test-token");
        let url = client
            .upload_evidence(evidence)
            .await
            .expect("retried 5xx should eventually succeed");
        assert_eq!(
            url.as_str(),
            format!("{}/v1/registry/{}", server.uri(), FAKE_SHA)
        );
    }

    /// Validates: Requirements 9.5
    /// Persistent 5xx exhausts the retry budget and surfaces an HttpStatus.
    #[tokio::test]
    async fn upload_evidence_surfaces_http_status_after_retry_exhaustion() {
        let server = MockServer::start().await;
        let evidence = valid_evidence();

        // Always return 503 — the retry loop must give up and surface an
        // HttpStatus error rather than spinning forever.
        Mock::given(method("POST"))
            .and(path("/v1/registry/delivery"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
            .mount(&server)
            .await;

        let client = RegistryClient::new(server.uri(), fast_retry()).with_bearer("test-token");
        let err = client
            .upload_evidence(evidence)
            .await
            .expect_err("persistent 5xx must surface as error");
        match err {
            RegistryClientError::HttpStatus { step, status, .. } => {
                assert_eq!(step, "upload_evidence");
                assert_eq!(status, 503);
            }
            other => panic!("expected HttpStatus(503), got {:?}", other),
        }

        // We attempted at least twice (max_attempts = 3 in fast_retry).
        let received = server.received_requests().await.unwrap();
        assert!(
            received.len() >= 2,
            "expected retry to issue >1 request, got {}",
            received.len()
        );
    }

    /// Validates: Requirements 7.5
    /// 4xx is treated as terminal — the SLA upload does not retry on 400.
    #[tokio::test]
    async fn upload_sla_does_not_retry_on_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/registry/sla"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;

        let client = RegistryClient::new(server.uri(), fast_retry()).with_bearer("test-token");
        let err = client
            .upload_sla(b"{}")
            .await
            .expect_err("4xx must not retry");
        match err {
            RegistryClientError::HttpStatus { status, .. } => assert_eq!(status, 400),
            other => panic!("expected HttpStatus(400), got {:?}", other),
        }
    }
}
