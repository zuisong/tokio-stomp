use std::time::Duration;

use anyhow::{anyhow, bail};
use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Decoder, Encoder, Framed};

use crate::frame::{self, Frame};
use crate::{FromServer, Message, Result, ToServer};

pub type ServerTransport = Framed<TcpStream, ServerCodec>;

/// Connect to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
// pub async fn connect(
//     server: impl Into<String>,
//     host: impl Into<String>,
//     login: Option<String>,
//     passcode: Option<String>,
// ) -> Result<ClientTransport> {
//     let address: String = server.into();
//     let addr = address.as_str().to_socket_addrs().unwrap().next().unwrap();
//     let tcp = TcpStream::connect(&addr).await?;
//     let mut transport = ServerCodec.framed(tcp);
//     client_handshake(&mut transport, host.into(), login, passcode).await?;
//     Ok(transport)
// }

async fn process_socket(tcp: TcpStream) -> Result<()> {
    let mut transport = ServerCodec.framed(tcp);
    match server_handshake(&mut transport).await {
        Ok(_) => {}
        Err(_) => {
            transport
                .send(Message {
                    content: FromServer::Error {
                        message: None,
                        content_type: None,
                        body: None,
                    },
                    extra_headers: vec![],
                })
                .await?;
            transport.close().await?;
        }
    };

    let (mut sink, mut stream) = transport.split();
    tokio::spawn(async move {
        loop {
            let msg = stream.next().await.transpose().unwrap();
            log::info!("server received {msg:?}")
        }
    });
    tokio::spawn(async move {
        loop {
            sink.send(Message {
                content: FromServer::Message {
                    destination: format!("now: {:#?}", std::time::Instant::now()),
                    message_id: env!("CARGO_PKG_VERSION").to_string(),
                    subscription: "11".to_string(),
                    content_type: Some("application/json".into()),
                    body: None,
                },
                extra_headers: vec![(
                    "server".to_string(),
                    format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
                )],
            })
            .await
            .unwrap();
            log::info!("server send a message");

            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    })
    .await?;

    log::error!("tokio server exit!");

    Ok(())
}

pub async fn listen(addr: impl tokio::net::ToSocketAddrs) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (socket, client_addr) = listener.accept().await?;
        log::info!(" connected from {client_addr}");
        process_socket(socket).await?;
    }
}

async fn server_handshake(transport: &mut ServerTransport) -> Result<()> {
    let connected = Message {
        content: FromServer::Connected {
            version: "1.2".into(),
            server: Some(
                format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")).into(),
            ),
            session: None,
            heartbeat: None,
        },
        extra_headers: vec![],
    };
    // Receive reply
    let msg = transport.next().await.transpose()?;
    if let Some(ToServer::Connect { .. }) = msg.as_ref().map(|m| &m.content) {
        // Send the message
        transport.send(connected).await?;
        Ok(())
    } else {
        Err(anyhow!("unexpected reply: {:?}", msg))
    }
}

pub struct ServerCodec;

impl Decoder for ServerCodec {
    type Item = Message<ToServer>;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        let (item, offset) = match frame::parse_frame(&src) {
            Ok((remain, frame)) => (
                Message::<ToServer>::try_from(frame),
                remain.as_ptr() as usize - src.as_ptr() as usize,
            ),
            Err(nom::Err::Incomplete(_)) => return Ok(None),
            Err(e) => bail!("Parse failed: {:?}", e),
        };
        src.advance(offset);
        item.map(|v| Some(v))
    }
}

impl Encoder<Message<FromServer>> for ServerCodec {
    type Error = anyhow::Error;

    fn encode(
        &mut self,
        item: Message<FromServer>,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        let f: Frame = (item).into();
        f.serialize(dst);
        Ok(())
    }
}
