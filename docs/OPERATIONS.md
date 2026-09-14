# Operations

## Three controls, three different effects

| Operator state | Effect |
| --- | --- |
| Armed | Entries and exits may execute if all other checks permit them |
| Halt buys | Stops new entries while allowing the existing exit path |
| Off | Disarms that lane; both entry and exit execution are stopped |

A guardian halt is not necessarily a full disarm. Read the specific incident and lane state. Do not repeatedly re-arm to suppress an unexplained fault.

## Stop the process

```sh
sudo systemctl stop copybot-hot.service
sudo systemctl is-active copybot-hot.service
```

Stopping the engine does not liquidate holdings and does not prove that venue-resting orders were canceled. Check the venue separately. Stopping a service is different from changing persistent arm intent. If you also stop observation services or timers, document that monitoring is no longer running.

## Restart and recovery

Keep the ledger, signal guard, operator state, registry, funding records, and pending-execution evidence together. A restart is not a reset. Never delete state to make a reconciliation warning disappear. The engine's existing recovery logic depends on those files to avoid repeating or forgetting execution.

Before an upgrade, stop the instance cleanly and take a consistent backup of its configuration, environment, runtime state, and exact executable. Store backups outside Git and restrict access. Record the source commit. Start only one live execution process for a funded account.

Roll back the executable and configuration deliberately. Do not blindly overwrite newer accounting with an old backup after trades have occurred: restoring old state can duplicate orders or discard fills. Reconcile changed holdings and unknown outcomes before resuming.

## Resolution and automatic redemption

The engine checks tracked holdings and pending reconciliation releases about once per minute. It discovers the condition and outcome index through Gamma, verifies the token binding with CTF `getCollectionId`, and reads the payout vector and wallet balance at a finalized block through `FILLWATCH_RPC` (default `https://polygon.drpc.org`). `CTF_ADDRESS` may override the default CTF contract, as in the observers. Configure a working read-only RPC before running live.

Missing tokens alone do not prove redemption. Reconciliation retains their cost as pending settlement evidence. For redeemed inventory, the engine discovers REDEEM transactions through the public activity API and verifies successful canonical receipts, the wallet's token burns, and the condition's payout. Direct CTF, legacy adapter, and supported collateral-wrapper redemptions are recognized, including losing shares with zero payout. Unsupported or incomplete evidence stays pending and is retried. Network reads run outside the order execution loop.

The verified balances and redemption quantities must cover the whole lane pool, including previous settlement credits. Durable counters prevent receipt reuse across lanes and restarts. This assumes the bot tracks the wallet's trading activity; arbitrary manual trades or external transfers can make attribution ambiguous. Preserve unresolved records for investigation. Historical journal mistakes are not automatically rewritten.

The engine owns settlement writes. The legacy `deploy/settlewatch.py --apply` command now refuses to write; its default mode remains diagnostic only. Do not run older settlement writers against the same ledger.

Settlement accounting does not submit a redemption transaction or credit spendable cash. The separate authenticated collateral-balance refresh must observe the returned funds before they can fund another buy. Pending-order reserves and the daily spending limit still apply; redemption does not reset daily turnover.

## Pending-feed health

The pending-transaction connection must produce a valid full-transaction subscription notification within 15 seconds of subscription acknowledgement and at least once every 90 seconds afterward. Ping/Pong and unrelated messages do not reset this deadline. Valid transactions for other destinations do reset it because they prove the provider is delivering the subscribed stream. Expiry reconnects the feed; a live WebSocket alone is not evidence of a healthy transaction stream.

## Things to check

- Fresh feed and observer timestamps, not just an active process.
- Intended mode and lane state; startup can restore existing operator intent.
- Ledger/custody agreement and unresolved or ambiguous execution.
- Correct funding basis before interpreting P&L.
- Guardian and watcher journals, notification delivery, and writable state paths.
- Venue-resting orders independently of dashboard connection status.

Engine logs go to journald through the service template. Configure bounded journal retention on the host. The provided logrotate template covers file-based observer logs; adjust paths before installing it. Never commit logs, screenshots, backup archives, or portfolio exports to report a bug. Use a synthetic reproduction.
