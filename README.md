# HardLight

A realtime, low-latency encrypted RPC binary protocol using WebSockets.

HardLight has two data models:

- RPC: a client connects to a server, and can call functions on the server
- Subscriptions: a client connects to a server, and can subscribe to events from the server

An example: multiple clients subscribe to a "chat" event using Subscriptions. Clients use an RPC function `send_message` to send a message, which will then persisted by the server and sent to all clients subscribed to the "chat" event.

Named after the fictional [Forerunner technology](https://www.halopedia.org/Hard_light) that "allows light to be transformed into a solid state, capable of bearing weight and performing a variety of tasks".

## Features

- **Feature sets**: depending on the context, certain endpoints can be disabled or enabled
  - Example: An unauthenticated client only has access to RPC methods to authenticate, and reconnects with authentication with the full set of RPC methods
- **Concurrent RPC**: up to 256 RPC calls can be occuring at the same time on a single connection
- **Subscriptions**: the server can push events to clients

## Why WebSockets?

WebSockets actually have very little abstraction over a TCP stream. From [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455#section-1.5):

> Conceptually, WebSocket is really just a layer on top of TCP that does the following:
>
> - adds a web origin-based security model for browsers
> - adds an addressing and protocol naming mechanism to support multiple services on one port and multiple host names on one IP address
> - layers a framing mechanism on top of TCP to get back to the IP packet mechanism that TCP is built on, but without length limits
> - includes an additional closing handshake in-band that is designed to work in the presence of proxies and other intermediaries

In effect, we gain the benefits of TLS, wide adoption & firewall support (it runs alongside HTTPS on TCP 443) while having little downsides. This means HardLight is usable in browsers, which was a requirement we had for the framework. In fact, we officially support using HardLight from browsers using the "wasm" feature.

At Valera, we use HardLight to connect clients to our servers, and for connecting some of our services to each another.

## Install

```shell
cargo add hardlight
```

## Usage

HardLight is designed to be simple, secure and fast. We take advantage of Rust's trait system to allow you to define your own RPC methods, and then use the `#[derive(RPC)]` macro to generate the necessary code to make it work.

Here's a very simple example of a counter service:

```rust
use hardlight::{RPC, Subscriptions};

/// These RPC methods are executed on the server and can be called by clients.
/// We'll store the counter in connection state, accessable using a HashMap-compatible API
#[derive(RPC)]
trait Counter {
    async fn increment(&self, amount: u32) -> u32;
    async fn decrement(&self, amount: u32) -> u32;
    async fn get(&self) -> u32;
}

/// These event types will be automatically pushed to clients
#[derive(Subscriptions)]
enum Subs {
    /// An event containing the new counter value
    Counter(u32)
}
```

The `Counter` trait is shared between clients and servers. Any inputs or outputs have to support rkyv's `Serialize`, `Deserialize`, `Archive` and `CheckBytes` traits. This means you can use any primitive type, or any type that implements these traits.

The `#[derive(RPC)]` macro will generate:

- a `Client` struct
- a `Handler` struct
- an enum, `Method`, of all the RPC method identifiers (e.g. `Increment`, `Decrement`, `Get`)
- input structs for each RPC method (e.g. `struct IncrementInput { amount: u32 }`)
- output types for each RPC method (e.g. `type IncrementOutput = u32`)

You'd be encouraged to put this trait in a separate crate or module, so you can use `Counter::Handler` and `Counter::Client` etc.

Both structs will expose methods for holding connection state in a hashmap. You can store anything serializable in the hashmap, and it will be available to all RPC methods. This is useful for storing authentication tokens, or other state that you want to be available to all RPC methods.

Subscriptions are a little more complex. They expand to traits for both the client and server, which are implemented by you. However, they don't have their own servers & clients - they're attached to RPC servers and clients.

The external runtime will handle networking, encryption and connection management.

## The `Handler`

The RPC handler generated by the macro expands to a struct that you implement your RPC methods on. It isn't a server itself, only exposing a function the runtime calls. Each connection has one handler.

This function has the signature `async fn handle_rpc_call(&mut self, method: Method, input: &[u8]) -> Result<Vec<u8>, Error>`. It deserializes any inputs, matches the method to the function, calls the appropriate method, and serializes the output.

### Connection state

As each connection has its own handler, we provide connection state in each handler's `self.state` using a `HashMap`. Here, you can control extra metadata for the connection. A typical use of this would be storing authentication data. Cookies are exposed here. However, this is slightly different from a regular HashMap:

- it's internally stored as a `parking_lot::Mutex`, as a handler can be called from multiple threads (which allows us to make RPC calls concurrently over a single connection)
- updates to the state will automatically propagate to the client as well

**Note:** Clients cannot update their state after the connection is established, but they can call RPC methods which can update the state from the server. This protects against race conditions.

### Implementing a handler

You then `impl Counter for Handler` to add your functionality. For example:

```rust
impl Counter for Handler {
    async fn increment(&self, amount: u32) -> u32 {
        // lock the state to the current thread
        let mut state = self.state.lock();
        // retrieve the counter from the state
        let mut counter = state.get("counter").unwrap_or(0);
        counter += amount;
        // store the counter in the state
        state.insert("counter", counter);
        counter
    } // the mutex will be automatically unlocked here

    async fn decrement(&self, amount: u32) -> u32 {
        let mut state = self.state.lock();
        let mut counter = state.get("counter").unwrap_or(0);
        counter -= amount;
        state.insert("counter", counter);
        counter
    }

    async fn get(&self) -> u32 {
        let state = self.state.lock();
        state.get("counter").unwrap_or(0)
    }
}
```

## Subscriptions

Subscriptions are a little different from other subscription models like GraphQL. Instead of the client setting up subscriptions to topics, you define the types of events that can be sent over HardLight connections. The server decides what and when to send events to clients, normally based on the connection state. This logic is handled by your app, not HardLight itself.

Other than that, subscriptions use a event-based interface. You attach handlers for each message type in the client.

Our general (conceptual) architecture at Valera looks like:

```console
UI <> Logic <> State <-------------> Logic <> State
|     frontend     |    HardLight    |  backend   |
```

We provide handlers to HardLight that modify state inside the frontend. The UI logic then updates the UI layer based on the state.
