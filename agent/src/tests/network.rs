use crate::network::{AgentMessage, TOPIC};

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
