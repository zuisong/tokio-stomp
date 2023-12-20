use futures::SinkExt;
use log::info;
use std::time::Duration;
use tokio_stomp::{client, server};

async fn client(listens: &str, _sends: &str, _msg: &[u8]) -> Result<(), anyhow::Error> {
    let conn = client::connect(
        "127.0.0.1:8081",
        "/".to_string(),
        "guest".to_string().into(),
        "guest".to_string().into(),
    )
    .await?;

    let mut conn = client::stomp_connection::StompConnection::new(conn);

    conn.send(client::subscribe(listens, "myid")).await?;

    loop {
        // conn.send(
        //     Message {
        //         content: ToServer::Send {
        //             destination: sends.into(),
        //             transaction: Some("11111".into()),
        //             body: Some(msg.to_vec()),
        //             content_type: None,
        //         },
        //         extra_headers: vec![],
        //     })
        //     .await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn server() -> Result<(), anyhow::Error> {
    server::listen("127.0.0.1:8081").await?;
    return Ok(());
}

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_thread_names(true).with_line_number(true))
        .init();

    info!("main");

    let fut1 = Box::pin(server());
    let fut2 = Box::pin(client("pong", "ping", b"PING!"));

    let (res, _) = futures::future::select(fut1, fut2).await.factor_first();
    res
}
