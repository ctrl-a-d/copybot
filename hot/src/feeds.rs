use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
#[derive(Debug)]
pub struct RawTx {
    pub source: String,
    pub hash: String,
    pub input: Vec<u8>,
    pub to_neg_risk: bool,
    pub to: String,
    pub seen_ns: u128,
    pub seen_mono_ns: u128,
    pub raw_json: String,
}
#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub name: String,
    pub url: String,
    pub sockets: usize,
}
pub const CTF_EXCHANGE_V2: &str = "e111180000d2663c0091e4f400237545b87b996b";
pub const NEG_RISK_CTF_EXCHANGE_V2: &str = "e2222d279d744050d28e00520010520000310f59";
#[derive(Debug, Default)]
pub struct WatchedAddresses {
    inner: std::sync::RwLock<Vec<String>>,
}
impl WatchedAddresses {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, addrs: impl IntoIterator<Item = String>) {
        let v: Vec<String> = addrs
            .into_iter()
            .map(|a| a.trim().trim_start_matches("0x").to_ascii_lowercase())
            .filter(|a| a.len() == 40 && a.chars().all(|c| c.is_ascii_hexdigit()))
            .collect();
        if let Ok(mut g) = self.inner.write() {
            *g = v;
        }
    }
    pub fn snapshot(&self) -> Vec<String> {
        self.inner.read().map(|g| g.clone()).unwrap_or_default()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.read().map(|g| g.is_empty()).unwrap_or(true)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKind {
    Exchange,
    NegRiskExchange,
    Watched,
}
pub fn classify_to(to_lower: &str, watch: &[String]) -> Option<TxKind> {
    if to_lower == CTF_EXCHANGE_V2 {
        return Some(TxKind::Exchange);
    }
    if to_lower == NEG_RISK_CTF_EXCHANGE_V2 {
        return Some(TxKind::NegRiskExchange);
    }
    if !to_lower.is_empty() && watch.iter().any(|w| w == to_lower) {
        return Some(TxKind::Watched);
    }
    None
}
pub const WATCH_REFRESH_FRAMES: u32 = 128;
const PROVE_TIMEOUT: Duration = Duration::from_secs(15);
#[derive(Default, Debug)]
pub struct FeedStats {
    pub frames: AtomicU64,
    pub exchange_txs: AtomicU64,
    pub reconnects: AtomicU64,
    pub errors: AtomicU64,
    pub wins: AtomicU64,
    pub recycle: std::sync::atomic::AtomicBool,
    pub connected: std::sync::atomic::AtomicBool,
    pub last_frame_mono_ns: AtomicU64,
    pub received_bytes: AtomicU64,
    // Zero means unlimited; the passive probe sets a bound for its paid-provider test.
    pub byte_limit: AtomicU64,
}
pub struct FeedRegistry {
    pub per: std::collections::HashMap<String, Arc<FeedStats>>,
    pub total: Arc<FeedStats>,
}
impl FeedRegistry {
    pub fn health_snapshot(&self) -> serde_json::Value {
        let mut sources = serde_json::Map::new();
        for (name, s) in &self.per {
            sources.insert(name.clone(), serde_json::json!({
                "connected": s.connected.load(Ordering::Acquire),
                "last_frame_mono_ns": s.last_frame_mono_ns.load(Ordering::Acquire),
                "received_bytes": s.received_bytes.load(Ordering::Relaxed),
                "byte_limit": s.byte_limit.load(Ordering::Relaxed)
            }));
        }
        sources.into()
    }
    pub fn snapshot(&self) -> Vec<(String, u64, u64, u64, u64)> {
        let mut v: Vec<_> = self
            .per
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    s.frames.load(Ordering::Relaxed),
                    s.exchange_txs.load(Ordering::Relaxed),
                    s.reconnects.load(Ordering::Relaxed),
                    s.errors.load(Ordering::Relaxed),
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
    pub fn record_win(&self, socket: &str) {
        if let Some(s) = self.per.get(socket) {
            s.wins.fetch_add(1, Ordering::Relaxed);
            self.total.wins.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn wins_snapshot(&self) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self
            .per
            .iter()
            .map(|(k, s)| (k.clone(), s.wins.load(Ordering::Relaxed)))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
    pub fn score_and_recycle(&self, keep_top: usize, min_sample: u64) -> Vec<String> {
        let ranked = self.wins_snapshot();
        let losers = pick_losers(&ranked, keep_top, min_sample);
        for name in &losers {
            if let Some(s) = self.per.get(name) {
                s.recycle.store(true, Ordering::Relaxed);
            }
        }
        for (_, s) in self.per.iter() {
            s.wins.store(0, Ordering::Relaxed);
        }
        losers
    }
}
pub fn pick_losers(
    ranked: &[(String, u64)],
    keep_top: usize,
    min_sample: u64,
) -> Vec<String> {
    let total: u64 = ranked.iter().map(|(_, w)| *w).sum();
    if total < min_sample || ranked.len() <= keep_top {
        return Vec::new();
    }
    let even = total as f64 / ranked.len() as f64;
    ranked
        .iter()
        .enumerate()
        .filter(|(i, (_, w))| *i >= keep_top && (*w as f64) < even * 0.5)
        .map(|(_, (n, _))| n.clone())
        .collect()
}
#[derive(Clone)]
pub struct StatsSink {
    own: Arc<FeedStats>,
    total: Arc<FeedStats>,
}
impl StatsSink {
    fn bump2(a: &AtomicU64, b: &AtomicU64) {
        a.fetch_add(1, Ordering::Relaxed);
        b.fetch_add(1, Ordering::Relaxed);
    }
    pub fn frame(&self) {
        Self::bump2(&self.own.frames, &self.total.frames)
    }
    pub fn exchange_tx(&self) {
        Self::bump2(&self.own.exchange_txs, &self.total.exchange_txs)
    }
    pub fn reconnect(&self) {
        Self::bump2(&self.own.reconnects, &self.total.reconnects)
    }
    pub fn error(&self) {
        Self::bump2(&self.own.errors, &self.total.errors)
    }
    pub fn win(&self) {
        Self::bump2(&self.own.wins, &self.total.wins)
    }
    fn take_recycle(&self) -> bool {
        self.own.recycle.swap(false, Ordering::Relaxed)
    }
}
pub fn spawn_all(
    feeds: &[FeedConfig],
    tx: mpsc::UnboundedSender<RawTx>,
    total: Arc<FeedStats>,
    watched: Arc<WatchedAddresses>,
) -> (Vec<tokio::task::JoinHandle<()>>, FeedRegistry) {
    let mut handles = Vec::new();
    let mut per = std::collections::HashMap::new();
    for f in feeds {
        for i in 0..f.sockets {
            let name = format!("{}{}", f.name, i);
            let own = Arc::new(FeedStats::default());
            per.insert(name.clone(), own.clone());
            let sink = StatsSink {
                own,
                total: total.clone(),
            };
            let url = f.url.clone();
            let tx = tx.clone();
            let watched = watched.clone();
            handles
                .push(
                    tokio::spawn(async move {
                        run_socket(name, url, tx, sink, watched).await;
                    }),
                );
        }
    }
    (handles, FeedRegistry { per, total })
}
pub fn backoff_ms(consecutive_failures: u32) -> u64 {
    match consecutive_failures {
        0 => 1_500,
        1..=3 => 2_000,
        4..=8 => 10_000,
        _ => 60_000,
    }
}
pub fn escalates(ran: Duration, ended_ok: bool, healthy: Duration) -> bool {
    !ended_ok && ran < healthy
}
async fn run_socket(
    name: String,
    url: String,
    tx: mpsc::UnboundedSender<RawTx>,
    stats: StatsSink,
    watched: Arc<WatchedAddresses>,
) {
    const HEALTHY_SESSION: Duration = Duration::from_secs(60);
    let mut consecutive_failures: u32 = 0;
    loop {
        let limit = stats.own.byte_limit.load(Ordering::Relaxed);
        if limit > 0 && stats.own.received_bytes.load(Ordering::Relaxed) >= limit {
            eprintln!("[{name}] configured receive-byte budget exhausted");
            return;
        }
        let started = std::time::Instant::now();
        let outcome = pump(&name, &url, &tx, &stats, watched.clone()).await;
        stats.own.connected.store(false, Ordering::Release);
        let ran = started.elapsed();
        let ended_ok = outcome.is_ok();
        if escalates(ran, ended_ok, HEALTHY_SESSION) {
            consecutive_failures = consecutive_failures.saturating_add(1);
            stats.error();
            if let Err(e) = &outcome {
                if consecutive_failures <= 3 {
                    eprintln!("[{name}] {}", redact_provider_error(&url, e));
                } else if consecutive_failures == 4 {
                    eprintln!("[{name}] repeated failures — backing off, silencing");
                }
            }
        } else {
            consecutive_failures = 0;
            if ended_ok {
                eprintln!("[{name}] stream ended cleanly");
            }
        }
        stats.reconnect();
        tokio::time::sleep(Duration::from_millis(backoff_ms(consecutive_failures)))
            .await;
    }
}
fn redact_provider_error(url: &str, error: &str) -> String {
    let mut safe = error.replace(url, "[provider endpoint]");
    if url.contains("alchemy.com") {
        if let Some(key) = url.rsplit('/').next().filter(|s| !s.is_empty()) {
            safe = safe.replace(key, "[credential]");
        }
    }
    safe
}
#[cfg(test)]
mod provider_credential_tests {
    use super::*;
    #[test]
    fn provider_errors_never_echo_alchemy_path_credentials() {
        let url="wss://polygon-mainnet.g.alchemy.com/v2/private-example-key";
        let error=format!("connect {url}: rejected private-example-key");
        let safe=redact_provider_error(url,&error);
        assert!(!safe.contains("private-example-key"));
        assert!(!safe.contains(url));
    }
}
// Inspect the subscription envelope before the destination prefilter without
// allocating a full JSON tree for every unrelated pending transaction.
fn is_transaction_notification(raw: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Notification<'a> {
        method: &'a str,
        #[serde(borrow)]
        params: Params<'a>,
    }
    #[derive(serde::Deserialize)]
    struct Params<'a> {
        #[serde(borrow)]
        result: Transaction<'a>,
    }
    #[derive(serde::Deserialize)]
    struct Transaction<'a> {
        hash: &'a str,
        input: &'a str,
    }
    let Ok(n) = serde_json::from_str::<Notification<'_>>(raw) else { return false; };
    n.method == "eth_subscription"
        && n.params.result.hash.len() == 66
        && n.params.result.hash.starts_with("0x")
        && n.params.result.hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
        && n.params.result.input.starts_with("0x")
        && n.params.result.input.len() % 2 == 0
        && n.params.result.input[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
async fn pump(
    name: &str,
    url: &str,
    out: &mpsc::UnboundedSender<RawTx>,
    stats: &StatsSink,
    watched: Arc<WatchedAddresses>,
) -> Result<(), String> {
    pump_with_timeouts(name, url, out, stats, watched, PROVE_TIMEOUT, Duration::from_secs(90)).await
}
async fn pump_with_timeouts(
    name: &str,
    url: &str,
    out: &mpsc::UnboundedSender<RawTx>,
    stats: &StatsSink,
    watched: Arc<WatchedAddresses>,
    prove_timeout: Duration,
    idle_timeout: Duration,
) -> Result<(), String> {
    let mut watch_list: Vec<String> = watched.snapshot();
    let mut frames_since_watch_refresh: u32 = 0;
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let sub = if url.contains("alchemy") {
        let mut to: Vec<String> = vec![
            format!("0x{CTF_EXCHANGE_V2}"), format!("0x{NEG_RISK_CTF_EXCHANGE_V2}")
        ];
        to.extend(watched.snapshot().into_iter().map(|a| format!("0x{a}")));
        serde_json::json!(
            { "jsonrpc" : "2.0", "id" : 1, "method" : "eth_subscribe", "params" :
            ["alchemy_pendingTransactions", { "toAddress" : to, "hashesOnly": false }] }
        )
    } else {
        serde_json::json!(
            { "jsonrpc" : "2.0", "id" : 1, "method" : "eth_subscribe", "params" :
            ["newPendingTransactions", true] }
        )
    };
    ws.send(Message::Text(sub.to_string()))
        .await
        .map_err(|e| format!("subscribe send: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => {
            if t.contains("\"error\"") {
                return Err(
                    format!("subscription rejected: {}", & t[..t.len().min(120)]),
                );
            }
        }
        Ok(Some(Ok(_))) => {}
        Ok(Some(Err(e))) => return Err(format!("ack: {e}")),
        Ok(None) => return Err("closed before ack".into()),
        Err(_) => return Err("ack timeout".into()),
    }
    stats.own.connected.store(true, Ordering::Release);
    let mut proved = false;
    let mut deadline = tokio::time::Instant::now() + prove_timeout;
    loop {
        // A transport heartbeat does not prove the pending subscription works.
        // Check explicitly too: an always-ready stream must not starve expiry.
        let next = if tokio::time::Instant::now() >= deadline {
            Err(())
        } else {
            tokio::time::timeout_at(deadline, ws.next()).await.map_err(|_| ())
        };
        if stats.take_recycle() {
            return Ok(());
        }
        let msg = match next {
            Err(_) if !proved => {
                return Err("subscribed but delivered NOTHING — black hole".into());
            }
            Err(_) => return Err("silent past 90s".into()),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => return Err(format!("recv: {e}")),
            Ok(Some(Ok(m))) => m,
        };
        let raw = match msg {
            Message::Text(t) => t,
            Message::Ping(p) => {
                if !matches!(tokio::time::timeout_at(deadline, ws.send(Message::Pong(p))).await, Ok(Ok(()))) {
                    return Err("pong send failed or timed out".into());
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let seen_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
        let seen_mono_ns = ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128;
        let bytes = stats.own.received_bytes.fetch_add(raw.len() as u64, Ordering::Relaxed) + raw.len() as u64;
        let limit = stats.own.byte_limit.load(Ordering::Relaxed);
        if limit > 0 && bytes >= limit {
            return Err("configured receive-byte budget exhausted".into());
        }
        if !is_transaction_notification(&raw) {
            continue;
        }
        proved = true;
        deadline = tokio::time::Instant::now() + idle_timeout;
        stats.own.last_frame_mono_ns.store(seen_mono_ns as u64, Ordering::Release);
        stats.frame();
        frames_since_watch_refresh += 1;
        if frames_since_watch_refresh >= WATCH_REFRESH_FRAMES {
            frames_since_watch_refresh = 0;
            watch_list = watched.snapshot();
        }
        let lower_has = |needle: &str| {
            raw
                .as_bytes()
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
        };
        let is_ctf = lower_has(CTF_EXCHANGE_V2);
        let is_neg = if is_ctf { false } else { lower_has(NEG_RISK_CTF_EXCHANGE_V2) };
        let is_watched = if is_ctf || is_neg {
            false
        } else {
            watch_list.iter().any(|a| lower_has(a))
        };
        if !is_ctf && !is_neg && !is_watched {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let r = &v["params"]["result"];
        let to = r["to"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("0x")
            .to_ascii_lowercase();
        let Some(kind) = classify_to(&to, &watch_list) else {
            continue;
        };
        let neg = kind == TxKind::NegRiskExchange;
        if kind != TxKind::Watched {
            stats.exchange_tx();
        }
        let (hash, input) = (
            r["hash"].as_str().unwrap_or(""),
            r["input"].as_str().unwrap_or(""),
        );
        if hash.is_empty() || input.len() < 10 {
            continue;
        }
        let bytes = match hex::decode(input.trim_start_matches("0x")) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if out
            .send(RawTx {
                source: name.to_string(),
                hash: hash.to_string(),
                input: bytes,
                to_neg_risk: neg,
                to: to.clone(),
                seen_ns,
                seen_mono_ns,
                raw_json: raw,
            })
            .is_err()
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod subscription_watchdog_tests {
    use super::*;

    fn notification(to: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "method": "eth_subscription",
            "params": {"subscription": "synthetic", "result": {
                "hash": format!("0x{}", "12".repeat(32)),
                "to": format!("0x{to}"), "input": "0x12345678"
            }}
        }).to_string()
    }

    async fn run_stream(
        messages: Vec<Message>,
        period: Duration,
        byte_limit: u64,
    ) -> (Result<(), String>, Arc<FeedStats>, Vec<RawTx>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _subscription = ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(r#"{"jsonrpc":"2.0","id":1,"result":"synthetic"}"#.into())).await.unwrap();
            let mut interval = tokio::time::interval(period);
            for message in messages {
                interval.tick().await;
                if ws.send(message).await.is_err() { return; }
            }
            let _ = ws.close(None).await;
        });
        let own = Arc::new(FeedStats::default());
        own.byte_limit.store(byte_limit, Ordering::Relaxed);
        let stats = StatsSink { own: own.clone(), total: Arc::new(FeedStats::default()) };
        let (out, mut received) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(Duration::from_secs(2), pump_with_timeouts(
            "synthetic", &format!("ws://{addr}"), &out, &stats,
            Arc::new(WatchedAddresses::new()), Duration::from_millis(100), Duration::from_millis(150),
        )).await.expect("watchdog failed to terminate");
        server.abort();
        let mut forwarded = Vec::new();
        while let Ok(tx) = received.try_recv() { forwarded.push(tx); }
        (result, own, forwarded)
    }

    #[tokio::test]
    async fn ping_pong_only_stream_expires_without_proving_subscription() {
        let messages = (0..100).map(|i| if i % 2 == 0 {
            Message::Ping(vec![1])
        } else { Message::Pong(vec![1]) }).collect();
        let (result, stats, forwarded) = run_stream(messages, Duration::from_millis(5), 0).await;
        assert!(result.unwrap_err().contains("black hole"));
        assert_eq!(stats.frames.load(Ordering::Relaxed), 0);
        assert_eq!(stats.last_frame_mono_ns.load(Ordering::Relaxed), 0);
        assert!(forwarded.is_empty());
    }

    #[tokio::test]
    async fn valid_notification_then_unrelated_text_expires_idle_deadline() {
        let mut messages = vec![Message::Text(notification(CTF_EXCHANGE_V2))];
        messages.extend((0..100).map(|_| Message::Text(r#"{"jsonrpc":"2.0","id":8,"result":"alive"}"#.into())));
        let (result, stats, forwarded) = run_stream(messages, Duration::from_millis(5), 0).await;
        assert!(result.unwrap_err().contains("silent past"));
        assert_eq!(stats.frames.load(Ordering::Relaxed), 1);
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].input, [0x12, 0x34, 0x56, 0x78]);
    }

    #[tokio::test]
    async fn unrelated_valid_transactions_keep_subscription_alive() {
        let messages = vec![Message::Text(notification(&"ab".repeat(20))); 8];
        let (result, stats, forwarded) = run_stream(messages, Duration::from_millis(40), 0).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(stats.frames.load(Ordering::Relaxed), 8);
        assert!(stats.last_frame_mono_ns.load(Ordering::Relaxed) > 0);
        assert!(forwarded.is_empty());
    }

    #[tokio::test]
    async fn unrelated_text_still_counts_towards_receive_budget() {
        let (result, stats, _) = run_stream(vec![Message::Text("not a transaction".into())], Duration::from_millis(5), 1).await;
        assert!(result.unwrap_err().contains("receive-byte budget"));
        assert!(stats.received_bytes.load(Ordering::Relaxed) > 1);
        assert_eq!(stats.frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn only_full_transaction_notifications_prove_subscription() {
        let valid = notification(CTF_EXCHANGE_V2);
        assert!(is_transaction_notification(&valid));
        assert!(!is_transaction_notification(&valid.replace("eth_subscription", "heartbeat")));
        assert!(!is_transaction_notification(&valid.replace("0x12345678", "0x1234567g")));
        assert!(!is_transaction_notification(r#"{"method":"eth_subscription","params":{"result":"0x1234"}}"#));
        assert!(!is_transaction_notification(r#"{"method":"eth_subscription","params":{"result":{"hash":"bad","input":"0x"}}}"#));
    }
}
