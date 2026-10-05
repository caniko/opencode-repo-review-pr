use repo_review::budget::{Budget, Period};

fn period() -> Period {
    Period {
        start: 100,
        end: 200,
        allowance: 50,
        reconciled_usage: 49,
        evidence: "dashboard receipt".into(),
        no_overage: true,
        included_requests: vec![],
    }
}

#[test]
fn reserves_last_credit_once_and_blocks_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let budget = Budget::new(dir.path(), "org/developer");
    budget.reconcile(period(), 101).unwrap();
    assert!(budget.reserve("first", 1, 101).unwrap());
    assert!(budget.reserve("first", 1, 101).unwrap());
    assert!(!budget.reserve("second", 1, 101).unwrap());
    assert_eq!(budget.show(101).unwrap().available, 0);
}

#[test]
fn expired_allowance_does_not_reset_on_calendar_rollover() {
    let dir = tempfile::tempdir().unwrap();
    let budget = Budget::new(dir.path(), "org/developer");
    budget.reconcile(period(), 101).unwrap();
    budget.reserve("ambiguous", 1, 101).unwrap();
    assert!(!budget.reserve("next-month", 1, 201).unwrap());
    let next = Period {
        start: 200,
        end: 300,
        reconciled_usage: 0,
        ..period()
    };
    budget.reconcile(next, 201).unwrap();
    assert_eq!(budget.show(201).unwrap().available, 49);
    assert!(!budget.reserve("rollback", 1, 101).unwrap());
}

#[test]
fn concurrent_clients_cannot_both_reserve_one_credit() {
    let dir = tempfile::tempdir().unwrap();
    Budget::new(dir.path(), "account")
        .reconcile(period(), 101)
        .unwrap();
    let clients: Vec<_> = (0..8)
        .map(|i| {
            let path = dir.path().to_owned();
            std::thread::spawn(move || {
                Budget::new(&path, "account")
                    .reserve(&format!("request-{i}"), 1, 101)
                    .unwrap()
            })
        })
        .collect();
    assert_eq!(
        clients
            .into_iter()
            .map(|c| c.join().unwrap())
            .filter(|won| *won)
            .count(),
        1
    );
}

#[test]
fn reconciliation_cannot_silently_free_a_charge_or_replay_an_expired_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let budget = Budget::new(dir.path(), "account");
    let initial = Period {
        reconciled_usage: 0,
        ..period()
    };
    budget.reconcile(initial.clone(), 101).unwrap();
    assert!(budget.reserve("charged", 3, 102).unwrap());
    budget
        .settle("charged", true, "provider completed", 103)
        .unwrap();
    assert_eq!(budget.show(104).unwrap().available, 47);
    budget.reconcile(initial, 105).unwrap();
    assert_eq!(budget.show(105).unwrap().available, 47);
    assert!(budget
        .reconcile(
            Period {
                included_requests: vec!["charged".into()],
                reconciled_usage: 3,
                ..period()
            },
            106
        )
        .is_ok());
    assert_eq!(budget.show(106).unwrap().available, 47);
    assert!(budget
        .reconcile(
            Period {
                reconciled_usage: 2,
                ..period()
            },
            107
        )
        .is_err());
    assert!(budget.reserve("pending", 1, 108).unwrap());
    assert!(!budget.reserve("pending", 1, 201).unwrap());
}
