use tiny_keccak::{Hasher, Keccak};

// Wrapped collateral used by legacy negative-risk markets.
const WRAPPED_USDC: [u8; 20] = [
    0x3a, 0x3b, 0xd7, 0xbb, 0x95, 0x28, 0xe1, 0x59, 0x57, 0x7f, 0x7c, 0x2e, 0x68, 0x5c, 0xc8, 0x1a,
    0x76, 0x50, 0x02, 0xe2,
];

fn keccak(data: &[u8]) -> [u8; 32] {
    let mut hash = Keccak::v256();
    let mut result = [0; 32];
    hash.update(data);
    hash.finalize(&mut result);
    result
}

fn decimal_token(value: &str) -> Option<[u8; 32]> {
    if value.is_empty() || value.len() > 78 {
        return None;
    }
    let mut result = [0u8; 32];
    for digit in value.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        let mut carry = (digit - b'0') as u16;
        for byte in result.iter_mut().rev() {
            let next = *byte as u16 * 10 + carry;
            *byte = next as u8;
            carry = next >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    if result == [0; 32] {
        return None;
    }
    Some(result)
}

fn collection_call(condition: [u8; 32], index: usize) -> Option<String> {
    if condition == [0; 32] || index >= 256 {
        return None;
    }
    let mut calldata = Vec::with_capacity(100);
    calldata.extend_from_slice(&keccak(b"getCollectionId(bytes32,bytes32,uint256)")[..4]);
    calldata.extend_from_slice(&[0; 32]);
    calldata.extend_from_slice(&condition);
    let mut index_set = [0u8; 32];
    index_set[31 - index / 8] = 1 << (index % 8);
    calldata.extend_from_slice(&index_set);
    Some(format!("0x{}", hex::encode(calldata)))
}

fn collection_result(body: &serde_json::Value) -> Option<[u8; 32]> {
    if body.get("error").is_some()
        || body["jsonrpc"].as_str() != Some("2.0")
        || body["id"].as_u64() != Some(1)
    {
        return None;
    }
    let result = body["result"].as_str()?;
    if result.len() != 66 || !result.starts_with("0x") {
        return None;
    }
    let bytes: [u8; 32] = hex::decode(&result[2..]).ok()?.try_into().ok()?;
    if bytes == [0; 32] {
        return None;
    }
    Some(bytes)
}

fn matches_collection(token: &[u8; 32], collection: &[u8; 32]) -> bool {
    [
        crate::merge::USDC,
        crate::merge::COLLATERAL_PUSD,
        WRAPPED_USDC,
    ]
    .iter()
    .any(|collateral| {
        let mut packed = [0u8; 52];
        packed[..20].copy_from_slice(collateral);
        packed[20..].copy_from_slice(collection);
        keccak(&packed) == *token
    })
}

/// Verify an off-chain token mapping against CTF at a pinned finalized block.
/// The caller supplies the finalized block number; moving block tags are rejected.
/// This performs an eth_call only and fails closed on any malformed or unavailable result.
pub async fn verifies(
    http: &reqwest::Client,
    rpc: &str,
    ctf: &str,
    block: &str,
    condition: [u8; 32],
    index: usize,
    token: &str,
) -> bool {
    let Some(token) = decimal_token(token) else {
        return false;
    };
    let Some(data) = collection_call(condition, index) else {
        return false;
    };
    if ctf.len() != 42 || !ctf.starts_with("0x") || hex::decode(&ctf[2..]).is_err() {
        return false;
    }
    let Some(block_digits) = block.strip_prefix("0x") else {
        return false;
    };
    if block_digits.is_empty()
        || block_digits.len() > 16
        || (block_digits.len() > 1 && block_digits.starts_with('0'))
        || !block_digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    let response = match http
        .post(rpc)
        .timeout(std::time::Duration::from_secs(10))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_call",
            "params": [{"to": ctf, "data": data}, block]
        }))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        _ => return false,
    };
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return false;
    };
    let Some(collection) = collection_result(&body) else {
        return false;
    };
    matches_collection(&token, &collection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn decimal(bytes: &[u8; 32]) -> String {
        let mut digits = vec![0u16];
        for byte in bytes {
            let mut carry = *byte as u16;
            for digit in &mut digits {
                let next = *digit * 256 + carry;
                *digit = next % 10;
                carry = next / 10;
            }
            while carry > 0 {
                digits.push(carry % 10);
                carry /= 10;
            }
        }
        digits
            .into_iter()
            .rev()
            .map(|d| (b'0' + d as u8) as char)
            .collect()
    }

    fn position(collateral: &[u8; 20], collection: &[u8; 32]) -> [u8; 32] {
        let mut packed = collateral.to_vec();
        packed.extend_from_slice(collection);
        keccak(&packed)
    }

    fn rpc_server(
        status: u16,
        response: String,
    ) -> (String, std::thread::JoinHandle<serde_json::Value>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let (header_end, length) = loop {
                let mut buf = [0; 1024];
                let count = stream.read(&mut buf).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buf[..count]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while request.len() < header_end + length {
                let mut buf = [0; 1024];
                let count = stream.read(&mut buf).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buf[..count]);
            }
            write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            serde_json::from_slice(&request[header_end..header_end + length]).unwrap()
        });
        (url, server)
    }

    #[test]
    fn decimal_uint256_is_exact_and_rejects_overflow() {
        let max = decimal(&[255; 32]);
        assert_eq!(decimal_token(&max), Some([255; 32]));
        let mut overflow = max.into_bytes();
        // uint256::MAX ends in 5; incrementing the final digit forms 2^256.
        *overflow.last_mut().unwrap() += 1;
        assert_eq!(decimal_token(std::str::from_utf8(&overflow).unwrap()), None);
        for invalid in ["", "0", "-1", "+1", "1.0", " 12", "1e3"] {
            assert_eq!(decimal_token(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn all_supported_collaterals_bind_to_the_collection() {
        let collection = [0x42; 32];
        // Independent Python Keccak-256 vector for packed USDC + synthetic collection.
        let expected = [
            0xa4, 0xf6, 0xea, 0x52, 0x31, 0xce, 0xdd, 0xc8, 0xad, 0xe3, 0xc5, 0x7a, 0x49, 0xa3,
            0xeb, 0x4d, 0x56, 0x81, 0x7b, 0x5d, 0x05, 0x08, 0x74, 0x95, 0xaa, 0xff, 0x06, 0x03,
            0xd3, 0xd0, 0xe8, 0x98,
        ];
        assert_eq!(position(&crate::merge::USDC, &collection), expected);
        for collateral in [
            crate::merge::USDC,
            crate::merge::COLLATERAL_PUSD,
            WRAPPED_USDC,
        ] {
            let token = position(&collateral, &collection);
            assert!(matches_collection(&token, &collection));
            assert!(!matches_collection(&token, &[0x43; 32]));
        }
        assert!(!matches_collection(
            &position(&[0x55; 20], &collection),
            &collection
        ));
    }

    #[test]
    fn call_binds_condition_parent_and_single_outcome_bit() {
        for index in [0, 1, 8, 255] {
            let encoded = collection_call([0x23; 32], index).unwrap();
            let data = hex::decode(&encoded[2..]).unwrap();
            assert_eq!(data.len(), 100);
            assert_eq!(&data[..4], &[0x85, 0x62, 0x96, 0xf7]);
            assert_eq!(&data[4..36], &[0; 32]);
            assert_eq!(&data[36..68], &[0x23; 32]);
            assert_eq!(data[68..].iter().map(|b| b.count_ones()).sum::<u32>(), 1);
            assert_eq!(data[99 - index / 8], 1 << (index % 8));
        }
        assert!(collection_call([0; 32], 0).is_none());
        assert!(collection_call([1; 32], 256).is_none());
    }

    #[tokio::test]
    async fn verifies_mapping_with_pinned_block_and_rejects_wrong_collection() {
        let collection = [0x42; 32];
        let token = decimal(&position(&crate::merge::USDC, &collection));
        for returned in [collection, [0x43; 32]] {
            let body = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":format!("0x{}", hex::encode(returned))});
            let (rpc, server) = rpc_server(200, body.to_string());
            let ctf = format!("0x{}", hex::encode(crate::merge::CTF));
            assert_eq!(
                verifies(
                    &reqwest::Client::new(),
                    &rpc,
                    &ctf,
                    "0x1234",
                    [0x23; 32],
                    1,
                    &token
                )
                .await,
                returned == collection
            );
            let request = server.join().unwrap();
            assert_eq!(request["method"], "eth_call");
            assert_eq!(request["params"][0]["to"], ctf);
            assert_eq!(
                request["params"][0]["data"],
                collection_call([0x23; 32], 1).unwrap()
            );
            assert_eq!(request["params"][1], "0x1234");
        }
    }

    #[tokio::test]
    async fn unavailable_or_malformed_rpc_never_verifies() {
        let collection = [0x42; 32];
        let token = decimal(&position(&crate::merge::USDC, &collection));
        let valid = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":format!("0x{}",hex::encode(collection))});
        for (status, body) in [
            (503, valid.to_string()),
            (200, "not json".into()),
            (
                200,
                serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000}}).to_string(),
            ),
            (
                200,
                serde_json::json!({"jsonrpc":"2.0","id":1,"result":"0x01"}).to_string(),
            ),
            (
                200,
                serde_json::json!({"jsonrpc":"2.0","id":2,"result":valid["result"]}).to_string(),
            ),
        ] {
            let (rpc, server) = rpc_server(status, body);
            assert!(
                !verifies(
                    &reqwest::Client::new(),
                    &rpc,
                    &format!("0x{}", hex::encode(crate::merge::CTF)),
                    "0x1234",
                    [0x23; 32],
                    0,
                    &token
                )
                .await
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn moving_or_noncanonical_block_tags_are_rejected_before_rpc() {
        for block in ["latest", "finalized", "pending", "0x", "0x00", "0xgg"] {
            assert!(
                !verifies(
                    &reqwest::Client::new(),
                    "invalid-url",
                    &format!("0x{}", hex::encode(crate::merge::CTF)),
                    block,
                    [1; 32],
                    0,
                    "123"
                )
                .await
            );
        }
    }
}
