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
impl Resolver {
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
