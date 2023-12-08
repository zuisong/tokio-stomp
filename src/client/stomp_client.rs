use crate::{FromServer, Message, ToServer};
use std::ops::ControlFlow;

use super::super::Result;
use super::ClientTransport;

pub struct StompClient {
    transport: ClientTransport,
}

impl StompClient {
    async fn received_frame_handler<F>(handler: F)
    where
        F: FnMut(Message<FromServer>) -> ControlFlow<()>,
    {
        todo!()
    }

    async fn send_frame(frame: Message<ToServer>) -> Result<()> {
        todo!()
    }
}
