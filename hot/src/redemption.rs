//! Read-only proof of redemption, distinct from a merge or disappearing inventory.
//! No ledger mutation or RPC is performed here. Callers must verify the returned
//! block hash against their block read and persist consumption of this evidence.
use serde_json::Value;
use std::collections::HashSet;

const SINGLE: &str = "c3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62";
const BATCH: &str = "4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb";
const TRANSFER: &str = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const PAYOUT: &str = "2682012a4a4f1973119f1c9b90745d1bd91fa2bab387344f044cb3586864d18d";
const LEGACY_PAYOUT: &str = "9140a6a270ef945260c03894b3c6b3b2695e9d5101feef0ff24fec960cfd3224";
const WRAPPER_PAYOUT: &str = "74a51ebefec30281ec6849b727ec7916f9b1a3e5e148d6771d98315215b38b96";
const LEGACY_ADAPTER: &str = "d91e80cf2e7be2e162c6513ced06f1dd0da35296";
const REDEEM_WRAPPER: &str = "a1200000d0002264c9a1698e001292d00e1b00af";
const WRAPPED_USDC: &str = "3a3bd7bb9528e159577f7c2e685cc81a765002e2";
type Check<T> = Result<T, String>;

#[derive(Debug, Clone)]
pub struct RedemptionContext {
    pub expected_tx: String,
    pub funder: [u8; 20],
    pub ctf: [u8; 20],
    pub condition: [u8; 32],
    /// Binary outcome order, matching the final payout vector.
    pub tokens: [String; 2],
    pub numerators: [u128; 2],
    pub denominator: u128,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TokenRedemption {
    pub token: String,
    pub units: u128,
    pub payout_units: u128,
    pub burn_log_indices: Vec<u64>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RedemptionEvidence {
    pub tx: String,
    pub block_number: u64,
    pub block_hash: String,
    pub redemption_log_index: u64,
    pub tokens: Vec<TokenRedemption>,
}

fn bytes<const N: usize>(s: &str) -> Check<[u8; N]> {
    let raw = hex::decode(s.strip_prefix("0x").unwrap_or(s)).map_err(|_| "invalid hex")?;
    raw.try_into().map_err(|_| "wrong hex width".into())
}
fn quantity(v: &Value) -> Check<u64> {
    let s = v
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .ok_or("missing hex quantity")?;
    u64::from_str_radix(s, 16).map_err(|_| "invalid hex quantity".into())
}
fn word(data: &[u8], i: usize) -> Check<[u8; 32]> {
    let start = i.checked_mul(32).ok_or("offset overflow")?;
    data.get(start..start.checked_add(32).ok_or("offset overflow")?)
        .ok_or("truncated ABI word")?
        .try_into()
        .map_err(|_| "invalid ABI word".into())
}
fn uint(w: [u8; 32]) -> Check<u128> {
    if w[..16].iter().any(|x| *x != 0) {
        return Err("integer exceeds supported range".into());
    }
    Ok(u128::from_be_bytes(w[16..].try_into().unwrap()))
}
fn address(w: [u8; 32]) -> Check<[u8; 20]> {
    if w[..12].iter().any(|x| *x != 0) {
        return Err("noncanonical address".into());
    }
    Ok(w[12..].try_into().unwrap())
}
fn token_word(s: &str) -> Check<[u8; 32]> {
    if s.is_empty() || s.len() > 78 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid decimal token".into());
    }
    let mut out = [0u8; 32];
    for digit in s.bytes() {
        let mut carry = (digit - b'0') as u16;
        for b in out.iter_mut().rev() {
            let n = (*b as u16) * 10 + carry;
            *b = n as u8;
            carry = n >> 8;
        }
        if carry != 0 {
            return Err("token exceeds uint256".into());
        }
    }
    Ok(out)
}
#[derive(Debug)]
struct Log {
    index: u64,
    emitter: [u8; 20],
    topics: Vec<[u8; 32]>,
    data: Vec<u8>,
}
impl Log {
    fn is(&self, sig: &str) -> bool {
        self.topics.first() == bytes::<32>(sig).ok().as_ref()
    }
    fn topic_addr(&self, i: usize) -> Check<[u8; 20]> {
        address(*self.topics.get(i).ok_or("missing address topic")?)
    }
}
#[derive(Debug, Clone)]
struct Move {
    token: [u8; 32],
    units: u128,
    from: [u8; 20],
    to: [u8; 20],
    index: u64,
}
fn transfers(l: &Log) -> Check<Vec<Move>> {
    if !l.is(SINGLE) && !l.is(BATCH) {
        return Ok(Vec::new());
    }
    if l.topics.len() != 4 {
        return Err("invalid ERC1155 topics".into());
    }
    let (from, to) = (l.topic_addr(2)?, l.topic_addr(3)?);
    let pairs = if l.is(SINGLE) {
        if l.data.len() != 64 {
            return Err("invalid TransferSingle length".into());
        }
        vec![(word(&l.data, 0)?, uint(word(&l.data, 1)?)?)]
    } else {
        // Require canonical two-array ABI, with a hard bound independent of RPC input.
        if uint(word(&l.data, 0)?)? != 64 {
            return Err("invalid batch ids offset".into());
        }
        let n = usize::try_from(uint(word(&l.data, 2)?)?).map_err(|_| "batch count overflow")?;
        if n == 0 || n > 256 {
            return Err("unsupported batch length".into());
        }
        if uint(word(&l.data, 1)?)? != ((3 + n) * 32) as u128
            || uint(word(&l.data, 3 + n)?)? != n as u128
            || l.data.len() != (4 + 2 * n) * 32
        {
            return Err("invalid TransferBatch layout".into());
        }
        (0..n)
            .map(|i| Ok((word(&l.data, 3 + i)?, uint(word(&l.data, 4 + n + i)?)?)))
            .collect::<Check<Vec<_>>>()?
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (token, units) in pairs {
        if !seen.insert(token) {
            return Err("duplicate token in transfer".into());
        }
        if units > 0 {
            out.push(Move {
                token,
                units,
                from,
                to,
                index: l.index,
            });
        }
    }
    Ok(out)
}
fn ctf_payout(l: &Log, ctx: &RedemptionContext) -> Check<(u128, Vec<usize>)> {
    if l.topics.len() != 4
        || l.topics[3] != [0; 32]
        || word(&l.data, 0)? != ctx.condition
        || uint(word(&l.data, 1)?)? != 96
    {
        return Err("unsupported redemption shape".into());
    }
    let n = usize::try_from(uint(word(&l.data, 3)?)?).map_err(|_| "index count overflow")?;
    if !(1..=2).contains(&n) || l.data.len() != (4 + n) * 32 {
        return Err("unsupported redemption index sets".into());
    }
    let mut indices = Vec::new();
    for i in 0..n {
        let idx = match uint(word(&l.data, 4 + i)?)? {
            1 => 0,
            2 => 1,
            _ => return Err("nonbinary redemption".into()),
        };
        if indices.contains(&idx) {
            return Err("duplicate redemption index".into());
        }
        indices.push(idx);
    }
    Ok((uint(word(&l.data, 2)?)?, indices))
}
fn cash_received(logs: &[Log], emitter: [u8; 20], from: [u8; 20], to: [u8; 20]) -> Check<u128> {
    let mut total = 0u128;
    for l in logs
        .iter()
        .filter(|l| l.emitter == emitter && l.is(TRANSFER))
    {
        if l.topics.len() != 3 || l.data.len() != 32 {
            return Err("invalid collateral transfer".into());
        }
        if l.topic_addr(1)? == from && l.topic_addr(2)? == to {
            total = total
                .checked_add(uint(word(&l.data, 0)?)?)
                .ok_or("collateral sum overflow")?;
        }
    }
    Ok(total)
}

/// Verify one wallet/condition redemption within a possibly multi-wallet receipt.
/// Unsupported routes or multiple matching redemption calls fail closed. The
/// receipt contains no timestamp; obtain it from its matching block separately.
pub fn verify_receipt(body: &Value, ctx: &RedemptionContext) -> Check<RedemptionEvidence> {
    if body.get("error").is_some() {
        return Err("RPC receipt error".into());
    }
    let receipt = body.get("result").unwrap_or(body);
    let tx: [u8; 32] = bytes(&ctx.expected_tx)?;
    if quantity(&receipt["status"])? != 1
        || bytes::<32>(
            receipt["transactionHash"]
                .as_str()
                .ok_or("missing transaction hash")?,
        )? != tx
    {
        return Err("failed or mismatched receipt".into());
    }
    let block_number = quantity(&receipt["blockNumber"])?;
    let block: [u8; 32] = bytes(receipt["blockHash"].as_str().ok_or("missing block hash")?)?;
    if block_number == 0
        || block == [0; 32]
        || ctx.funder == [0; 20]
        || ctx.ctf == [0; 20]
        || ctx.denominator == 0
        || ctx.numerators[0].checked_add(ctx.numerators[1]) != Some(ctx.denominator)
    {
        return Err("invalid redemption context or block".into());
    }
    let tokens = [token_word(&ctx.tokens[0])?, token_word(&ctx.tokens[1])?];
    if tokens[0] == tokens[1] {
        return Err("duplicate context token".into());
    }
    let values = receipt["logs"].as_array().ok_or("missing receipt logs")?;
    if values.len() > 8192 {
        return Err("receipt exceeds log bound".into());
    }
    let mut logs = Vec::new();
    let mut seen = HashSet::new();
    for v in values {
        let index = quantity(&v["logIndex"])?;
        if !seen.insert(index)
            || v["removed"].as_bool() == Some(true)
            || bytes::<32>(v["transactionHash"].as_str().ok_or("log missing tx hash")?)? != tx
            || quantity(&v["blockNumber"])? != block_number
            || bytes::<32>(v["blockHash"].as_str().ok_or("log missing block hash")?)? != block
        {
            return Err("duplicate, removed or mismatched log".into());
        }
        let topics = v["topics"]
            .as_array()
            .ok_or("missing topics")?
            .iter()
            .map(|t| bytes::<32>(t.as_str().ok_or("invalid topic")?))
            .collect::<Check<Vec<_>>>()?;
        if topics.len() > 4 {
            return Err("too many topics".into());
        }
        let data = hex::decode(
            v["data"]
                .as_str()
                .and_then(|s| s.strip_prefix("0x"))
                .ok_or("missing log data")?,
        )
        .map_err(|_| "invalid log data")?;
        logs.push(Log {
            index,
            emitter: bytes(v["address"].as_str().ok_or("missing emitter")?)?,
            topics,
            data,
        });
    }
    logs.sort_by_key(|l| l.index);
    let legacy = bytes::<20>(LEGACY_ADAPTER)?;
    let wrapper = bytes::<20>(REDEEM_WRAPPER)?;
    // A terminal event identifies the wallet even when a relayer batches users.
    let mut candidates = Vec::new();
    for (i, l) in logs.iter().enumerate() {
        let direct = l.emitter == ctx.ctf
            && l.is(PAYOUT)
            && l.topic_addr(1)? == ctx.funder
            && word(&l.data, 0)? == ctx.condition;
        let adapted = ((l.emitter == wrapper && l.is(WRAPPER_PAYOUT))
            || (l.emitter == legacy && l.is(LEGACY_PAYOUT)))
            && l.topic_addr(1)? == ctx.funder
            && l.topics.get(2) == Some(&ctx.condition);
        if direct || adapted {
            candidates.push((i, direct));
        }
    }
    if candidates.len() != 1 {
        return Err("missing or ambiguous wallet redemption".into());
    }
    let (end, direct) = candidates[0];
    let terminal = &logs[end];
    let start = logs[..end]
        .iter()
        .rposition(|l| l.emitter == terminal.emitter && l.topics.first() == terminal.topics.first())
        .map(|i| i + 1)
        .unwrap_or(0);
    let segment = &logs[start..=end];
    let payouts: Vec<_> = segment
        .iter()
        .filter(|l| l.emitter == ctx.ctf && l.is(PAYOUT))
        .collect();
    if payouts.len() != 1 {
        return Err("ambiguous CTF redemption segment".into());
    }
    let payout_log = payouts[0];
    let (payout, indices) = ctf_payout(payout_log, ctx)?;
    let redeemer = payout_log.topic_addr(1)?;
    let collateral = payout_log.topic_addr(2)?;
    let zero = [0u8; 20];
    if direct {
        if collateral != crate::merge::USDC && collateral != crate::merge::COLLATERAL_PUSD {
            return Err("unsupported direct collateral".into());
        }
    } else if redeemer != legacy || collateral != bytes::<20>(WRAPPED_USDC)? {
        return Err("unsupported adapter redemption".into());
    }
    let mut moves = Vec::new();
    for l in segment.iter().filter(|l| l.emitter == ctx.ctf) {
        moves.extend(transfers(l)?);
    }
    let root = if direct { ctx.funder } else { terminal.emitter };
    let mut evidence = Vec::new();
    let mut calculated = 0u128;
    for idx in indices {
        let token_moves: Vec<_> = moves.iter().filter(|m| m.token == tokens[idx]).collect();
        let burns: Vec<_> = token_moves
            .iter()
            .filter(|m| m.from == redeemer && m.to == zero && m.index < payout_log.index)
            .copied()
            .collect();
        let units = burns.iter().try_fold(0u128, |s, m| {
            s.checked_add(m.units).ok_or("burn sum overflow")
        })?;
        if units == 0 {
            continue;
        }
        // Exact transfer chain excludes another user's burns and adapter inventory.
        let sum = |from, to| -> Check<u128> {
            token_moves
                .iter()
                .filter(|m| m.from == from && m.to == to && m.index < burns[0].index)
                .try_fold(0u128, |s, m| {
                    s.checked_add(m.units)
                        .ok_or_else(|| "transfer sum overflow".into())
                })
        };
        if !direct
            && (sum(ctx.funder, root)? != units || (root != legacy && sum(root, legacy)? != units))
        {
            return Err("adapter burn is not covered by wallet transfers".into());
        }
        let allowed = |m: &&Move| {
            m.from == redeemer && m.to == zero
                || (!direct && m.from == ctx.funder && m.to == root)
                || (!direct && root != legacy && m.from == root && m.to == legacy)
        };
        if token_moves.iter().any(|m| !allowed(m)) {
            return Err("ambiguous token movements in redemption".into());
        }
        let paid = units
            .checked_mul(ctx.numerators[idx])
            .ok_or("payout multiplication overflow")?
            / ctx.denominator;
        calculated = calculated.checked_add(paid).ok_or("payout sum overflow")?;
        evidence.push(TokenRedemption {
            token: ctx.tokens[idx].clone(),
            units,
            payout_units: paid,
            burn_log_indices: burns.iter().map(|m| m.index).collect(),
        });
    }
    if evidence.is_empty() || calculated != payout {
        return Err("burn quantities disagree with final payout".into());
    }
    if direct {
        if cash_received(segment, collateral, ctx.ctf, ctx.funder)? != payout {
            return Err("direct payout not delivered".into());
        }
    } else {
        let terminal_amount = if terminal.emitter == wrapper {
            if terminal.topics.len() != 3 || terminal.data.len() != 32 {
                return Err("invalid wrapper event".into());
            }
            uint(word(&terminal.data, 0)?)?
        } else {
            if terminal.topics.len() != 3
                || terminal.data.len() != 160
                || uint(word(&terminal.data, 0)?)? != 64
                || uint(word(&terminal.data, 2)?)? != 2
            {
                return Err("invalid adapter event".into());
            }
            uint(word(&terminal.data, 1)?)?
        };
        if terminal_amount != payout {
            return Err("adapter payout disagreement".into());
        }
        let received = if terminal.emitter == wrapper {
            cash_received(segment, crate::merge::COLLATERAL_PUSD, zero, ctx.funder)?
        } else {
            cash_received(
                segment,
                crate::merge::USDC,
                bytes::<20>(WRAPPED_USDC)?,
                ctx.funder,
            )?
        };
        if received != payout {
            return Err("adapter payout not delivered to wallet".into());
        }
    }
    Ok(RedemptionEvidence {
        tx: format!("0x{}", hex::encode(tx)),
        block_number,
        block_hash: format!("0x{}", hex::encode(block)),
        redemption_log_index: payout_log.index,
        tokens: evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn ctx() -> RedemptionContext {
        RedemptionContext {
            expected_tx: format!("0x{}", "11".repeat(32)),
            funder: [7; 20],
            ctf: crate::merge::CTF,
            condition: [9; 32],
            tokens: ["101".into(), "102".into()],
            numerators: [0, 1],
            denominator: 1,
        }
    }
    fn wa(a: [u8; 20]) -> [u8; 32] {
        let mut w = [0; 32];
        w[12..].copy_from_slice(&a);
        w
    }
    fn wu(n: u128) -> [u8; 32] {
        let mut w = [0; 32];
        w[16..].copy_from_slice(&n.to_be_bytes());
        w
    }
    fn log(
        c: &RedemptionContext,
        emitter: [u8; 20],
        sig: &str,
        topics: Vec<[u8; 32]>,
        data: Vec<[u8; 32]>,
        i: u64,
    ) -> Value {
        let mut t = vec![format!("0x{sig}")];
        t.extend(topics.iter().map(|w| format!("0x{}", hex::encode(w))));
        json!({"address":format!("0x{}",hex::encode(emitter)),"topics":t,"data":format!("0x{}",hex::encode(data.concat())),"transactionHash":c.expected_tx,"blockHash":format!("0x{}","22".repeat(32)),"blockNumber":"0x10","logIndex":format!("0x{i:x}"),"removed":false})
    }
    fn receipt(c: &RedemptionContext, logs: Vec<Value>) -> Value {
        json!({"status":"0x1","transactionHash":c.expected_tx,"blockNumber":"0x10","blockHash":format!("0x{}","22".repeat(32)),"logs":logs})
    }
    fn burn(c: &RedemptionContext, from: [u8; 20], token: u128, units: u128, i: u64) -> Value {
        log(
            c,
            c.ctf,
            SINGLE,
            vec![wa(from), wa(from), wa([0; 20])],
            vec![wu(token), wu(units)],
            i,
        )
    }
    fn batch(c: &RedemptionContext, from: [u8; 20], to: [u8; 20], q: [u128; 2], i: u64) -> Value {
        log(
            c,
            c.ctf,
            BATCH,
            vec![wa(to), wa(from), wa(to)],
            vec![
                wu(64),
                wu(160),
                wu(2),
                wu(101),
                wu(102),
                wu(2),
                wu(q[0]),
                wu(q[1]),
            ],
            i,
        )
    }
    fn cash(
        c: &RedemptionContext,
        token: [u8; 20],
        from: [u8; 20],
        to: [u8; 20],
        n: u128,
        i: u64,
    ) -> Value {
        log(c, token, TRANSFER, vec![wa(from), wa(to)], vec![wu(n)], i)
    }
    fn payout(
        c: &RedemptionContext,
        who: [u8; 20],
        collateral: [u8; 20],
        n: u128,
        i: u64,
    ) -> Value {
        log(
            c,
            c.ctf,
            PAYOUT,
            vec![wa(who), wa(collateral), [0; 32]],
            vec![c.condition, wu(96), wu(n), wu(2), wu(1), wu(2)],
            i,
        )
    }
    fn direct(c: &RedemptionContext) -> Value {
        receipt(
            c,
            vec![
                burn(c, c.funder, 101, 6_000_000, 1),
                burn(c, c.funder, 102, 10_000_000, 2),
                cash(c, crate::merge::USDC, c.ctf, c.funder, 10_000_000, 3),
                payout(c, c.funder, crate::merge::USDC, 10_000_000, 4),
            ],
        )
    }
    fn adapter(c: &RedemptionContext) -> Value {
        let a = bytes::<20>(REDEEM_WRAPPER).unwrap();
        let b = bytes::<20>(LEGACY_ADAPTER).unwrap();
        let collateral = bytes::<20>(WRAPPED_USDC).unwrap();
        receipt(
            c,
            vec![
                batch(c, c.funder, a, [6_000_000, 10_000_000], 1),
                batch(c, a, b, [6_000_000, 10_000_000], 2),
                burn(c, b, 101, 6_000_000, 3),
                burn(c, b, 102, 10_000_000, 4),
                cash(c, collateral, c.ctf, b, 10_000_000, 5),
                payout(c, b, collateral, 10_000_000, 6),
                cash(
                    c,
                    crate::merge::COLLATERAL_PUSD,
                    [0; 20],
                    c.funder,
                    10_000_000,
                    7,
                ),
                log(
                    c,
                    a,
                    WRAPPER_PAYOUT,
                    vec![wa(c.funder), c.condition],
                    vec![wu(10_000_000)],
                    8,
                ),
            ],
        )
    }
    #[test]
    fn proves_both_winning_and_losing_burns_and_returns_durable_identity() {
        let c = ctx();
        let e = verify_receipt(&direct(&c), &c).unwrap();
        assert_eq!(e.redemption_log_index, 4);
        assert_eq!(e.block_number, 16);
        assert_eq!(e.tokens.len(), 2);
        assert_eq!(e.tokens[0].units, 6_000_000);
        assert_eq!(e.tokens[0].payout_units, 0);
        assert_eq!(e.tokens[1].payout_units, 10_000_000);
        assert_eq!(e.tokens[1].burn_log_indices, vec![2]);
        let mut split = ctx();
        split.numerators = [1, 1];
        split.denominator = 2;
        let mut r = direct(&split);
        r["logs"][2]["data"] = json!(format!("0x{}", hex::encode(wu(8_000_000))));
        r["logs"][3] = payout(&split, split.funder, crate::merge::USDC, 8_000_000, 4);
        assert_eq!(
            verify_receipt(&r, &split).unwrap().tokens[0].payout_units,
            3_000_000
        );
    }
    #[test]
    fn adapter_requires_complete_wallet_transfer_burn_and_cash_path() {
        let c = ctx();
        assert_eq!(
            verify_receipt(&adapter(&c), &c).unwrap().tokens[1].units,
            10_000_000
        );
        for remove in [0, 1, 3, 5, 6, 7] {
            let mut r = adapter(&c);
            r["logs"].as_array_mut().unwrap().remove(remove);
            assert!(verify_receipt(&r, &c).is_err(), "removed {remove}");
        }
        let mut r = adapter(&c);
        r["logs"][0]["topics"][2] = json!(format!("0x{}", hex::encode(wa([8; 20]))));
        assert!(verify_receipt(&r, &c).is_err());
    }
    #[test]
    fn batched_other_wallet_redemption_cannot_supply_our_burns_or_cash() {
        let c = ctx();
        let mut other = ctx();
        other.funder = [8; 20];
        let mut r = adapter(&other);
        let ours = adapter(&c);
        for mut l in ours["logs"].as_array().unwrap().clone() {
            l["logIndex"] = json!(format!("0x{:x}", quantity(&l["logIndex"]).unwrap() + 8));
            r["logs"].as_array_mut().unwrap().push(l);
        }
        assert_eq!(verify_receipt(&r, &c).unwrap().tokens[1].units, 10_000_000);
        // Remove only our payout delivery; another wallet's equal payout is not proof.
        r["logs"].as_array_mut().unwrap().remove(14);
        assert!(verify_receipt(&r, &c).is_err());
    }
    #[test]
    fn rejected_receipts_do_not_become_redemption_proof() {
        let c = ctx();
        for change in 0..8 {
            let mut r = direct(&c);
            match change {
                0 => r["status"] = json!("0x0"),
                1 => r["transactionHash"] = json!(format!("0x{}", "33".repeat(32))),
                2 => r["logs"][0]["removed"] = json!(true),
                3 => r["logs"][0]["blockHash"] = json!(format!("0x{}", "33".repeat(32))),
                4 => r["logs"][0]["logIndex"] = r["logs"][1]["logIndex"].clone(),
                5 => {
                    r["logs"].as_array_mut().unwrap().pop();
                }
                6 => r["logs"][3]["topics"][1] = json!(format!("0x{}", hex::encode(wa([8; 20])))),
                _ => r["logs"][0]["data"] = json!("0x01"),
            }
            assert!(verify_receipt(&r, &c).is_err(), "change {change}");
        }
    }
    #[test]
    fn legacy_adapter_and_zero_payout_redemptions_are_proven() {
        let c = ctx();
        let a = bytes::<20>(LEGACY_ADAPTER).unwrap();
        let collateral = bytes::<20>(WRAPPED_USDC).unwrap();
        let r = receipt(
            &c,
            vec![
                batch(&c, c.funder, a, [6_000_000, 10_000_000], 1),
                burn(&c, a, 101, 6_000_000, 2),
                burn(&c, a, 102, 10_000_000, 3),
                cash(&c, collateral, c.ctf, a, 10_000_000, 4),
                payout(&c, a, collateral, 10_000_000, 5),
                cash(&c, crate::merge::USDC, collateral, c.funder, 10_000_000, 6),
                log(
                    &c,
                    a,
                    LEGACY_PAYOUT,
                    vec![wa(c.funder), c.condition],
                    vec![wu(64), wu(10_000_000), wu(2), wu(6_000_000), wu(10_000_000)],
                    7,
                ),
            ],
        );
        assert_eq!(verify_receipt(&r, &c).unwrap().tokens.len(), 2);
        let loser = receipt(
            &c,
            vec![
                burn(&c, c.funder, 101, 6_000_000, 1),
                payout(&c, c.funder, crate::merge::USDC, 0, 2),
            ],
        );
        let e = verify_receipt(&loser, &c).unwrap();
        assert_eq!(e.tokens.len(), 1);
        assert_eq!(e.tokens[0].payout_units, 0);
    }

    #[test]
    fn rejects_malformed_batch_and_overflow_without_panicking() {
        let c = ctx();
        for bad in [0u128, 32, 128, u128::MAX] {
            let mut r = adapter(&c);
            let mut data = hex::decode(
                r["logs"][0]["data"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("0x"),
            )
            .unwrap();
            data[..32].copy_from_slice(&wu(bad));
            r["logs"][0]["data"] = json!(format!("0x{}", hex::encode(data)));
            assert!(verify_receipt(&r, &c).is_err());
        }
        let mut c = ctx();
        c.tokens[0] = "9".repeat(78);
        assert!(verify_receipt(&direct(&c), &c).is_err());
    }
}
