//! The inclusion bid each new envelope carries.
//!
//! A bid is fixed when the envelope is built: the engine never replaces an
//! envelope in flight with a higher-fee copy (see
//! `docs/architecture/transactions.md` for why). So the bid is chosen to be
//! included at the first attempt, and raised on the next envelope when the
//! previous one expired unincluded:
//!
//! `bid = min(cap, max(floor, market, 2 * previous expired bid))`
//!
//! where `market` is a percentile of the inclusion fees the RPC node saw
//! charged over its recent window. The node reports each Soroban
//! transaction's whole inclusion fee, which for a fee bump covers two
//! operations, so using it as a per-operation bid errs high, never low.
//!
//! The network charges each operation the ledger's base fee, or, when its
//! lane was full, the lowest per-operation bid it let in, and never more
//! than the bid. So a bid above the market costs nothing extra outside
//! congestion, and the cap bounds what the fee source pays per operation
//! during it.

use fermah_pay_stellar_chain::rpc::FeePercentile;

/// The lowest inclusion fee per operation the network accepts.
pub const NETWORK_MINIMUM: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeePolicy {
    /// Lowest bid per operation, in stroops.
    pub floor: u32,
    /// Highest bid per operation, in stroops, whatever the market or the
    /// escalation asks for.
    pub cap: u32,
    /// Which percentile of recent Soroban inclusion fees to match.
    pub percentile: FeePercentile,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FeePolicyError {
    #[error("the lowest inclusion fee must be at least {NETWORK_MINIMUM} stroops")]
    FloorBelowNetworkMinimum,
    #[error("the highest inclusion fee must not be below the lowest")]
    CapBelowFloor,
}

impl FeePolicy {
    pub const fn new(
        floor: u32,
        cap: u32,
        percentile: FeePercentile,
    ) -> Result<Self, FeePolicyError> {
        if floor < NETWORK_MINIMUM {
            return Err(FeePolicyError::FloorBelowNetworkMinimum);
        }
        if cap < floor {
            return Err(FeePolicyError::CapBelowFloor);
        }
        Ok(Self { floor, cap, percentile })
    }

    /// A fixed bid: no market reading or escalation moves it.
    #[must_use]
    pub const fn fixed(stroops: u32) -> Self {
        Self { floor: stroops, cap: stroops, percentile: FeePercentile::P90 }
    }

    /// The bid for a new envelope, given the market percentile (`None` if it
    /// could not be read) and the bid of the source's previous envelope if
    /// that one expired without being included.
    #[must_use]
    pub fn bid(&self, market: Option<u64>, expired_bid: Option<u32>) -> Bid {
        let market_bid = market.map(|fee| u32::try_from(fee).unwrap_or(u32::MAX));
        let escalated = expired_bid.map(|previous| previous.saturating_mul(2));
        let wanted = self.floor.max(market_bid.unwrap_or(0)).max(escalated.unwrap_or(0));
        Bid { stroops: wanted.min(self.cap), wanted, market, escalated_from: expired_bid }
    }
}

/// A chosen bid and what it was chosen from, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bid {
    /// Stroops per operation the envelope carries.
    pub stroops: u32,
    /// What floor, market and escalation asked for before the cap.
    pub wanted: u32,
    pub market: Option<u64>,
    pub escalated_from: Option<u32>,
}

impl Bid {
    #[must_use]
    pub const fn capped(&self) -> bool {
        self.wanted > self.stroops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> FeePolicy {
        FeePolicy::new(10_000, 1_000_000, FeePercentile::P90).unwrap()
    }

    #[test]
    fn test_quiet_market_bids_the_floor() {
        // An empty fee window reports zeros.
        for market in [None, Some(0), Some(100), Some(10_000)] {
            assert_eq!(policy().bid(market, None).stroops, 10_000, "{market:?}");
        }
    }

    #[test]
    fn test_market_above_the_floor_is_matched() {
        let bid = policy().bid(Some(10_001), None);
        assert_eq!((bid.stroops, bid.capped()), (10_001, false));
    }

    #[test]
    fn test_market_above_the_cap_is_capped() {
        let exact = policy().bid(Some(1_000_000), None);
        let above = policy().bid(Some(1_000_001), None);
        let huge = policy().bid(Some(u64::MAX), None);
        assert_eq!((exact.stroops, exact.capped()), (1_000_000, false));
        assert_eq!((above.stroops, above.wanted, above.capped()), (1_000_000, 1_000_001, true));
        assert_eq!((huge.stroops, huge.wanted), (1_000_000, u32::MAX));
    }

    #[test]
    fn test_after_an_expiry_the_bid_doubles() {
        assert_eq!(policy().bid(None, Some(10_000)).stroops, 20_000);
        assert_eq!(policy().bid(Some(100), Some(40_000)).stroops, 80_000);
    }

    #[test]
    fn test_market_above_the_escalation_wins() {
        assert_eq!(policy().bid(Some(50_000), Some(10_000)).stroops, 50_000);
    }

    #[test]
    fn test_escalation_stops_at_the_cap() {
        let at_cap = policy().bid(None, Some(1_000_000));
        let near = policy().bid(None, Some(600_000));
        let overflow = policy().bid(None, Some(u32::MAX));
        assert_eq!((at_cap.stroops, at_cap.capped()), (1_000_000, true));
        assert_eq!(near.stroops, 1_000_000);
        assert_eq!(overflow.stroops, 1_000_000);
    }

    #[test]
    fn test_escalation_from_a_tiny_previous_bid_never_undercuts_the_floor() {
        assert_eq!(policy().bid(None, Some(100)).stroops, 10_000);
    }

    #[test]
    fn test_fixed_policy_ignores_market_and_escalation() {
        assert_eq!(FeePolicy::fixed(100).bid(Some(900_000), Some(5_000)).stroops, 100);
    }

    #[test]
    fn test_policy_bounds_are_checked() {
        assert_eq!(
            FeePolicy::new(99, 1_000, FeePercentile::P90),
            Err(FeePolicyError::FloorBelowNetworkMinimum)
        );
        assert_eq!(
            FeePolicy::new(1_000, 999, FeePercentile::P90),
            Err(FeePolicyError::CapBelowFloor)
        );
        assert!(FeePolicy::new(100, 100, FeePercentile::P90).is_ok());
    }
}
