//! KyberSwap's aggregator on Base: it searches every exchange for the best route (often a few
//! percent better than one Uniswap pool for memecoins). No API key; Kyber asks for a client id.
//! Only its MetaAggregationRouterV2 is accepted, and a built swap is decoded and checked before
//! anyone signs it: the router, the tokens, exactly the quoted amount in, no fees, the user as the
//! one who receives, and a minimum out within the slippage asked for.

use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

const API: &str = "https://aggregator-api.kyberswap.com/base/api/v1/";
const CLIENT_ID: &str = "atlas";
/// Kyber's MetaAggregationRouterV2 on Base (docs.kyberswap.com, "Contracts & Addresses").
pub const KYBER_ROUTER: &str = "0x6131b5fae19ea4f9d964eac0408e4408b66337b5";
// Its AggregationExecutorProxy: the only contract the router may hand the input to.
const KYBER_EXECUTOR: &str = "0x8f10b468b06c6fd214b65f87778827f7d113f996";
// swap((address,address,bytes,(address,address,address[],uint256[],address[],uint256[],address,
// uint256,uint256,uint256,bytes),bytes))
const SWAP_SELECTOR: &str = "e21fd0e9";

#[derive(Debug, Error)]
pub enum KyberError {
    #[error("KyberSwap request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("KyberSwap refused: {0}")]
    Rejected(String),
    #[error("KyberSwap returned an unexpected answer: {0}")]
    InvalidResponse(&'static str),
}

#[derive(Clone)]
pub struct KyberClient {
    http: Client,
    base: Url,
}

/// A route Kyber found: exactly `amount_in` of `token_in` for about `amount_out` of `token_out`.
#[derive(Clone, Debug)]
pub struct KyberRoute {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: u128,
    pub amount_out: u128,
    /// Dollars in and out by Kyber's prices (0 when it has none), and the network fee in dollars.
    pub amount_in_usd: f64,
    pub amount_out_usd: f64,
    pub gas_usd: f64,
    // Handed back unchanged to build the swap.
    summary: Value,
}

impl KyberRoute {
    /// How much worse the fill is than the market price (0.02 = 2%), when Kyber prices both sides.
    pub fn price_impact(&self) -> Option<f64> {
        (self.amount_in_usd > 0.0 && self.amount_out_usd > 0.0)
            .then(|| 1.0 - self.amount_out_usd / self.amount_in_usd)
    }
}

/// A checked swap to send from the user's wallet: `to` is always Kyber's router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KyberSwap {
    pub to: String,
    pub data: String,
    pub amount_in: u128,
    pub amount_out: u128,
    pub min_out: u128,
}

impl KyberClient {
    pub fn new() -> Result<Self, KyberError> {
        Ok(Self {
            // Kyber's edge refuses requests without a User-Agent (403).
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent("atlas-engine")
                .build()?,
            base: Url::parse(API).map_err(|_| KyberError::InvalidResponse("base URL"))?,
        })
    }

    /// The best route for exactly `amount_in` of `token_in` (base units) into `token_out`.
    pub async fn route(
        &self,
        token_in: &str,
        token_out: &str,
        amount_in: u128,
    ) -> Result<KyberRoute, KyberError> {
        let mut url = self
            .base
            .join("routes")
            .map_err(|_| KyberError::InvalidResponse("URL"))?;
        url.query_pairs_mut()
            .append_pair("tokenIn", token_in)
            .append_pair("tokenOut", token_out)
            .append_pair("amountIn", &amount_in.to_string());
        let response = self
            .http
            .get(url)
            .header("x-client-id", CLIENT_ID)
            .send()
            .await?;
        parse_route(&checked(response).await?, token_in, token_out, amount_in)
    }

    /// Builds `route` as a swap from `wallet` that pays `wallet`, with a minimum out `slippage_bps`
    /// under the route and a deadline `deadline` (unix seconds).
    pub async fn build(
        &self,
        route: &KyberRoute,
        wallet: &str,
        slippage_bps: u16,
        deadline: u64,
    ) -> Result<KyberSwap, KyberError> {
        if !(1..=500).contains(&slippage_bps) {
            return Err(KyberError::InvalidResponse("slippage"));
        }
        let url = self
            .base
            .join("route/build")
            .map_err(|_| KyberError::InvalidResponse("URL"))?;
        let response = self
            .http
            .post(url)
            .header("x-client-id", CLIENT_ID)
            .json(&json!({
                "routeSummary":route.summary,"sender":wallet,"recipient":wallet,
                "slippageTolerance":slippage_bps,"deadline":deadline,"source":CLIENT_ID
            }))
            .send()
            .await?;
        parse_build(&checked(response).await?, route, wallet, slippage_bps)
    }
}

async fn checked(response: reqwest::Response) -> Result<Value, KyberError> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() || body["code"].as_i64() != Some(0) {
        let reason = body["message"]
            .as_str()
            .unwrap_or("no reason given")
            .chars()
            .take(200)
            .collect();
        return Err(KyberError::Rejected(reason));
    }
    Ok(body)
}

fn units(value: &Value) -> Option<u128> {
    value.as_str()?.parse().ok()
}
fn dollars(value: &Value) -> f64 {
    value
        .as_str()
        .and_then(|v| v.parse().ok())
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0)
}
fn same_address(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn parse_route(
    body: &Value,
    token_in: &str,
    token_out: &str,
    amount_in: u128,
) -> Result<KyberRoute, KyberError> {
    let invalid = KyberError::InvalidResponse;
    let data = &body["data"];
    let summary = &data["routeSummary"];
    if !same_address(
        data["routerAddress"].as_str().unwrap_or_default(),
        KYBER_ROUTER,
    ) {
        return Err(invalid("another router"));
    }
    if !same_address(summary["tokenIn"].as_str().unwrap_or_default(), token_in)
        || !same_address(summary["tokenOut"].as_str().unwrap_or_default(), token_out)
        || units(&summary["amountIn"]) != Some(amount_in)
    {
        return Err(invalid("route differs from the request"));
    }
    let amount_out = units(&summary["amountOut"])
        .filter(|out| *out > 0)
        .ok_or(invalid("no amount out"))?;
    Ok(KyberRoute {
        token_in: token_in.to_ascii_lowercase(),
        token_out: token_out.to_ascii_lowercase(),
        amount_in,
        amount_out,
        amount_in_usd: dollars(&summary["amountInUsd"]),
        amount_out_usd: dollars(&summary["amountOutUsd"]),
        gas_usd: dollars(&summary["gasUsd"]),
        summary: summary.clone(),
    })
}

fn parse_build(
    body: &Value,
    route: &KyberRoute,
    wallet: &str,
    slippage_bps: u16,
) -> Result<KyberSwap, KyberError> {
    let invalid = KyberError::InvalidResponse;
    let data = &body["data"];
    if !same_address(
        data["routerAddress"].as_str().unwrap_or_default(),
        KYBER_ROUTER,
    ) {
        return Err(invalid("another router"));
    }
    if !matches!(data["transactionValue"].as_str(), Some("0") | None) {
        return Err(invalid("asks for ETH"));
    }
    let calldata = data["data"].as_str().ok_or(invalid("no calldata"))?;
    let swap = decode_swap(calldata).ok_or(invalid("calldata"))?;
    // The slippage asked for, plus a basis point for Kyber's rounding.
    let floor = route.amount_out * u128::from(9_999 - slippage_bps) / 10_000;
    if swap.call_target != KYBER_EXECUTOR
        || !(swap.approve_target == KYBER_EXECUTOR || swap.approve_target == ZERO)
        || swap.src_token != route.token_in
        || swap.dst_token != route.token_out
        || swap.dst_receiver != wallet.to_ascii_lowercase()
        || swap.amount != route.amount_in
        || swap.src_receivers.iter().any(|r| r != KYBER_EXECUTOR)
        || swap.src_amounts.iter().sum::<u128>() != route.amount_in
        || !swap.fees_empty
        || swap.min_return == 0
        || swap.min_return < floor
    {
        return Err(invalid("swap differs from the route"));
    }
    Ok(KyberSwap {
        to: KYBER_ROUTER.into(),
        data: calldata.to_ascii_lowercase(),
        amount_in: route.amount_in,
        amount_out: route.amount_out,
        min_out: swap.min_return,
    })
}

const ZERO: &str = "0x0000000000000000000000000000000000000000";

// What the router's `swap` call says it will do (addresses lowercase, 0x-prefixed).
#[derive(Debug, PartialEq, Eq)]
struct SwapCall {
    call_target: String,
    approve_target: String,
    src_token: String,
    dst_token: String,
    src_receivers: Vec<String>,
    src_amounts: Vec<u128>,
    fees_empty: bool,
    dst_receiver: String,
    amount: u128,
    min_return: u128,
}

fn decode_swap(calldata: &str) -> Option<SwapCall> {
    let hex = calldata.strip_prefix("0x")?;
    if hex.get(..8)? != SWAP_SELECTOR || hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Vec<u8> = (8..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect::<Option<_>>()?;
    let word = |at: usize| bytes.get(at..at + 32);
    let uint = |at: usize| -> Option<u128> {
        let w = word(at)?;
        w[..16]
            .iter()
            .all(|b| *b == 0)
            .then(|| w[16..].try_into().ok().map(u128::from_be_bytes))?
    };
    let offset = |at: usize| -> Option<usize> { usize::try_from(uint(at)?).ok() };
    let address = |at: usize| -> Option<String> {
        let w = word(at)?;
        w[..12].iter().all(|b| *b == 0).then(|| {
            let mut text = String::from("0x");
            for b in &w[12..] {
                text.push_str(&format!("{b:02x}"));
            }
            text
        })
    };
    // swap(params): params is one dynamic tuple at offset `t`; its `desc` is another inside it.
    let t = offset(0)?;
    let d = t + offset(t + 96)?;
    let list = |at: usize| -> Option<(usize, usize)> {
        let start = d + offset(at)?;
        Some((start + 32, offset(start)?))
    };
    let (receivers_at, receivers) = list(d + 64)?;
    let (amounts_at, amounts) = list(d + 96)?;
    let (_, fee_receivers) = list(d + 128)?;
    let (_, fee_amounts) = list(d + 160)?;
    if receivers > 16 || amounts > 16 {
        return None;
    }
    Some(SwapCall {
        call_target: address(t)?,
        approve_target: address(t + 32)?,
        src_token: address(d)?,
        dst_token: address(d + 32)?,
        src_receivers: (0..receivers)
            .map(|i| address(receivers_at + 32 * i))
            .collect::<Option<_>>()?,
        src_amounts: (0..amounts)
            .map(|i| uint(amounts_at + 32 * i))
            .collect::<Option<_>>()?,
        fees_empty: fee_receivers == 0 && fee_amounts == 0,
        dst_receiver: address(d + 192)?,
        amount: uint(d + 224)?,
        min_return: uint(d + 256)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    const BRETT: &str = "0x532f27101965dd16442E59d40670FaF5eBB142E4";
    const WALLET: &str = "0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97";

    // Captured from Kyber's live API on 2026-10-01: $1 of USDC into BRETT, routed and built for WALLET
    // with 1% slippage.
    fn fixture() -> Value {
        serde_json::from_str(include_str!("kyberswap_fixture.json")).unwrap()
    }
    fn route() -> KyberRoute {
        parse_route(&fixture()["route"], USDC, BRETT, 1_000_000).unwrap()
    }

    #[test]
    fn reads_the_live_route() {
        let route = route();
        assert_eq!(route.amount_out, 174_735_685_616_291_217_408);
        assert!(route.gas_usd > 0.0 && route.gas_usd < 0.1);
        assert!(route.price_impact().unwrap().abs() < 0.01);
        // Another amount, token or router than asked: refused.
        assert!(parse_route(&fixture()["route"], USDC, BRETT, 2_000_000).is_err());
        assert!(parse_route(&fixture()["route"], BRETT, USDC, 1_000_000).is_err());
        let mut other = fixture()["route"].clone();
        other["data"]["routerAddress"] = json!("0x6868d319c8c9a78f7d39dc3602c5c917315132d7");
        assert!(parse_route(&other, USDC, BRETT, 1_000_000).is_err());
    }

    #[test]
    fn the_live_build_decodes_to_exactly_the_route() {
        let call = decode_swap(fixture()["build"]["data"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(call.call_target, KYBER_EXECUTOR);
        assert_eq!(call.src_token, USDC.to_ascii_lowercase());
        assert_eq!(call.dst_token, BRETT.to_ascii_lowercase());
        assert_eq!(call.dst_receiver, WALLET.to_ascii_lowercase());
        assert_eq!(call.amount, 1_000_000);
        assert_eq!(call.src_amounts, vec![1_000_000]);
        assert!(call.fees_empty);
        // 1% under the route.
        assert_eq!(call.min_return, 172_988_328_760_128_305_232);
        let swap = parse_build(&fixture()["build"], &route(), WALLET, 100).unwrap();
        assert_eq!(swap.to, KYBER_ROUTER);
        assert_eq!(swap.amount_in, 1_000_000);
        assert!(swap.data.starts_with("0xe21fd0e9"));
    }

    #[test]
    fn refuses_a_build_that_pays_anyone_else_or_differs() {
        let build = fixture()["build"].clone();
        let route = route();
        // Built for another wallet (the calldata pays WALLET).
        assert!(parse_build(
            &build,
            &route,
            "0x0000000000000000000000000000000000000001",
            100
        )
        .is_err());
        // A tighter slippage than the calldata's minimum allows.
        assert!(parse_build(&build, &route, WALLET, 98).is_err());
        // Another router, ETH asked for, a different amount in, or calldata for another call.
        let mut other = build.clone();
        other["data"]["routerAddress"] = json!("0x6868d319c8c9a78f7d39dc3602c5c917315132d7");
        assert!(parse_build(&other, &route, WALLET, 100).is_err());
        let mut eth = build.clone();
        eth["data"]["transactionValue"] = json!("1000");
        assert!(parse_build(&eth, &route, WALLET, 100).is_err());
        let mut more = route.clone();
        more.amount_in = 2_000_000;
        assert!(parse_build(&build, &more, WALLET, 100).is_err());
        let mut call = build.clone();
        let data = call["data"]["data"]
            .as_str()
            .unwrap()
            .replacen("e21fd0e9", "8af033fb", 1);
        call["data"]["data"] = json!(data);
        assert!(parse_build(&call, &route, WALLET, 100).is_err());
        // The receiver word swapped for another address.
        let mut stolen = build;
        let data = stolen["data"]["data"].as_str().unwrap().replace(
            "0000000000000000000000004838b106fce9647bdf1e7877bf73ce8b0bad5f97",
            "0000000000000000000000000000000000000000000000000000000000000bad",
        );
        stolen["data"]["data"] = json!(data);
        assert!(parse_build(&stolen, &route, WALLET, 100).is_err());
    }

    // Network: a real route and build (nothing is sent).
    // cargo test -p engine-execution live_kyberswap -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_kyberswap_base() {
        let client = KyberClient::new().unwrap();
        let route = client.route(USDC, BRETT, 1_000_000).await.unwrap();
        println!(
            "$1 → {:.2} BRETT (impact {:?}, gas ${:.4})",
            route.amount_out as f64 / 1e18,
            route.price_impact(),
            route.gas_usd
        );
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let swap = client.build(&route, WALLET, 100, deadline).await.unwrap();
        println!(
            "built for {}, min out {:.2} BRETT",
            swap.to,
            swap.min_out as f64 / 1e18
        );
        let back = client
            .route(BRETT, USDC, route.amount_out / 2)
            .await
            .unwrap();
        println!("half back → ${:.4}", back.amount_out as f64 / 1_000_000.0);
    }
}
