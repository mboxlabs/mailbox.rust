use mboxlabs_mailbox::{Mailbox, OutgoingMail, MailMessage};
use mboxlabs_mailbox::error::MailboxError;
use mboxlabs_mailbox::providers::memory::MemoryProvider;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut mailbox = Mailbox::new();
    mailbox.register_provider(Box::new(MemoryProvider::new()));

    // 1. Start the Service
    let service_addr = "mem:service/calculator";
    let mailbox_clone = mailbox.clone();

    println!("Starting service at {}", service_addr);
    mailbox.subscribe(service_addr.parse()?, Box::new(move |msg: MailMessage| {
        let mailbox = mailbox_clone.clone();
        Box::pin(async move {
            println!("[Service] Received request: {:?}", msg.body);

            // 订阅回调现在返回 Result：返回 Err 且可重试时消息会退回队列（与 TS / Python 一致），
            // 而不是像以前那样失败后静默丢失。
            let args = match msg.body["args"].as_array() {
                Some(args) => args.clone(),
                None => return Err(MailboxError::Retriable("missing args".to_string())),
            };
            let op = msg.body["op"].as_str().unwrap_or("");
            let a = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
            let b = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);

            let result = match op {
                "add" => a + b,
                "sub" => a - b,
                _ => 0,
            };

            println!("[Service] Computed result: {}", result);

            // Reply to sender
            let reply = OutgoingMail {
                id: None,
                from: msg.to.clone(),
                to: msg.from.clone(),
                body: json!({ "result": result }),
                headers: HashMap::new(),
                meta: HashMap::new(),
            };

            if let Err(e) = mailbox.post(reply).await {
                eprintln!("[Service] Failed to send reply: {}", e);
            }

            Ok(())
        })
    })).await?;

    // 2. Client sends request
    let client_addr = "mem:client/user1";
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));

    println!("Starting client at {}", client_addr);
    mailbox.subscribe(client_addr.parse()?, Box::new(move |msg: MailMessage| {
        let tx = tx.clone();
        Box::pin(async move {
            println!("[Client] Received reply: {:?}", msg.body);
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(msg);
            }
            Ok(())
        })
    })).await?;

    println!("[Client] Sending request: 10 + 20");
    mailbox.post(OutgoingMail {
        id: None,
        from: client_addr.parse()?,
        to: service_addr.parse()?,
        body: json!({ "op": "add", "args": [10, 20] }),
        headers: HashMap::new(),
        meta: HashMap::new(),
    }).await?;

    // Wait for reply
    let reply = tokio::time::timeout(Duration::from_secs(2), rx).await??;
    println!("[Client] Got result: {}", reply.body["result"]);

    Ok(())
}
