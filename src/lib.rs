//! tokio-stomp - A library for asynchronous streaming of STOMP messages

extern crate core;

use custom_debug_derive::Debug as CustomDebug;

pub mod client;
mod frame;
pub mod server;

pub(crate) type Result<T> = anyhow::Result<T>;

/// A representation of a STOMP frame
#[derive(CustomDebug)]
pub struct Message<T> {
    /// The message content
    pub content: T,
    /// Headers present in the frame which were not required by the content
    pub extra_headers: Vec<(String, String)>,
}

fn pretty_bytes(b: &Option<Vec<u8>>, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    if let Some(v) = b {
        write!(f, "{}", String::from_utf8_lossy(v))
    } else {
        write!(f, "None")
    }
}

/// A STOMP message sent from the server
/// See the [Spec](https://stomp.github.io/stomp-specification-1.2.html) for more information
#[derive(CustomDebug, Clone)]
pub enum FromServer {
    #[doc(hidden)] // The user shouldn't need to know about this one
    Connected {
        version: String,
        session: Option<String>,
        server: Option<String>,
        heartbeat: Option<(u32, u32)>,
    },
    /// Conveys messages from subscriptions to the client
    Message {
        destination: String,
        message_id: String,
        subscription: String,
        content_type: Option<String>,
        #[debug(with = "pretty_bytes")]
        body: Option<Vec<u8>>,
    },
    /// Sent from the server to the client once a server has successfully
    /// processed a client frame that requests a receipt
    Receipt { receipt_id: String },
    /// Something went wrong. After sending an Error, the server will close the connection
    Error {
        message: Option<String>,
        content_type: Option<String>,
        #[debug(with = "pretty_bytes")]
        body: Option<Vec<u8>>,
    },
}

/// A STOMP message sent by the client.
/// See the [Spec](https://stomp.github.io/stomp-specification-1.2.html) for more information
#[derive(CustomDebug, Clone)]
pub enum ToServer {
    #[doc(hidden)] // The user shouldn't need to know about this one
    Connect {
        accept_version: String,
        host: String,
        login: Option<String>,
        passcode: Option<String>,
        heartbeat: Option<(u32, u32)>,
    },
    /// Send a message to a destination in the messaging system
    Send {
        destination: String,
        transaction: Option<String>,
        content_type: Option<String>,
        #[debug(with = "pretty_bytes")]
        body: Option<Vec<u8>>,
    },
    /// Register to listen to a given destination
    Subscribe {
        destination: String,
        id: String,
        ack: Option<AckMode>,
    },
    /// Remove an existing subscription
    Unsubscribe { id: String },
    /// Acknowledge consumption of a message from a subscription using
    /// 'client' or 'client-individual' acknowledgment.
    Ack {
        // TODO ack and nack should be automatic?
        id: String,
        transaction: Option<String>,
    },
    /// Notify the server that the client did not consume the message
    Nack {
        id: String,
        transaction: Option<String>,
    },
    /// Start a transaction
    Begin { transaction: String },
    /// Commit an in-progress transaction
    Commit { transaction: String },
    /// Roll back an in-progress transaction
    Abort { transaction: String },
    /// Gracefully disconnect from the server
    /// Clients MUST NOT send any more frames after the DISCONNECT frame is sent.
    Disconnect { receipt: Option<String> },
}

#[derive(Debug, Clone, Copy)]
pub enum AckMode {
    Auto,
    Client,
    ClientIndividual,
}
