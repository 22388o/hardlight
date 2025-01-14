// have to use this as rust doesn't have a stablised feature in nightly yet
// see: https://github.com/rust-lang/rust/issues/91611
use async_trait::async_trait;
use hardlight::{
    tungstenite, Client, Handler, HandlerResult, RpcHandlerError, Server, ServerConfig, State,
    StateUpdateChannel,
};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tracing::{debug, error, info};

use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

use parking_lot::{Mutex, MutexGuard};

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt::init();

    info!("Starting server on localhost:8080");
    let config = ServerConfig::new_self_signed("localhost:8080");
    info!("Config: {:?}", config);
    let server = Server::new(config, CounterHandler::init());

    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // wait for the server to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let mut client = CounterClient::new_self_signed("localhost:8080");
    client.connect().await.unwrap();

    let first_value = client.get().await.expect("get failed");
    let num_tasks = 12;
    let num_increments_per_task = 100;
    info!("Incrementing counter using {num_tasks} tasks with {num_increments_per_task} increments each");
    info!("First value: {}", first_value);

    let counter = Arc::new(client);


    let mut tasks = Vec::new();
    for _ in 0..num_tasks {
        let counter = counter.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..num_increments_per_task {
                let start = tokio::time::Instant::now();
                let _ = counter.increment(1).await;
                let elapsed = start.elapsed();
                info!("Task: {:?}", elapsed);
            }
        }));
    }
    
    for task in tasks {
        task.await.expect("task failed");
    }
    

    let final_value = counter.get().await.expect("get failed");

    info!("Final value: {}", final_value);

    // make sure server-side mutex is working...
    assert!(final_value == first_value + (num_tasks * num_increments_per_task) as u32);

    Ok(())
}

#[async_trait]
trait Counter {
    async fn increment(&self, amount: u32) -> HandlerResult<u32>;
    async fn decrement(&self, amount: u32) -> HandlerResult<u32>;
    // We'll deprecate this at some point as we can just send it using Events
    async fn get(&self) -> HandlerResult<u32>;
}

#[derive(Clone, Default)]
struct CounterState {
    counter: u32,
}

// enum Events {
//     Increment(u32),
//     Decrement(u32),
// }

// currently implementing everything manually to work out what functionality
// the macros will need to provide

// runtime and stuff we can put in the root project and keep those as we move
// along but no macros for time being

// RPC server that implements the Counter trait
struct CounterHandler {
    // the runtime will provide the state when it creates the handler
    state: Arc<CounterConnectionState>,
}

impl CounterHandler {
    fn init(
    ) -> impl Fn(StateUpdateChannel) -> Box<dyn Handler + Send + Sync> + Send + Sync + 'static + Copy
    {
        |state_update_channel| Box::new(Self::new(state_update_channel))
    }
}

// generated argument structs

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct IncrementArgs {
    amount: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct DecrementArgs {
    amount: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct RpcCall {
    method: Method,
    args: Vec<u8>,
}

#[async_trait]
impl Handler for CounterHandler {
    fn new(state_update_channel: StateUpdateChannel) -> Self {
        Self {
            state: Arc::new(CounterConnectionState::new(state_update_channel)),
        }
    }

    async fn handle_rpc_call(&self, input: &[u8]) -> Result<Vec<u8>, RpcHandlerError> {
        let call: RpcCall = rkyv::from_bytes(input).map_err(|_| RpcHandlerError::BadInputBytes)?;

        match call.method {
            Method::Increment => {
                let args: IncrementArgs =
                    rkyv::from_bytes(&call.args).map_err(|_| RpcHandlerError::BadInputBytes)?;
                let result = self.increment(args.amount).await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            Method::Decrement => {
                let args: DecrementArgs =
                    rkyv::from_bytes(&call.args).map_err(|_| RpcHandlerError::BadInputBytes)?;
                let result = self.decrement(args.amount).await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            Method::Get => {
                let result = self.get().await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
        }
    }
}

#[async_trait]
impl Counter for CounterHandler {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        // lock the state to the current thread
        let mut state: StateGuard = self.state.lock();
        state.counter += amount;
        Ok(state.counter)
    } // state is automatically unlocked here; any changes are sent to the client
      // automagically ✨

    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        let mut state = self.state.lock();
        state.counter -= amount;
        Ok(state.counter)
    }

    async fn get(&self) -> HandlerResult<u32> {
        let state = self.state.lock();
        Ok(state.counter)
    }
}

/// ConnectionState is a wrapper around the user's state that will be the
/// "owner" of a connection's state
struct CounterConnectionState {
    /// CounterState is locked under an internal mutex so multiple threads can
    /// use it safely
    state: Mutex<CounterState>,
    /// The channel is given by the runtime when it creates the connection,
    /// allowing us to tell the runtime when the connection's state is modified
    /// so it can send the changes to the client automatically
    channel: Arc<mpsc::Sender<Vec<(String, Vec<u8>)>>>,
}

impl CounterConnectionState {
    fn new(channel: StateUpdateChannel) -> Self {
        Self {
            // use default values for the state
            state: Mutex::new(Default::default()),
            channel: Arc::new(channel),
        }
    }

    /// locks the state to the current thread by providing a StateGuard
    /// the StateGuard gets
    fn lock(&self) -> StateGuard {
        let state = self.state.lock();
        StateGuard {
            starting_state: state.clone(),
            state,
            channel: self.channel.clone(),
        }
    }
}

/// StateGuard is effectively a MutexGuard that sends any changes back to the
/// runtime when it's dropped. We have to generate it in a custom way because
struct StateGuard<'a> {
    /// The StateGuard is given ownership of a lock to the state
    state: MutexGuard<'a, CounterState>,
    /// A copy of the state before we locked it
    /// We use this to compare changes when the StateGuard is dropped
    starting_state: CounterState,
    /// A channel pointer that we can use to send changes to the runtime
    /// which will handle sending them to the client
    channel: Arc<mpsc::Sender<Vec<(String, Vec<u8>)>>>,
}

impl<'a> Drop for StateGuard<'a> {
    /// Our custom drop implementation will send any changes to the runtime
    fn drop(&mut self) {
        // "diff" the two states to see what changed
        let mut changes = Vec::new();

        if self.state.counter != self.starting_state.counter {
            changes.push((
                "counter".to_string(),
                rkyv::to_bytes::<u32, 1024>(&self.state.counter)
                    .unwrap()
                    .to_vec(),
            ));
        }

        // if there are no changes, don't bother sending anything
        if changes.is_empty() {
            return;
        }

        // send the changes to the runtime
        // we have to spawn a new task because we can't await inside a drop
        let channel = self.channel.clone();
        tokio::spawn(async move {
            channel.send(changes).await.unwrap();
        });
    }
}

// the Deref and DerefMut traits allow us to use the StateGuard as if it were a
// CounterState (e.g. state.counter instead of state.state.counter)
impl Deref for StateGuard<'_> {
    type Target = CounterState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

// RPC client that implements the Counter trait
struct CounterClient {
    host: String,
    self_signed: bool,
    shutdown: Option<oneshot::Sender<()>>,
    rpc_tx: Option<mpsc::Sender<(Vec<u8>, oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>)>>,
}

impl CounterClient {
    pub fn new_self_signed(host: &str) -> Self {
        Self {
            host: host.to_string(),
            self_signed: true,
            shutdown: None,
            rpc_tx: None,
        }
    }

    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            self_signed: false,
            shutdown: None,
            rpc_tx: None,
        }
    }

    pub async fn connect(&mut self) -> Result<(), tungstenite::Error> {
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (control_channels_tx, control_channels_rx) = oneshot::channel();
        let (error_tx, error_rx) = oneshot::channel();
        let (ok_tx, ok_rx) = oneshot::channel();

        let self_signed = self.self_signed;
        let host = self.host.clone();

        tokio::spawn(async move {
            let mut client: Client<CounterState> = if self_signed {
                Client::new_self_signed(&host)
            } else {
                Client::new(&host)
            };

            if let Err(e) = client
                .connect(shutdown_rx, control_channels_tx, ok_tx)
                .await
            {
                error_tx.send(e).unwrap()
            };
        });

        select! {
            _ = ok_rx => {
                // at this point, the client will NOT return any errors, so we
                // can safely ignore the error_rx channel
                debug!("Ok received from client")
            }
            e = error_rx => {
                error!("Error received from client: {:?}", e);
                return Err(e.unwrap());
            }
        }

        let (rpc_tx,) = control_channels_rx.await.unwrap();

        self.shutdown = Some(shutdown);
        self.rpc_tx = Some(rpc_tx);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        match self.shutdown.take() {
            Some(shutdown) => {
                let _ = shutdown.send(());
            }
            None => {}
        }
    }

    async fn handle_rpc_call(&self, method: Method, args: Vec<u8>) -> HandlerResult<Vec<u8>> {
        if let Some(rpc_chan) = self.rpc_tx.clone() {
            let (tx, rx) = oneshot::channel();
            rpc_chan
                .send((
                    rkyv::to_bytes::<RpcCall, 1024>(&RpcCall { method, args })
                        .map_err(|_| RpcHandlerError::BadInputBytes)?
                        .to_vec(),
                    tx,
                ))
                .await
                .unwrap();
            rx.await.unwrap()
        } else {
            Err(RpcHandlerError::ClientNotConnected)
        }
    }
}

impl Drop for CounterClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[async_trait]
impl Counter for CounterClient {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        match self
            .handle_rpc_call(
                Method::Increment,
                rkyv::to_bytes::<IncrementArgs, 1024>(&IncrementArgs { amount })
                    .map_err(|_| RpcHandlerError::BadInputBytes)?
                    .to_vec(),
            )
            .await
        {
            Ok(c) => rkyv::from_bytes(&c).map_err(|_| RpcHandlerError::BadOutputBytes),
            Err(e) => Err(e),
        }
    }
    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        match self
            .handle_rpc_call(
                Method::Decrement,
                rkyv::to_bytes::<DecrementArgs, 1024>(&DecrementArgs { amount })
                    .map_err(|_| RpcHandlerError::BadInputBytes)?
                    .to_vec(),
            )
            .await
        {
            Ok(c) => rkyv::from_bytes(&c).map_err(|_| RpcHandlerError::BadOutputBytes),
            Err(e) => Err(e),
        }
    }
    // We'll deprecate this at some point as we can just send it using Events
    async fn get(&self) -> HandlerResult<u32> {
        match self.handle_rpc_call(Method::Get, vec![]).await {
            Ok(c) => rkyv::from_bytes(&c).map_err(|_| RpcHandlerError::BadOutputBytes),
            Err(e) => Err(e),
        }
    }
}

impl State for CounterState {
    fn apply_changes(&mut self, changes: Vec<(String, Vec<u8>)>) -> HandlerResult<()> {
        for (field, new_value) in changes {
            match field.as_ref() {
                "counter" => {
                    self.counter =
                        rkyv::from_bytes(&new_value).map_err(|_| RpcHandlerError::BadInputBytes)?
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// we need to be able to serialise and deserialise the method enum
// so we can match it on the server side
#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
#[repr(u8)]
/// The RPC method to call on the server
enum Method {
    Increment,
    Decrement,
    Get,
}
