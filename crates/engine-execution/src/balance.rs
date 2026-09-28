//! USDC accounting across a user's wallet and Circle Gateway.
//! Amounts stay in integer base units; no floating-point money conversion.

use crate::gateway::{GatewayBalances, GatewayDeposits};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsdcBalanceBuckets {
    pub wallet_base_units: u128,
    pub gateway_confirmed_base_units: u128,
    pub gateway_pending_base_units: u128,
    pub spendable_base_units: u128,
    pub total_accounted_base_units: u128,
}

#[derive(Debug, Error)]
pub enum BalanceError {
    #[error("Circle returned a token other than USDC")]
    WrongToken,
    #[error("Circle returned a balance for an unexpected depositor")]
    WrongDepositor,
    #[error("Circle returned an invalid USDC amount")]
    InvalidAmount,
    #[error("USDC balance exceeds the supported integer range")]
    Overflow,
}

pub fn usdc_buckets(
    wallet_base_units: u128,
    depositor: &str,
    balances: &GatewayBalances,
    deposits: &GatewayDeposits,
) -> Result<UsdcBalanceBuckets, BalanceError> {
    if balances.token != "USDC" || deposits.token != "USDC" {
        return Err(BalanceError::WrongToken);
    }
    let mut confirmed = 0u128;
    for entry in &balances.balances {
        if !entry.depositor.eq_ignore_ascii_case(depositor) {
            return Err(BalanceError::WrongDepositor);
        }
        confirmed = confirmed
            .checked_add(decimal_usdc_to_base_units(&entry.balance)?)
            .ok_or(BalanceError::Overflow)?;
    }
    let mut pending = 0u128;
    for entry in &deposits.deposits {
        if !entry.depositor.eq_ignore_ascii_case(depositor) {
            return Err(BalanceError::WrongDepositor);
        }
        if entry.status == "pending" {
            pending = pending
                .checked_add(
                    entry
                        .amount
                        .parse::<u128>()
                        .map_err(|_| BalanceError::InvalidAmount)?,
                )
                .ok_or(BalanceError::Overflow)?;
        }
    }
    let spendable = wallet_base_units
        .checked_add(confirmed)
        .ok_or(BalanceError::Overflow)?;
    let total = spendable
        .checked_add(pending)
        .ok_or(BalanceError::Overflow)?;
    Ok(UsdcBalanceBuckets {
        wallet_base_units,
        gateway_confirmed_base_units: confirmed,
        gateway_pending_base_units: pending,
        spendable_base_units: spendable,
        total_accounted_base_units: total,
    })
}

fn decimal_usdc_to_base_units(amount: &str) -> Result<u128, BalanceError> {
    let (whole, fractional) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BalanceError::InvalidAmount);
    }
    if fractional.len() > 6 || !fractional.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BalanceError::InvalidAmount);
    }
    let whole: u128 = whole.parse().map_err(|_| BalanceError::InvalidAmount)?;
    let fraction: u128 = format!("{fractional:0<6}")
        .parse()
        .map_err(|_| BalanceError::InvalidAmount)?;
    whole
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(fraction))
        .ok_or(BalanceError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{GatewayBalanceEntry, GatewayDeposit};

    #[test]
    fn pending_base_deposit_keeps_full_total_visible() {
        let depositor = "0x123";
        let balances = GatewayBalances {
            token: "USDC".into(),
            balances: vec![GatewayBalanceEntry {
                domain: 6,
                depositor: depositor.into(),
                balance: "0".into(),
                pending_batch: Some("0".into()),
            }],
            success: None,
        };
        let deposits = GatewayDeposits {
            token: "USDC".into(),
            deposits: vec![GatewayDeposit {
                domain: 6,
                depositor: depositor.into(),
                transaction_hash: "0xabc".into(),
                amount: "5000000".into(),
                status: "pending".into(),
                block_height: Some("47415771".into()),
            }],
            success: None,
        };
        let b = usdc_buckets(15_000_000, depositor, &balances, &deposits).unwrap();
        assert_eq!(b.spendable_base_units, 15_000_000);
        assert_eq!(b.gateway_pending_base_units, 5_000_000);
        assert_eq!(b.total_accounted_base_units, 20_000_000);
    }

    #[test]
    fn decimal_amounts_require_exact_usdc_precision() {
        assert_eq!(decimal_usdc_to_base_units("1.25").unwrap(), 1_250_000);
        assert!(decimal_usdc_to_base_units("1.0000001").is_err());
        assert!(decimal_usdc_to_base_units("-1").is_err());
    }
}
