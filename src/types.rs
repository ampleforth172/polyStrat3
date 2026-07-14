//! Shared types: events into the engine, actions out of strategies.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Interned token id. An `Arc<str>` under the hood: cloning is a refcount
/// bump, so the hot path (snapshots, actions, OMS keys, throttle keys) can
/// own tokens without heap allocation. Serializes exactly like a plain JSON
/// string — journals written before the interning are still replayable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenId(Arc<str>);

impl TokenId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for TokenId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Default for TokenId {
    fn default() -> Self {
        TokenId(Arc::from(""))
    }
}

impl From<&str> for TokenId {
    fn from(s: &str) -> Self {
        TokenId(Arc::from(s))
    }
}

impl From<String> for TokenId {
    fn from(s: String) -> Self {
        TokenId(Arc::from(s))
    }
}

impl std::borrow::Borrow<str> for TokenId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for TokenId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for TokenId {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for TokenId {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<TokenId> for str {
    fn eq(&self, other: &TokenId) -> bool {
        self == &*other.0
    }
}

impl PartialEq<TokenId> for &str {
    fn eq(&self, other: &TokenId) -> bool {
        *self == &*other.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for TokenId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TokenId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(TokenId(Arc::from(String::deserialize(deserializer)?)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    Yes,
    No,
}

impl Outcome {
    pub fn other(&self) -> Outcome {
        match self {
            Outcome::Yes => Outcome::No,
            Outcome::No => Outcome::Yes,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Yes => "YES",
            Outcome::No => "NO",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Best bid/ask with sizes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BookTop {
    pub bid: Option<f64>,
    pub bid_sz: f64,
    pub ask: Option<f64>,
    pub ask_sz: f64,
}

impl BookTop {
    pub fn mid(&self) -> Option<f64> {
        match (self.bid, self.ask) {
            (Some(b), Some(a)) => Some((b + a) / 2.0),
            _ => None,
        }
    }
}

/// Discovered 15m market window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketInfo {
    pub slug: String,
    pub condition_id: String,
    pub token_yes: TokenId,
    pub token_no: TokenId,
    pub end_date_iso: String,
    /// Expiry as epoch seconds.
    pub end_ts: f64,
}

impl MarketInfo {
    pub fn token(&self, o: Outcome) -> &TokenId {
        match o {
            Outcome::Yes => &self.token_yes,
            Outcome::No => &self.token_no,
        }
    }

    pub fn outcome_of(&self, token: &str) -> Option<Outcome> {
        if token == self.token_yes {
            Some(Outcome::Yes)
        } else if token == self.token_no {
            Some(Outcome::No)
        } else {
            None
        }
    }
}

/// Why an order was placed — used for logging and taker lifecycle handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderTag {
    EntryBuy,
    TakeProfit,
    StopLoss,
    QuoteBid,
    QuoteAsk,
}

impl fmt::Display for OrderTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            OrderTag::EntryBuy => "entry",
            OrderTag::TakeProfit => "tp",
            OrderTag::StopLoss => "sl",
            OrderTag::QuoteBid => "quote-bid",
            OrderTag::QuoteAsk => "quote-ask",
        };
        write!(f, "{s}")
    }
}

/// Everything that can wake the engine. All feeds funnel into one queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Chainlink price tick from RTDS.
    PriceTick { px: f64, ts: f64 },
    /// Polymarket CLOB book top for one token.
    Book { token: TokenId, top: BookTop },
    /// Confirmed own trade (live: user channel; dry-run: simulated fill).
    UserTrade {
        token: TokenId,
        side: Side,
        px: f64,
        sz: f64,
        order_id: String,
        trade_id: String,
        maker: bool,
    },
    /// Binance partial book snapshot (top-N).
    BinanceBook {
        bids: Vec<(f64, f64)>,
        asks: Vec<(f64, f64)>,
        ts: f64,
    },
    /// Binance aggTrade.
    BinanceTrade {
        px: f64,
        sz: f64,
        is_buyer_maker: bool,
        ts: f64,
    },
    /// Result of an async live order submission.
    OrderAck {
        client_id: u64,
        result: Result<String, String>,
    },
    /// On-chain settled token balance (live mode): tokens only become
    /// sellable once the buy trade is MINED — this event carries the actual
    /// spendable size from the Data API.
    PositionSync { token: TokenId, settled: f64 },
    /// Feed-level notice (reconnects etc.) — logged only.
    FeedInfo(String),
}

/// One desired resting order: a (price, qty) target the OMS reconciles
/// the book toward. At most one target per (side, tag) per token.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetOrder {
    pub side: Side,
    pub px: f64,
    pub sz: f64,
    pub tag: OrderTag,
}

/// Strategy output. Executed in order by the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Place {
        token: TokenId,
        side: Side,
        px: f64,
        sz: f64,
        tag: OrderTag,
    },
    /// Declare the FULL desired order set for one token. The engine diffs
    /// against working orders: unchanged targets produce no traffic; a
    /// changed price/qty cancels the old order first, then places the new
    /// one; working orders with no matching target are cancelled. An empty
    /// set cancels everything on the token.
    Targets {
        token: TokenId,
        orders: Vec<TargetOrder>,
    },
    /// Cancel all resting orders on one token.
    CancelToken(TokenId),
    /// Cancel everything.
    CancelAll,
    /// Stop quoting for the remainder of this market window.
    Halt(String),
}

/// Epoch seconds as f64.
pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
