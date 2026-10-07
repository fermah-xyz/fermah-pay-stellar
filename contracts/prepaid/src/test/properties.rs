//! Property tests: random sequences of deposits, charge batches, withdrawals,
//! revenue withdrawals, daily limit changes, day changes, mandates, recurring
//! charge batches, revocations and allowance changes made directly in the
//! USDC contract, each checked against a reference model written
//! independently of the contract. After every step the contract's balances,
//! totals, charge outcomes, the treasury's and wallets' USDC and the
//! allowances must equal the model's, and the contract's liabilities must
//! equal the sum of its balances. A mandate's period is one day here, so day
//! changes move both the daily limits and the periods.
//!
//! `PROPTEST_CASES` raises the number of sequences, as the `Randomized`
//! workflow does.

use std::collections::HashSet;
use std::vec::Vec;

use proptest::prelude::*;

use fermah_pay_stellar_chain::prepaid::{MandateIntent, RecurringChargeRequest};

use super::recurring::{allowance, authorize, charge as charge_recurring, revoke};
use super::*;

const DAY: u64 = 86_400;

const BUYERS: usize = 3;
const WALLET: i128 = 100 * USDC;

#[derive(Clone, Debug)]
enum Op {
    Deposit {
        buyer: usize,
        amount: i128,
        id: u8,
    },
    Charges(Vec<(usize, u64, i128)>),
    Withdraw {
        buyer: usize,
        amount: i128,
        id: u8,
    },
    WithdrawRevenue {
        amount: i128,
        id: u8,
    },
    SetDailyLimits {
        per_buyer: i128,
        per_seller: i128,
    },
    NextDay,
    Authorize {
        buyer: usize,
        mandate: u8,
        amount: i128,
        cycles: u32,
    },
    /// Buyer, attempt identifier, mandate, period, amount.
    Recurring(Vec<(usize, u64, u8, u32, i128)>),
    Revoke {
        buyer: usize,
    },
    /// The buyer sets the contract's allowance in the USDC contract itself.
    SetAllowance {
        buyer: usize,
        amount: i128,
    },
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
        2 => Just(Op::NextDay),
        2 => (0..BUYERS, 0..3_u8, 1..=6 * USDC, 1..=4_u32)
            .prop_map(|(buyer, mandate, amount, cycles)| Op::Authorize {
                buyer,
                mandate,
                amount,
                cycles
            }),
        4 => prop::collection::vec(
            (0..BUYERS, 0..12_u64, 0..3_u8, 0..5_u32, 1..=6 * USDC),
            1..5
        )
        .prop_map(Op::Recurring),
        1 => (0..BUYERS).prop_map(|buyer| Op::Revoke { buyer }),
        1 => (0..BUYERS, 0..=12 * USDC).prop_map(|(buyer, amount)| Op::SetAllowance {
            buyer,
            amount
        }),
    ]
}

/// A mandate as the model keeps it.
#[derive(Clone, Copy)]
struct ModelMandate {
    id: u8,
    amount: i128,
    cycles: u32,
    start_day: u64,
    next_cycle: u32,
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
    mandates: [Option<ModelMandate>; BUYERS],
    allowances: [i128; BUYERS],
    recurring_ids: HashSet<(usize, u64)>,
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

    /// A recurring charge: once per period of the mandate it names, within
    /// the mandate's amount, the largest charge and the seller's daily limit
    /// (not the buyer's, which counts prepaid charges), and as far as the
    /// allowance and then the wallet allow, straight from the wallet into
    /// revenue.
    fn charge_recurring(
        &mut self,
        buyer: usize,
        id: u64,
        mandate: u8,
        cycle: u32,
        amount: i128,
    ) -> RecurringOutcome {
        if !self.recurring_ids.insert((buyer, id)) {
            return RecurringOutcome::Duplicate;
        }
        let Some(m) = self.mandates[buyer].filter(|m| m.id == mandate) else {
            return RecurringOutcome::NoMandate;
        };
        let due = self.day - m.start_day;
        if cycle >= m.cycles || due >= u64::from(m.cycles) {
            return RecurringOutcome::MandateExpired;
        }
        if cycle < m.next_cycle {
            return RecurringOutcome::AlreadyCharged;
        }
        if u64::from(cycle) > due {
            return RecurringOutcome::NotDue;
        }
        if u64::from(cycle) < due {
            return RecurringOutcome::PeriodOver;
        }
        if amount > m.amount {
            return RecurringOutcome::AboveMandate;
        }
        if amount > MAX_CHARGE {
            return RecurringOutcome::AboveLimit;
        }
        if self.limits.is_some_and(|(_, per_seller)| self.seller_today + amount > per_seller) {
            return RecurringOutcome::AboveDailyLimit;
        }
        if amount > self.allowances[buyer] {
            return RecurringOutcome::AllowanceShort;
        }
        if amount > self.wallets[buyer] {
            return RecurringOutcome::WalletShort;
        }
        self.allowances[buyer] -= amount;
        self.wallets[buyer] -= amount;
        self.treasury += amount;
        self.revenue += amount;
        self.seller_today += amount;
        self.mandates[buyer] = Some(ModelMandate { next_cycle: cycle + 1, ..m });
        RecurringOutcome::Charged
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
                let accepted =
                    !model.deposit_ids.contains(&(*buyer, *id)) && model.wallets[*buyer] >= *amount;
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
                    w.signed(&w.treasury, w.deployment().cosigner_withdraw_authorization(&intent)),
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
            Op::Authorize { buyer, mandate, amount, cycles } => {
                let intent = MandateIntent {
                    owner: buyers[*buyer].key.address(),
                    mandate_id: id32(*mandate),
                    amount: *amount,
                    period_secs: DAY,
                    cycles: *cycles,
                    live_until: w.env.ledger().sequence() + 1_000,
                };
                // Refused above the largest charge, and for the identifier
                // of the buyer's current mandate.
                let current = model.mandates[*buyer].as_ref().map(|m| m.id);
                if *amount > MAX_CHARGE || current == Some(*mandate) {
                    assert_eq!(
                        authorize(&w, &buyers[*buyer], &intent),
                        Err(contract_error(Error::InvalidMandate))
                    );
                    continue;
                }
                authorize(&w, &buyers[*buyer], &intent).unwrap();
                model.mandates[*buyer] = Some(ModelMandate {
                    id: *mandate,
                    amount: *amount,
                    cycles: *cycles,
                    start_day: model.day,
                    next_cycle: 0,
                });
                model.allowances[*buyer] = amount * i128::from(*cycles);
            }
            Op::Recurring(entries) => {
                let requests: Vec<RecurringChargeRequest> = entries
                    .iter()
                    .map(|(buyer, id, mandate, cycle, amount)| RecurringChargeRequest {
                        owner: buyers[*buyer].key.address(),
                        charge_id: charge_id(*id),
                        mandate_id: id32(*mandate),
                        cycle: *cycle,
                        amount: *amount,
                        last_ledger: w.env.ledger().sequence() + 100,
                    })
                    .collect();
                let got = charge_recurring(&w, &requests).unwrap();
                let expected: Vec<RecurringOutcome> = entries
                    .iter()
                    .map(|(buyer, id, mandate, cycle, amount)| {
                        model.charge_recurring(*buyer, *id, *mandate, *cycle, *amount)
                    })
                    .collect();
                assert_eq!(got, expected, "step {step}: {op:?}");
            }
            Op::Revoke { buyer } => {
                revoke(&w, &buyers[*buyer]).unwrap();
                model.mandates[*buyer] = None;
                model.allowances[*buyer] = 0;
            }
            Op::SetAllowance { buyer, amount } => {
                w.env.mock_all_auths();
                let live_until = if *amount > 0 { w.env.ledger().sequence() + 1_000 } else { 0 };
                token::Client::new(&w.env, &w.usdc).approve(
                    &buyers[*buyer].address,
                    &w.contract,
                    amount,
                    &live_until,
                );
                w.env.set_auths(&[]);
                model.allowances[*buyer] = *amount;
            }
        }

        let totals = w.client().get_totals();
        let mut sum = 0;
        for (i, buyer) in buyers.iter().enumerate() {
            let balance = w.client().get_balance(&buyer.address);
            assert_eq!(balance, model.balances[i].unwrap_or(0), "step {step}: buyer {i}");
            assert_eq!(w.usdc_balance(buyer), model.wallets[i], "step {step}: wallet {i}");
            assert_eq!(allowance(&w, buyer), model.allowances[i], "step {step}: allowance {i}");
            assert_eq!(
                w.client().get_mandate(&buyer.address).map(|m| m.next_cycle),
                model.mandates[i].map(|m| m.next_cycle),
                "step {step}: mandate {i}"
            );
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
