//! Against a local development node: Anvil accepts what the signer signs.
//! Ignored by default; start an offline node (`anvil --port 18546`, no
//! fork) and run with `LPBOT_ANVIL_RPC=http://127.0.0.1:18546 cargo test -p
//! lpbot-signer -- --ignored`.

use alloy_primitives::{address, hex, Bytes, U256};
use lpbot_signer::{decode_signed, Eip1559Tx, Key};
use serde_json::{json, Value};

/// Anvil's development key #0, funded on every fresh Anvil node.
const DEV_KEY: [u8; 32] = hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

fn rpc(url: &str, method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let reply: Value = ureq::post(url)
        .send_json(body)
        .expect("the node answers")
        .into_json()
        .expect("JSON");
    assert!(reply.get("error").is_none(), "{method}: {reply}");
    reply["result"].clone()
}

fn number(value: &Value) -> u64 {
    u64::from_str_radix(value.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

#[test]
#[ignore = "needs a local Anvil node; see the file's comment"]
fn anvil_accepts_and_mines_what_the_signer_signs() {
    let url = std::env::var("LPBOT_ANVIL_RPC").expect("set LPBOT_ANVIL_RPC");
    let key = Key::from_secret(&DEV_KEY).unwrap();
    let chain_id = number(&rpc(&url, "eth_chainId", json!([])));
    let base = number(&rpc(&url, "eth_gasPrice", json!([])));
    for data in [
        Bytes::new(),
        Bytes::from(vec![0xc6, 0xfe, 0x87, 0x47, 0, 1, 2, 3]),
    ] {
        let nonce = number(&rpc(
            &url,
            "eth_getTransactionCount",
            json!([key.address(), "pending"]),
        ));
        let tx = Eip1559Tx {
            chain_id,
            nonce,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: u128::from(base) * 2 + 1_000_000_000,
            gas_limit: 100_000,
            to: address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
            value: U256::from(1_000u64),
            data,
        };
        let signed = key.sign(&tx).unwrap();
        assert_eq!(decode_signed(&signed.raw).unwrap().1, key.address());
        let hash = rpc(
            &url,
            "eth_sendRawTransaction",
            json!([format!("0x{}", hex::encode(&signed.raw))]),
        );
        assert_eq!(hash.as_str().unwrap(), signed.hash.to_string());
        let receipt = rpc(&url, "eth_getTransactionReceipt", json!([hash]));
        assert_eq!(receipt["status"], "0x1", "{receipt}");
        assert_eq!(
            receipt["from"].as_str().unwrap().to_lowercase(),
            key.address().to_string().to_lowercase()
        );
    }
}
