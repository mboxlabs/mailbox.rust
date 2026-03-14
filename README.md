# 📮 Mailbox — Rust Implementation

> A lightweight, pluggable "mailbox/queue" kernel that treats all communication as "delivering a letter to an address". An address represents a unique mailbox, accessible through different transport protocols (like `mem:`, `mailto:`, `slack:`) via pluggable Providers.
> Build fault-tolerant, distributed, human-machine collaborative systems using mailbox-based async communication.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)

## 🌟 Why Mailbox?

| Traditional Approach | Mailbox Approach |
|---------------------|------------------|
| ❌ Shared state + locks | ✅ Independent mailboxes + messages |
| ❌ Callback hell | ✅ `async/await` seamless integration |
| ❌ Complex human-machine collaboration | ✅ Human = a mailbox address |
| ❌ Offline scenarios difficult | ✅ Messages auto-buffered and retried |

### Erlang Inspiration

> _🙏 Tribute: Erlang's Actor Model_
> _"In the 1980s, when computers were as big as rooms,
> Erlang's creators proposed a revolutionary idea:
> **Each process has its own mailbox, communicates via messages, and crashes are part of the design, not failures**"_
> —— Joe Armstrong, Robert Virding, Mike Williams

Mailbox is **deeply inspired by Erlang's Actor model**, but with key evolution:

| Erlang (1986) | Mailbox (Today) | Why It Matters |
|---------------|-----------------|----------------|
| `Pid ! Message` | `send({ to: 'protocol://address' })` | **Address is identity, protocol is routing**: `address` part is the mailbox's unique ID. `protocol` (e.g. `mem`, `mailto`) determines routing. Same address accessible via different protocols. |
| In-process FIFO mailbox | Pluggable Providers | **Transport-agnostic**: Seamlessly switch between memory/email/Wechat/Mastodon |
| Same-node communication | Cross-network, cross-organization | **Truly distributed**: Humans and machines participate equally |

## 🚀 Quick Start

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
mboxlabs-mailbox = { path = "path/to/mailbox" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

### Basic Example

```rust
use mboxlabs_mailbox::{Mailbox, OutgoingMail, MailMessage};
use mboxlabs_mailbox::providers::memory::MemoryProvider;
use serde_json::json;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create mailbox instance and register a memory provider
    let mut mailbox = Mailbox::new();
    mailbox.register_provider(Box::new(MemoryProvider::new()));

    // 2. Start the mailbox (initializes providers)
    mailbox.start().await?;

    // 3. Subscribe to an address and define message handler
    let subscription = mailbox.subscribe(
        "mem:service@example.com/inbox".parse()?,
        Box::new(|message| {
            Box::pin(async move {
                println!("Received message! From: {}", message.from);
                println!("Content: {:?}", message.body);
            })
        })
    ).await?;

    println!("Mailbox established, listening on 'mem:service@example.com/inbox'...");

    // 4. Send a message to that address
    mailbox.post(OutgoingMail {
        id: None,
        from: "mem:client@example.com/user-1".parse()?,
        to: "mem:service@example.com/inbox".parse()?,
        body: json!({ "text": "Hello, Mailbox!" }),
        headers: HashMap::new(),
        meta: HashMap::new(),
    }).await?;

    // 5. Clean up: Unsubscribe and stop the mailbox (releases resources)
    subscription.unsubscribe().await?;
    mailbox.stop().await?;

    // Give async tasks time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    Ok(())
}
```

Output:
```
Mailbox established, listening on 'mem:service@example.com/inbox'...
Received message! From: mem:client@example.com/user-1
Content: Object {"text": String("Hello, Mailbox!")}
```

## 📪 Mailbox Address

A **MailAddress** is the foundation of the entire system, serving as the unique universal identifier for any destination. It follows the [RFC 3986](https://tools.ietf.org/html/rfc3986) URI specification.

- **Format**: `protocol:user@physical_address[/logical_address]`
- **Example**: `mem:api@myservice.com/utils/greeter`

A mailbox address consists of three parts:

- **`protocol`**: Specifies the **transport method** for messages (e.g., `mem` for memory bus, `mailto` for email). It tells `Mailbox` which provider should handle the message.
- **`user@physical_address`**: The **globally unique, protocol-agnostic ID** of the logical mailbox or service. The same physical address can be accessed via different protocols (e.g., `mem:api@myservice.com` and `mailto:api@myservice.com` point to the same logical entity).
- **`/logical_address`** (optional): An optional path for internal routing. For example, when combined with `tool-rpc`, it can route messages to specific tools within a larger service, allowing one physical address to serve as a unified gateway for multiple logical functions.

## 🎯 Core Features

### 1. Subscribe Pattern (Push)

```rust
let subscription = mailbox.subscribe(
    "mem:service/inbox".parse()?,
    Box::new(|msg| {
        Box::pin(async move {
            println!("Got: {:?}", msg.body);
        })
    })
).await?;

// Unsubscribe when done
subscription.unsubscribe().await?;
```

### 2. Fetch Pattern (Pull)

**Auto-acknowledgment:**
```rust
let msg = mailbox.fetch(
    "mem:service/inbox".parse()?,
    FetchOptions::default()
).await?;

if let Some(msg) = msg {
    println!("Fetched: {:?}", msg.message.body);
    // Auto-acknowledged
}
```

**Manual acknowledgment:**
```rust
let msg = mailbox.fetch(
    "mem:service/inbox".parse()?,
    FetchOptions {
        manual_ack: true,
        ack_timeout: Some(5000), // 5 seconds
    }
).await?;

if let Some(msg) = msg {
    // Process message...
    msg.ack().await?; // Acknowledge
    // Or: msg.nack(true).await?; // Negative ack with requeue
}
```

### 3. Status Query

```rust
let status = mailbox.status("mem:service/inbox".parse()?).await?;
println!("State: {}", status.state);
println!("Unread: {:?}", status.unread_count);
```

### 4. Accessing Providers

You can gracefully access registered providers via the `providers()` getter or `get_provider()` method:

```rust
// Get all providers (returns a reference to HashMap)
let all_providers = mailbox.providers();
if all_providers.contains_key("mem") {
    println!("Memory provider is registered");
}

// Get a specific provider
// protocol can be "mem" or "mem:"
let provider = mailbox.get_provider("mem", false).unwrap();

// Get provider or return error if not found
let slack_provider = mailbox.get_provider("slack", true)?;
```

## 🏗️ Architecture

### Provider Trait

Implement the `MailboxProvider` trait to create custom providers:

```rust
#[async_trait]
pub trait MailboxProvider: Send + Sync {
    fn protocol(&self) -> &str;

    /// Optional: Initialize connections, pre-warm caches
    async fn init(&self) -> Result<()> { Ok(()) }

    /// Optional: Specific closing logic, release resources
    async fn close(&self) -> Result<()> { Ok(()) }

    async fn send(&self, message: MailMessage) -> Result<MailMessage>;
    async fn subscribe(
        &self,
        address: Url,
        callback: Box<dyn Fn(MailMessage) -> BoxFuture<'static, ()> + Send + Sync>
    ) -> Result<Box<dyn Subscription>>;
    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>>;
    async fn status(&self, address: Url) -> Result<MailboxStatus>;
    fn generate_id(&self) -> String;
}
```

### Built-in Providers

- **MemoryProvider**: In-memory message bus for local communication
  - Topic-based routing
  - FIFO queue with manual/auto acknowledgment
  - Stale message requeueing
  - Thread-safe singleton event bus

## 🌐 WASM Support

This library is WASM-compatible! Build for different targets using `wasm-pack`:

### Build for Node.js

```bash
wasm-pack build --target nodejs
```

### Build for Web

```bash
wasm-pack build --target web
```

### Build for Bundlers (Webpack, Rollup, etc.)

```bash
wasm-pack build --target bundler
```

The library uses conditional compilation to support both native and WASM environments:
- **Native**: Uses `tokio::spawn` for async tasks
- **WASM**: Uses `wasm_bindgen_futures::spawn_local`

### Key WASM Compatibility Features

- ✅ UUID generation with `js` feature for WASM
- ✅ Async/await support via `wasm-bindgen-futures`
- ✅ Conditional compilation for platform-specific code
- ✅ Optimized WASM binary size

For detailed WASM testing instructions, examples, and troubleshooting, see [WASM_TESTING.md](WASM_TESTING.md).

## 🧪 Testing

Run the test suite:

```bash
cargo test
```

Tests include:
- Send and subscribe
- Fetch with auto-ack
- Fetch with manual-ack
- Nack with requeue
- Concurrent access

## 📚 Examples

### P2P RPC Communication

See `examples/p2p_rpc.rs` for a complete example of request-reply pattern:

```bash
cargo run --example p2p_rpc
```

This demonstrates:
- Service listening on a mailbox
- Client sending requests
- Service processing and replying
- Client receiving responses

## 🔧 Dependencies

- `tokio`: Async runtime (native) / minimal features (WASM)
- `serde` / `serde_json`: Serialization
- `url`: URL parsing
- `uuid`: UUID v4 generation with WASM support
- `async-trait`: Async trait support
- `chrono`: Timestamp handling
- `once_cell`: Lazy static initialization (modern alternative to lazy_static)
- `dashmap`: Concurrent hash map
- `wasm-bindgen`: WASM interop

### WASM-Specific Dependencies

For WASM target, additional dependencies are configured:
- `wasm-bindgen-futures`: Async support for WASM

See [WASM_TESTING.md](WASM_TESTING.md) for detailed WASM build and testing instructions.

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## 📄 License

This project is licensed under the MIT License - see the [LICENSE-MIT](../../LICENSE-MIT) file for details.

## 🙏 Acknowledgments

Inspired by:
- Erlang's Actor Model and OTP
- The original TypeScript Mailbox implementation
- The principle that "the postal system has worked for 500 years because it doesn't assume the recipient is waiting at the door!"

---

> **Remember**: In the Mailbox world, **every mailbox is an independent universe, and messages are messengers traveling through space and time** 🌌
