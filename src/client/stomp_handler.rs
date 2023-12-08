use crate::client::ClientTransport;
use crate::{FromServer, Message, ToServer};

struct StompHandler {
    transport: ClientTransport,
}

impl StompHandler {
    // fn on_connect<F>(
    //     f: F
    // ) where F: FnOnce(FromServer) -> () {}

    async fn subscribe<F>(&mut self, destination: String, call_back: F) where F: FnMut(Message<FromServer>) -> () {}
    async fn unsubscribe(&mut self, destination: String) {}
    async fn ack(&mut self, message: Message<ToServer>){}
}