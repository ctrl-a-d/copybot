# Configuration

The engine retains its existing TOML configuration format; runtime operator and registry state remain JSON. Packaging has not introduced a new configuration parser or strategy model.

## Configuration layers

| Layer | Location | Purpose |
| --- | --- | --- |
| Boot configuration | `deploy/copybot2.toml` | Custody, feeds, initial lanes, sizing, execution |
| Process environment | `deploy/copybot.env` | Signing key, observer configuration, feature gates |
| Operator state | Under configured `control_path` | Arm/off/halt intent and related state |
| Accounting and recovery | Configured ledger and `data/`, `run/` | Durable execution and reconciliation evidence |

Only examples ship. Do not commit any populated layer. The bot does not load `.env` files automatically; the service templates use systemd's `EnvironmentFile` mechanism.

## Sizing

`mode = "pct"` uses a fractional copy percentage: `0.005` means 0.5%, not 5%. `shares` and `usd` are also accepted by the existing parser. Percentage copying is subject to minimum order sizes, budgets, execution prices, and the configured exit policy; it is not a guarantee of an exact share ratio.

Use either `bankroll_usd` with derived budget fractions or the parser's absolute-cap mode. Do not mix derived and absolute caps. Set measured `leader_max_order_usd` and `leader_peak_exposure_usd` for your selected leader. The engine validates these rather than borrowing another leader's statistics.

`compound` controls the existing realized-gain/loss sizing behavior. Set it intentionally. A lane budget is a strategy allocation inside a shared account, not security isolation between different owners.

## Execution

The parser accepts `taker` or `hybrid`. Split buy/sell slippage fields take precedence over the legacy combined field. `copy_makers` and `copy_maker_sells` are separate choices. Preserve them when transferring a lane between boot configuration and the runtime registry.

Do not infer units from the historical `_c` suffix alone: inspect `hot/src/config.rs` and `hot/src/lanes.rs` for the value's actual use. This export preserves those calculations.

`sell_all_frac = 0.0` requests the current any-sell flatten policy. A nonzero threshold changes when an exit becomes a full flatten. `sell_floor_frac` constrains the exit price relative to the leader; allowing broad slippage can fill at a substantially worse price. Setting it to zero does not guarantee a full fill in an empty market.

## Observers

All services need the same install directory and engine port. The legacy observer wallet environment variables must be supplied for your instance. Runtime lane discovery and shared-wallet checks still use the engine's API; these observers are not interchangeable with a multi-tenant permissions system.

Notification credentials and recipients are optional and empty in the template. `GUARDIAN_ENFORCE`, `MERGE_RECONCILE`, `COPYBOT_MERGE_ENABLE`, and `ORPHAN_SWEEP` retain their existing meanings. The example leaves optional live-action gates off; enabling an on-chain feature requires the corresponding custody/RPC setup. Do not set optional gates simply to clear a warning.

## Optional proportional-copy policy

Two independent, opt-in settings support portfolios with additions, opposite outcomes and several related markets. They do not select a category or a trader. Omitted settings preserve the previous notional sizing and opposite-outcome cooldown.

| Setting | Default | Effect |
| --- | --- | --- |
| `lane.sizing.sizing_basis` | `"notional"` | `"shares"` targets a percentage of observed filled shares, independent of the copy price. Only valid with `mode = "pct"`. |
| `lane.execution.allow_opposite_outcomes` | `false` | Allows distinct orders on both outcomes of an identified condition within one lane. Duplicate-fill, identity and spending checks still apply. |

For example, the following overrides express a 25% share target with proportional exits. Apply them to an otherwise complete **disabled** lane and supply its own measured limits:

```toml
[lane.sizing]
mode = "pct"
pct = 0.25
max_effective_pct = 0.25
compound = false
sizing_basis = "shares"
min_fill_floor = false

[lane.execution]
allow_opposite_outcomes = true
sell_all_frac = 1.0
```

Share targets are cumulative per source order and rounded down to two decimal places. A later distinct fill can bring an initially subminimum target above the minimum. A fill received at a better price may exceed the signed BUY order's minimum share quantity; share mode tracks the actual distinct filled quantity instead of capping it at that signed minimum. Existing signing and order amount encoding remain in use; this setting does not guarantee venue acceptance or attainable prices.

`min_fill_floor=false` is required in share mode: a minimum-dollar bump would enlarge the intended leg. Below-minimum buys are refused, not silently increased. An order exceeding its per-fill cap returns `PerFillCap` instead of being partly clipped. Other price, cash, daily, per-market and open-exposure caps remain effective. Choosing a share ratio does not make a budget sufficient, and no order is exempt because it might be a hedge.

The existing minimum BUY quantity is five shares and the configured minimum cash amount still applies. SELL sizing retains its existing fractional exit behavior; native willingness to create a small sell is not proof the venue will accept it. Preserve evidence of minimum-size failures rather than treating those trades as copied.

In share mode, source-fill identities and accumulated quantity are written to the signal-guard journal before a copy can be submitted. Duplicate observations are rejected across restart; new partial fills can continue the same order. Records without enough historical identity information remain conservative (`UnverifiedFillHistory` or the existing cooldown). Never clear the journal to force an order through. Identity retention remains bounded by the existing 24-hour guard lifetime. Dry mode does not simulate real fills or the complete live guard/accounting lifecycle.

A persistence failure stops new buys through the existing incident latch. If a process crashes after persisting the source observation but before committing a copy, replaying that same observation is conservatively refused. A later distinct fill may catch up, but automatic recovery of every missed buy is not guaranteed.

Recovery uses the same sizing basis as entries. In share mode, price differences do not make a correctly proportioned holding appear oversized; the recovery ceiling allows one quantity tick of rounding tolerance. A genuine leader exit still requests an exit.

For an existing installation, the persisted wallet registry takes precedence over boot seeds. Its per-wallet JSON fields are `sizing_basis` and `allow_opposite_outcomes`, alongside the existing `min_fill_floor` and `sell_all_frac`. Disable and unload the lane before changing built settings, then rebuild it through the normal lifecycle. The dashboard status reports the **running lane's** policy; the HTTP wallet editor does not silently accept these new configuration fields. Editing TOML alone will not replace an existing registry entry.

Price bands remain an explicit operator decision through the existing `min_buy_price` and `max_buy_price` settings. This policy does not widen them automatically. Redemption, settlement, physical-wallet reconciliation and their optional gates retain their existing behavior. Test them separately: resolved tokens' old purchase cost is not unresolved exposure, and claimable payouts are not spendable until redemption succeeds.
