//! Resolve locally held tokens even after an external redeemer burns them.
//! Market metadata identifies the condition; only the CTF payout vector proves
//! final resolution. An empty portfolio or a quoted price is not that proof.
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
struct Outcome {
    condition: [u8; 32],
    index: usize,
    tokens: [String; 2],
}

fn market_outcome(body: &Value, token: &str) -> Option<Outcome> {
    let markets = body.as_array()?;
    if markets.len() != 1 {
        return None;
    }
    let market = &markets[0];
    let condition = crate::merge::condition_id_bytes(market["conditionId"].as_str()?)?;
    let tokens: Vec<String> = match &market["clobTokenIds"] {
        Value::String(s) => serde_json::from_str(s).ok()?,
        other => serde_json::from_value(other.clone()).ok()?,
    };
    if tokens.len() != 2
        || tokens[0] == tokens[1]
        || tokens
            .iter()
            .any(|t| t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(Outcome {
        condition,
        index: tokens.iter().position(|t| t == token)?,
        tokens: tokens.try_into().ok()?,
    })
}

fn rpc_word(body: &Value) -> Option<u128> {
    if body.get("error").is_some() {
        return None;
    }
    let raw = hex::decode(body["result"].as_str()?.strip_prefix("0x")?).ok()?;
    // Larger integers are refused rather than rounded or truncated.
    if raw.len() != 32 || raw[..16].iter().any(|b| *b != 0) {
        return None;
    }
    Some(u128::from_be_bytes(raw[16..].try_into().ok()?))
}

async fn call(http: &reqwest::Client, rpc: &str, ctf: &str, data: String) -> Option<u128> {
    let body: Value = http
        .post(rpc)
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_call",
            "params": [{"to": ctf, "data": data}, "latest"],
        }))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    rpc_word(&body)
}

async fn final_payout(
    http: &reqwest::Client,
    rpc: &str,
    ctf: &str,
    outcome: &Outcome,
) -> Option<f64> {
    let condition = hex::encode(outcome.condition);
    let denominator = call(http, rpc, ctf, format!("0xdd34de67{condition}")).await?;
    if denominator == 0 {
        return None;
    }
    let numerator = call(
        http,
        rpc,
        ctf,
        format!("0x0504c814{condition}{:064x}", outcome.index),
    )
    .await?;
    if numerator > denominator {
        return None;
    }
    Some(numerator as f64 / denominator as f64)
}

#[derive(Default)]
pub struct Resolver {
    // Only immutable metadata is cached. Unresolved or failed reads are retried.
    outcomes: HashMap<String, Outcome>,
}

#[derive(Clone, Debug)]
pub struct Claim {
    pub token: String,
    pub held: f64,
    pub released: f64,
    pub settled: f64,
    pub first_buy: i64,
}

#[derive(Clone, Debug)]
pub struct Proof {
    pub payout: f64,
    pub balance_units: u128,
    pub redeemed_units: u128,
}

pub fn claims(ledger: &crate::ledger::Ledger) -> Vec<Claim> {
    let tokens: std::collections::HashSet<_> = ledger
        .lanes
        .keys()
        .flat_map(|lane| ledger.settlement_tokens(lane))
        .collect();
    tokens
        .into_iter()
        .filter_map(|token| {
            Some(Claim {
                held: ledger.pool_claim(&token),
                released: ledger
                    .lanes
                    .keys()
                    .filter_map(|lane| ledger.pending_release(lane, &token))
                    .map(|p| p.0)
                    .sum(),
                settled: ledger.pool_settled_shares(&token),
                first_buy: ledger.first_tracked_buy(&token).unwrap_or(0),
                token,
            })
        })
        .collect()
}

/// The caller holds the control mutex throughout this operation. A proof is
/// checked against the current whole-wallet lane pool, not an earlier snapshot.
pub fn book_verified_pool(
    ledger: &mut crate::ledger::Ledger,
    token: &str,
    proof: &Proof,
) -> Result<Vec<(String, f64)>, &'static str> {
    if !ledger.persistence_ok() {
        return Err("ledger persistence unavailable");
    }
    let held = ledger.pool_claim(token);
    let released: f64 = ledger
        .lanes
        .keys()
        .filter_map(|lane| ledger.pending_release(lane, token))
        .map(|p| p.0)
        .sum();
    if !proof.covers(held, released, ledger.pool_settled_shares(token)) {
        return Err("custody/redemption evidence does not cover the current lane pool");
    }
    let mut booked = Vec::new();
    let mut lanes: Vec<_> = ledger.lanes.keys().cloned().collect();
    lanes.sort();
    for lane in lanes {
        let mut delta = 0.0;
        let mut changed = false;
        if let Some((shares, cost)) = ledger.pending_release(&lane, token) {
            let already =
                share_units(ledger.pool_settled_shares(token)).ok_or("invalid settled units")?;
            ledger.record_realised_adjustment_tx(
                &lane,
                token,
                shares,
                shares * proof.payout,
                cost / shares,
                "verified redemption of previously reconciled shares",
                &format!("resolution:{lane}:{token}:{already}"),
            );
            if !ledger.persistence_ok() {
                return Err("settlement credit could not be persisted");
            }
            delta += shares * proof.payout - cost;
            changed = true;
        }
        if let Some(realised) = book(ledger, &lane, token, proof.payout) {
            delta += realised;
            changed = true;
        }
        if !ledger.persistence_ok() {
            return Err("settlement could not be persisted");
        }
        if changed {
            booked.push((lane, delta));
        }
    }
    Ok(booked)
}

fn share_units(shares: f64) -> Option<u128> {
    let units = shares * 1_000_000.0;
    if !units.is_finite()
        || units < 0.0
        || units > u64::MAX as f64
        || (units - units.round()).abs() > 0.01
    {
        return None;
    }
    Some(units.round() as u128)
}

impl Proof {
    pub fn covers(&self, held: f64, released: f64, settled: f64) -> bool {
        let (Some(h), Some(r), Some(s)) = (
            share_units(held),
            share_units(released),
            share_units(settled),
        ) else {
            return false;
        };
        // Require conservation across the entire lane pool, including earlier
        // durable credits. Surplus or missing evidence is unattributed inventory.
        self.balance_units.checked_add(self.redeemed_units)
            == h.checked_add(r).and_then(|n| n.checked_add(s))
            && self.redeemed_units >= r
            && self.payout.is_finite()
            && (0.0..=1.0).contains(&self.payout)
    }
}

async fn rpc_json(http: &reqwest::Client, rpc: &str, method: &str, params: Value) -> Option<Value> {
    let body: Value = http
        .post(rpc)
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    if body.get("error").is_some() {
        return None;
    }
    body.get("result").filter(|v| !v.is_null()).cloned()
}

fn quantity(v: &Value) -> Option<u64> {
    u64::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok()
}

async fn word_at(
    http: &reqwest::Client,
    rpc: &str,
    ctf: &str,
    block: &str,
    data: String,
) -> Option<u128> {
    let value = rpc_json(http, rpc, "eth_call", json!([{"to":ctf,"data":data},block])).await?;
    rpc_word(&json!({"result":value}))
}

fn token_word(token: &str) -> Option<String> {
    let mut word = [0u8; 32];
    if token.is_empty() {
        return None;
    }
    for digit in token.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        let mut carry = (digit - b'0') as u16;
        for byte in word.iter_mut().rev() {
            let value = *byte as u16 * 10 + carry;
            *byte = value as u8;
            carry = value >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    Some(hex::encode(word))
}

fn address(raw: &str) -> Option<[u8; 20]> {
    hex::decode(raw.strip_prefix("0x")?).ok()?.try_into().ok()
}

impl Resolver {
    /// No keys or transaction submission. All balances and payout vectors are
    /// read at one finalized block, preventing pre-burn balance double counting.
    pub async fn verified_payouts(
        &mut self,
        http: &reqwest::Client,
        gamma: &str,
        data_api: &str,
        rpc: &str,
        ctf: &str,
        funder: &str,
        claims: &[Claim],
    ) -> HashMap<String, Proof> {
        let mut verified = HashMap::new();
        let (Some(funder_bytes), Some(ctf_bytes)) = (address(funder), address(ctf)) else {
            return verified;
        };
        let Some(head) = rpc_json(
            http,
            rpc,
            "eth_getBlockByNumber",
            json!(["finalized", false]),
        )
        .await
        else {
            return verified;
        };
        let (Some(number), Some(block), Some(until)) = (
            quantity(&head["number"]), head["number"].as_str(),
            quantity(&head["timestamp"]).and_then(|t| i64::try_from(t).ok()),
        )
        else {
            return verified;
        };
        if number == 0 || until <= 0 || !head["hash"].as_str().is_some_and(|hash| {
            valid_hash(hash) && hash[2..].bytes().any(|b| b != b'0')
        }) {
            return verified;
        }
        let tokens: Vec<_> = claims.iter().map(|c| c.token.clone()).collect();
        // Metadata is cached by the existing resolver; price quotes are never
        // used as payout evidence. The vector is checked again at finalized head.
        self.payouts(http, gamma, rpc, ctf, &tokens).await;
        let mut transactions: Option<Option<Vec<RedemptionTransaction>>> = None;
        let mut receipts: HashMap<String, Option<Value>> = HashMap::new();
        let mut blocks: HashMap<u64, Option<Value>> = HashMap::new();
        blocks.insert(number, Some(head.clone()));
        for claim in claims {
            if claim.first_buy <= 0 || claim.first_buy > until { continue; }
            let Some(outcome) = self.outcomes.get(&claim.token) else {
                continue;
            };
            let condition = hex::encode(outcome.condition);
            let Some(denominator) =
                word_at(http, rpc, ctf, block, format!("0xdd34de67{condition}")).await
            else {
                continue;
            };
            if denominator == 0 {
                continue;
            }
            let mut numerators = [0; 2];
            let mut vector_ok = true;
            for (index, numerator) in numerators.iter_mut().enumerate() {
                match word_at(
                    http,
                    rpc,
                    ctf,
                    block,
                    format!("0x0504c814{condition}{index:064x}"),
                )
                .await
                {
                    Some(n) if n <= denominator => *numerator = n,
                    _ => {
                        vector_ok = false;
                        break;
                    }
                }
            }
            if !vector_ok || numerators[0].checked_add(numerators[1]) != Some(denominator) {
                continue;
            }
            let mut tokens_bound = true;
            for (index, token) in outcome.tokens.iter().enumerate() {
                if !crate::token_binding::verifies(http, rpc, ctf, block, outcome.condition, index, token).await {
                    tokens_bound = false;
                    break;
                }
            }
            if !tokens_bound { continue; }
            let Some(token) = token_word(&claim.token) else {
                continue;
            };
            let Some(balance) = word_at(
                http,
                rpc,
                ctf,
                block,
                format!(
                    "0x00fdd58e{}{}",
                    hex::encode(crate::merge::word_addr(&funder_bytes)),
                    token
                ),
            )
            .await
            else {
                continue;
            };
            let mut proof = Proof {
                payout: numerators[outcome.index] as f64 / denominator as f64,
                balance_units: balance,
                redeemed_units: 0,
            };
            if proof.covers(claim.held, claim.released, claim.settled) {
                verified.insert(claim.token.clone(), proof);
                continue;
            }
            if transactions.is_none() {
                let since = claims.iter().map(|c| c.first_buy).filter(|t| *t > 0).min().unwrap_or(0);
                transactions = Some(redemption_transactions(http, data_api, funder, since, until).await);
            }
            let Some(Some(transactions)) = transactions.as_ref() else { continue; };
            let context_base = crate::redemption::RedemptionContext {
                expected_tx: String::new(),
                funder: funder_bytes,
                ctf: ctf_bytes,
                condition: outcome.condition,
                tokens: outcome.tokens.clone(),
                numerators,
                denominator,
            };
            let mut seen = std::collections::HashSet::new();
            let mut complete = true;
            for transaction in transactions.iter().filter(|t| t.condition == outcome.condition) {
                let tx = &transaction.tx;
                if !receipts.contains_key(tx) {
                    let receipt = rpc_json(http, rpc, "eth_getTransactionReceipt", json!([tx])).await;
                    receipts.insert(tx.clone(), receipt);
                }
                let Some(Some(receipt)) = receipts.get(tx) else {
                    complete = false;
                    break;
                };
                let mut context = context_base.clone();
                context.expected_tx = tx.clone();
                let Ok(evidence) = crate::redemption::verify_receipt(receipt, &context) else {
                    complete = false;
                    break;
                };
                if evidence.block_number > number {
                    continue;
                }
                if !blocks.contains_key(&evidence.block_number) {
                    let header = rpc_json(
                        http,
                        rpc,
                        "eth_getBlockByNumber",
                        json!([format!("0x{:x}", evidence.block_number), false]),
                    )
                    .await;
                    blocks.insert(evidence.block_number, header);
                }
                let Some(Some(header)) = blocks.get(&evidence.block_number) else {
                    complete = false;
                    break;
                };
                let timestamp = quantity(&header["timestamp"]).and_then(|t| i64::try_from(t).ok());
                if header["hash"].as_str() != Some(evidence.block_hash.as_str())
                    || quantity(&header["number"]) != Some(evidence.block_number)
                    || !timestamp.is_some_and(|t| t > 0 && t <= until)
                {
                    complete = false;
                    break;
                }
                if timestamp.is_some_and(|t| t < claim.first_buy) {
                    continue;
                }
                for redeemed in &evidence.tokens {
                    if redeemed.token == claim.token
                        && seen.insert((evidence.tx.clone(), redeemed.token.clone()))
                    {
                        let Some(total) = proof.redeemed_units.checked_add(redeemed.units) else {
                            complete = false;
                            break;
                        };
                        proof.redeemed_units = total;
                    }
                }
                if !complete { break; }
            }
            if complete && proof.covers(claim.held, claim.released, claim.settled) {
                verified.insert(claim.token.clone(), proof);
            }
        }
        verified
    }

    pub async fn payouts(
        &mut self,
        http: &reqwest::Client,
        gamma: &str,
        rpc: &str,
        ctf: &str,
        tokens: &[String],
    ) -> HashMap<String, f64> {
        self.outcomes.retain(|token, _| tokens.contains(token));
        let address = ctf.strip_prefix("0x").and_then(|s| hex::decode(s).ok());
        if !matches!(address, Some(ref bytes) if bytes.len() == 20) {
            return HashMap::new();
        }
        let jobs: Vec<_> = tokens
            .iter()
            .map(|token| (token.clone(), self.outcomes.get(token).cloned()))
            .collect();
        let results: Vec<_> = stream::iter(jobs)
            .map(|(token, cached)| {
                let http = http.clone();
                let (gamma, rpc, ctf) = (gamma.to_string(), rpc.to_string(), ctf.to_string());
                async move {
                    let outcome = if let Some(outcome) = cached {
                        Some(outcome)
                    } else {
                        let body = async {
                            http.get(format!("{}/markets", gamma.trim_end_matches('/')))
                                .query(&[("clob_token_ids", token.as_str()), ("closed", "true")])
                                .send()
                                .await
                                .ok()?
                                .error_for_status()
                                .ok()?
                                .json::<Value>()
                                .await
                                .ok()
                        }
                        .await;
                        body.as_ref().and_then(|body| market_outcome(body, &token))
                    };
                    let payout = match &outcome {
                        Some(outcome) => final_payout(&http, &rpc, &ctf, outcome).await,
                        None => None,
                    };
                    (token, outcome, payout)
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;
        let mut payouts = HashMap::new();
        for (token, outcome, payout) in results {
            if let Some(outcome) = outcome {
                self.outcomes.insert(token.clone(), outcome);
            }
            if let Some(payout) = payout {
                payouts.insert(token, payout);
            }
        }
        payouts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RedemptionTransaction {
    tx: String,
    condition: [u8; 32],
}

fn valid_hash(value: &str) -> bool {
    value.len() == 66 && value.starts_with("0x") && hex::decode(&value[2..]).is_ok()
}

fn redemption_page(body: &Value, since: i64, until: i64) -> Option<(Vec<RedemptionTransaction>, bool)> {
    let rows = body.as_array()?;
    if rows.len() > 200 { return None; }
    let mut transactions = Vec::new();
    for row in rows {
        if row["type"].as_str()? != "REDEEM" { return None; }
        let timestamp = row["timestamp"].as_i64()?;
        if timestamp < since || timestamp > until { return None; }
        let tx = row["transactionHash"].as_str()?;
        if !valid_hash(tx) { return None; }
        let condition = crate::merge::condition_id_bytes(row["conditionId"].as_str()?)?;
        transactions.push(RedemptionTransaction { tx: tx.to_ascii_lowercase(), condition });
    }
    Some((transactions, rows.len() < 200))
}

async fn redemption_transactions(
    http: &reqwest::Client,
    data_api: &str,
    funder: &str,
    since: i64,
    until: i64,
) -> Option<Vec<RedemptionTransaction>> {
    if since <= 0 || until < since { return None; }
    let mut transactions = std::collections::HashMap::new();
    // Discovery is bounded, but a truncated or failed scan is not a complete
    // burn total. Never let a convenient subset hide surplus redemption evidence.
    for page in 0..25 {
        let body: Value = http.get(format!("{}/activity", data_api.trim_end_matches('/')))
            .query(&[
                ("user", funder.to_string()),
                ("type", "REDEEM".into()),
                ("limit", "200".into()),
                ("offset", (page * 200).to_string()),
                ("start", since.to_string()),
                ("end", until.to_string()),
                ("sortBy", "TIMESTAMP".into()),
                ("sortDirection", "DESC".into()),
            ])
            .send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
        let (rows, complete) = redemption_page(&body, since, until)?;
        for row in rows {
            transactions.insert((row.tx.clone(), row.condition), row);
        }
        if complete {
            return Some(transactions.into_values().collect());
        }
    }
    None
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use std::io::{BufRead, Write};

    fn row(tx: u8, condition: u8) -> Value {
        json!({"type":"REDEEM", "timestamp":150,
            "transactionHash":format!("0x{}", hex::encode([tx; 32])),
            "conditionId":format!("0x{}", hex::encode([condition; 32]))})
    }
    fn server(pages: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for (status, page) in pages {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "missing discovery request");
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                paths.push(first);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() { break; }
                }
                let body = page.to_string();
                write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            paths
        });
        (url, handle)
    }
    async fn discover(pages: Vec<(u16, Value)>) -> (Option<Vec<RedemptionTransaction>>, Vec<String>) {
        let (url, server) = server(pages);
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(2)).build().unwrap();
        let result = redemption_transactions(&http, &url, "0x1111111111111111111111111111111111111111", 100, 200).await;
        (result, server.join().unwrap())
    }

    #[test]
    fn malformed_or_out_of_range_activity_is_not_silently_discarded() {
        let good = row(1, 2);
        assert_eq!(redemption_page(&json!([good.clone()]), 100, 200).unwrap().0[0].condition, [2; 32]);
        for (key, value) in [
            ("type", json!("TRADE")), ("timestamp", json!(99)), ("timestamp", json!(201)),
            ("conditionId", json!("0x12")), ("transactionHash", json!("0x12")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(redemption_page(&json!([good.clone(), bad]), 100, 200).is_none());
        }
        assert!(redemption_page(&json!({"error":"unavailable"}), 100, 200).is_none());
        assert!(redemption_page(&json!(vec![good; 201]), 100, 200).is_none());
    }

    #[tokio::test]
    async fn complete_scan_preserves_condition_and_deduplicates_per_condition() {
        let (transactions, requests) = discover(vec![
            (200, json!(vec![row(1, 2); 200])),
            (200, json!([row(1, 2), row(1, 3)])),
        ]).await;
        let transactions = transactions.unwrap();
        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions.iter().filter(|t| t.condition == [2; 32]).count(), 1);
        assert_eq!(transactions.iter().filter(|t| t.condition == [3; 32]).count(), 1);
        assert!(requests[1].contains("offset=200"));
        for request in requests {
            assert!(request.contains("start=100"));
            assert!(request.contains("end=200"));
            assert!(request.contains("sortDirection=DESC"));
        }
    }

    #[tokio::test]
    async fn later_page_failure_never_returns_a_usable_subset() {
        for second in [(503, json!([])), (200, json!({"error":"unavailable"}))] {
            let (result, _) = discover(vec![(200, json!(vec![row(1, 2); 200])), second]).await;
            assert!(result.is_none());
        }
    }

    #[tokio::test]
    async fn page_limit_without_a_terminal_page_is_incomplete() {
        let (result, requests) = discover(vec![(200, json!(vec![row(1, 2); 200])); 25]).await;
        assert!(result.is_none());
        assert_eq!(requests.len(), 25);
        assert!(requests[24].contains("offset=4800"));
    }
}

/// Called under the control lock after network reads. Rechecks current holdings
/// so repeated polls, concurrent reconciliation and replay cannot double-credit.
pub fn book(
    ledger: &mut crate::ledger::Ledger,
    lane: &str,
    token: &str,
    payout: f64,
) -> Option<f64> {
    if !payout.is_finite() || !(0.0..=1.0).contains(&payout) {
        return None;
    }
    let position = ledger.lanes.get(lane)?.positions.get(token)?;
    if position.shares <= 1e-9 {
        return None;
    }
    let delta = position.shares * payout - position.cost;
    ledger.record_settlement(lane, token, payout);
    // Existing ledger persistence faults halt buying through Control::tick_with.
    if !ledger.persistence_ok() {
        return None;
    }
    Some(delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    struct Server {
        url: String,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
        rpc: Arc<Mutex<(Value, Value)>>,
        paths: Arc<Mutex<Vec<String>>>,
    }
    fn word(n: u128) -> Value {
        json!({"result": format!("0x{n:064x}")})
    }
    fn market() -> Value {
        json!([{"conditionId": format!("0x{}", "1".repeat(64)),
            "clobTokenIds": "[\"101\",\"102\"]"}])
    }
    impl Server {
        fn new(metadata: Value, den: Value, num: Value) -> Self {
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", socket.local_addr().unwrap());
            socket.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let rpc = Arc::new(Mutex::new((den, num)));
            let paths = Arc::new(Mutex::new(Vec::new()));
            let (done, responses, seen) = (stop.clone(), rpc.clone(), paths.clone());
            let worker = std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let (mut client, _) = match socket.accept() {
                        Ok(c) => c,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("{e}"),
                    };
                    client
                        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                        .unwrap();
                    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    seen.lock().unwrap().push(first.clone());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap();
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).unwrap();
                    let response = if first.starts_with("GET /markets?") {
                        assert!(first.contains("clob_token_ids="));
                        assert!(first.contains("closed=true"));
                        metadata.clone()
                    } else {
                        assert!(first.starts_with("POST / "));
                        let body: Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(body["method"], "eth_call");
                        assert_eq!(body["params"][1], "latest");
                        let pair = responses.lock().unwrap();
                        if body["params"][0]["data"]
                            .as_str()
                            .unwrap()
                            .starts_with("0xdd34de67")
                        {
                            pair.0.clone()
                        } else {
                            pair.1.clone()
                        }
                    }
                    .to_string();
                    write!(client, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                }
            });
            Self {
                url,
                stop,
                worker: Some(worker),
                rpc,
                paths,
            }
        }
        async fn payouts(&self, resolver: &mut Resolver) -> HashMap<String, f64> {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap();
            resolver
                .payouts(
                    &client,
                    &self.url,
                    &self.url,
                    &format!("0x{}", hex::encode(crate::merge::CTF)),
                    &["101".into()],
                )
                .await
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[tokio::test]
    async fn already_redeemed_positions_settle_without_a_wallet_snapshot() {
        for (name, numerator, expected) in
            [("winner", 1, 1.0), ("loser", 0, 0.0), ("split", 1, 0.5)]
        {
            let den = if name == "split" { 2 } else { 1 };
            let server = Server::new(market(), word(den), word(numerator));
            let payouts = server.payouts(&mut Resolver::default()).await;
            assert_eq!(payouts.get("101"), Some(&expected));
            let path = std::env::temp_dir()
                .join(format!("resolution-{name}-{}.jsonl", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let cfgs = vec![
                ("a".into(), crate::risk::RiskConfig::default()),
                ("b".into(), crate::risk::RiskConfig::default()),
            ];
            let mut ledger = crate::ledger::Ledger::new(path.to_str().unwrap(), &cfgs);
            // Two lanes own the same token. No current positions or REDEEM API rows.
            for lane in ["a", "b"] {
                ledger.record_fill(lane, "101", 0, 10.0, 0.6, 0.0);
                assert_eq!(ledger.open_usd(lane), 6.0);
                assert_eq!(
                    book(&mut ledger, lane, "101", payouts["101"]),
                    Some(10.0 * expected - 6.0)
                );
                assert_eq!(ledger.open_usd(lane), 0.0);
                assert_eq!(book(&mut ledger, lane, "101", payouts["101"]), None);
                assert_eq!(ledger.lanes[lane].spent_today, 6.0);
            }
            let mut replay = crate::ledger::Ledger::new(path.to_str().unwrap(), &cfgs);
            for lane in ["a", "b"] {
                assert_eq!(replay.open_usd(lane), 0.0);
                assert_eq!(replay.lanes[lane].risk.realised_pnl, 10.0 * expected - 6.0);
                assert_eq!(book(&mut replay, lane, "101", expected), None);
            }
            std::fs::remove_file(path).unwrap();
            assert!(server
                .paths
                .lock()
                .unwrap()
                .iter()
                .all(|p| !p.contains("positions") && !p.contains("activity")));
        }
    }

    #[tokio::test]
    async fn unresolved_and_failed_rpc_reads_retry_without_caching_a_payout() {
        let server = Server::new(market(), word(0), word(1));
        let mut resolver = Resolver::default();
        assert!(server.payouts(&mut resolver).await.is_empty());
        *server.rpc.lock().unwrap() = (json!({"error": {"message": "unavailable"}}), word(1));
        assert!(server.payouts(&mut resolver).await.is_empty());
        *server.rpc.lock().unwrap() = (word(1), json!({"result": "0x"}));
        assert!(server.payouts(&mut resolver).await.is_empty());
        *server.rpc.lock().unwrap() = (word(1), word(2));
        assert!(server.payouts(&mut resolver).await.is_empty());
        *server.rpc.lock().unwrap() = (word(2), word(1));
        assert_eq!(server.payouts(&mut resolver).await["101"], 0.5);
        assert_eq!(
            server
                .paths
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with("GET"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn empty_or_unrelated_metadata_does_not_release_holdings() {
        for body in [
            json!([]),
            json!({"error": "unavailable"}),
            json!([{"conditionId": format!("0x{}", "1".repeat(64)), "clobTokenIds": "[\"201\",\"202\"]"}]),
        ] {
            let server = Server::new(body, word(1), word(1));
            assert!(server.payouts(&mut Resolver::default()).await.is_empty());
            assert_eq!(server.paths.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn metadata_and_rpc_values_must_be_unambiguous() {
        assert_eq!(market_outcome(&market(), "102").unwrap().index, 1);
        let mut body = market();
        body[0]["clobTokenIds"] = json!(["101", "102"]);
        assert!(market_outcome(&body, "101").is_some());
        body[0]["clobTokenIds"] = json!(["101", "101"]);
        assert!(market_outcome(&body, "101").is_none());
        body = json!([market()[0].clone(), market()[0].clone()]);
        assert!(market_outcome(&body, "101").is_none());
        for value in [
            json!({"result": "0x1"}),
            json!({"result": "0x"}),
            json!({"result": format!("0x{}", "f".repeat(64))}),
            json!({"result": null}),
        ] {
            assert_eq!(rpc_word(&value), None);
        }
    }
}
