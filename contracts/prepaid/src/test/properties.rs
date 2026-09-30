//! Property tests: random sequences of deposits, charge batches, withdrawals,
//! revenue withdrawals, daily limit changes and day changes, each checked
//! against a reference model written independently of the contract. After
//! every step the contract's balances, totals, charge outcomes and the
//! treasury's USDC must equal the model's, and the contract's liabilities
//! must equal the sum of its balances.
//!
//! `PROPTEST_CASES` raises the number of sequences, e.g. for a nightly run.

use std::collections::HashSet;
use std::vec::Vec;

use proptest::prelude::*;

use super::*;

const BUYERS: usize = 3;
const WALLET: i128 = 100 * USDC;

#[derive(Clone, Debug)]
enum Op {
    Deposit { buyer: usize, amount: i128, id: u8 },
    Charges(Vec<(usize, u64, i128)>),
    Withdraw { buyer: usize, amount: i128, id: u8 },
    WithdrawRevenue { amount: i128, id: u8 },
    SetDailyLimits { per_buyer: i128, per_seller: i128 },
    NextDay,
}

fn op() -> impl Strategy<Value = Op> {
    let buyer = 0..BUYERS;
    prop_oneof![
        4 => (buyer.clone(), MIN_DEPOSIT..=3 * USDC, 0..6_u8)
            .prop_map(|(buyer, amount, id)| Op::Deposit { buyer, amount, id }),
        6 => prop::collection::vec((buyer.clone(), 0..12_u64, 1..=6 * USDC), 1..6)
            .prop_map(Op::Charges),
        2 => (buyer, 1..=3 * USDC, 0..6_u8)
            .prop_map(|(buyer, amount, id)| Op::Withdraw { buyer, amount, id }),
        1 => (1..=3 * USDC, 0..4_u8).prop_map(|(amount, id)| Op::WithdrawRevenue { amount, id }),
        1 => (1..=8 * USDC, 1..=15 * USDC)
            .prop_map(|(per_buyer, per_seller)| Op::SetDailyLimits { per_buyer, per_seller }),
        1 => Just(Op::NextDay),
    ]
}

/// What the contract should hold, derived from the rules as documented, not
/// from the contract's code.
#[derive(Default)]
struct Model {
    balances: [Option<i128>; BUYERS],
    revenue: i128,
    treasury: i128,
    wallets: [i128; BUYERS],
    charge_ids: HashSet<(usize, u64)>,
    deposit_ids: HashSet<(usize, u8)>,
    withdrawal_ids: HashSet<(usize, u8)>,
    revenue_ids: HashSet<u8>,
    limits: Option<(i128, i128)>,
    day: u64,
    charged_today: [i128; BUYERS],
    seller_today: i128,
}

impl Model {
    fn next_day(&mut self) {
        self.day += 1;
        self.charged_today = [0; BUYERS];
        self.seller_today = 0;
    }

    fn charge(&mut self, buyer: usize, id: u64, amount: i128) -> Outcome {
        if self.charge_ids.contains(&(buyer, id)) {
            return Outcome::Duplicate;
        }
        self.charge_ids.insert((buyer, id));
        let Some(balance) = self.balances[buyer] else { return Outcome::UnknownAccount };
        if amount > MAX_CHARGE {
            return Outcome::AboveLimit;
        }
        if amount > balance {
            return Outcome::InsufficientBalance;
        }
        if let Some((per_buyer, per_seller)) = self.limits
            && (self.charged_today[buyer] + amount > per_buyer
                || self.seller_today + amount > per_seller)
        {
            return Outcome::AboveDailyLimit;
        }
        self.balances[buyer] = Some(balance - amount);
        self.revenue += amount;
        self.charged_today[buyer] += amount;
        self.seller_today += amount;
        Outcome::Charged
    }
}

fn run(ops: &[Op]) {
    let w = world();
    let buyers: Vec<Party> = (0..BUYERS).map(|_| w.party(WALLET)).collect();
    for (i, buyer) in buyers.iter().enumerate() {
        w.owners.borrow_mut().insert(label(i), buyer.address.clone());
    }
    let mut model = Model { wallets: [WALLET; BUYERS], ..Model::default() };

    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Deposit { buyer, amount, id } => {
                let accepted = !model.deposit_ids.contains(&(*buyer, *id))
                    && model.wallets[*buyer] >= *amount;
                let result = w.deposit(&buyers[*buyer], label(*buyer), *amount, *id);
                assert_eq!(result.is_ok(), accepted, "step {step}: {op:?} -> {result:?}");
                if accepted {
                    model.deposit_ids.insert((*buyer, *id));
                    model.balances[*buyer] = Some(model.balances[*buyer].unwrap_or(0) + amount);
                    model.wallets[*buyer] -= amount;
                    model.treasury += amount;
                }
            }
            Op::Charges(entries) => {
                // One identifier twice in a batch is allowed: the second is a
                // duplicate of the first.
                let requests: Vec<ChargeRequest> = entries
                    .iter()
                    .map(|(buyer, id, amount)| w.charge(label(*buyer), *id, *amount))
                    .collect();
                let outcomes = w.charge_batch(&requests).unwrap();
                let expected: Vec<Outcome> = entries
                    .iter()
                    .map(|(buyer, id, amount)| model.charge(*buyer, *id, *amount))
                    .collect();
                let got: Vec<Outcome> = outcomes.iter().collect();
                assert_eq!(got, expected, "step {step}: {op:?}");
            }
            Op::Withdraw { buyer, amount, id } => {
                let accepted = !model.withdrawal_ids.contains(&(*buyer, *id))
                    && model.balances[*buyer].is_some_and(|balance| balance >= *amount);
                let owner = &buyers[*buyer];
                let intent = w.withdraw_intent(owner, *amount, owner, *id);
                let auths = [
                    w.signed(owner, w.deployment().owner_withdraw_authorization(&intent)),
                    w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
                ];
                let result = withdraw_with(&w, &intent, owner, owner, &auths);
                assert_eq!(result.is_ok(), accepted, "step {step}: {op:?} -> {result:?}");
                if accepted {
                    model.withdrawal_ids.insert((*buyer, *id));
                    model.balances[*buyer] = model.balances[*buyer].map(|b| b - amount);
                    model.wallets[*buyer] += amount;
                    model.treasury -= amount;
                }
            }
            Op::WithdrawRevenue { amount, id } => {
                let accepted = !model.revenue_ids.contains(id) && model.revenue >= *amount;
                let result = withdraw_revenue(&w, *amount, *id);
                assert_eq!(result.is_ok(), accepted, "step {step}: {op:?} -> {result:?}");
                if accepted {
                    model.revenue_ids.insert(*id);
                    model.revenue -= amount;
                    model.treasury -= amount;
                }
            }
            Op::SetDailyLimits { per_buyer, per_seller } => {
                set_daily(&w, *per_buyer, *per_seller).unwrap();
                model.limits = Some((*per_buyer, *per_seller));
            }
            Op::NextDay => {
                next_day(&w);
                model.next_day();
            }
        }

        let totals = w.client().get_totals();
        let mut sum = 0;
        for (i, buyer) in buyers.iter().enumerate() {
            let balance = w.client().get_balance(&buyer.address);
            assert_eq!(balance, model.balances[i].unwrap_or(0), "step {step}: buyer {i}");
            assert_eq!(w.usdc_balance(buyer), model.wallets[i], "step {step}: wallet {i}");
            sum += balance;
        }
        assert_eq!(totals.liabilities, sum, "step {step}: liabilities are the sum of balances");
        assert_eq!(totals.revenue, model.revenue, "step {step}: revenue");
        assert_eq!(w.usdc_balance(&w.treasury), model.treasury, "step {step}: treasury");
        assert_eq!(
            model.treasury,
            totals.liabilities + totals.revenue,
            "step {step}: the treasury holds what the contract owes"
        );
    }
}

fn label(buyer: usize) -> u8 {
    u8::try_from(buyer).unwrap() + 1
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(48),
        ..ProptestConfig::default()
    })]

    #[test]
    fn test_random_sequences_match_the_model(ops in prop::collection::vec(op(), 1..40)) {
        run(&ops);
    }
}
