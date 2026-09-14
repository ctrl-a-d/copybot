use crate::{
    config::Root,
    lanes::{Execution, Lane, LaneConfig, Router, Sizing, SizingBasis, Skip},
    signal_guard::{Progress, Refusal, SignalGuard},
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "copy-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn cfg() -> LaneConfig {
    LaneConfig {
        name: "example".into(),
        wallet20: [0x11; 20],
        sizing: Sizing::Pct(0.3),
        sizing_basis: SizingBasis::Shares,
        allow_opposite_outcomes: true,
        execution: Execution::Taker,
        buy_slippage_c: 0.01,
        sell_slippage_c: 0.02,
        copy_maker_sells: true,
        sell_floor_frac: 0.5,
        min_order_usd: 1.0,
        max_usd_per_fill: 100.0,
        daily_budget_usd: 1000.0,
        per_market_usd: 200.0,
        max_open_usd: 500.0,
        max_buy_price: 0.95,
        min_buy_price: 0.02,
        min_fill_floor: false,
        sell_all_frac: 1.0,
        max_effective_pct: 1.0,
        compound: false,
        copy_makers: true,
        exclude_political: false,
    }
}
fn router(c: LaneConfig) -> Router {
    c.validate().unwrap();
    let lane = Lane::new(c);
    lane.mark_ready();
    lane.state.armed.store(true, Ordering::Relaxed);
    Router::new(vec![lane])
}
fn buy(salt: u8, token: &str, quantity: f64, price: f64) -> crate::calldata::Decoded {
    crate::calldata::Decoded {
        condition_id: [9; 32],
        token_id: token.into(),
        side: 0,
        price,
        order_size: quantity,
        fill_size: quantity,
        role: "taker",
        salt: [salt; 32],
        occurrence: 0,
    }
}
fn observe(g: &SignalGuard, d: &crate::calldata::Decoded, id: &str, t: i64) -> Progress {
    g.observe_fill(
        &d.salt,
        &d.condition_id,
        "example",
        &d.token_id,
        id,
        d.fill_size,
        t,
    )
    .unwrap()
}
#[test]
fn opposite_legs_and_fractional_exit_preserve_the_selected_ratio() {
    let path = Temp::new();
    let g = SignalGuard::open(&path.0, 1000).unwrap();
    let r = router(cfg());
    let l = r.lane(0).unwrap();
    for (salt, tok, q, p) in [(1, "101", 40.0, 0.8), (2, "102", 20.0, 0.2)] {
        let d = buy(salt, tok, q, p);
        let progress = observe(&g, &d, &format!("fill-{salt}"), 1000 + salt as i64);
        g.check_with_policy(
            &d.salt,
            &d.condition_id,
            "example",
            tok,
            1000 + salt as i64,
            true,
        )
        .unwrap();
        let i = r.decide(0, &d, progress).unwrap();
        assert_eq!(i.shares, q * 0.3);
        g.commit_with_policy(
            &d.salt,
            &d.condition_id,
            "example",
            tok,
            i.shares,
            1000 + salt as i64,
            true,
        )
        .unwrap();
        l.holdings.lock().unwrap().insert(tok.into(), i.shares);
        l.set_his(tok, q);
    }
    let mut sell = buy(3, "102", 10.0, 0.4);
    sell.side = 1;
    assert_eq!(r.decide(0, &sell, Progress::default()).unwrap().shares, 3.0);
}
#[test]
fn distinct_partial_fills_accumulate_beyond_signed_buy_minimum_and_survive_restart() {
    let path = Temp::new();
    let g = SignalGuard::open(&path.0, 1000).unwrap();
    let r = router(cfg());
    let mut d = buy(1, "101", 120.0, 0.5);
    d.order_size = 100.0;
    let p = observe(&g, &d, "transaction-a:0", 1000);
    let i = r.decide(0, &d, p).unwrap();
    assert_eq!(i.shares, 36.0);
    g.commit_with_policy(
        &d.salt,
        &d.condition_id,
        "example",
        "101",
        i.shares,
        1000,
        true,
    )
    .unwrap();
    drop(g);
    let g = SignalGuard::open(&path.0, 1001).unwrap();
    assert_eq!(
        g.observe_fill(
            &d.salt,
            &d.condition_id,
            "example",
            "101",
            "transaction-a:0",
            120.0,
            1001
        ),
        Err(Refusal::DuplicateOrder)
    );
    d.fill_size = 60.0;
    let p = observe(&g, &d, "transaction-b:0", 1002);
    assert_eq!(p.his_filled, 180.0);
    assert_eq!(p.our_copied, 36.0);
    assert_eq!(r.decide(0, &d, p).unwrap().shares, 18.0);
}
#[test]
fn minimum_and_caps_never_enlarge_or_trim_a_share_based_leg() {
    let r = router(cfg());
    let d = buy(1, "101", 10.0, 0.5);
    assert_eq!(
        r.decide(
            0,
            &d,
            Progress {
                his_filled: 10.0,
                our_copied: 0.0
            }
        ),
        Err(Skip::BelowVenueMinimum)
    );
    let mut c = cfg();
    c.max_usd_per_fill = 5.0;
    let r = router(c);
    let d = buy(2, "101", 100.0, 0.5);
    assert_eq!(
        r.decide(
            0,
            &d,
            Progress {
                his_filled: 100.0,
                our_copied: 0.0
            }
        ),
        Err(Skip::PerFillCap)
    );
    assert_eq!(r.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed), 0);
    let d = buy(3, "102", 100.0, 0.98);
    assert_eq!(
        r.decide(0, &d, Progress::default()),
        Err(Skip::PriceOutOfBand)
    );
}
#[test]
fn precision_is_bounded_and_a_later_fill_can_complete_a_small_target() {
    let path = Temp::new();
    let g = SignalGuard::open(&path.0, 1000).unwrap();
    let mut c = cfg();
    c.sizing = Sizing::Pct(1.0);
    let r = router(c);
    let mut d = buy(1, "101", 2.75, 0.5);
    d.order_size = 20.0;
    assert_eq!(
        r.decide(0, &d, observe(&g, &d, "a", 1000)),
        Err(Skip::BelowVenueMinimum)
    );
    d.fill_size = 3.129;
    let i = r.decide(0, &d, observe(&g, &d, "b", 1001)).unwrap();
    assert_eq!(i.shares, 5.87);
    assert!(i.shares <= 5.879 && 5.879 - i.shares < 0.01);
}
#[test]
fn allowing_opposite_outcomes_keeps_identity_and_legacy_guards() {
    let path = Temp::new();
    let g = SignalGuard::open(&path.0, 1000).unwrap();
    let d = buy(1, "101", 20.0, 0.5);
    observe(&g, &d, "a", 1000);
    g.commit_with_policy(&d.salt, &d.condition_id, "example", "101", 6.0, 1000, true)
        .unwrap();
    assert_eq!(
        g.check_with_policy(&d.salt, &d.condition_id, "example", "102", 1001, true),
        Err(Refusal::InvalidIdentity)
    );
    assert!(matches!(
        g.check(&[2; 32], &d.condition_id, "example", "102", 1001),
        Err(Refusal::MarketCooldown { .. })
    ));
    let path2 = Temp::new();
    std::fs::write(&path2.0,serde_json::json!({"t":1000,"salt":hex::encode([1;32]),"condition":hex::encode([9;32]),"lane":"example","his_filled":20.0,"our_copied":6.0}).to_string()+"\n").unwrap();
    let old = SignalGuard::open(&path2.0, 1001).unwrap();
    assert!(matches!(
        old.check_with_policy(&[2; 32], &[9; 32], "example", "102", 1001, true),
        Err(Refusal::MarketCooldown { .. })
    ));
}
#[test]
fn observations_without_copy_and_wal_commits_remain_deduplicated() {
    let path = Temp::new();
    let walpath = Temp::new();
    let g = SignalGuard::open(&path.0, 1000).unwrap();
    let d = buy(1, "101", 2.0, 0.5);
    observe(&g, &d, "small", 1000);
    drop(g);
    let g = SignalGuard::open(&path.0, 1001).unwrap();
    assert_eq!(
        g.observe_fill(
            &d.salt,
            &d.condition_id,
            "example",
            "101",
            "small",
            2.0,
            1001
        ),
        Err(Refusal::DuplicateOrder)
    );
    let p = g
        .observe_fill(
            &d.salt,
            &d.condition_id,
            "example",
            "101",
            "next",
            20.0,
            1002,
        )
        .unwrap();
    assert_eq!(p.his_filled, 22.0);
    let wal = crate::wal::Wal::open(&walpath.0).unwrap();
    g.commit_with_wal_policy(
        &d.salt,
        &d.condition_id,
        "example",
        "101",
        6.6,
        1002,
        &wal,
        None,
        true,
    )
    .unwrap();
    drop(g);
    drop(wal);
    let g = SignalGuard::open_with_wal(&path.0, Some(&walpath.0), 1003).unwrap();
    let p = g
        .observe_fill(
            &d.salt,
            &d.condition_id,
            "example",
            "101",
            "third",
            10.0,
            1003,
        )
        .unwrap();
    assert_eq!(p.his_filled, 32.0);
    assert_eq!(p.our_copied, 6.6);
}
#[test]
fn share_recovery_does_not_sell_down_to_a_price_adjusted_target() {
    let shares = crate::budget::target_shares_with_basis(
        SizingBasis::Shares,
        100.0,
        0.2,
        0.8,
        0.3,
        1.0,
        1.0,
        1.0,
        false,
    )
    .unwrap();
    assert!((shares - 30.01).abs() < 1e-9);
    let notional = crate::budget::target_shares_with_basis(
        SizingBasis::Notional,
        100.0,
        0.2,
        0.8,
        0.3,
        1.0,
        1.0,
        1.0,
        false,
    )
    .unwrap();
    assert_eq!(notional, 8.5);
}
#[test]
fn policy_survives_config_to_registry_to_runtime_and_old_configs_keep_defaults() {
    let text = r#"
[bot]
clob_host="https://clob.polymarket.com"
funder="example"
signer="example"
events_path="unused"
control_path="unused"
[[lane]]
name="example"
wallet="0x1111111111111111111111111111111111111111"
enabled=true
leader_max_order_usd=100.0
leader_peak_exposure_usd=500.0
[lane.sizing]
mode="pct"
pct=0.3
max_effective_pct=1.0
sizing_basis="shares"
min_fill_floor=false
[lane.budget]
bankroll_usd=1000.0
[lane.execution]
allow_opposite_outcomes=true
sell_all_frac=1.0
"#;
    let root: Root = toml::from_str(text).unwrap();
    let lanes = root.build_lanes().unwrap();
    assert_eq!(lanes[0].cfg.sizing_basis, SizingBasis::Shares);
    assert!(lanes[0].cfg.allow_opposite_outcomes);
    let specs = root.wallet_specs();
    let restored: crate::wallets::WalletSpec =
        serde_json::from_str(&serde_json::to_string(&specs[0]).unwrap()).unwrap();
    let (lane, _) = crate::config::build_runtime_lane(&restored).unwrap();
    assert_eq!(lane.cfg.sizing_basis, SizingBasis::Shares);
    assert!(lane.cfg.allow_opposite_outcomes);
    let old = text
        .replace("sizing_basis=\"shares\"\n", "")
        .replace("allow_opposite_outcomes=true\n", "");
    let old: Root = toml::from_str(&old).unwrap();
    assert_eq!(
        old.build_lanes().unwrap()[0].cfg.sizing_basis,
        SizingBasis::Notional
    );
    assert!(!old.wallet_specs()[0].allow_opposite_outcomes);
    let bad = text.replace("min_fill_floor=false", "min_fill_floor=true");
    assert!(toml::from_str::<Root>(&bad).unwrap().build_lanes().is_err());
    assert!(toml::from_str::<Root>(
        &text.replace("sizing_basis=\"shares\"", "sizing_basis=\"unknown\"")
    )
    .is_err());
}
