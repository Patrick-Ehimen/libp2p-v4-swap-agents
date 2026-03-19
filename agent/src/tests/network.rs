use crate::network::{build_peer_score_params, AgentMessage, INTENT_TOPIC, TOPIC};

#[test]
fn topic_constant() {
    assert_eq!(TOPIC, "v4-swap-agents");
}

#[test]
fn chat_message_roundtrip() {
    let msg = AgentMessage::Chat {
        content: "hello".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
    match decoded {
        AgentMessage::Chat { content } => assert_eq!(content, "hello"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn swap_executed_roundtrip() {
    let msg = AgentMessage::SwapExecuted {
        agent: "peer1".into(),
        direction: "A→B".into(),
        amount: "100".into(),
        tx_hash: "0xabc".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
    match decoded {
        AgentMessage::SwapExecuted {
            agent,
            direction,
            amount,
            tx_hash,
        } => {
            assert_eq!(agent, "peer1");
            assert_eq!(direction, "A→B");
            assert_eq!(amount, "100");
            assert_eq!(tx_hash, "0xabc");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn swap_request_roundtrip() {
    let msg = AgentMessage::SwapRequest {
        direction: "B→A".into(),
        amount: "50".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
    match decoded {
        AgentMessage::SwapRequest { direction, amount } => {
            assert_eq!(direction, "B→A");
            assert_eq!(amount, "50");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn serialized_json_has_type_tag() {
    let chat = serde_json::to_value(&AgentMessage::Chat {
        content: "hi".into(),
    })
    .unwrap();
    assert_eq!(chat["type"], "Chat");

    let exec = serde_json::to_value(&AgentMessage::SwapExecuted {
        agent: "a".into(),
        direction: "d".into(),
        amount: "1".into(),
        tx_hash: "0x".into(),
    })
    .unwrap();
    assert_eq!(exec["type"], "SwapExecuted");

    let req = serde_json::to_value(&AgentMessage::SwapRequest {
        direction: "d".into(),
        amount: "1".into(),
    })
    .unwrap();
    assert_eq!(req["type"], "SwapRequest");
}

#[test]
fn intent_topic_constant() {
    assert_eq!(INTENT_TOPIC, "v4-swap-intents");
}

#[test]
fn swap_intent_roundtrip() {
    let msg = AgentMessage::SwapIntent {
        agent: "peer1".into(),
        direction: "TKNA -> TKNB".into(),
        amount: "10".into(),
        min_price: Some("0.95".into()),
        max_price: Some("1.05".into()),
        timestamp: 1700000000,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
    match decoded {
        AgentMessage::SwapIntent {
            agent,
            direction,
            amount,
            min_price,
            max_price,
            timestamp,
        } => {
            assert_eq!(agent, "peer1");
            assert_eq!(direction, "TKNA -> TKNB");
            assert_eq!(amount, "10");
            assert_eq!(min_price, Some("0.95".into()));
            assert_eq!(max_price, Some("1.05".into()));
            assert_eq!(timestamp, 1700000000);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn swap_intent_optional_prices_none() {
    let msg = AgentMessage::SwapIntent {
        agent: "peer2".into(),
        direction: "TKNB -> TKNA".into(),
        amount: "5".into(),
        min_price: None,
        max_price: None,
        timestamp: 1700000001,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
    match decoded {
        AgentMessage::SwapIntent {
            min_price,
            max_price,
            ..
        } => {
            assert!(min_price.is_none());
            assert!(max_price.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn swap_intent_serialized_json_has_type_tag() {
    let intent = serde_json::to_value(&AgentMessage::SwapIntent {
        agent: "a".into(),
        direction: "d".into(),
        amount: "1".into(),
        min_price: None,
        max_price: None,
        timestamp: 0,
    })
    .unwrap();
    assert_eq!(intent["type"], "SwapIntent");
}

// --- Peer score parameter tests ---

#[test]
fn p4_invalid_message_weight_is_negative() {
    let (params, _) = build_peer_score_params();
    let swap_topic_hash = libp2p::gossipsub::IdentTopic::new(TOPIC).hash();
    let topic_params = params
        .topics
        .get(&swap_topic_hash)
        .expect("swap topic should exist");
    assert!(
        topic_params.invalid_message_deliveries_weight < 0.0,
        "P4 weight should be negative to penalize invalid messages"
    );
}

#[test]
fn p7_behaviour_penalty_configured() {
    let (params, _) = build_peer_score_params();
    assert!(
        params.behaviour_penalty_weight < 0.0,
        "P7 behaviour penalty weight should be negative"
    );
    assert!(
        params.behaviour_penalty_decay > 0.0 && params.behaviour_penalty_decay < 1.0,
        "P7 decay should be between 0 and 1"
    );
}

#[test]
fn p3_remains_disabled() {
    let (params, _) = build_peer_score_params();
    let swap_topic_hash = libp2p::gossipsub::IdentTopic::new(TOPIC).hash();
    let topic_params = params.topics.get(&swap_topic_hash).unwrap();
    assert_eq!(
        topic_params.mesh_message_deliveries_weight, 0.0,
        "P3 should remain disabled for small networks"
    );
}

#[test]
fn p5_weight_is_positive() {
    let (params, _) = build_peer_score_params();
    assert!(params.app_specific_weight > 0.0);
}

#[test]
fn both_topics_have_score_params() {
    let (params, _) = build_peer_score_params();
    let swap_topic = libp2p::gossipsub::IdentTopic::new(TOPIC).hash();
    let intent_topic = libp2p::gossipsub::IdentTopic::new(INTENT_TOPIC).hash();
    assert!(params.topics.contains_key(&swap_topic));
    assert!(params.topics.contains_key(&intent_topic));
}

#[test]
fn thresholds_are_ordered() {
    let (_, thresholds) = build_peer_score_params();
    assert!(thresholds.gossip_threshold > thresholds.publish_threshold);
    assert!(thresholds.publish_threshold > thresholds.graylist_threshold);
}
