#![recursion_limit = "128"]

use env_logger;
use futures::Future;
use interledger::{
    cli,
    node::{AccountDetails, InterledgerNode},
};
use interledger_packet::Address;
use serde_json::json;
use std::str::FromStr;
use tokio::runtime::Builder as RuntimeBuilder;

mod redis_helpers;
use redis_helpers::*;

mod test_helpers;
use test_helpers::{
    create_account, get_balance, send_money, start_eth_engine, start_ganache, start_xrp_engine,
};

#[test]
fn eth_xrp_interoperable() {
    let eth_decimals = 9;
    let xrp_decimals = 6;
    // Nodes 1 and 2 are peers, Node 2 is the parent of Node 3
    let _ = env_logger::try_init();
    let context = TestContext::new();

    let mut ganache_pid = start_ganache();

    // Each node will use its own DB within the redis instance
    let mut connection_info1 = context.get_client_connection_info();
    connection_info1.db = 1;
    let mut connection_info2 = context.get_client_connection_info();
    connection_info2.db = 2;
    let mut connection_info3 = context.get_client_connection_info();
    connection_info3.db = 3;

    let node1_http = get_open_port(Some(3010));
    let node1_settlement = get_open_port(Some(3011));
    let node1_engine = get_open_port(Some(3012));

    let node2_http = get_open_port(Some(3020));
    let node2_settlement = get_open_port(Some(3021));
    let node2_engine = get_open_port(Some(3022));
    let node2_xrp_engine_port = get_open_port(Some(3023));
    let node2_btp = get_open_port(Some(3024));

    let node3_http = get_open_port(Some(3030));
    let node3_settlement = get_open_port(Some(3031));
    let _node3_engine = get_open_port(Some(3032)); // unused engine
    let node3_xrp_engine_port = get_open_port(Some(3033));

    // spawn 2 redis servers for the XRP engines
    let node2_redis_port = get_open_port(Some(6379));
    let node3_redis_port = get_open_port(Some(6380));
    let mut node2_engine_redis = RedisServer::spawn_with_port(node2_redis_port);
    let mut node3_engine_redis = RedisServer::spawn_with_port(node3_redis_port);
    let mut node2_xrp_engine = start_xrp_engine(
        &format!("http://localhost:{}", node2_settlement),
        node2_redis_port,
        node2_xrp_engine_port,
        "rGCUgMH4omQV1PUuYFoMAnA7esWFhE7ZEV",
        "sahVoeg97nuitefnzL9GHjp2Z6kpj",
    );
    let mut node3_xrp_engine = start_xrp_engine(
        &format!("http://localhost:{}", node3_settlement),
        node3_redis_port,
        node3_xrp_engine_port,
        "r3GDnYaYCk2XKzEDNYj59yMqDZ7zGih94K",
        "ssnYUDNeNQrNij2EVJG6dDw258jA6",
    );

    let node1_eth_key =
        "380eb0f3d505f087e438eca80bc4df9a7faa24f868e69fc0440261a0fc0567dc".to_string();
    let node2_eth_key =
        "cc96601bc52293b53c4736a12af9130abf347669b3813f9ec4cafdf6991b087e".to_string();
    let node1_eth_engine_fut = start_eth_engine(
        connection_info1.clone(),
        node1_engine,
        node1_eth_key,
        node1_settlement,
    );
    let node2_eth_engine_fut = start_eth_engine(
        connection_info2.clone(),
        node2_engine,
        node2_eth_key,
        node2_settlement,
    );

    let mut runtime = RuntimeBuilder::new()
        .panic_handler(|_| panic!("Tokio worker panicked"))
        .build()
        .unwrap();

    let node1 = InterledgerNode {
        ilp_address: Address::from_str("example.alice").unwrap(),
        default_spsp_account: Some(0),
        admin_auth_token: "admin".to_string(),
        redis_connection: connection_info1,
        btp_address: ([127, 0, 0, 1], get_open_port(None)).into(),
        http_address: ([127, 0, 0, 1], node1_http).into(),
        settlement_address: ([127, 0, 0, 1], node1_settlement).into(),
        secret_seed: cli::random_secret(),
        route_broadcast_interval: Some(200),
    };
    let node1_clone = node1.clone();
    runtime.spawn(
        // TODO insert the accounts via HTTP request
        node1_eth_engine_fut.and_then(move |_| {
            node1_clone
                .insert_account(AccountDetails {
                    ilp_address: Address::from_str("example.alice").unwrap(),
                    asset_code: "ETH".to_string(),
                    asset_scale: eth_decimals,
                    btp_incoming_token: None,
                    btp_uri: None,
                    http_endpoint: Some(format!("http://localhost:{}/ilp", node1_http)),
                    http_incoming_token: Some("in_alice".to_string()),
                    http_outgoing_token: Some("out_alice".to_string()),
                    max_packet_amount: u64::max_value(),
                    min_balance: None,
                    settle_threshold: None,
                    settle_to: None,
                    send_routes: false,
                    receive_routes: false,
                    routing_relation: None,
                    round_trip_time: None,
                    packets_per_minute_limit: None,
                    amount_per_minute_limit: None,
                    settlement_engine_url: None,
                })
                .and_then(move |_|
            // TODO insert the accounts via HTTP request
            node1_clone
                .insert_account(AccountDetails {
                    ilp_address: Address::from_str("example.bob").unwrap(),
                    asset_code: "ETH".to_string(),
                    asset_scale: eth_decimals,
                    btp_incoming_token: None,
                    btp_uri: None,
                    http_endpoint: Some(format!("http://localhost:{}/ilp", node2_http)),
                    http_incoming_token: Some("bob".to_string()),
                    http_outgoing_token: Some("alice".to_string()),
                    max_packet_amount: u64::max_value(),
                    min_balance: Some(-100_000),
                    settle_threshold: Some(70000),
                    settle_to: Some(10000),
                    send_routes: true,
                    receive_routes: true,
                    routing_relation: Some("Peer".to_string()),
                    round_trip_time: None,
                    packets_per_minute_limit: None,
                    amount_per_minute_limit: None,
                    settlement_engine_url: Some(format!("http://localhost:{}", node1_engine)),
                }))
                .and_then(move |_| node1.serve())
        }),
    );

    let node2 = InterledgerNode {
        ilp_address: Address::from_str("example.bob").unwrap(),
        default_spsp_account: Some(0),
        admin_auth_token: "admin".to_string(),
        redis_connection: connection_info2,
        btp_address: ([127, 0, 0, 1], node2_btp).into(),
        http_address: ([127, 0, 0, 1], node2_http).into(),
        settlement_address: ([127, 0, 0, 1], node2_settlement).into(),
        secret_seed: cli::random_secret(),
        route_broadcast_interval: Some(200),
    };
    let node2_clone = node2.clone();
    runtime.spawn(
        node2_eth_engine_fut
            .and_then(move |_| {
                node2_clone
                    .insert_account(AccountDetails {
                        ilp_address: Address::from_str("example.alice").unwrap(),
                        asset_code: "ETH".to_string(),
                        asset_scale: eth_decimals,
                        btp_incoming_token: None,
                        btp_uri: None,
                        http_endpoint: Some(format!("http://localhost:{}/ilp", node1_http)),
                        http_incoming_token: Some("alice".to_string()),
                        http_outgoing_token: Some("bob".to_string()),
                        max_packet_amount: u64::max_value(),
                        min_balance: Some(-100_000),
                        settle_threshold: None,
                        settle_to: None,
                        send_routes: true,
                        receive_routes: true,
                        routing_relation: Some("Peer".to_string()),
                        round_trip_time: None,
                        packets_per_minute_limit: None,
                        amount_per_minute_limit: None,
                        settlement_engine_url: Some(format!("http://localhost:{}", node2_engine)),
                    })
                    .and_then(move |_| {
                        node2_clone.insert_account(AccountDetails {
                            ilp_address: Address::from_str("example.bob.charlie").unwrap(),
                            asset_code: "XRP".to_string(),
                            asset_scale: xrp_decimals,
                            btp_incoming_token: None,
                            btp_uri: None,
                            http_endpoint: Some(format!("http://localhost:{}/ilp", node3_http)),
                            http_incoming_token: Some("charlie".to_string()),
                            http_outgoing_token: Some("bob".to_string()),
                            max_packet_amount: u64::max_value(),
                            min_balance: Some(-100),
                            settle_threshold: Some(70000),
                            settle_to: Some(5000),
                            send_routes: false,
                            receive_routes: true,
                            routing_relation: Some("Child".to_string()),
                            round_trip_time: None,
                            packets_per_minute_limit: None,
                            amount_per_minute_limit: None,
                            settlement_engine_url: Some(format!(
                                "http://localhost:{}",
                                node2_xrp_engine_port
                            )),
                        })
                    })
            })
            .and_then(move |_| node2.serve())
            .and_then(move |_| {
                let client = reqwest::r#async::Client::new();
                client
                    .put(&format!("http://localhost:{}/rates", node2_http))
                    .header("Authorization", "Bearer admin")
                    // Let's say 0.001 ETH = 1 XRP for this example
                    .json(&json!({"XRP": 1000, "ETH": 1}))
                    .send()
                    .map_err(|err| panic!(err))
                    .and_then(|res| {
                        res.error_for_status()
                            .expect("Error setting exchange rates");
                        Ok(())
                    })
            }),
    );

    let node3 = InterledgerNode {
        ilp_address: Address::from_str("example.bob.charlie").unwrap(),
        default_spsp_account: Some(0),
        admin_auth_token: "admin".to_string(),
        redis_connection: connection_info3,
        btp_address: ([127, 0, 0, 1], get_open_port(None)).into(),
        http_address: ([127, 0, 0, 1], node3_http).into(),
        settlement_address: ([127, 0, 0, 1], node3_settlement).into(),
        secret_seed: cli::random_secret(),
        route_broadcast_interval: Some(200),
    };
    let node3_clone = node3.clone();
    runtime.spawn(
        // Wait a bit to make sure the other node's BTP server is listening
        delay(50)
            .map_err(|err| panic!(err))
            .and_then(move |_| {
                node3_clone
                    .insert_account(AccountDetails {
                        ilp_address: Address::from_str("example.bob.charlie").unwrap(),
                        asset_code: "XRP".to_string(),
                        asset_scale: xrp_decimals,
                        btp_incoming_token: None,
                        btp_uri: None,
                        http_endpoint: Some(format!("http://localhost:{}/ilp", node3_http)),
                        http_incoming_token: Some("in_charlie".to_string()),
                        http_outgoing_token: Some("out_charlie".to_string()),
                        max_packet_amount: u64::max_value(),
                        min_balance: None,
                        settle_threshold: None,
                        settle_to: None,
                        send_routes: false,
                        receive_routes: false,
                        routing_relation: None,
                        round_trip_time: None,
                        packets_per_minute_limit: None,
                        amount_per_minute_limit: None,
                        settlement_engine_url: None,
                    })
                    .and_then(move |_| {
                        node3_clone.insert_account(AccountDetails {
                            ilp_address: Address::from_str("example.bob").unwrap(),
                            asset_code: "XRP".to_string(),
                            asset_scale: xrp_decimals,
                            btp_incoming_token: None,
                            btp_uri: None,
                            http_endpoint: Some(format!("http://localhost:{}/ilp", node2_http)),
                            http_incoming_token: Some("bob".to_string()),
                            http_outgoing_token: Some("charlie".to_string()),
                            max_packet_amount: u64::max_value(),
                            min_balance: Some(-100_000),
                            settle_threshold: None,
                            settle_to: None,
                            send_routes: true,
                            receive_routes: false,
                            routing_relation: Some("Parent".to_string()),
                            round_trip_time: None,
                            packets_per_minute_limit: None,
                            amount_per_minute_limit: None,
                            settlement_engine_url: Some(format!(
                                "http://localhost:{}",
                                node3_xrp_engine_port
                            )),
                        })
                    })
            })
            .and_then(move |_| node3.serve()),
    );

    runtime
        .block_on(
            // Wait for the nodes to spin up
            delay(500)
                .map_err(|_| panic!("Something strange happened"))
                .and_then(move |_| {
                    // Insert accounts for the 3 nodes (4 total since node2 has
                    // eth & xrp)
                    create_account(node1_engine, "1")
                        .and_then(move |_| create_account(node2_engine, "0"))
                        .and_then(move |_| create_account(node2_xrp_engine_port, "1"))
                        .and_then(move |_| create_account(node3_xrp_engine_port, "1"))
                        // Pay 70k Gwei --> 70 drops
                        .and_then(move |_| send_money(node1_http, node3_http, 70000, "in_alice"))
                        // Pay 1k Gwei --> 1 drop
                        // This will trigger a 71 Gwei settlement from Alice to Bob.
                        .and_then(move |_| send_money(node1_http, node3_http, 1000, "in_alice"))
                        .and_then(move |_| {
                            // wait for the settlements
                            delay(10000).map_err(|err| panic!(err)).and_then(move |_| {
                                futures::future::join_all(vec![
                                    get_balance(0, node1_http, "admin"),
                                    get_balance(1, node1_http, "admin"),
                                    get_balance(0, node2_http, "admin"),
                                    get_balance(1, node2_http, "admin"),
                                    get_balance(0, node3_http, "admin"),
                                    get_balance(1, node3_http, "admin"),
                                ])
                                .and_then(move |balances| {
                                    // Alice has paid Charlie in total 71k Gwei through Bob.
                                    assert_eq!(balances[0], -71000);
                                    // Since Alice has configured Bob's
                                    // `settle_threshold` and `settle_to` to be
                                    // 70k and 10k respectively, once she
                                    // exceeded the 70k threshold, she made a 61k
                                    // Gwei settlement to Bob so that their debt
                                    // settles down to 10k.
                                    // From her perspective, Bob's account has a
                                    // positive 10k balance since she owes him money.
                                    assert_eq!(balances[1], 10000);
                                    // From Bob's perspective, Alice's account
                                    // has a negative sign since he is owed money.
                                    assert_eq!(balances[2], -10000);
                                    // As Bob forwards money to Charlie, he also
                                    // eventually exceeds the `settle_threshold`
                                    // which incidentally is set to 70k. As a
                                    // result, he must make a XRP ledger
                                    // settlement of 66k Drops to get his debt
                                    // back to the `settle_to` value of charlie,
                                    // which is 5k (71k - 5k = 66k).
                                    assert_eq!(balances[3], 5000);
                                    // Charlie's balance indicates that he's
                                    // received 71k drops (the total amount Alice sent him)
                                    assert_eq!(balances[4], 71000);
                                    // And he sees is owed 5k by Bob.
                                    assert_eq!(balances[5], -5000);

                                    node2_engine_redis.kill().unwrap();
                                    node3_engine_redis.kill().unwrap();
                                    node2_xrp_engine.kill().unwrap();
                                    node3_xrp_engine.kill().unwrap();
                                    ganache_pid.kill().unwrap();
                                    Ok(())
                                })
                            })
                        })
                }),
        )
        .unwrap();
}
