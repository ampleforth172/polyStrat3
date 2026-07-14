//! Live order execution via the official Polymarket Rust SDK
//! (`polymarket_client_sdk_v2`). Auth, EIP-712 signing, L2 headers and
//! protocol detection are all inside the SDK; this is a thin adapter from
//! engine Actions to SDK calls. The `heartbeats` feature acts as a
//! dead-man's switch: if this process dies, the server cancels all
//! resting orders.

use std::str::FromStr;

use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::{Side as SdkSide, SignatureType};
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk_v2::data::types::response::Position;
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;

use crate::config::Credentials;
use crate::types::Side;

type AuthedClient = Client<Authenticated<Normal>>;

pub struct LiveExec {
    client: AuthedClient,
    signer: PrivateKeySigner,
    /// Funds-holding address: the funder when configured, else the signer.
    owner: String,
    owner_addr: Address,
    /// SDK Data-API client (positions, trades) — public endpoints.
    data: polymarket_client_sdk_v2::data::Client,
}

/// f64 -> Decimal via the shortest round-trip string, so tick-rounded
/// prices keep their exact scale (0.47 -> "0.47", not "0.470000").
fn to_decimal(v: f64) -> Result<Decimal, String> {
    Decimal::from_str(&format!("{v}")).map_err(|e| format!("decimal {v}: {e}"))
}

impl LiveExec {
    /// Authenticate against the CLOB. `SignatureType::Proxy` (wire value 1)
    /// matches the Python client's email/Magic-wallet setup, with an explicit
    /// funder when provided.
    pub async fn connect(host: &str, creds: &Credentials) -> Result<Self, String> {
        let signer = PrivateKeySigner::from_str(creds.expose_private_key())
            .map_err(|e| format!("bad private key: {e}"))?
            .with_chain_id(Some(POLYGON));

        let base = Client::new(host, ClobConfig::default())
            .map_err(|e| format!("clob client: {e}"))?;
        let mut builder = base
            .authentication_builder(&signer)
            .signature_type(SignatureType::Proxy);
        if !creds.funder.is_empty() {
            let funder = Address::from_str(&creds.funder)
                .map_err(|e| format!("bad funder address: {e}"))?;
            builder = builder.funder(funder);
        }
        let client = builder
            .authenticate()
            .await
            .map_err(|e| format!("authenticate: {e}"))?;
        tracing::info!("[LIVE] authenticated against {host}");
        let owner_addr = if creds.funder.is_empty() {
            signer.address()
        } else {
            Address::from_str(&creds.funder).map_err(|e| format!("bad funder address: {e}"))?
        };
        let data = polymarket_client_sdk_v2::data::Client::default();
        Ok(Self {
            client,
            signer,
            owner: owner_addr.to_string(),
            owner_addr,
            data,
        })
    }

    /// Build, sign and post one limit order. Returns the exchange order id.
    pub async fn place(&self, token: &str, side: Side, px: f64, sz: f64) -> Result<String, String> {
        let token_id = U256::from_str(token).map_err(|e| format!("bad token id: {e}"))?;
        let sdk_side = match side {
            Side::Buy => SdkSide::Buy,
            Side::Sell => SdkSide::Sell,
        };
        let order = self
            .client
            .limit_order()
            .token_id(token_id)
            .price(to_decimal(px)?)
            .size(to_decimal(sz)?)
            .side(sdk_side)
            .post_only(false)
            .build()
            .await
            .map_err(|e| format!("build order: {e}"))?;
        let signed = self
            .client
            .sign(&self.signer, order)
            .await
            .map_err(|e| format!("sign order: {e}"))?;
        let resp = self
            .client
            .post_order(signed)
            .await
            .map_err(|e| format!("post order: {e}"))?;
        if !resp.success {
            return Err(format!("post order not successful: {resp:?}"));
        }
        Ok(resp.order_id.to_string())
    }

    /// The address that holds funds/tokens (funder for proxy wallets).
    pub fn owner_address(&self) -> &str {
        &self.owner
    }

    /// Signer (EOA) address derived from the private key.
    pub fn signer_address(&self) -> String {
        self.signer.address().to_string()
    }

    /// Signer address as the typed alloy Address (for SDK WS auth).
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// The L2 API credentials this client authenticated with — required for
    /// the authenticated user-channel WebSocket.
    pub fn api_creds(&self) -> polymarket_client_sdk_v2::auth::Credentials {
        self.client.credentials().clone()
    }

    /// Settled position sizes for one market — BOTH tokens — via the SDK
    /// Data API. Returns (token_id, size) pairs; a token absent from the
    /// response holds zero.
    pub async fn market_positions(&self, condition_id: &str) -> Result<Vec<Position>, String> {
        use polymarket_client_sdk_v2::data::types::request::PositionsRequest;
        use polymarket_client_sdk_v2::data::types::MarketFilter;
        use polymarket_client_sdk_v2::types::B256;
        let cond =
            B256::from_str(condition_id).map_err(|e| format!("bad condition id: {e}"))?;
        let req = PositionsRequest::builder()
            .user(self.owner_addr)
            .filter(MarketFilter::markets([cond]))
            .size_threshold(Decimal::new(1, 2))
            .build();
        let positions = self
            .data
            .positions(&req)
            .await
            .map_err(|e| format!("positions: {e}"))?;
        Ok(positions)
    }

    /// USDC (collateral) balance and exchange allowances for the account.
    /// The default request asks for collateral; the SDK fills in the
    /// client's signature type automatically.
    pub async fn balance_allowance(
        &self,
    ) -> Result<polymarket_client_sdk_v2::clob::types::response::BalanceAllowanceResponse, String>
    {
        use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
        self.client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
            .map_err(|e| format!("balance_allowance: {e}"))
    }

    /// All open orders on the account, Debug-formatted for inspection.
    pub async fn open_orders(&self) -> Result<Vec<String>, String> {
        use polymarket_client_sdk_v2::clob::types::request::OrdersRequest;
        let resp = self
            .client
            .orders(&OrdersRequest::default(), None)
            .await
            .map_err(|e| format!("orders: {e}"))?;
        Ok(resp.data.iter().map(|o| format!("{o:?}")).collect())
    }

    pub async fn cancel_orders(&self, order_ids: &[String]) -> Result<(), String> {
        if order_ids.is_empty() {
            return Ok(());
        }
        let refs: Vec<&str> = order_ids.iter().map(String::as_str).collect();
        self.client
            .cancel_orders(&refs)
            .await
            .map(|_| ())
            .map_err(|e| format!("cancel orders: {e}"))
    }

    pub async fn cancel_all(&self) -> Result<(), String> {
        self.client
            .cancel_all_orders()
            .await
            .map(|_| ())
            .map_err(|e| format!("cancel all: {e}"))
    }
}
