use std::collections::HashMap;
use crate::risk::{LaneRisk, RiskConfig};
#[derive(Debug, Default, Clone)]
pub struct Position {
    pub shares: f64,
    pub cost: f64,
    pub proceeds: f64,
    pub opened_t: i64,
    pub last_fill_t: i64,
    pub his_cost: f64,
    pub his_shares_known: f64,
}
impl Position {
    pub fn avg_his_price(&self) -> Option<f64> {
        if self.his_shares_known > 1e-9 {
            Some(self.his_cost / self.his_shares_known)
        } else {
            None
        }
    }
    pub fn avg_cost(&self) -> f64 {
        if self.shares > 1e-9 { self.cost / self.shares } else { 0.0 }
    }
}
pub fn utc_day(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}
pub fn year_start_secs(now: i64) -> i64 {
    let z = utc_day(now) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    days_from_civil(year, 1, 1) * 86_400
}
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RealisedWindows {
    pub total: f64,
    pub d1: f64,
    pub d7: f64,
    pub d30: f64,
    pub ytd: f64,
    pub y1: f64,
}
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
#[derive(Debug)]
pub struct LaneBook {
    pub name: String,
    pub risk: LaneRisk,
    pub positions: HashMap<String, Position>,
    // Reconciliation can remove custody without identifying its proceeds.
    // Retain that basis until settlement evidence accounts for the release.
    pending_releases: HashMap<String, (f64, f64)>,
    release_windows: HashMap<String, (i64, i64)>,
    settled_shares: HashMap<String, f64>,
    first_buys: HashMap<String, i64>,
    pub spent_today: f64,
    pub spent_day: i64,
    pub fired: u64,
    pub filled: u64,
    pub rejected: u64,
}
impl LaneBook {
    pub fn fresh(name: &str, cfg: RiskConfig) -> LaneBook {
        LaneBook {
            name: name.to_string(),
            risk: LaneRisk::new(name, cfg),
            positions: HashMap::new(),
            pending_releases: HashMap::new(),
            release_windows: HashMap::new(),
            settled_shares: HashMap::new(),
            first_buys: HashMap::new(),
            spent_today: 0.0,
            spent_day: utc_day(now_secs()),
            fired: 0,
            filled: 0,
            rejected: 0,
        }
    }
    pub fn roll_day(&mut self, now: i64) {
        let d = utc_day(now);
        if self.spent_day != d {
            self.spent_day = d;
            self.spent_today = 0.0;
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
#[must_use = "a Booked receipt is the proof a fill reached disk — dropping it discards \
              the only thing that may close a pending row"]
pub struct Booked {
    pub shares: f64,
    pub price: f64,
}
pub struct Ledger {
    pub path: String,
    pub lane_ids: HashMap<String, String>,
    pub lanes: HashMap<String, LaneBook>,
    pub corrupt_lines: u64,
    pub events: u64,
    pub write_failures: u64,
    pub last_write_error: Option<String>,
    pub booked_orders: std::collections::HashSet<String>,
}
impl Ledger {
    pub fn new(path: &str, cfgs: &[(String, RiskConfig)]) -> Self {
        let mut lanes = HashMap::new();
        for (name, cfg) in cfgs {
            lanes.insert(name.clone(), LaneBook::fresh(name, *cfg));
        }
        let mut l = Self {
            lane_ids: HashMap::new(),
            path: path.into(),
            lanes,
            corrupt_lines: 0,
            events: 0,
            write_failures: 0,
            last_write_error: None,
            booked_orders: std::collections::HashSet::new(),
        };
        l.replay();
        l
    }
    pub fn ensure_lane(&mut self, name: &str, cfg: RiskConfig) {
        if self.lanes.contains_key(name) {
            return;
        }
        self.lanes.insert(name.to_string(), LaneBook::fresh(name, cfg));
        self.replay_lane(name);
    }
    fn replay_lane(&mut self, name: &str) {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(_) => return,
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v["lane"].as_str() == Some(name) {
                    self.apply(&v, false);
                }
            }
        }
    }
    fn replay(&mut self) {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.last_write_error = Some(format!("ledger read {}: {e}", self.path));
                return;
            }
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    self.apply(&v, false);
                    self.events += 1;
                }
                Err(_) => self.corrupt_lines += 1,
            }
        }
    }
    fn append(&mut self, row: &serde_json::Value) -> bool {
        use std::io::Write;
        if self.path.is_empty() {
            return true;
        }
        let result = (|| -> std::io::Result<()> {
            let path = std::path::Path::new(&self.path);
            let existed = path.exists();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{row}")?;
            f.flush()?;
            f.sync_data()?;
            if !existed {
                let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.write_failures += 1;
            self.last_write_error = Some(format!("ledger append {}: {e}", self.path));
            eprintln!(
                "[CRITICAL] {}", self.last_write_error.as_deref()
                .unwrap_or("ledger append failed")
            );
            return false;
        }
        true
    }
    pub fn record_fill(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        price: f64,
        fee: f64,
    ) -> bool {
        self.record_fill_with_his_price(lane, token, side, shares, price, fee, None)
    }
    #[must_use = "a dropped fill leaves inventory NO LANE TRACKS — handle the false case"]
    pub fn record_fill_with_his_price(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        price: f64,
        fee: f64,
        his_price: Option<f64>,
    ) -> bool {
        let row = serde_json::json!(
            { "ev" : "fill", "lane" : lane, "token" : token, "side" : side, "shares" :
            shares, "price" : price, "fee" : fee, "his_price" : his_price, "t" :
            now_secs() }
        );
        let booked = self.apply(&row, true);
        if !booked {
            eprintln!(
                "[CRITICAL] {lane}: FILL NOT BOOKED — token {token} side {side} \
                       {shares} @ {price}. The venue gave us these shares and the ledger \
                       refused the row (missing lane book, or the durable append failed). \
                       This is unattributed inventory: reconcile it before trading."
            );
        }
        booked
    }
    pub fn record_recon_fill(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        price: f64,
        fee: f64,
    ) {
        let (shares, price) = if side == 1 {
            let Some(pos) = self.lanes.get(lane).and_then(|b| b.positions.get(token)) else {
                return;
            };
            if !shares.is_finite() || shares <= 1e-9 || pos.shares <= 1e-9 {
                return;
            }
            (shares.min(pos.shares), pos.avg_cost())
        } else {
            (shares, price)
        };
        let row = serde_json::json!(
            { "ev" : "fill", "lane" : lane, "token" : token, "side" : side, "shares" :
            shares, "price" : price, "fee" : fee, "recon" : true, "t" : now_secs() }
        );
        self.apply(&row, true);
    }
    pub fn pending_release(&self, lane: &str, token: &str) -> Option<(f64, f64)> {
        self.lanes.get(lane)?.pending_releases.get(token).copied()
            .filter(|(shares, _)| *shares > 1e-9)
    }
    // Bounds span the accumulated releases, including a partially accounted
    // batch. They are not evidence that any particular share was redeemed.
    pub fn pending_release_window(&self, lane: &str, token: &str) -> Option<(i64, i64)> {
        self.pending_release(lane, token)?;
        self.lanes.get(lane)?.release_windows.get(token).copied()
    }
    pub fn settlement_tokens(&self, lane: &str) -> Vec<String> {
        let Some(book) = self.lanes.get(lane) else { return Vec::new(); };
        let mut tokens: Vec<String> = book.positions.iter()
            .filter(|(_, p)| p.shares > 1e-9)
            .map(|(token, _)| token.clone())
            .chain(book.pending_releases.iter()
                .filter(|(_, (shares, _))| *shares > 1e-9)
                .map(|(token, _)| token.clone()))
            .collect();
        tokens.sort();
        tokens.dedup();
        tokens
    }
    pub fn pool_settled_shares(&self, token: &str) -> f64 {
        self.lanes.values().filter_map(|b| b.settled_shares.get(token)).sum()
    }
    pub fn first_tracked_buy(&self, token: &str) -> Option<i64> {
        self.lanes.values().filter_map(|b| b.first_buys.get(token)).copied()
            .min().filter(|t| *t > 0)
    }
    pub fn realised_reset(&mut self, lane: &str, why: &str) {
        let row = serde_json::json!(
            { "ev" : "realised_reset", "lane" : lane, "why" : why, "t" : now_secs() }
        );
        self.apply(&row, true);
    }
    pub fn newest_realised_reset_t(&self) -> Option<i64> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .filter(|v| v["ev"].as_str() == Some("realised_reset"))
            .filter_map(|v| v["t"].as_i64())
            .max()
    }
    pub fn clear_risk(&mut self, lane: &str, why: &str) -> bool {
        let was = self
            .lanes
            .get(lane)
            .map(|b| b.risk.tripped.is_some())
            .unwrap_or(false);
        let row = serde_json::json!(
            { "ev" : "risk_cleared", "lane" : lane, "why" : why, "t" : now_secs() }
        );
        self.apply(&row, true);
        was
    }
    pub fn record_settlement(&mut self, lane: &str, token: &str, payout: f64) {
        let row = serde_json::json!(
            { "ev" : "settle", "lane" : lane, "token" : token, "payout" : payout, "t" :
            now_secs() }
        );
        self.apply(&row, true);
    }
    pub fn record_merge(
        &mut self,
        lane: &str,
        yes_token: &str,
        no_token: &str,
        shares: f64,
    ) {
        let row = serde_json::json!(
            { "ev" : "merge", "lane" : lane, "yes" : yes_token, "no" : no_token, "shares"
            : shares, "t" : now_secs() }
        );
        self.apply(&row, true);
    }
    pub fn mergeable(&self, lane: &str, yes_token: &str, no_token: &str) -> f64 {
        let held = |t: &str| {
            self
                .lanes
                .get(lane)
                .and_then(|b| b.positions.get(t))
                .map(|p| p.shares)
                .unwrap_or(0.0)
        };
        held(yes_token).min(held(no_token))
    }
    pub fn record_realised_adjustment(
        &mut self,
        lane: &str,
        token: &str,
        shares: f64,
        proceeds: f64,
        avg_cost: f64,
        why: &str,
    ) {
        let pnl = proceeds - shares * avg_cost;
        let row = serde_json::json!(
            { "ev" : "realised_adjust", "lane" : lane, "token" : token, "shares" :
            shares, "proceeds" : proceeds, "avg_cost" : avg_cost, "pnl" : pnl, "why" :
            why, "t" : now_secs() }
        );
        self.apply(&row, true);
    }
    pub fn record_realised_adjustment_tx(
        &mut self,
        lane: &str,
        token: &str,
        shares: f64,
        proceeds: f64,
        avg_cost: f64,
        why: &str,
        key: &str,
    ) {
        let pnl = proceeds - shares * avg_cost;
        let row = serde_json::json!(
            { "ev" : "realised_adjust", "lane" : lane, "token" : token, "shares" :
            shares, "proceeds" : proceeds, "avg_cost" : avg_cost, "pnl" : pnl, "why" :
            why, "key" : key, "alt_key" : crate ::settlement::cross_writer_key(lane,
            token), "t" : now_secs() }
        );
        self.apply(&row, true);
    }
    pub fn corrected_keys(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        let Ok(raw) = std::fs::read_to_string(&self.path) else { return out };
        for line in raw.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                for field in ["key", "alt_key"] {
                    if let Some(key) = v[field].as_str() {
                        out.insert(key.to_string());
                    }
                }
            }
        }
        out
    }
    pub fn last_recon_release(
        &self,
        lane: &str,
        token: &str,
        not_before: i64,
    ) -> Option<(f64, f64, i64)> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let mut found = None;
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue
            };
            if v["ev"].as_str() == Some("fill") && v["lane"].as_str() == Some(lane)
                && v["token"].as_str() == Some(token) && v["side"].as_u64() == Some(1)
                && v["recon"].as_bool() == Some(true)
            {
                let t = v["t"].as_i64().unwrap_or(0);
                if t < not_before {
                    continue;
                }
                let shares = v["shares"].as_f64().unwrap_or(0.0);
                let price = v["price"].as_f64().unwrap_or(0.0);
                found = Some((shares, price, t));
            }
        }
        found
    }
    fn apply(&mut self, row: &serde_json::Value, durable: bool) -> bool {
        let lane_name = row["lane"].as_str().unwrap_or("");
        let ev = row["ev"].as_str().unwrap_or("");
        let token = row["token"].as_str().unwrap_or("").to_string();
        if !self.lanes.contains_key(lane_name) {
            return false;
        }
        if durable && !self.append(row) {
            return false;
        }
        if let Some(h) = row["order_hash"].as_str() {
            if !h.is_empty() {
                self.booked_orders.insert(h.to_string());
            }
        }
        let Some(lane) = self.lanes.get_mut(lane_name) else {
            return false;
        };
        match ev {
            "fill" => {
                let shares = row["shares"].as_f64().unwrap_or(0.0);
                let price = row["price"].as_f64().unwrap_or(0.0);
                let fee = row["fee"].as_f64().unwrap_or(0.0);
                let side = row["side"].as_u64().unwrap_or(0) as u8;
                let tok_tail = token[token.len().saturating_sub(8)..].to_string();
                let pos = lane.positions.entry(token.clone()).or_default();
                let was_flat = pos.shares <= 1e-9;
                pos.last_fill_t = pos.last_fill_t.max(row["t"].as_i64().unwrap_or(0));
                if side == 0 {
                    pos.shares += shares;
                    pos.cost += shares * price + fee;
                    if let Some(hp) = row["his_price"].as_f64() {
                        pos.his_cost += shares * hp;
                        pos.his_shares_known += shares;
                    }
                    if was_flat {
                        pos.opened_t = row["t"].as_i64().unwrap_or(0);
                    }
                    let now = now_secs();
                    lane.roll_day(now);
                    let row_t = row["t"].as_i64().unwrap_or(0);
                    let is_recon = row["recon"].as_bool().unwrap_or(false);
                    if shares > 1e-9 {
                        lane.first_buys.entry(token.clone())
                            .and_modify(|t| *t = (*t).min(row_t)).or_insert(row_t);
                    }
                    if !is_recon && row_t > 0 && utc_day(row_t) == utc_day(now) {
                        lane.spent_today += shares * price + fee;
                    }
                    if was_flat {
                        lane.risk.on_open();
                    }
                } else {
                    let sold = shares.min(pos.shares);
                    if shares > pos.shares + 1e-6 {
                        eprintln!(
                            "[ledger] {lane_name}: sell row claims {shares:.4} sh \
of …{} but only {:.4} tracked — excess IGNORED (replay-safe), investigate the row",
                            tok_tail, pos.shares
                        );
                    }
                    let avg = pos.avg_cost();
                    let is_recon = row["recon"].as_bool().unwrap_or(false);
                    if is_recon && sold > 1e-9 {
                        let release = lane.pending_releases.entry(token.clone()).or_default();
                        release.0 += sold;
                        release.1 += sold * avg;
                        let t = row["t"].as_i64().unwrap_or(0);
                        lane.release_windows.entry(token.clone())
                            .and_modify(|window| { window.0 = window.0.min(t); window.1 = window.1.max(t); })
                            .or_insert((t, t));
                    }
                    let realised = if is_recon {
                        0.0
                    } else {
                        sold * price - sold * avg - fee
                    };
                    pos.cost -= sold * avg;
                    if let Some(his_avg) = pos.avg_his_price() {
                        let his_sold = sold.min(pos.his_shares_known);
                        pos.his_cost -= his_sold * his_avg;
                        pos.his_shares_known -= his_sold;
                    }
                    pos.shares -= sold;
                    pos.proceeds += sold * price - fee;
                    if !is_recon {
                        lane.risk.on_close(realised);
                    }
                    if pos.shares <= 1e-9 {
                        lane.risk.on_flat();
                    }
                }
                lane.filled += 1;
            }
            "realised_reset" => {
                lane.risk.realised_pnl = 0.0;
            }
            "risk_cleared" => {
                lane.risk.clear();
            }
            "realised_adjust" => {
                lane.risk.on_adjust(row["pnl"].as_f64().unwrap_or(0.0));
                let settlement_key = crate::settlement::cross_writer_key(lane_name, &token);
                if ["key", "alt_key"].iter().any(|field| row[*field].as_str() == Some(&settlement_key)) {
                    let covered = row["shares"].as_f64().unwrap_or(0.0);
                    if covered.is_finite() && covered > 1e-9 {
                        *lane.settled_shares.entry(token.clone()).or_default() += covered;
                        if let Some(release) = lane.pending_releases.get_mut(&token) {
                            let consumed = covered.min(release.0);
                            let avg = row["avg_cost"].as_f64().unwrap_or(0.0);
                            let covered_cost = if avg.is_finite() && avg >= 0.0 {
                                (consumed * avg).min(release.1)
                            } else {
                                0.0
                            };
                            release.0 = (release.0 - consumed).max(0.0);
                            release.1 = (release.1 - covered_cost).max(0.0);
                            if release.0 <= 1e-9 {
                                lane.pending_releases.remove(&token);
                                lane.release_windows.remove(&token);
                            }
                        }
                    }
                }
            }
            "settle" => {
                let payout = row["payout"].as_f64().unwrap_or(0.0);
                if let Some(pos) = lane.positions.get_mut(&token) {
                    if pos.shares > 1e-9 {
                        *lane.settled_shares.entry(token.clone()).or_default() += pos.shares;
                        let realised = pos.shares * payout - pos.cost;
                        lane.risk.on_close(realised);
                        lane.risk.on_flat();
                        pos.shares = 0.0;
                        pos.cost = 0.0;
                        pos.his_cost = 0.0;
                        pos.his_shares_known = 0.0;
                    }
                }
            }
            "merge" => {
                let yes_tok = row["yes"].as_str().unwrap_or("").to_string();
                let no_tok = row["no"].as_str().unwrap_or("").to_string();
                let want = row["shares"].as_f64().unwrap_or(0.0);
                let (ys, avg_yes) = lane
                    .positions
                    .get(&yes_tok)
                    .map(|p| (p.shares, p.avg_cost()))
                    .unwrap_or((0.0, 0.0));
                let (ns, avg_no) = lane
                    .positions
                    .get(&no_tok)
                    .map(|p| (p.shares, p.avg_cost()))
                    .unwrap_or((0.0, 0.0));
                let m = want.min(ys).min(ns);
                if m > 1e-9 {
                    lane.risk.on_close(m - m * avg_yes - m * avg_no);
                    let mut flats = 0;
                    if let Some(p) = lane.positions.get_mut(&yes_tok) {
                        p.cost -= m * avg_yes;
                        p.shares -= m;
                        p.proceeds += m * avg_yes;
                        if p.shares <= 1e-9 {
                            flats += 1;
                        }
                    }
                    if let Some(p) = lane.positions.get_mut(&no_tok) {
                        p.cost -= m * avg_no;
                        p.shares -= m;
                        p.proceeds += m * avg_no;
                        if p.shares <= 1e-9 {
                            flats += 1;
                        }
                    }
                    for _ in 0..flats {
                        lane.risk.on_flat();
                    }
                    lane.filled += 1;
                }
            }
            _ => {}
        }
        true
    }
    pub fn book_fill(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        price: f64,
        fee: f64,
        his_price: Option<f64>,
        order_hash: &str,
    ) -> Option<Booked> {
        self.book_fill_ex(
            lane,
            token,
            side,
            shares,
            price,
            fee,
            his_price,
            order_hash,
            false,
        )
    }
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn book_fill_ex(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        price: f64,
        fee: f64,
        his_price: Option<f64>,
        order_hash: &str,
        resting: bool,
    ) -> Option<Booked> {
        self.book_fill_ex_px(
            lane,
            token,
            side,
            shares,
            price,
            fee,
            his_price,
            order_hash,
            resting,
            false,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn book_fill_ex_px(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        price: f64,
        fee: f64,
        his_price: Option<f64>,
        order_hash: &str,
        resting: bool,
        px_provisional: bool,
    ) -> Option<Booked> {
        if !order_hash.is_empty() && self.booked_orders.contains(order_hash) {
            return Some(Booked { shares, price });
        }
        let mut row = serde_json::json!(
            { "ev" : "fill", "lane" : lane, "token" : token, "side" : side, "shares" :
            shares, "price" : price, "fee" : fee, "his_price" : his_price, "exec" : if
            resting { "maker" } else { "taker" }, "order_hash" : order_hash, "t" :
            now_secs() }
        );
        if let Some(id) = self.lane_ids.get(lane) {
            row["lane_id"] = serde_json::json!(id);
        }
        if px_provisional {
            row["px_provisional"] = serde_json::json!(true);
        }
        if self.apply(&row, true) { Some(Booked { shares, price }) } else { None }
    }
    pub fn open_usd(&self, lane: &str) -> f64 {
        self.lanes
            .get(lane)
            .map(|b| {
                b.positions.values().filter(|p| p.shares > 1e-9).map(|p| p.cost).sum()
            })
            .unwrap_or(0.0)
    }
    pub fn avg_cost(&self, lane: &str, token: &str) -> f64 {
        self.lanes
            .get(lane)
            .and_then(|l| l.positions.get(token))
            .map(|p| p.avg_cost())
            .unwrap_or(0.0)
    }
    pub fn holdings(&self, lane: &str) -> HashMap<String, f64> {
        self.lanes
            .get(lane)
            .map(|b| {
                b.positions
                    .iter()
                    .filter(|(_, p)| p.shares > 1e-9)
                    .map(|(t, p)| (t.clone(), p.shares))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn lane_epoch(&self, lane: &str) -> Option<i64> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .filter(|v| v["lane"].as_str() == Some(lane))
            .filter_map(|v| v["t"].as_i64())
            .filter(|t| *t > 0)
            .min()
    }
    pub fn equity_series(&self, lane: Option<&str>) -> Vec<(i64, f64)> {
        equity_series_at(&self.path, lane)
    }
    pub fn book_merge(
        &mut self,
        lane: &str,
        token_a: &str,
        token_b: &str,
        pairs: f64,
        mark_a: f64,
        mark_b: f64,
        tx_hash: &str,
    ) -> Option<(Booked, Booked)> {
        if !(pairs.is_finite() && pairs > 0.0) {
            return None;
        }
        let total = mark_a + mark_b;
        let price_a = if total.is_finite() && total > 0.0 && mark_a.is_finite()
            && mark_a >= 0.0
        {
            (mark_a / total).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let price_b = 1.0 - price_a;
        let a = self
            .book_fill_ex_px(
                lane,
                token_a,
                1,
                pairs,
                price_a,
                0.0,
                None,
                &format!("{tx_hash}:a"),
                false,
                false,
            )?;
        let b = self
            .book_fill_ex_px(
                lane,
                token_b,
                1,
                pairs,
                price_b,
                0.0,
                None,
                &format!("{tx_hash}:b"),
                false,
                false,
            )?;
        Some((a, b))
    }
    pub fn realised_windows_at(
        path: &str,
        lane: Option<&str>,
        now: i64,
    ) -> RealisedWindows {
        RealisedWindows::from_series(&equity_series_at(path, lane), now)
    }
}
pub fn equity_series_at(path: &str, lane: Option<&str>) -> Vec<(i64, f64)> {
    {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut scratch = Ledger {
            lane_ids: HashMap::new(),
            path: String::new(),
            lanes: HashMap::new(),
            corrupt_lines: 0,
            events: 0,
            write_failures: 0,
            last_write_error: None,
            booked_orders: std::collections::HashSet::new(),
        };
        let mut out: Vec<(i64, f64)> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue
            };
            let row_lane = v["lane"].as_str().unwrap_or("");
            if row_lane.is_empty() {
                continue;
            }
            scratch
                .lanes
                .entry(row_lane.to_string())
                .or_insert_with(|| LaneBook::fresh(row_lane, RiskConfig::default()));
            scratch.apply(&v, false);
            if let Some(name) = lane {
                if row_lane != name {
                    continue;
                }
            }
            let Some(t) = v["t"].as_i64().filter(|t| *t > 0) else { continue };
            let total = match lane {
                Some(name) => {
                    scratch.lanes.get(name).map(|b| b.risk.realised_pnl).unwrap_or(0.0)
                }
                None => scratch.lanes.values().map(|b| b.risk.realised_pnl).sum(),
            };
            let total = (total * 100.0).round() / 100.0;
            match out.last_mut() {
                Some(p) if p.0 == t => p.1 = total,
                _ => out.push((t, total)),
            }
        }
        out
    }
}
impl Ledger {
    #[cfg(test)]
    pub fn realised_windows(&self, lane: Option<&str>, now: i64) -> RealisedWindows {
        RealisedWindows::from_series(&self.equity_series(lane), now)
    }
}
impl RealisedWindows {
    pub fn from_series(series: &[(i64, f64)], now: i64) -> RealisedWindows {
        let total = series.last().map(|p| p.1).unwrap_or(0.0);
        let cum_at = |cutoff: i64| -> f64 {
            series.iter().rev().find(|p| p.0 <= cutoff).map(|p| p.1).unwrap_or(0.0)
        };
        let win = |cutoff: i64| ((total - cum_at(cutoff)) * 100.0).round() / 100.0;
        RealisedWindows {
            total: (total * 100.0).round() / 100.0,
            d1: win(now - 86_400),
            d7: win(now - 7 * 86_400),
            d30: win(now - 30 * 86_400),
            ytd: win(year_start_secs(now)),
            y1: win(now - 365 * 86_400),
        }
    }
}
impl Ledger {
    pub fn pool_claim(&self, tok: &str) -> f64 {
        self.lanes.values().filter_map(|b| b.positions.get(tok)).map(|p| p.shares).sum()
    }
    pub fn adoptable(
        &self,
        lane: &str,
        chain: &HashMap<String, f64>,
        quiet_secs: i64,
        now: i64,
        max_shares: f64,
    ) -> Vec<(String, f64, f64)> {
        let Some(b) = self.lanes.get(lane) else { return Vec::new() };
        let mut out = Vec::new();
        for (tok, pos) in b.positions.iter() {
            if pos.shares <= 1e-9 {
                continue;
            }
            if pos.last_fill_t <= 0 {
                continue;
            }
            if now - pos.last_fill_t < quiet_secs {
                continue;
            }
            let on_chain = chain.get(tok).copied().unwrap_or(0.0);
            let extra = on_chain - pos.shares;
            if extra <= 0.001 {
                continue;
            }
            if extra >= max_shares {
                continue;
            }
            let claimed_by_pool: f64 = self.pool_claim(tok);
            let unexplained = on_chain - claimed_by_pool;
            if unexplained <= 1e-9 {
                continue;
            }
            let take = extra.min(unexplained);
            if take <= 0.001 {
                continue;
            }
            out.push((tok.clone(), take, pos.avg_cost()));
        }
        out
    }
    pub fn releasable(
        &self,
        lane: &str,
        chain: &HashMap<String, f64>,
        quiet_secs: i64,
        now: i64,
    ) -> Vec<(String, f64, f64)> {
        let Some(b) = self.lanes.get(lane) else { return Vec::new() };
        let mut out = Vec::new();
        for (tok, pos) in b.positions.iter() {
            if pos.shares <= 1e-9 {
                continue;
            }
            if pos.last_fill_t <= 0 {
                continue;
            }
            if now - pos.last_fill_t < quiet_secs {
                continue;
            }
            let on_chain = chain.get(tok).copied().unwrap_or(0.0);
            let gone = pos.shares - on_chain;
            if gone <= 1e-9 {
                continue;
            }
            if gone <= 0.01 && on_chain > 0.0 {
                continue;
            }
            out.push((tok.clone(), gone, pos.avg_cost()));
        }
        out
    }
    pub fn open_positions(
        &self,
        lane: &str,
    ) -> Vec<(String, f64, f64, i64, Option<f64>, f64)> {
        self.lanes
            .get(lane)
            .map(|b| {
                b.positions
                    .iter()
                    .filter(|(_, p)| p.shares > 1e-9)
                    .map(|(t, p)| (
                        t.clone(),
                        p.shares,
                        p.avg_cost(),
                        p.opened_t,
                        p.avg_his_price(),
                        p.his_shares_known,
                    ))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn persistence_ok(&self) -> bool {
        self.last_write_error.is_none() && self.corrupt_lines == 0
    }
}
pub fn scan_for_settlement(
    path: &str,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashMap<(String, String), (f64, f64, i64)>,
) {
    let mut keys = std::collections::HashSet::new();
    let mut releases = std::collections::HashMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else { return (keys, releases) };
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue
        };
        if let Some(key) = v["key"].as_str() {
            keys.insert(key.to_string());
        }
        if v["ev"].as_str() == Some("fill") && v["side"].as_u64() == Some(1)
            && v["recon"].as_bool() == Some(true)
        {
            let (Some(lane), Some(token)) = (v["lane"].as_str(), v["token"].as_str())
            else { continue };
            releases
                .insert(
                    (lane.to_string(), token.to_string()),
                    (
                        v["shares"].as_f64().unwrap_or(0.0),
                        v["price"].as_f64().unwrap_or(0.0),
                        v["t"].as_i64().unwrap_or(0),
                    ),
                );
        }
    }
    (keys, releases)
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Continuity {
    pub lines: u64,
    pub bytes: u64,
    pub prefix_sha256: String,
}
#[derive(Debug, Clone, PartialEq)]
pub enum ContinuityVerdict {
    FirstRun,
    Intact,
    Broken(String),
}
fn prefix_digest(raw: &str, lines: u64) -> (String, u64) {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut n = 0u64;
    for line in raw.lines() {
        if n >= lines {
            break;
        }
        h.update(line.as_bytes());
        h.update(b"\n");
        n += 1;
    }
    (hex::encode(h.finalize()), n)
}
pub const LEDGER_GROWTH_ALARM_LINES: u64 = 250_000;

#[cfg(test)]
mod release_accounting_tests {
    use super::*;

    fn ledger() -> Ledger {
        Ledger::new("", &[("a".into(), RiskConfig::default()), ("b".into(), RiskConfig::default())])
    }
    fn fill(l: &mut Ledger, lane: &str, side: u8, shares: f64, price: f64, recon: bool, t: i64) {
        assert!(l.apply(&serde_json::json!({"ev":"fill","lane":lane,"token":"101",
            "side":side,"shares":shares,"price":price,"recon":recon,"t":t}), false));
    }
    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn stale_release_rows_retain_only_effective_shares_and_actual_basis() {
        let mut l = ledger();
        fill(&mut l, "a", 0, 10.0, 0.4, false, 100);
        fill(&mut l, "a", 1, 6.0, 0.6, false, 101);
        fill(&mut l, "a", 1, 20.0, 0.9, true, 102);
        fill(&mut l, "a", 1, 20.0, 0.9, true, 103);
        let (shares, cost) = l.pending_release("a", "101").unwrap();
        near(shares, 4.0);
        near(cost, 1.6);
        assert_eq!(l.pending_release_window("a", "101"), Some((102, 102)));
        near(l.lanes["a"].risk.realised_pnl, 1.2);
        near(l.open_usd("a"), 0.0);
        assert_eq!(l.settlement_tokens("a"), ["101"]);
    }

    #[test]
    fn release_remains_uncredited_when_final_payout_is_known() {
        let mut l = ledger();
        fill(&mut l, "a", 0, 10.0, 0.4, false, 100);
        l.record_recon_fill("a", "101", 1, 4.0, 0.9, 0.0);
        l.record_settlement("a", "101", 1.0);
        near(l.lanes["a"].risk.realised_pnl, 3.6);
        near(l.pool_settled_shares("101"), 6.0);
        assert_eq!(l.pending_release("a", "101"), Some((4.0, 1.6)));
        l.record_settlement("a", "101", 1.0);
        near(l.pool_settled_shares("101"), 6.0);
        assert_eq!(l.settlement_tokens("a"), ["101"]);
    }

    #[test]
    fn historical_adjustments_consume_only_covered_release_and_preserve_pnl() {
        let mut l = ledger();
        fill(&mut l, "a", 0, 10.0, 0.4, false, 100);
        fill(&mut l, "a", 1, 10.0, 0.9, true, 101);
        l.record_realised_adjustment_tx("a", "101", 2.5, 2.5, 0.4, "redemption", "tx1");
        assert_eq!(l.pending_release("a", "101"), Some((7.5, 3.0)));
        near(l.pool_settled_shares("101"), 2.5);
        l.record_realised_adjustment("a", "101", 1.0, 0.8, 0.4, "manual correction");
        assert_eq!(l.pending_release("a", "101"), Some((7.5, 3.0)));
        near(l.lanes["a"].risk.realised_pnl, 1.9);
        // The older writer used key rather than alt_key. Existing journal PnL
        // remains authoritative even when its requested quantity was excessive.
        assert!(l.apply(&serde_json::json!({"ev":"realised_adjust","lane":"a","token":"101",
            "shares":20.0,"avg_cost":0.4,"pnl":12.0,"key":"settle:a:101"}), false));
        assert_eq!(l.pending_release("a", "101"), None);
        assert_eq!(l.pending_release_window("a", "101"), None);
        near(l.pool_settled_shares("101"), 22.5);
        near(l.lanes["a"].risk.realised_pnl, 13.9);
    }

    #[test]
    fn release_basis_accumulates_across_distinct_inventory_cycles() {
        let mut l = ledger();
        fill(&mut l, "a", 0, 4.0, 0.2, false, 100);
        fill(&mut l, "a", 1, 4.0, 0.2, true, 101);
        fill(&mut l, "a", 0, 6.0, 0.7, false, 102);
        fill(&mut l, "a", 1, 6.0, 0.7, true, 103);
        let (shares, cost) = l.pending_release("a", "101").unwrap();
        near(shares, 10.0);
        near(cost, 5.0);
        assert_eq!(l.pending_release_window("a", "101"), Some((101, 103)));
        l.record_realised_adjustment_tx("a", "101", 4.0, 4.0, 0.2, "redemption", "tx1");
        let (shares, cost) = l.pending_release("a", "101").unwrap();
        near(shares, 6.0);
        near(cost, 4.2);
        assert_eq!(l.pending_release_window("a", "101"), Some((101, 103)));
    }

    #[test]
    fn replay_restores_releases_and_pool_evidence_counters_without_pnl_changes() {
        let dir = std::env::temp_dir().join(format!("copybot-release-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let rows = [
            serde_json::json!({"ev":"fill","lane":"a","token":"101","side":0,"shares":10.0,"price":0.4,"t":100}),
            serde_json::json!({"ev":"fill","lane":"a","token":"101","side":1,"shares":10.0,"price":0.9,"recon":true,"t":101}),
            serde_json::json!({"ev":"realised_adjust","lane":"a","token":"101","shares":3.0,"avg_cost":0.4,"pnl":1.8,"alt_key":"settle:a:101","key":"tx1","t":102}),
            serde_json::json!({"ev":"fill","lane":"b","token":"101","side":0,"shares":2.0,"price":0.5,"recon":true,"t":90}),
            serde_json::json!({"ev":"settle","lane":"b","token":"101","payout":0.0,"t":103}),
        ];
        std::fs::write(&path, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let cfgs = [("a".into(), RiskConfig::default()), ("b".into(), RiskConfig::default())];
        let replayed = Ledger::new(path.to_str().unwrap(), &cfgs);
        let mut live = ledger();
        for row in &rows { assert!(live.apply(row, false)); }
        assert_eq!(replayed.pending_release("a", "101"), live.pending_release("a", "101"));
        assert_eq!(replayed.pending_release_window("a", "101"), Some((101, 101)));
        near(replayed.pending_release("a", "101").unwrap().1, 2.8);
        near(replayed.pool_settled_shares("101"), 5.0);
        assert_eq!(replayed.first_tracked_buy("101"), Some(90));
        near(replayed.lanes["a"].risk.realised_pnl, 1.8);
        near(replayed.lanes["b"].risk.realised_pnl, -1.0);
        let mut later_lane = Ledger::new(path.to_str().unwrap(), &cfgs[..1]);
        later_lane.ensure_lane("b", RiskConfig::default());
        near(later_lane.pool_settled_shares("101"), 5.0);
        assert_eq!(later_lane.first_tracked_buy("101"), Some(90));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_reconciliation_rows_clamp_quantity_and_snapshot_current_cost() {
        let dir = std::env::temp_dir().join(format!("copybot-release-clamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let mut l = Ledger::new(path.to_str().unwrap(), &[("a".into(), RiskConfig::default())]);
        l.record_fill("a", "101", 0, 4.0, 0.4, 0.0);
        l.record_recon_fill("a", "101", 1, 20.0, 0.9, 0.0);
        let before = std::fs::read_to_string(&path).unwrap();
        l.record_recon_fill("a", "101", 1, 20.0, 0.9, 0.0);
        l.record_recon_fill("a", "102", 1, 20.0, 0.9, 0.0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let row: serde_json::Value = serde_json::from_str(before.lines().last().unwrap()).unwrap();
        assert_eq!(row["shares"], 4.0);
        assert_eq!(row["price"], 0.4);
        assert_eq!(l.pending_release("a", "101"), Some((4.0, 1.6)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_release_append_does_not_move_inventory_or_pending_basis() {
        let mut l = ledger();
        fill(&mut l, "a", 0, 10.0, 0.4, false, 100);
        let path = std::env::temp_dir().join(format!("copybot-release-unwritable-{}", std::process::id()));
        std::fs::write(&path, b"regular file").unwrap();
        l.path = path.join("ledger.jsonl").to_str().unwrap().into();
        l.record_recon_fill("a", "101", 1, 10.0, 0.4, 0.0);
        near(l.lanes["a"].positions["101"].shares, 10.0);
        assert_eq!(l.pending_release("a", "101"), None);
        assert!(!l.persistence_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_old_buy_timestamp_does_not_allow_a_later_evidence_cutoff() {
        let mut l = ledger();
        fill(&mut l, "a", 0, 1.0, 0.4, true, 0);
        fill(&mut l, "b", 0, 1.0, 0.4, false, 100);
        assert_eq!(l.first_tracked_buy("101"), None);
        assert_eq!(l.first_tracked_buy("unknown"), None);
    }
}

impl Ledger {
    pub fn continuity(&self) -> Option<Continuity> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let total = raw.lines().count() as u64;
        let (prefix_sha256, lines) = prefix_digest(&raw, total);
        Some(Continuity {
            lines,
            bytes: raw.len() as u64,
            prefix_sha256,
        })
    }
    pub fn verify_continuity(&self, sidecar: &str) -> ContinuityVerdict {
        let Ok(prev_raw) = std::fs::read_to_string(sidecar) else {
            return ContinuityVerdict::FirstRun;
        };
        let Ok(prev) = serde_json::from_str::<Continuity>(&prev_raw) else {
            return ContinuityVerdict::Broken(
                "the continuity record itself is unreadable".into(),
            );
        };
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return ContinuityVerdict::Broken(format!("cannot read {}", self.path));
        };
        let total = raw.lines().count() as u64;
        if total < prev.lines {
            return ContinuityVerdict::Broken(
                format!(
                    "the ledger LOST history: {total} lines now, {} at the last boot",
                    prev.lines
                ),
            );
        }
        let (digest, seen) = prefix_digest(&raw, prev.lines);
        if seen < prev.lines {
            return ContinuityVerdict::Broken(
                format!(
                    "only {seen} of the {} previously replayed lines remain", prev.lines
                ),
            );
        }
        if digest != prev.prefix_sha256 {
            return ContinuityVerdict::Broken(
                format!(
                    "the first {} lines no longer hash to what they did at the last boot — \
history was REWRITTEN, not appended to",
                    prev.lines
                ),
            );
        }
        ContinuityVerdict::Intact
    }
    pub fn record_continuity(&self, sidecar: &str) -> Result<(), String> {
        let Some(c) = self.continuity() else {
            return Err(format!("cannot read {} to fingerprint it", self.path));
        };
        let body = serde_json::to_string(&c).map_err(|e| e.to_string())?;
        let tmp = format!("{sidecar}.tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("write {tmp}: {e}"))?;
        std::fs::rename(&tmp, sidecar).map_err(|e| format!("replace {sidecar}: {e}"))
    }
}
