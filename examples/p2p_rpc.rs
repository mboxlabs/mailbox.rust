use mailbox::{Mailbox, OutgoingMail, MailMessage};
use mailbox::providers::memory::MemoryProvider;
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

            let op = msg.body["op"].as_str().unwrap_or("");
            let args = msg.body["args"].as_array().unwrap();
            let a = args[0].as_i64().unwrap_or(0);
            let b = args[1].as_i64().unwrap_or(0);

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
