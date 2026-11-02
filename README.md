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
                Ok(())
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

There are two ways to receive mail, and one message goes to **one of them only**, decided
when `post()` runs:

| State of the address when `post()` runs | Where the message goes |
|------|------|
| at least one active subscriber | handed to every subscriber's callback; **not** queued, so `fetch()` gets nothing |
| no subscriber | queued; `fetch()` gets it, subscribers get nothing |

Mutually exclusive per message: a message already handed to a subscriber can never be
fetched, and a queued message is never replayed to a subscriber that appears later.

### 1. Subscribe

```rust
let subscription = mailbox.subscribe(
    "mem:service/inbox".parse()?,
    Box::new(|msg| {
        Box::pin(async move {
            println!("Got: {:?}", msg.body);
            Ok(())
        })
    })
).await?;

// Unsubscribe when done
subscription.unsubscribe().await?;
```

The callback returns `Result<()>`. An `Err` is classified by `MailboxError::is_retriable()`:
`MailboxError::Retriable(..)` or a `Transport` error whose code is `503 / 504 / ECONNRESET /
ETIMEDOUT` is retriable; anything else is fatal, so one bad message is not retried forever.
Same rule as the TypeScript and Python sides.

```rust
Box::new(|_| Box::pin(async {
    Err(MailboxError::Retriable("upstream down".to_string())) // → retriable
}))
```

One address can have several subscribers, and subscribing is a **broadcast**: one message becomes
one _delivery_ per subscriber, and every delivery settles on its own (`DeliveryResult` keyed by
`subscription_id + message_id`, retry chain included). What happens to the message is decided
once **all** deliveries have settled — never by whichever subscriber failed first:

| Broadcast outcome                | What the mailbox does                                                                       |
| -------------------------------- | ------------------------------------------------------------------------------------------- |
| Some delivery succeeded          | message stays handled — **not** requeued; failing deliveries go to the dead-letter hook      |
| Every delivery failed, retriable | message goes back to the head of the queue, `fetch()` picks it up (one copy)                |
| Every delivery failed, fatal     | discarded, so one bad message is not retried forever                                        |

Requeueing a message somebody already handled would hand the same side effect to a `fetch()`
consumer a second time, which is why partial failure is settled per delivery. `post()` returns at
dispatch, not at settlement, so `status()` right after `post()` says nothing about the outcome —
use the dead-letter hook (see §5) to observe what was dropped.

### 2. Fetch

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

`ack_timeout` 的三态语义（与 TS / Python 端一致）：

| 传值 | 含义 |
|------|------|
| `None`（缺省） | 用 `DEFAULT_ACK_TIMEOUT`（30 秒），未 ack 的消息会重新入队 |
| `Some(t)`，`t > 0` | 该超时（毫秒） |
| `Some(Duration::ZERO)` | 关闭 stale 扫描，消息常驻 in-flight 直到 ack/nack |

不传 ≠ 永不超时：否则崩溃的消费者会把消息永久卡在 in-flight。

### 3. Status Query

```rust
let status = mailbox.status("mem:service/inbox".parse()?).await?;
println!("State: {}", status.state);
println!("Unread: {:?}", status.unread_count);              // 仍在队列里等待
println!("In flight: {:?}", status.in_flight_count);        // 已取出、尚未 ack/nack
println!("Subscribers: {:?}", status.extra.get("subscriber_count"));
```

`unread_count` 只统计队列里等待的消息；`manual_ack: true` 取出的消息单独计入
`in_flight_count`，一直不 ack 会回到队列。

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

### 5. Subscriber Failures and Retry (opt-in)

The subscription callback returns `Result<()>`. A retriable error (`MailboxError::Retriable`,
or a `Transport` error whose code is `503 / 504 / ECONNRESET / ETIMEDOUT`) requeues the
message so `fetch()` can pick it up; anything else is dropped, so one bad message is not
retried forever.

By default nothing calls the callback again. Opt into automatic redelivery:

```rust
use std::time::Duration;
use mboxlabs_mailbox::RetryPolicy;
use mboxlabs_mailbox::providers::memory::MemoryProvider;

let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(3, Duration::from_millis(1000))));
```

Only the failing subscriber is re-invoked — subscribing is a broadcast, and re-broadcasting
would run the side effects of the subscribers that already succeeded a second time. Attempts
are counted per (subscriber, message); when the budget runs out the message goes back to the
queue, and unsubscribing while a retry is pending returns it to the queue as well. A message
is never queued twice: `requeue()` skips one that is already queued or in flight.

Retry is opt-in because AMQP / SQS style transports already redeliver and ship dead-letter
queues; retrying on top of them would double-deliver. `MemoryProvider::new()` keeps the
default: requeue, no automatic redelivery.

**Observing what gets dropped.** A broadcast that somebody already handled is deliberately
*not* requeued (see §1), so the failed deliveries would otherwise vanish without a trace. The
two hooks on the provider make them visible:

```rust
use std::sync::Arc;
use mboxlabs_mailbox::{DeliveryResult, MailMessage};

provider.set_dead_letter_hook(Some(Arc::new(move |message: MailMessage, failed: Vec<DeliveryResult>| {
    // 部分失败 / 全部不可重试：写死信队列、告警、打点
    eprintln!("{} dropped {} delivery(ies)", message.id, failed.len());
})));

provider.set_retry_exhausted_hook(Some(Arc::new(move |message: MailMessage, attempts: u32| {
    eprintln!("{} failed after {} attempts", message.id, attempts);
})));
```

`failed` entries are `DeliveryResult { delivery_id, subscription_id, message_id, ok, reason }`
with `reason` in `Retriable | Fatal | Cancelled` — i.e. which subscriber failed and why.

**Stuck callbacks are bounded.** A subscriber callback that never returns would park the
settlement barrier forever: the message would be neither requeued nor dead-lettered, and
`unread_count` would not show it. Each *attempt* therefore gets a settle watchdog
(`DEFAULT_DELIVERY_SETTLE_TIMEOUT`, 30s):

```rust
provider.set_delivery_settle_timeout(Duration::from_millis(500)); // Duration::ZERO disables it
```

When it fires, the delivery settles as `Cancelled` and the message goes back to the queue; the
callback finishing later does not settle it twice. The watchdog only times attempts, never the
backoff wait, so it cannot cut a healthy retry chain. A running callback is never settled on the
subscriber's behalf either — unsubscribing or `stop()` only cancels deliveries that are *waiting
for a retry*, otherwise a callback that was about to succeed would have its side effect replayed
through `fetch()`.

Two caveats specific to this port: on `wasm32` tokio's `time` feature is unavailable, so the
backoff degrades to "retry without waiting" (`max_attempts` still bounds the loop) and the settle
watchdog is not armed. Provider code that needs a deadline on that target should enforce it in
the callback itself.

The policy pieces live in `mboxlabs_mailbox::retry` (`RetryPolicy`, `next_retry_delay`,
`AttemptLedger`) and the settlement vocabulary in `mboxlabs_mailbox::delivery`
(`DeliveryResult`, `FailureReason`, `delivery_key`, `should_requeue_after_broadcast`,
`to_delivery_results`); both are re-exported from the crate root, so other packages can reuse the
same backoff and accounting. The retriable check is `MailboxError::is_retriable()`.

## 🏗️ Architecture

### Provider Trait

Implement the `MailboxProvider` trait to create custom providers:

```rust
#[async_trait]
pub trait MailboxProvider: Send + Sync {
    fn protocol(&self) -> &str;

    /// Optional: Initialize connections, pre-warm caches
    async fn init(&self) -> Result<()> { Ok(()) }

    /// Optional: release resources (`ProviderCore::close()` cancels subscriptions + pending retries)
    async fn close(&self) -> Result<()> { Ok(()) }

    /// Concrete: injects `mbx-sent-at`, then delegates to `_send`
    async fn send(&self, message: MailMessage) -> Result<MailMessage> { /* … */ }
    /// Provider responsibility: the protocol-specific dispatch
    async fn _send(&self, message: MailMessage) -> Result<()>;

    async fn subscribe(
        &self,
        address: Url,
        callback: SubscriptionHandler   // Box<dyn Fn(MailMessage) -> BoxFuture<'static, Result<()>>>
    ) -> Result<Box<dyn Subscription>>;
    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>>;

    /// Optional: defaults to `state: "unknown"`
    async fn status(&self, address: Url) -> Result<MailboxStatus> { /* … */ }
    fn generate_id(&self) -> String;
}

pub trait Subscription: Send + Sync {
    fn id(&self) -> &str { "" }                                  // = DeliveryResult.subscription_id
    fn status(&self) -> SubscriptionStatus { SubscriptionStatus::Active }
    fn address(&self) -> Option<Url> { None }
    async fn unsubscribe(&mut self) -> Result<()>;
}
```

Rust has no base class, so what the TypeScript / Python `MailboxProvider` base class shares
lives in a reusable kernel instead: **`core::ProviderCore`** (delivery bookkeeping) plus the
pure vocabulary in **`delivery`** (`DeliveryResult`, `FailureReason`, `delivery_key`,
`should_requeue_after_broadcast`) and the retry math in **`retry`**. A provider that supports
subscriptions creates one `ProviderCore`, hands it its `ack` / `nack` implementations, and lets
it drive every delivery:

```rust
use mboxlabs_mailbox::core::{ProviderCore, ProviderCoreOptions};

let core = ProviderCore::new("mybus", ProviderCoreOptions {
    broadcast: true,                    // subscribe == broadcast: N deliveries per message
    nack: Some(my_nack_hook()),         // requeue / dead-letter per the settlement verdict
    ..ProviderCoreOptions::default()
});

// subscribe(): core keeps the callback and returns the listener to register on the bus
let (sub_id, listener) = core.register_subscription(address, callback);
bus.add(topic, listener.clone());
core.attach_unsubscribe(&sub_id, Box::new(move || remove(topic, listener)));
let subscription = core.subscription_object(sub_id);

// send(): dispatch and attach the settlement barrier — synchronously, before any await
let deliveries = bus.publish(topic, message.clone());
core.track_broadcast_settlement(message, deliveries);
```

Going through the core is what makes the semantics equal across the three ports: implicit ack on
success, retriable / fatal classification, per-delivery settlement, the broadcast barrier
(`should_requeue_after_broadcast`), opt-in redelivery of the failing subscriber only, the settle
watchdog, the `dead_letter` / `retry_exhausted` hooks, and cancelling pending retries on
unsubscribe / close. Providers that dispatch on their own must go through the core's delivery
entry (`deliver_to_local_subscription`) — calling the user callback directly means nobody
bookkeeps that delivery, so its failure neither requeues nor dead-letters. As a safety net, a
broadcast delivery that settles with no barrier attached falls back to a whole-message requeue
and logs a warning naming the provider.

`handle_subscription_error()` remains for providers that deliberately do their own handling; with
`ProviderCore` it would double-handle the same failure.

### Built-in Providers

- **MemoryProvider**: In-memory message bus for local communication
  - Topic-based routing
  - FIFO queue with manual/auto acknowledgment
  - Stale message requeueing, with requeue deduplication (a message is never queued twice)
  - Per-delivery settlement on top of `ProviderCore`: partial failure is not requeued, only a
    broadcast nobody succeeded goes back to the queue (one copy)
  - Opt-in subscription retry (`RetryPolicy`); by default a failed subscriber only requeues
    the message and nothing re-invokes the callback
  - Dead-letter / retry-exhausted hooks, and a settle watchdog for stuck callbacks
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
- ⚠️ No timers on `wasm32` (tokio's `time` feature is off for that target): retry backoff
  degrades to "redeliver without waiting" (`max_attempts` still bounds it) and the delivery
  settle watchdog is not armed. Enforce deadlines inside the subscription callback.

For detailed WASM testing instructions, examples, and troubleshooting, see [WASM_TESTING.md](WASM_TESTING.md).

## 🧪 Testing

Run the test suite:

```bash
cargo test
```

Tests include:
- Send and subscribe (including the `mbx-sent-at` header the provider contract requires)
- Fetch with auto-ack
- Fetch with manual-ack
- Nack with requeue
- Concurrent access
- In-flight visibility in `status()` and stale in-flight recovery
- Requeue deduplication and the retry helpers (`retry.rs`)
- Opt-in subscription retry: targeted redelivery, exhausted budget falls back to the queue,
  unsubscribing during a pending retry requeues the message, the exhausted hook fires
- Delivery settlement (`delivery.rs` + `providers/memory.rs`), mirroring the TypeScript suite:
  partial failure is not requeued, the dead-letter hook reports the dropped delivery by
  `subscription_id`, all-failed broadcasts requeue exactly one copy, the barrier waits for the
  slowest delivery, `post()` does not await settlement, unsubscribing never concludes for an
  in-flight callback, same-id concurrent posts settle independently, and the watchdog bounds
  stuck callbacks without cutting a healthy retry chain or the backoff

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
