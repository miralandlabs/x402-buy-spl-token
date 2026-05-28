use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use {
    crate::error::Error,
    crate::x402::models::{PaymentProofBody, SettlementProof, SupportedResponse},
    reqwest::Client,
    reqwest::StatusCode,
    serde_json::{json, Value},
    std::sync::Arc,
    tracing::{error, info},
};

/// HTTP client for a **pr402** facilitator (`X402_FACILITATOR_URL` = origin + `/api/v1/facilitator`).
pub struct FacilitatorClient {
    client: Arc<Client>,
    base_url: String,
}

impl FacilitatorClient {
    pub fn new(base_url: String) -> Self {
        Self {
            client: Arc::new(Client::new()),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    pub fn capabilities_url(&self) -> String {
        format!("{}/capabilities", self.base_url)
    }

    fn build_get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        self.client.get(url).header("X-API-Version", "1")
    }

    fn build_post(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        self.client.post(url).header("X-API-Version", "1")
    }

    /// POST full x402 v2 verify body (`x402Version`, `paymentPayload`, `paymentRequirements`).
    pub async fn verify(&self, body: &Value) -> Result<Value, Error> {
        let response = self
            .build_post("verify")
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("facilitator verify request: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "".into());
            error!("facilitator verify HTTP {}: {}", status, error_text);
            return Err(Error::PaymentRequired(format!(
                "verify failed: {}",
                error_text
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::Internal(format!("verify JSON: {}", e)))
    }

    async fn settle_post_raw(&self, body: &Value) -> Result<(StatusCode, String), Error> {
        let response = self
            .build_post("settle")
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("facilitator settle request: {}", e)))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// Same JSON body as verify (merged with facilitator `correlationId` when needed).
    pub async fn settle(&self, body: &Value) -> Result<Value, Error> {
        let (status, text) = self.settle_post_raw(body).await?;
        if !status.is_success() {
            error!("facilitator settle HTTP {}: {}", status, text);
            return Err(Error::Internal(format!("settle failed: {}", text)));
        }
        serde_json::from_str(&text).map_err(|e| Error::Internal(format!("settle JSON: {}", e)))
    }

    pub async fn supported(&self) -> Result<SupportedResponse, Error> {
        info!("facilitator GET supported");
        let response = self
            .build_get("supported")
            .send()
            .await
            .map_err(|e| Error::Internal(format!("facilitator supported request: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "".into());
            return Err(Error::Internal(format!(
                "supported HTTP {}: {}",
                status, error_text
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::Internal(format!("supported JSON: {}", e)))
    }

    /// `GET /sellers/{wallet}/rails/{scheme}` — canonical `payTo` / PDA for one scheme (+ optional `asset`).
    pub async fn discovery(
        &self,
        wallet: &str,
        scheme: &str,
        asset: Option<&str>,
    ) -> Result<Value, Error> {
        let path = format!("sellers/{wallet}/rails/{scheme}");
        let mut req = self.build_get(&path);
        if let Some(m) = asset {
            req = req.query(&[("asset", m)]);
        }
        let response = req
            .send()
            .await
            .map_err(|e| Error::Internal(format!("facilitator seller rail request: {}", e)))?;
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "".into());
            return Err(Error::Internal(format!(
                "seller rail HTTP {}: {}",
                status, error_text
            )));
        }
        response
            .json()
            .await
            .map_err(|e| Error::Internal(format!("seller rail JSON: {}", e)))
    }

    /// Multi-rail seller preview (`GET /sellers/{wallet}/preview`).
    pub async fn onboard(&self, wallet: &str) -> Result<serde_json::Value, Error> {
        info!("facilitator GET sellers/{}/preview", wallet);
        let path = format!("sellers/{wallet}/preview");
        let response =
            self.build_get(&path).send().await.map_err(|e| {
                Error::Internal(format!("facilitator seller preview request: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "".into());
            return Err(Error::Internal(format!(
                "seller preview HTTP {}: {}",
                status, error_text
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::Internal(format!("seller preview JSON: {}", e)))
    }

    pub async fn verify_and_settle(&self, body: &Value) -> Result<SettlementProof, Error> {
        let verify_json = self.verify(body).await?;

        // x402 v2 spec §7.1: `isValid`.
        let valid = verify_json
            .get("isValid")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !valid {
            let reason = verify_json
                .get("invalidReason")
                .and_then(|v| v.as_str())
                .unwrap_or("invalid");
            return Err(Error::PaymentRequired(format!(
                "verify invalid: {}",
                reason
            )));
        }

        let mut settle_body = body.clone();
        if let Some(cid) = verify_json
            .get("correlationId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            merge_correlation_id(&mut settle_body, cid);
        }

        let (settle_status, settle_text) = self.settle_post_raw(&settle_body).await?;
        if !settle_status.is_success() {
            if is_duplicate_settle_body(&settle_text) {
                return Ok(SettlementProof::from_facilitator_json(
                    synthetic_settlement_after_duplicate(&verify_json, body, &settle_text),
                ));
            }
            error!("facilitator settle HTTP {}: {}", settle_status, settle_text);
            return Err(Error::Internal(format!("settle failed: {}", settle_text)));
        }

        let settle_json: Value = serde_json::from_str(&settle_text).map_err(|e| {
            Error::Internal(format!(
                "settle JSON: {e}; body_prefix={}",
                settle_text.chars().take(400).collect::<String>()
            ))
        })?;
        let success = settle_json
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !success {
            let reason = settle_json
                .get("errorReason")
                .and_then(|v| v.as_str())
                .unwrap_or("settle error");
            return Err(Error::Internal(format!("settle: {}", reason)));
        }

        Ok(SettlementProof::from_facilitator_json(settle_json))
    }
}

fn is_duplicate_settle_body(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("already been processed")
        || lower.contains("alreadyprocessed")
        || lower.contains("this transaction has already been processed")
}

fn network_from_proof(proof: &Value) -> String {
    proof
        .pointer("/paymentRequirements/network")
        .or_else(|| proof.pointer("/payment_requirements/network"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn synthetic_settlement_after_duplicate(
    verify: &Value,
    proof: &Value,
    settle_error_snippet: &str,
) -> Value {
    let payer = verify.get("payer").cloned().unwrap_or(Value::Null);
    let network = network_from_proof(proof);
    json!({
        "success": true,
        "payer": payer,
        "network": network,
        "transaction": "",
        "settlementNote": "verify succeeded; settle reported duplicate on-chain — treating as idempotent success",
        "settleErrorPreview": settle_error_snippet.chars().take(240).collect::<String>(),
    })
}

fn merge_correlation_id(body: &mut Value, cid: &str) {
    if let Some(obj) = body.as_object_mut() {
        if !obj.contains_key("correlationId") {
            obj.insert("correlationId".to_string(), Value::String(cid.to_string()));
        }
    }
}

/// Parse payment proof from `PAYMENT-SIGNATURE` (x402 v2):
/// UTF-8 JSON **or** base64-encoded UTF-8 JSON (x402 clients vary).
pub fn parse_payment_proof(raw: &str) -> Result<PaymentProofBody, Error> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(Error::PaymentRequired("empty payment header".into()));
    }

    if let Ok(v) = serde_json::from_str::<Value>(t) {
        validate_pr402_verify_shape(&v)?;
        return Ok(PaymentProofBody(v));
    }

    let bytes = B64.decode(t).map_err(|_| {
        Error::PaymentRequired("payment header is not JSON or valid base64 JSON".into())
    })?;
    let s = String::from_utf8(bytes)
        .map_err(|_| Error::PaymentRequired("payment header base64 is not UTF-8 JSON".into()))?;
    let v: Value = serde_json::from_str(&s)
        .map_err(|_| Error::PaymentRequired("payment header base64 payload is not JSON".into()))?;
    validate_pr402_verify_shape(&v)?;
    Ok(PaymentProofBody(v))
}

fn validate_pr402_verify_shape(v: &Value) -> Result<(), Error> {
    let x402 = v
        .get("x402Version")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| Error::PaymentRequired("missing x402Version".into()))?;
    if x402 != 2 {
        return Err(Error::PaymentRequired(format!(
            "expected x402Version 2, got {}",
            x402
        )));
    }
    if v.get("paymentPayload").is_none() || v.get("paymentRequirements").is_none() {
        return Err(Error::PaymentRequired(
            "missing paymentPayload or paymentRequirements".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod parse_payment_proof_tests {
    use super::parse_payment_proof;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn parses_raw_json() {
        let body = json!({
            "x402Version": 2,
            "paymentPayload": { "payload": { "transaction": "..." } },
            "paymentRequirements": { "network": "solana" }
        });
        let raw = body.to_string();
        let parsed = parse_payment_proof(&raw).expect("should parse raw JSON");
        assert_eq!(parsed.0.get("x402Version").unwrap().as_u64().unwrap(), 2);
    }

    #[test]
    fn parses_base64_json() {
        let body = json!({
            "x402Version": 2,
            "paymentPayload": { "payload": { "transaction": "..." } },
            "paymentRequirements": { "network": "solana" }
        });
        let b64 = B64.encode(body.to_string());
        let parsed = parse_payment_proof(&b64).expect("should parse base64 JSON");
        assert_eq!(parsed.0.get("x402Version").unwrap().as_u64().unwrap(), 2);
    }

    #[test]
    fn fails_on_invalid_json() {
        let err = parse_payment_proof("{ invalid }").unwrap_err();
        assert!(err.to_string().contains("payment header is not JSON"));
    }
}

#[cfg(test)]
mod settle_duplicate_tests {
    use super::is_duplicate_settle_body;

    #[test]
    fn detects_solana_duplicate_messages() {
        assert!(is_duplicate_settle_body(
            "This transaction has already been processed"
        ));
        assert!(is_duplicate_settle_body("AlreadyProcessed"));
        assert!(!is_duplicate_settle_body("insufficient lamports"));
    }
}
