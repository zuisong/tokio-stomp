use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_stomp::{client, server, FromServer, Message, ToServer};

async fn client(listens: &str, sends: &str, msg: &[u8]) -> Result<(), anyhow::Error> {
    let mut conn = client::connect(
        "127.0.0.1:8081",
        "/".to_string(),
        "guest".to_string().into(),
        "guest".to_string().into(),
    )
    .await?;

    conn.send(client::subscribe(listens, "myid")).await?;

    loop {
        conn.send(Message {
            content: ToServer::Send {
                destination: sends.into(),
                transaction: Some("11111".into()),
                body: Some(msg.to_vec()),
                content_type: None,
            },
            extra_headers: vec![],
        })
        .await?;
        let msg = conn.next().await.transpose()?;
        if let Some(msg) = msg {
            println!("Client Received{msg:?}");
        } else {
            anyhow::bail!("Unexpected: {:?}", msg)
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn server() -> Result<(), anyhow::Error> {
    server::listen("127.0.0.1:8081").await?;
    return Ok(());
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let fut1 = Box::pin(server());
    let fut2 = Box::pin(client("pong", "ping", b"PING!"));

    let (res, _) = futures::future::select(fut1, fut2).await.factor_first();
    res
}
