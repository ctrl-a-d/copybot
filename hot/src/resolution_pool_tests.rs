use crate::{
    ledger::Ledger,
    resolution::{book_verified_pool, Proof},
    risk::RiskConfig,
};

fn configs() -> Vec<(String, RiskConfig)> {
    vec![
        ("a".into(), RiskConfig::default()),
        ("b".into(), RiskConfig::default()),
    ]
}

#[test]
fn settlement_before_reconciliation_cannot_create_a_second_credit() {
    let mut ledger = Ledger::new("", &configs());
    assert!(ledger.record_fill("a", "101", 0, 10.0, 0.6, 0.0));
    let before = Proof {
        payout: 1.0,
        balance_units: 10_000_000,
        redeemed_units: 0,
    };
    assert_eq!(
        book_verified_pool(&mut ledger, "101", &before).unwrap(),
        vec![("a".into(), 4.0)]
    );
    ledger.record_recon_fill("a", "101", 1, 10.0, 0.6, 0.0);
    assert_eq!(ledger.pending_release("a", "101"), None);
    let after = Proof {
        payout: 1.0,
        balance_units: 0,
        redeemed_units: 10_000_000,
    };
    assert!(book_verified_pool(&mut ledger, "101", &after)
        .unwrap()
        .is_empty());
    assert_eq!(ledger.lanes["a"].risk.realised_pnl, 4.0);
}

#[test]
fn reconciliation_before_settlement_keeps_losing_cost_until_verified_burn() {
    let mut ledger = Ledger::new("", &configs());
    assert!(ledger.record_fill("a", "101", 0, 10.0, 0.6, 0.0));
    ledger.record_recon_fill("a", "101", 1, 10.0, 0.6, 0.0);
    for (balance_units, redeemed_units) in [(0, 0), (10_000_000, 0), (0, 20_000_000)] {
        assert!(book_verified_pool(
            &mut ledger,
            "101",
            &Proof {
                payout: 0.0,
                balance_units,
                redeemed_units
            }
        )
        .is_err());
        assert_eq!(ledger.pending_release("a", "101"), Some((10.0, 6.0)));
        assert_eq!(ledger.lanes["a"].risk.realised_pnl, 0.0);
    }
    let proof = Proof {
        payout: 0.0,
        balance_units: 0,
        redeemed_units: 10_000_000,
    };
    assert_eq!(
        book_verified_pool(&mut ledger, "101", &proof).unwrap(),
        vec![("a".into(), -6.0)]
    );
    assert!(book_verified_pool(&mut ledger, "101", &proof)
        .unwrap()
        .is_empty());
    assert_eq!(ledger.pending_release("a", "101"), None);
}

#[test]
fn pooled_evidence_covers_every_lane_and_survives_partial_commit_restart() {
    let path = std::env::temp_dir().join(format!("resolution-pool-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = configs();
    let mut ledger = Ledger::new(path.to_str().unwrap(), &cfg);
    assert!(ledger.record_fill("a", "101", 0, 4.0, 0.5, 0.0));
    assert!(ledger.record_fill("b", "101", 0, 6.0, 0.5, 0.0));
    ledger.record_recon_fill("a", "101", 1, 4.0, 0.5, 0.0);
    let proof = Proof {
        payout: 1.0,
        balance_units: 6_000_000,
        redeemed_units: 4_000_000,
    };
    assert!(book_verified_pool(
        &mut ledger,
        "101",
        &Proof {
            payout: 1.0,
            balance_units: 0,
            redeemed_units: 4_000_000
        }
    )
    .is_err());
    // Simulate durable completion of the first lane, followed by process loss.
    ledger.record_realised_adjustment_tx(
        "a",
        "101",
        4.0,
        4.0,
        0.5,
        "verified redemption",
        "resolution:a:101:0",
    );
    drop(ledger);
    let mut replay = Ledger::new(path.to_str().unwrap(), &cfg);
    assert_eq!(
        book_verified_pool(&mut replay, "101", &proof).unwrap(),
        vec![("b".into(), 3.0)]
    );
    drop(replay);
    let mut replay = Ledger::new(path.to_str().unwrap(), &cfg);
    assert!(book_verified_pool(&mut replay, "101", &proof)
        .unwrap()
        .is_empty());
    assert_eq!(replay.lanes["a"].risk.realised_pnl, 2.0);
    assert_eq!(replay.lanes["b"].risk.realised_pnl, 3.0);
    assert_eq!(replay.pool_settled_shares("101"), 10.0);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn changed_pool_and_invalid_proofs_do_not_mutate_accounting() {
    let mut ledger = Ledger::new("", &configs());
    assert!(ledger.record_fill("a", "101", 0, 10.0, 0.6, 0.0));
    let proof = Proof {
        payout: 1.0,
        balance_units: 10_000_000,
        redeemed_units: 0,
    };
    assert!(ledger.record_fill("b", "101", 0, 5.0, 0.4, 0.0));
    assert!(book_verified_pool(&mut ledger, "101", &proof).is_err());
    for payout in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        assert!(book_verified_pool(
            &mut ledger,
            "101",
            &Proof {
                payout,
                balance_units: 15_000_000,
                redeemed_units: 0
            }
        )
        .is_err());
    }
    assert_eq!(ledger.pool_claim("101"), 15.0);
    assert_eq!(ledger.lanes["a"].risk.realised_pnl, 0.0);
}
