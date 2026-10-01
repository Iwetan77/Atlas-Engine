//! Shared domain types for Atlas Engine.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Chain {
    Solana,
    Base,
    Arc,
    Ethereum,
    Arbitrum,
    Optimism,
    Polygon,
    Unichain,
    Aptos,
    Near,
    Monad,
    Sui,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Venue {
    Daya,
    Circle,
    Jupiter,
    OneInch,
    Hyperliquid,
    Jito,
    Marinade,
    Aave,
    Moonwell,
    NearIntents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentKind {
    Buy,
    Sell,
    Send,
    OffRamp,
    PerpOpen,
    PerpClose,
    YieldDeposit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentStage {
    Discover,
    Validate,
    Execute,
    Settle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Intent {
    pub id: String,
    pub user_id: String,
    pub kind: IntentKind,
    pub query: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Quote {
    pub intent_id: String,
    pub venue: Venue,
    pub chain: Chain,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentEvent {
    pub intent: Intent,
    pub stage: IntentStage,
}
