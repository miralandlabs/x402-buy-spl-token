use serde::{Deserialize, Serialize};

fn default_extensions_object() -> serde_json::Value {
    serde_json::json!({})
}

/// Resource information for x402 v2 [`PaymentRequired`](https://github.com/coinbase/x402/blob/main/specs/x402-specification-v2.md#512-field-descriptions).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    pub url: String,
    pub description: String,
    pub mime_type: String,
}

/// One `accepts` entry — `asset` is Solana mint (base58) or native SOL mint pubkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirementsLine {
    pub scheme: String,
    pub network: String,
    pub asset: String,
    pub amount: String,
    pub pay_to: String,
    pub max_timeout_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Payment-Required body for x402 v2 (spec §5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    pub x402_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub resource: ResourceInfo,
    pub accepts: Vec<serde_json::Value>,
    #[serde(default = "default_extensions_object")]
    pub extensions: serde_json::Value,
}

impl PaymentRequired {
    pub fn with_error(mut self, message: impl Into<String>) -> Self {
        self.error = Some(message.into());
        self
    }
}

/// pr402 facilitator /supported row
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedPaymentKind {
    pub x402_version: u8,
    pub scheme: String,
    pub network: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedResponse {
    pub kinds: Vec<SupportedPaymentKind>,
    /// Spec §7.3: extension identifier strings.
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub signers: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PaymentProofBody(pub serde_json::Value);

/// Facilitator settle response JSON (pr402 exposes `transaction`; other stacks may use `proof`).
#[derive(Debug, Clone)]
pub struct SettlementProof {
    pub response: serde_json::Value,
}

impl SettlementProof {
    /// x402 v2: base64-encode the full settlement JSON for the `PAYMENT-RESPONSE` header.
    pub fn header_value(&self) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        STANDARD.encode(self.response.to_string())
    }

    pub fn from_facilitator_json(v: serde_json::Value) -> Self {
        Self { response: v }
    }
}
