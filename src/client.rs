use std::{str::FromStr, sync::Arc, time::SystemTime};

use futures_util::{SinkExt, StreamExt};
use rustls_native_certs::load_native_certs;
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_rustls::rustls::{
    client::{ServerCertVerified, ServerCertVerifier},
    Certificate, ClientConfig as TLSClientConfig, RootCertStore, ServerName,
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        error::ProtocolError,
        handshake::client::generate_key,
        http::{HeaderValue, Request},
        Error, Message,
    },
    Connector,
};
use tracing::{debug, error, span, warn, Level};
use version::Version;

use crate::{
    server::{HandlerResult, HL_VERSION},
    wire::{ClientMessage, RpcHandlerError, ServerMessage},
};

pub struct ClientConfig {
    tls: TLSClientConfig,
    host: String,
}

pub trait State {
    fn apply_changes(&mut self, changes: Vec<(String, Vec<u8>)>) -> HandlerResult<()>;
}

pub struct Client<T>
where
    T: State + Default,
{
    config: ClientConfig,
    state: T,
    hl_version_string: HeaderValue,
}

impl<T> Client<T>
where
    T: State + Default,
{
    /// Creates a new client that doesn't verify the server's certificate.
    pub fn new_self_signed(host: &str) -> Self {
        let tls = TLSClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {}))
            .with_no_client_auth();
        let config = ClientConfig {
            tls,
            host: host.to_string(),
        };
        Self::new_with_config(config)
    }

    /// Create a new client using the system's root certificates.
    pub fn new(host: &str) -> Self {
        let mut root_store = RootCertStore::empty();
        for cert in load_native_certs().unwrap() {
            root_store.add(&Certificate(cert.0)).unwrap();
        }
        let tls = TLSClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let config = ClientConfig {
            tls,
            host: host.to_string(),
        };
        Self::new_with_config(config)
    }

    /// Create a new client using the given configuration.
    pub fn new_with_config(config: ClientConfig) -> Self {
        let version = Version::from_str(HL_VERSION).unwrap();
        Self {
            config,
            state: T::default(),
            hl_version_string: format!("hl/{}", version.major).parse().unwrap(),
        }
    }

    pub async fn connect(
        &mut self,
        // Allows the application's wrapping client to shut down the connection
        mut shutdown: oneshot::Receiver<()>,
        // Sends control channels to the application so it can send RPC calls,
        // events, and other things to the server.
        control_channels_tx: oneshot::Sender<(
            mpsc::Sender<(Vec<u8>, oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>)>,
        )>,
        // This will send immediately once the client has connected to the server.
        // The client is guaranteed to not return an error after this is sent
        // so it is safe to ignore the result.
        ok_tx: oneshot::Sender<()>,
    ) -> Result<(), Error> {
        let span = span!(Level::DEBUG, "connection", host = self.config.host);
        let _enter = span.enter();

        let connector = Connector::Rustls(Arc::new(self.config.tls.clone()));

        let req = Request::builder()
            .method("GET")
            .header("Host", self.config.host.clone())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key())
            .header("Sec-WebSocket-Protocol", self.hl_version_string.clone())
            .uri(format!("wss://{}/", self.config.host))
            .body(())
            .expect("Failed to build request");

        debug!("Connecting to server...");
        let (mut stream, res) = connect_async_tls_with_config(req, None, Some(connector)).await?;

        let protocol = res.headers().get("Sec-WebSocket-Protocol");
        if protocol.is_none() || protocol.unwrap() != &self.hl_version_string {
            error!("Received bad version from server. Wanted {:?}, got {:?}", self.hl_version_string, protocol);
            return Err(Error::Protocol(ProtocolError::HandshakeIncomplete));
        }
        
        debug!("Connected to server. Sending ok to application...");
        ok_tx.send(()).unwrap();
        debug!("Ok sent.");
        debug!("Sending control channels to application...");
        let (rpc_tx, mut rpc_rx) = mpsc::channel(10);
        control_channels_tx.send((rpc_tx,)).unwrap();
        debug!("Control channels sent.");

        // keep track of active RPC calls
        // we have to do this dumb thing because we can't copy a oneshot::Sender
        let mut active_rpc_calls: [Option<oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>>; 256] = [
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None,
        ];

        debug!("Starting RPC handler loop");
        loop {
            select! {
                // await RPC requests from the application
                Some((internal, completion_tx)) = rpc_rx.recv() => {
                    debug!("Received RPC request from application");
                    // find a free rpc id
                    if let Some(id) = active_rpc_calls.iter().position(|x| x.is_none()) {
                        let span = span!(Level::DEBUG, "rpc", id = id as u8);
                        let _enter = span.enter();
                        debug!("Found free RPC id");

                        let msg = ClientMessage::RPCRequest {
                            id: id as u8,
                            internal
                        };

                        let binary = match rkyv::to_bytes::<ClientMessage, 1024>(&msg) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                warn!("Failed to serialize RPC call. Ignoring. Error: {e}");
                                // we don't care if the receiver has dropped
                                let _ = completion_tx.send(Err(RpcHandlerError::BadInputBytes));
                                continue
                            }
                        }.to_vec();

                        debug!("Sending RPC call to server");

                        match stream.send(Message::Binary(binary)).await {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to send RPC call. Ignoring. Error: {e}");
                                // we don't care if the receiver has dropped
                                let _ = completion_tx.send(Err(RpcHandlerError::ClientNotConnected));
                                continue
                            }
                        }

                        debug!("RPC call sent to server");

                        active_rpc_calls[id] = Some(completion_tx);
                    } else {
                        warn!("No free RPC id available. Responding with an error.");
                        let _ = completion_tx.send(Err(RpcHandlerError::TooManyCallsInFlight));
                    }
                }
                // await RPC responses from the server
                Some(msg) = stream.next() => {
                    if let Ok(msg) = msg {
                        if let Message::Binary(bytes) = msg {
                            let msg: ServerMessage = match rkyv::from_bytes(&bytes) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    warn!("Received invalid RPC response. Ignoring. Error: {e}");
                                    continue;
                                }
                            };
                            match msg {
                                ServerMessage::RPCResponse { id, output } => {
                                    let span = span!(Level::DEBUG, "rpc", id = id as u8);
                                    let _enter = span.enter();
                                    debug!("Received RPC response from server");
                                    if let Some(completion_tx) = active_rpc_calls[id as usize].take() {
                                        let _ = completion_tx.send(output);
                                    } else {
                                        warn!("Received RPC response for unknown RPC call. Ignoring.");
                                    }
                                }
                                ServerMessage::StateChange(changes) => {
                                    let span = span!(Level::DEBUG, "state_change");
                                    let _enter = span.enter();
                                    debug!("Received {} state change(s) from server", changes.len());
                                    if let Err(e) = self.state.apply_changes(changes) {
                                        warn!("Failed to apply state changes. Error: {:?}", e);
                                    };
                                }
                                ServerMessage::NewEvent { .. } => {
                                    warn!("NewEvent has not been implemented yet. Ignoring.")
                                }
                            }
                        }
                    }
                }
                // await shutdown signal
                _ = &mut shutdown => {
                    break;
                }
            }
        }

        debug!("RPC handler loop exited.");
        Ok(())
    }

    pub fn state(&self) -> &T {
        &self.state
    }
}

struct NoCertificateVerification {}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}
