use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use futures::lock::Mutex;
use futures::{SinkExt, StreamExt};

use crate::client::ClientTransport;
use crate::{FromServer, Message, ToServer};

use super::super::Result;

pub struct StompConnection {
    client: Arc<Mutex<ClientTransport>>,
    subscribes: HashMap<String, Box<dyn FnMut(Message<FromServer>) -> ControlFlow<()>>>,
}

impl StompConnection {
    pub fn new(transport: ClientTransport) -> StompConnection {
        let client = Arc::new(Mutex::new(transport));
        let conn = StompConnection {
            client: client.clone(),
            subscribes: HashMap::new(),
        };

        futures::executor::block_on(async move {
            loop {
                if let Ok(msg) = client
                    .clone()
                    .as_ref()
                    .lock()
                    .await
                    .next()
                    .await
                    .transpose()
                {
                    log::info!("StompConnection received {msg:?}")
                }
            }
        });

        // tokio::spawn(async move {
        //     loop {
        //         if let Ok(msg) = client.clone().as_ref()
        //             .lock().await
        //             .next().await
        //             .transpose() {
        //             log::info!("StompConnection received {msg:?}")
        //         }
        //     }
        // });

        conn
    }

    fn subscribe<F>(&mut self, channels: String, mut func: F) -> Result<()>
    where
        F: FnMut(Message<FromServer>) -> ControlFlow<()>,
    {
        // self.subscribes.insert(channels,  Box::new( func));
        Ok(())
    }

    pub async fn send(&mut self, frame: Message<ToServer>) -> Result<()> {
        self.client.clone().lock().await.send(frame).await?;
        Ok(())
    }

    async fn unsubscribe(&mut self, destination: String) {}
}
