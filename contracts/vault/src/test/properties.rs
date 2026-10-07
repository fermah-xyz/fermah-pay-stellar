//! Random sequences of deposits, limit changes, charges, withdrawals, exits,
//! revenue payouts and the passing of ledgers and days, each checked after
//! every step against a model written from the documented rules.

use proptest::prelude::*;

use super::*;

const BUYERS: usize = 3;
const WALLET: i128 = 50 * USDC;

#[derive(Clone, Debug)]
enum Op {
    Deposit { buyer: usize, amount: i128, id: u8, cap: Option<i128> },
    SetCap { buyer: usize, cap: i128 },
    Charge { buyer: usize, amount: i128 },
    Withdraw { buyer: usize, amount: i128, id: u8 },
    RequestExit { buyer: usize, amount: i128 },
    Exit { buyer: usize },
    WithdrawRevenue { amount: i128, id: u8 },
    Advance { ledgers: u32 },
    NextDay,
}

fn usdc_tenths(max: i128) -> impl Strategy<Value = i128> {
    (1..=max * 10).prop_map(|tenths| tenths * USDC / 10)
}

fn op() -> impl Strategy<Value = Op> {
    let buyer = 0..BUYERS;
    prop_oneof![
        (buyer.clone(), usdc_tenths(20), 0u8..4, prop::option::of(0..=20 * USDC))
            .prop_map(|(buyer, amount, id, cap)| Op::Deposit { buyer, amount, id, cap }),
        (buyer.clone(), 0..=20 * USDC).prop_map(|(buyer, cap)| Op::SetCap { buyer, cap }),
        (buyer.clone(), usdc_tenths(10)).prop_map(|(buyer, amount)| Op::Charge { buyer, amount }),
        (buyer.clone(), usdc_tenths(10), 0u8..4).prop_map(|(buyer, amount, id)| Op::Withdraw {
            buyer,
            amount,
            id
        }),
        (buyer.clone(), usdc_tenths(20))
            .prop_map(|(buyer, amount)| Op::RequestExit { buyer, amount }),
        buyer.prop_map(|buyer| Op::Exit { buyer }),
        (usdc_tenths(10), 0u8..4).prop_map(|(amount, id)| Op::WithdrawRevenue { amount, id }),
        prop_oneof![
            Just(1u32),
            Just(NOTICE_LEDGERS - 1),
            Just(NOTICE_LEDGERS),
            0..2 * NOTICE_LEDGERS
        ]
        .prop_map(|ledgers| Op::Advance { ledgers }),
        Just(Op::NextDay),
    ]
}

#[derive(Clone, Debug, Default)]
struct Buyer {
    wallet: i128,
    balance: Option<i128>,
    cap: i128,
    pending: Option<(i128, u32)>,
    exit: Option<(i128, u32)>,
    day: u64,
    charged: i128,
    deposits: std::collections::BTreeSet<u8>,
    withdrawals: std::collections::BTreeSet<u8>,
}

impl Buyer {
    fn cap_in_force(&self, now: u32) -> i128 {
        match self.pending {
            Some((cap, at)) if now >= at => cap,
            _ => self.cap,
        }
    }

    /// The rules for a limit change: at once unless lower than the limit in
    /// force, which waits the notice and replaces any pending one.
    fn change_cap(&mut self, cap: i128, now: u32) {
        self.cap = self.cap_in_force(now);
        self.pending = None;
        if cap >= self.cap {
            self.cap = cap;
        } else {
            self.pending = Some((cap, now + NOTICE_LEDGERS));
        }
    }
}

#[derive(Debug, Default)]
struct Model {
    buyers: [Buyer; BUYERS],
    revenue: i128,
    revenue_ids: std::collections::BTreeSet<u8>,
    held: i128,
}

fn run(ops: &[Op]) {
    let w = world();
    let buyers: StdVec<Address> = (0..BUYERS).map(|_| w.buyer(WALLET)).collect();
    let mut model = Model::default();
    for buyer in &mut model.buyers {
        buyer.wallet = WALLET;
    }
    let mut charge_ids = 0u8;

    for (step, op) in ops.iter().enumerate() {
        let now = w.now();
        let today = w.env.ledger().timestamp() / 86_400;
        match op.clone() {
            Op::Deposit { buyer, amount, id, cap } => {
                let b = &mut model.buyers[buyer];
                let accepted = !b.deposits.contains(&id) && b.wallet >= amount;
                let result = w.client().try_deposit(&buyers[buyer], &amount, &w.id(id), &cap);
                assert_eq!(result.is_ok(), accepted, "step {step}: {op:?} -> {result:?}");
                if accepted {
                    b.deposits.insert(id);
                    b.wallet -= amount;
                    b.balance = Some(b.balance.unwrap_or(0) + amount);
                    model.held += amount;
                    if let Some(cap) = cap {
                        b.change_cap(cap, now);
                    }
                }
            }
            Op::SetCap { buyer, cap } => {
                let b = &mut model.buyers[buyer];
                let result = w.client().try_set_cap(&buyers[buyer], &cap);
                assert_eq!(
                    result.is_ok(),
                    b.balance.is_some(),
                    "step {step}: {op:?} -> {result:?}"
                );
                if b.balance.is_some() {
                    b.change_cap(cap, now);
                }
            }
            Op::Charge { buyer, amount } => {
                charge_ids = charge_ids.wrapping_add(1);
                let b = &mut model.buyers[buyer];
                let today_charged = if b.day == today { b.charged } else { 0 };
                let expected = match b.balance {
                    None => Outcome::UnknownAccount,
                    Some(balance) if amount > balance => Outcome::InsufficientBalance,
                    Some(_) if today_charged + amount > b.cap_in_force(now) => Outcome::AboveCap,
                    Some(balance) => {
                        b.balance = Some(balance - amount);
                        b.day = today;
                        b.charged = today_charged + amount;
                        model.revenue += amount;
                        Outcome::Charged
                    }
                };
                let charge = Charge(buyers[buyer].clone(), w.id(charge_ids), amount, now);
                assert_eq!(w.settle(&[charge]), [expected], "step {step}: {op:?}");
            }
            Op::Withdraw { buyer, amount, id } => {
                let b = &mut model.buyers[buyer];
                let accepted =
                    !b.withdrawals.contains(&id) && b.balance.is_some_and(|x| x >= amount);
                let result =
                    w.client().try_withdraw(&buyers[buyer], &amount, &buyers[buyer], &w.id(id));
                assert_eq!(result.is_ok(), accepted, "step {step}: {op:?} -> {result:?}");
                if accepted {
                    b.withdrawals.insert(id);
                    b.balance = b.balance.map(|x| x - amount);
                    b.wallet += amount;
                    model.held -= amount;
                }
            }
            Op::RequestExit { buyer, amount } => {
                let b = &mut model.buyers[buyer];
                let result = w.client().try_request_exit(&buyers[buyer], &amount, &buyers[buyer]);
                assert_eq!(
                    result.is_ok(),
                    b.balance.is_some(),
                    "step {step}: {op:?} -> {result:?}"
                );
                if b.balance.is_some() {
                    b.exit = Some((amount, now + NOTICE_LEDGERS));
                }
            }
            Op::Exit { buyer } => {
                let b = &mut model.buyers[buyer];
                let payable = b.exit.filter(|(_, unlock)| now >= *unlock);
                let result = w.client().try_exit(&buyers[buyer]);
                assert_eq!(result.is_ok(), payable.is_some(), "step {step}: {op:?} -> {result:?}");
                if let Some((amount, _)) = payable {
                    let paid = amount.min(b.balance.unwrap());
                    b.balance = b.balance.map(|x| x - paid);
                    b.wallet += paid;
                    b.exit = None;
                    model.held -= paid;
                }
            }
            Op::WithdrawRevenue { amount, id } => {
                let accepted = !model.revenue_ids.contains(&id) && model.revenue >= amount;
                let result = w.client().try_withdraw_revenue(&w.seller, &amount, &w.id(id));
                assert_eq!(result.is_ok(), accepted, "step {step}: {op:?} -> {result:?}");
                if accepted {
                    model.revenue_ids.insert(id);
                    model.revenue -= amount;
                    model.held -= amount;
                }
            }
            Op::Advance { ledgers } => w.advance(ledgers),
            Op::NextDay => w.next_day(),
        }

        let now = w.now();
        let mut liabilities = 0;
        for (i, buyer) in buyers.iter().enumerate() {
            let b = &model.buyers[i];
            assert_eq!(
                w.client().get_balance(buyer),
                b.balance.unwrap_or(0),
                "step {step}: balance {i}"
            );
            assert_eq!(w.token().balance(buyer), b.wallet, "step {step}: wallet {i}");
            assert_eq!(w.client().get_cap(buyer), b.cap_in_force(now), "step {step}: cap {i}");
            let exit = w.client().get_account(buyer).and_then(|a| match a.exit {
                Exit::Requested(e) => Some((e.amount, e.unlock_at)),
                Exit::None => None,
            });
            assert_eq!(exit, b.exit, "step {step}: exit {i}");
            liabilities += b.balance.unwrap_or(0);
        }
        assert_eq!(
            w.client().get_totals(),
            Totals { liabilities, revenue: model.revenue },
            "step {step}: totals"
        );
        assert_eq!(w.held(), model.held, "step {step}: held");
        assert_eq!(model.held, liabilities + model.revenue, "step {step}: solvency");
    }
}

proptest! {
    #[test]
    fn test_random_sequences_match_the_model(ops in prop::collection::vec(op(), 1..40)) {
        run(&ops);
    }
}
