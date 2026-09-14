const F_MAKER: usize = 1;
const F_TOKEN: usize = 3;
const F_MAKER_AMT: usize = 4;
const F_TAKER_AMT: usize = 5;
const F_SIDE: usize = 6;
pub const MATCH_SELECTOR: [u8; 4] = [0x3c, 0x2b, 0x43, 0x99];
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub condition_id: [u8; 32],
    pub token_id: String,
    pub side: u8,
    pub price: f64,
    pub order_size: f64,
    pub fill_size: f64,
    pub role: &'static str,
    pub salt: [u8; 32],
    pub occurrence: u16,
}
pub const TAKER_OCCURRENCE: u16 = u16::MAX;
#[inline]
fn word(b: &[u8], off: usize) -> Option<&[u8]> {
    b.get(off..off.checked_add(32)?)
}
#[inline]
fn word_usize(b: &[u8], off: usize) -> Option<usize> {
    let w = word(b, off)?;
    if w[..24].iter().any(|&x| x != 0) {
        return None;
    }
    let mut v = 0usize;
    for &byte in &w[24..] {
        v = v.checked_mul(256)?.checked_add(byte as usize)?;
    }
    Some(v)
}
#[inline]
fn word_u128(b: &[u8], off: usize) -> Option<u128> {
    let w = word(b, off)?;
    if w[..16].iter().any(|&x| x != 0) {
        return None;
    }
    let mut v = [0u8; 16];
    v.copy_from_slice(&w[16..]);
    Some(u128::from_be_bytes(v))
}
fn word_dec_string(b: &[u8], off: usize) -> Option<String> {
    let w = word(b, off)?;
    let mut digits: Vec<u8> = Vec::with_capacity(78);
    let mut acc = w.to_vec();
    if acc.iter().all(|&x| x == 0) {
        return Some("0".into());
    }
    while acc.iter().any(|&x| x != 0) {
        let mut rem = 0u32;
        for byte in acc.iter_mut() {
            let cur = rem * 256 + *byte as u32;
            *byte = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    digits.reverse();
    String::from_utf8(digits).ok()
}
#[inline]
fn addr_matches(b: &[u8], off: usize, wallet_20: &[u8; 20]) -> bool {
    match word(b, off) {
        Some(w) => w[..12].iter().all(|&x| x == 0) && &w[12..] == wallet_20,
        None => false,
    }
}
fn shares_and_price(
    b: &[u8],
    ord: usize,
    fill_raw: u128,
) -> Option<(f64, f64, f64, u8)> {
    let maker_amt = word_u128(b, ord + F_MAKER_AMT * 32)?;
    let taker_amt = word_u128(b, ord + F_TAKER_AMT * 32)?;
    let side = u8::try_from(word_u128(b, ord + F_SIDE * 32)?).ok()?;
    if maker_amt == 0 || taker_amt == 0 || side > 1 {
        return None;
    }
    let (ma, ta) = (maker_amt as f64, taker_amt as f64);
    let price = if side == 0 { ma / ta } else { ta / ma };
    if !(price > 0.0 && price < 1.0) {
        return None;
    }
    let order_size = (if side == 0 { ta } else { ma }) / 1e6;
    if fill_raw > maker_amt { return None; }
    let taking = fill_raw.checked_mul(taker_amt)? / maker_amt;
    let (shares, cash) = if side == 0 { (taking, fill_raw) } else { (fill_raw, taking) };
    if shares == 0 || cash == 0 { return None; }
    Some((shares as f64 / 1e6, cash as f64 / shares as f64, order_size, side))
}
pub fn participants(calldata: &[u8]) -> Vec<[u8; 20]> {
    let mut out = Vec::new();
    if calldata.len() < 4 + 32 * 7 || calldata[..4] != MATCH_SELECTOR {
        return out;
    }
    let b = &calldata[4..];
    let mut push = |off: usize| {
        if let Some(w) = word(b, off) {
            if w[..12].iter().all(|&x| x == 0) {
                let mut a = [0u8; 20];
                a.copy_from_slice(&w[12..]);
                out.push(a);
            }
        }
    };
    if let Some(t) = word_usize(b, 32) {
        if t <= b.len() { push(t + F_MAKER * 32); }
    }
    if let Some(mo) = word_usize(b, 64) {
        if let Some(n) = word_usize(b, mo) {
            if n <= 512 {
                let Some(body) = mo.checked_add(32) else { return out };
                if body > b.len() { return out; }
                for i in 0..n {
                    if let Some(rel) = word_usize(b, body + i * 32) {
                        if rel <= b.len() - body { push(body + rel + F_MAKER * 32); }
                    }
                }
            }
        }
    }
    out
}
pub fn decode_all(calldata: &[u8], wallet_20: &[u8; 20]) -> Vec<Decoded> {
    match decode_all_inner(calldata, wallet_20) {
        Some(mut v) => {
            // Net pre-fee asset flows, including mixed mint/merge legs and
            // unused taker making amounts refunded by the exchange.
            let b = &calldata[4..];
            let execution = taker_execution(b);
            v.retain_mut(|d| {
                if d.role != "taker" { return true; }
                let Some((shares, cash)) = execution else { return false; };
                d.fill_size = shares as f64 / 1e6;
                d.price = cash as f64 / shares as f64;
                d.price > 0.0 && d.price < 1.0
            });
            v
        },
        None => Vec::new(),
    }
}
fn decode_all_inner(calldata: &[u8], wallet_20: &[u8; 20]) -> Option<Vec<Decoded>> {
    if calldata.len() < 4 + 32 * 7 || calldata[..4] != MATCH_SELECTOR {
        return None;
    }
    let b = &calldata[4..];
    let mut condition_id = [0u8; 32];
    condition_id.copy_from_slice(word(b, 0)?);
    if condition_id.iter().all(|&x| x == 0) {
        return None;
    }
    let taker_order_off = word_usize(b, 32)?;
    let maker_orders_off = word_usize(b, 64)?;
    let taker_fill = word_u128(b, 96)?;
    let maker_fills_off = word_usize(b, 128)?;
    let n_fills = word_usize(b, maker_fills_off)?;
    let n_makers = word_usize(b, maker_orders_off)?;
    if n_makers == 0 || n_makers > 512 || n_fills != n_makers || taker_order_off > b.len() || maker_orders_off > b.len() {
        return None;
    }
    let makers_body = maker_orders_off + 32;
    let mut out: Vec<Decoded> = Vec::new();
    let push = |
        ord: usize,
        fill_raw: u128,
        role: &'static str,
        occurrence: u16,
        out: &mut Vec<Decoded>,
    | -> Option<()> {
        if fill_raw == 0 {
            return None;
        }
        let (size, price, order_size, side) = shares_and_price(b, ord, fill_raw)?;
        let mut salt = [0u8; 32];
        salt.copy_from_slice(word(b, ord)?);
        let token_id = word_dec_string(b, ord + F_TOKEN * 32)?;
        if token_id.len() < 6 {
            return None;
        }
        if size <= 0.0 || size > order_size * 1.001 {
            return None;
        }
        if salt.iter().all(|&x| x == 0) {
            return None;
        }
        out.push(Decoded {
            condition_id,
            token_id,
            side,
            price,
            order_size,
            fill_size: size,
            role,
            salt,
            occurrence,
        });
        Some(())
    };
    for i in 0..n_makers {
        let Some(rel) = word_usize(b, makers_body + i * 32) else { continue };
        if rel > b.len().saturating_sub(makers_body) { continue; }
        let ord = makers_body + rel;
        if !addr_matches(b, ord + F_MAKER * 32, wallet_20) {
            continue;
        }
        if i >= n_fills {
            continue;
        }
        let Some(fill) = word_u128(b, maker_fills_off + 32 + i * 32) else { continue };
        let _ = push(ord, fill, "maker", i as u16, &mut out);
    }
    if addr_matches(b, taker_order_off + F_MAKER * 32, wallet_20) {
        let _ = push(taker_order_off, taker_fill, "taker", TAKER_OCCURRENCE, &mut out);
    }
    Some(out)
}
fn taker_execution(b: &[u8]) -> Option<(u128, u128)> {
    let taker = word_usize(b, 32)?;
    if taker > b.len() { return None; }
    let side = word_u128(b, taker + F_SIDE * 32)?;
    if side > 1 { return None; }
    let token = word(b, taker + F_TOKEN * 32)?;
    let makers = word_usize(b, 64)?;
    let fills = word_usize(b, 128)?;
    let n = word_usize(b, makers)?;
    if n == 0 || n > 512 || word_usize(b, fills)? != n { return None; }
    let body = makers.checked_add(32)?;
    if body > b.len() { return None; }
    let mut making = 0u128;
    let mut taking = 0u128;
    let mut mixed = false;
    let mut other_token: Option<&[u8]> = None;
    for i in 0..n {
        let rel = word_usize(b, body + i * 32)?;
        if rel > b.len() - body { return None; }
        let o = body + rel;
        let maker_side = word_u128(b, o + F_SIDE * 32)?;
        let maker_token = word(b, o + F_TOKEN * 32)?;
        if maker_side > 1 { return None; }
        let ma = word_u128(b, o + F_MAKER_AMT * 32)?;
        let ta = word_u128(b, o + F_TAKER_AMT * 32)?;
        let f = word_u128(b, fills.checked_add(32+i*32)?)?;
        if ma == 0 || ta == 0 || f == 0 || f > ma { return None; }
        let mt = f.checked_mul(ta)? / ma;
        let (leg_making, leg_taking) = if maker_side != side {
            if maker_token != token { return None; }
            (mt, f)
        } else {
            if maker_token == token || other_token.is_some_and(|t| t != maker_token) { return None; }
            other_token = Some(maker_token);
            mixed = true;
            if side == 0 {
                // Mint mt pairs; maker supplies f cash and receives mt of
                // the other token. Taker supplies mt-f and receives mt.
                (mt.checked_sub(f)?, mt)
            } else {
                // Merge f pairs; maker supplies f other-token shares and
                // receives mt cash. Taker supplies f and receives f-mt.
                (f, f.checked_sub(mt)?)
            }
        };
        making = making.checked_add(leg_making)?;
        taking = taking.checked_add(leg_taking)?;
    }
    if making == 0 || taking == 0 || making > word_u128(b,96)? { return None; }
    let ma = word_u128(b,taker+F_MAKER_AMT*32)?;
    let ta = word_u128(b,taker+F_TAKER_AMT*32)?;
    // Mixed settlement checks the signed minimum against the submitted fill
    // before refunding unused making assets; complementary checks actual use.
    let checked_making = if mixed { word_u128(b,96)? } else { making };
    if ma == 0 || checked_making > ma || taking < checked_making.checked_mul(ta)? / ma { return None; }
    Some(if side == 0 { (taking,making) } else { (making,taking) })
}

/// Identity-only observations never authorize execution. Kept separate so a
/// safely identified but unsupported leg can still get a contemporaneous book.
pub fn identities(calldata: &[u8], wallet: &[u8;20]) -> Vec<([u8;32],String,u8)> {
    let mut out=Vec::new();
    if calldata.len()<228 || calldata[..4]!=MATCH_SELECTOR { return out; }
    let b=&calldata[4..];
    let condition: [u8;32]=b[..32].try_into().unwrap();
    if condition==[0;32] { return out; }
    let mut offsets=Vec::new();
    if let Some(t)=word_usize(b,32) { offsets.push(t); }
    if let Some(m)=word_usize(b,64) {
        if let Some(n)=word_usize(b,m) {
            if n<=512 {
                if let Some(body)=m.checked_add(32).filter(|v| *v<=b.len()) {
                    for i in 0..n {
                        if let Some(rel)=word_usize(b,body+i*32) {
                            if rel<=b.len()-body { offsets.push(body+rel); }
                        }
                    }
                }
            }
        }
    }
    for o in offsets {
        if o>b.len() || !addr_matches(b,o+32,wallet) { continue; }
        let (Some(token),Some(side))=(word_dec_string(b,o+96),word_u128(b,o+192)) else { continue; };
        if side<=1 && token!="0" {
            let v=(condition,token,side as u8);
            if !out.contains(&v) { out.push(v); }
        }
    }
    out
}
pub fn decode_pending(calldata: &[u8], wallet_20: &[u8; 20]) -> Option<Decoded> {
    let mut all = decode_all(calldata, wallet_20);
    if all.len() == 1 { all.pop() } else { None }
}

#[cfg(test)]
mod execution_tests {
    use super::*;
    fn w(n:u128)->Vec<u8>{let mut a=vec![0;16];a.extend(n.to_be_bytes());a}
    fn order(wallet:u8,side:u128,ma:u128,ta:u128)->Vec<u8>{
        let mut o=w(7);
        for _ in 0..2{o.extend([0u8;12]);o.extend([wallet;20]);}
        for n in [123456789,ma,ta,side,0,1,0,0,384]{o.extend(w(n));}
        o.extend(w(0));o
    }
    fn fixture(side:u128, maker_side:u128, ma:u128,ta:u128,mma:u128,mta:u128,tf:u128,mf:u128)->Vec<u8>{
        let taker=order(0x11,side,ma,ta);let maker=order(0x22,maker_side,mma,mta);
        let mut makers=w(1);makers.extend(w(32));makers.extend(maker);
        let mut fills=w(1);fills.extend(w(mf));let mut fees=w(1);fees.extend(w(0));
        let mut b=MATCH_SELECTOR.to_vec();b.extend(w(1));b.extend(w(224));b.extend(w((224+taker.len())as u128));b.extend(w(tf));
        b.extend(w((224+taker.len()+makers.len())as u128));b.extend(w(0));b.extend(w((224+taker.len()+makers.len()+fills.len())as u128));
        b.extend(taker);b.extend(makers);b.extend(fills);b.extend(fees);b
    }
    #[test] fn taker_buy_price_improvement(){
        let b=fixture(0,1,80_000_000,100_000_000,100_000_000,50_000_000,50_000_000,100_000_000);
        let d=decode_all(&b,&[0x11;20]);assert_eq!(d.len(),1);assert_eq!(d[0].price,0.5);assert_eq!(d[0].fill_size,100.0);
        let m=decode_all(&b,&[0x22;20]);assert_eq!(m[0].price,0.5);assert_eq!(m[0].fill_size,100.0);
    }
    #[test] fn taker_sell_actual_maker_implied_consumption(){
        let b=fixture(1,0,100_000_000,20_000_000,50_000_000,100_000_000,100_000_000,50_000_000);
        let d=decode_all(&b,&[0x11;20]);assert_eq!(d[0].price,0.5);assert_eq!(d[0].fill_size,100.0);
    }
    #[test] fn unsigned_buy_minimum_is_not_maximum_shares(){
        let b=fixture(0,1,80_000_000,100_000_000,160_000_000,80_000_000,80_000_000,160_000_000);
        let d=decode_all(&b,&[0x11;20]);assert_eq!(d[0].fill_size,160.0);
    }
    #[test] fn unsupported_mixed_taker_not_reported_as_limit_execution(){
        let b=fixture(0,0,50_000_000,100_000_000,50_000_000,100_000_000,50_000_000,50_000_000);
        assert!(decode_all(&b,&[0x11;20]).is_empty());
        assert_eq!(identities(&b,&[0x11;20]).len(),1);
    }
    fn other_token(mut b:Vec<u8>)->Vec<u8>{
        let mo=word_usize(&b[4..],64).unwrap();
        let o=4+mo+32+word_usize(&b[4..],mo+32).unwrap();
        b[o+96..o+128].copy_from_slice(&w(987654321));b
    }
    #[test] fn mint_buy_uses_net_cash_and_refund(){
        // Spend 30 of the 40 submitted; maker funds the other 70 of 100 pairs.
        let b=other_token(fixture(0,0,50_000_000,100_000_000,70_000_000,100_000_000,40_000_000,70_000_000));
        let d=decode_all(&b,&[0x11;20]);assert_eq!(d.len(),1);assert_eq!(d[0].price,0.3);assert_eq!(d[0].fill_size,100.0);
    }
    #[test] fn merge_sell_uses_net_collateral_proceeds(){
        let b=other_token(fixture(1,1,100_000_000,20_000_000,100_000_000,60_000_000,100_000_000,100_000_000));
        let d=decode_all(&b,&[0x11;20]);assert_eq!(d.len(),1);assert_eq!(d[0].price,0.4);assert_eq!(d[0].fill_size,100.0);
    }
    #[test] fn mixed_integer_rounding_and_invalid_budget(){
        let b=other_token(fixture(0,0,10,20,3,10,8,1));
        let d=decode_all(&b,&[0x11;20]);assert!(d.is_empty()); // minimum from submitted fill fails
        let b=other_token(fixture(0,0,9,10,3,10,2,1));
        let d=decode_all(&b,&[0x11;20]);assert_eq!(d[0].fill_size,0.000003);assert_eq!(d[0].price,2.0/3.0);
        let b=other_token(fixture(0,0,9,10,3,10,1,1));assert!(decode_all(&b,&[0x11;20]).is_empty());
    }
    #[test] fn malformed_offsets_and_enum_are_rejected(){
        let mut b=fixture(0,1,80_000_000,100_000_000,100_000_000,50_000_000,50_000_000,100_000_000);
        b[36..68].fill(255);assert!(decode_all(&b,&[0x11;20]).is_empty());let _=participants(&b);
        let b=fixture(256,1,80_000_000,100_000_000,100_000_000,50_000_000,50_000_000,100_000_000);
        assert!(decode_all(&b,&[0x11;20]).is_empty());
    }
}
