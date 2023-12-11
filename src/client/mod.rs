mod stomp_handler;

use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;

use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed};

pub type ClientTransport = Framed<TcpStream, ClientCodec>;

use crate::frame::{self, Frame};
use crate::{FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};

/// Connect to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
pub async fn connect(
    server: impl tokio::net::ToSocketAddrs,
    host: impl Into<String>,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<ClientTransport> {
    let tcp = TcpStream::connect(server).await?;
    let mut transport = ClientCodec.framed(tcp);
    client_handshake(&mut transport, host.into(), login, passcode).await?;
    Ok(transport)
}

async fn client_handshake(
    transport: &mut ClientTransport,
    host: String,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<()> {
    let connect = Message {
        content: ToServer::Connect {
            accept_version: "1.2".into(),
            host,
            login,
            passcode,
            heartbeat: None,
        },
        extra_headers: vec![],
    };
    // Send the message
    transport.send(connect).await?;
    // Receive reply
    let msg = transport.next().await.transpose()?;
    if let Some(FromServer::Connected { .. }) = msg.as_ref().map(|m| &m.content) {
        Ok(())
    } else {
        Err(anyhow!("unexpected reply: {:?}", msg))
    }
}

/// Convenience function to build a Subscribe message
pub fn subscribe(dest: impl Into<String>, id: impl Into<String>) -> Message<ToServer> {
    Message {
        content: ToServer::Subscribe {
            destination: dest.into(),
            id: id.into(),
            ack: None,
        },
        extra_headers: vec![],
    }
}
pub struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = Message<FromServer>;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        let (item, offset) = match frame::parse_frame(&src) {
            Ok((remain, frame)) => (
                Message::<FromServer>::try_from(&frame),
                remain.as_ptr() as usize - src.as_ptr() as usize,
            ),
            Err(nom::Err::Incomplete(_)) => return Ok(None),
            Err(e) => bail!("Parse failed: {:?}", e),
        };
        src.advance(offset);
        item.map(|v| Some(v))
    }
}

impl Encoder<Message<ToServer>> for ClientCodec {
    type Error = anyhow::Error;

    fn encode(
        &mut self,
        item: Message<ToServer>,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        let f: Frame = (&item).into();

        f.serialize(dst);
        Ok(())
    }
}
