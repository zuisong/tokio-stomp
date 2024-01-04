use std::any::Any;
use testcontainers::{clients, GenericImage};

#[tokio::test]
async fn test_connect() {
    let docker = clients::Cli::default();
    let redis = docker.run(GenericImage::new(
        "apache/activemq-artemis",
        "latest-alpine",
    ));
    dbg!(&redis.ports());

    let port = redis.ports().map_to_host_port_ipv4(61613).unwrap();
    dbg!(port);
    tokio::time::sleep(Duration::from_secs(10)).await;
    connect(port).await.unwrap();
    dbg!(&redis.ports());
    docker.type_id();
}

use std::net::Ipv4Addr;
use std::time::Duration;

use futures::future::ok;
use futures::prelude::*;
use tokio_stomp::*;

async fn connect(port: u16) -> Result<(), anyhow::Error> {
    let conn = client::connect(
        (Ipv4Addr::LOCALHOST, port),
        "/".to_string(),
        "artemis".to_string().into(),
        "artemis".to_string().into(),
    )
    .await?;
    dbg!(111);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let (mut sink, stream) = conn.split();

    let fut1 = async move {
        sink.send(client::subscribe("rusty", "myid")).await?;
        println!("Subscribe sent");

        tokio::time::sleep(Duration::from_millis(200)).await;

        sink.send(Message {
            content: ToServer::Send {
                destination: "rusty".into(),
                transaction: None,
                content_type: None,
                body: Some(b"Hello there rustaceans!".to_vec()),
            },
            extra_headers: vec![],
        })
        .await?;
        println!("Message sent");

        tokio::time::sleep(Duration::from_millis(200)).await;

        sink.send(Message {
            content: ToServer::Unsubscribe { id: "myid".into() },
            extra_headers: vec![],
        })
        .await?;
        println!("Unsubscribe sent");

        tokio::time::sleep(Duration::from_millis(200)).await;

        tokio::time::sleep(Duration::from_secs(1)).await;
        sink.send(Message {
            content: ToServer::Disconnect { receipt: None },
            extra_headers: vec![],
        })
        .await?;
        println!("Disconnect sent");

        Ok(())
    };

    // Listen from the main thread. Once the Disconnect message is sent from
    // the sender thread, the server will disconnect the client and the future
    // will resolve, ending the program
    let fut2 = stream.try_for_each(|item| {
        if let FromServer::Message { body, .. } = item.content {
            println!(
                "Message received: {:?}",
                String::from_utf8_lossy(&body.unwrap())
            );
        } else {
            println!("{:?}", item);
        }
        ok(())
    });

    futures::future::select(Box::pin(fut1), Box::pin(fut2))
        .await
        .factor_first()
        .0
}
