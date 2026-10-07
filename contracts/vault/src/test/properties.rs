//! Random sequences of deposits, limit changes, charges admitted now and
//! settled later (across ledgers and UTC days), withdrawals, exits, revenue
//! payouts and the passing of ledgers and days, each checked after every
//! step against a model written from the documented rules
//! (docs/architecture/vault-contract.md), not from the contract's code.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;

const BUYERS: usize = 3;
const WALLET: i128 = 50 * USDC;

#[derive(Clone, Debug)]
enum Op {
    Deposit {
        buyer: usize,
        amount: i128,
        id: u8,
        cap: Option<i128>,
    },
    SetCap {
        buyer: usize,
        cap: i128,
    },
    /// A charge admitted now with a window of `window` ledgers, settled by a
    /// later `Settle`.
    Admit {
        buyer: usize,
        amount: i128,
        window: u32,
    },
    Settle,
    Withdraw {
        buyer: usize,
        amount: i128,
        id: u8,
    },
    RequestExit {
        buyer: usize,
        amount: i128,
    },
    Exit {
        buyer: usize,
    },
    WithdrawRevenue {
        amount: i128,
        id: u8,
    },
    Advance {
        ledgers: u32,
    },
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
        (buyer.clone(), usdc_tenths(10), prop_oneof![Just(0u32), 1..=MAX_CHARGE_WINDOW])
            .prop_map(|(buyer, amount, window)| Op::Admit { buyer, amount, window }),
        Just(Op::Settle),
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
    /// What was charged against each admission day.
    charged: BTreeMap<u64, i128>,
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

    /// A limit at least the one in force applies at once and drops any
    /// pending one; a lower one waits the notice, except that one no lower
    /// than the pending lower limit keeps that one's effective ledger.
    fn change_cap(&mut self, cap: i128, now: u32) {
        let in_force = self.cap_in_force(now);
        let pending = self.pending.filter(|(_, at)| now < *at);
        self.cap = in_force;
        self.pending = None;
        if cap >= in_force {
            self.cap = cap;
        } else {
            let at = match pending {
                Some((pending_cap, at)) if cap >= pending_cap => at,
                _ => now + NOTICE_LEDGERS,
            };
            self.pending = Some((cap, at));
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

/// A charge waiting to settle: buyer, identifier, amount, last ledger, day.
type Queued = (usize, u8, i128, u32, u64);

fn run(ops: &[Op]) {
    let w = world();
    let buyers: StdVec<Address> = (0..BUYERS).map(|_| w.buyer(WALLET)).collect();
    let mut model = Model::default();
    for buyer in &mut model.buyers {
        buyer.wallet = WALLET;
    }
    let mut charge_ids = 0u8;
    let mut queue: StdVec<Queued> = StdVec::new();

    for (step, op) in ops.iter().enumerate() {
        let now = w.now();
        let today = w.today();
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
                assert_eq!(result.is_ok(), b.balance.is_some(), "step {step}: {op:?}");
                if b.balance.is_some() {
                    b.change_cap(cap, now);
                }
            }
            Op::Admit { buyer, amount, window } => {
                charge_ids = charge_ids.wrapping_add(1);
                queue.push((buyer, charge_ids, amount, now + window, today));
            }
            Op::Settle => {
                let mut charges = StdVec::new();
                let mut expected = StdVec::new();
                for (buyer, id, amount, last_ledger, day) in queue.drain(..) {
                    let b = &mut model.buyers[buyer];
                    // Refusals, in the documented order.
                    let outcome = if last_ledger < now || day + 1 < today {
                        Outcome::Expired
                    } else if let Some(balance) = b.balance {
                        let counted = b.charged.get(&day).copied().unwrap_or(0);
                        if amount > balance {
                            Outcome::InsufficientBalance
                        } else if counted + amount > b.cap_in_force(now) {
                            Outcome::AboveCap
                        } else {
                            b.balance = Some(balance - amount);
                            b.charged.insert(day, counted + amount);
                            model.revenue += amount;
                            Outcome::Charged
                        }
                    } else {
                        Outcome::UnknownAccount
                    };
                    charges.push(Charge(buyers[buyer].clone(), w.id(id), amount, last_ledger, day));
                    expected.push(outcome);
                }
                if !charges.is_empty() {
                    assert_eq!(w.settle(&charges), expected, "step {step}: {op:?}");
                }
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
                assert_eq!(result.is_ok(), b.balance.is_some(), "step {step}: {op:?}");
                if b.balance.is_some() {
                    // No more than the pending amount keeps its unlock.
                    let unlock = match b.exit {
                        Some((pending, unlock)) if amount <= pending => unlock,
                        _ => now + NOTICE_LEDGERS,
                    };
                    b.exit = Some((amount, unlock));
                }
            }
            Op::Exit { buyer } => {
                let b = &mut model.buyers[buyer];
                let paid = b
                    .exit
                    .filter(|(_, unlock)| now >= *unlock)
                    .map(|(amount, _)| amount.min(b.balance.unwrap_or(0)))
                    .filter(|paid| *paid > 0);
                let result = w.client().try_exit(&buyers[buyer]);
                assert_eq!(result.is_ok(), paid.is_some(), "step {step}: {op:?} -> {result:?}");
                if let Some(paid) = paid {
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
            assert_eq!(w.client().get_balance(buyer), b.balance.unwrap_or(0), "step {step}: {i}");
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
