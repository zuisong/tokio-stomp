use anyhow::{anyhow, bail};
use bytes::{BufMut, BytesMut};
use nom::{
    branch::alt,
    bytes::complete::{escaped_transform, take_while_m_n},
    bytes::streaming::{is_not, tag, take, take_until},
    character::streaming::{alpha1, line_ending, not_line_ending},
    combinator::{complete, opt},
    combinator::{map_res, value},
    multi::{count, many0, many_till},
    sequence::{delimited, separated_pair, terminated, tuple},
    IResult, Parser,
};

use crate::{AckMode, FromServer, Message, Result, ToServer};
use custom_debug_derive::Debug as CustomDebug;
use std::borrow::Cow;

#[derive(CustomDebug)]
pub(crate) struct Frame<'a> {
    command: Cow<'a, str>,
    headers: Vec<(String, String)>,
    body: Option<Cow<'a, [u8]>>,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(
        command: impl Into<Cow<'a, str>>,
        headers: Vec<(&str, impl Into<Option<String>>)>,
        body: Option<Vec<u8>>,
        extra_headers: Vec<(String, String)>,
    ) -> Frame<'a> {
        let mut headers: Vec<_> = headers
            .into_iter()
            .filter_map(|(k, vv)| vv.into().map(|v| (k.to_string(), v)))
            .collect();

        headers.extend(extra_headers);

        Frame {
            command: command.into(),
            headers,
            body: body.map(Cow::Owned),
        }
    }

    pub(crate) fn serialize(&self, buffer: &mut BytesMut) {
        fn write_escaped(b: u8, buffer: &mut BytesMut) {
            let escaped: &[u8] = match b {
                b'\r' => b"\\r",
                b'\n' => b"\\n",
                b':' => b"\\c",
                b'\\' => b"\\\\",
                b => return buffer.put_u8(b),
            };
            buffer.put_slice(escaped)
        }
        let requires = self.command.len()
            + self.body.as_ref().map(|b| b.len() + 20).unwrap_or(0)
            + self
                .headers
                .iter()
                .fold(0, |acc, &(ref k, ref v)| acc + k.len() + v.len())
            + 30;
        if buffer.remaining_mut() < requires {
            buffer.reserve(requires);
        }
        buffer.put_slice(self.command.as_bytes());
        buffer.put_u8(b'\n');
        self.headers.iter().for_each(|(key, ref val)| {
            for byte in key.as_bytes() {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b':');
            for byte in val.as_bytes() {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b'\n');
        });
        if let Some(ref body) = self.body {
            buffer.put_slice(&get_content_length_header(&body));
            buffer.put_u8(b'\n');
            buffer.put_slice(body);
        } else {
            buffer.put_u8(b'\n');
        }
        buffer.put_u8(b'\x00');
    }
}

fn get_content_length(headers: &Vec<(String, String)>) -> Option<usize> {
    for (name, value) in headers {
        if name.as_str() == "content-length" {
            return value.parse::<usize>().ok();
        }
    }
    None
}

fn map_empty_slice(s: &[u8]) -> Option<&[u8]> {
    Some(s).filter(|c| !c.is_empty())
}

pub(crate) fn parse_frame(input: &[u8]) -> IResult<&[u8], Frame> {
    // dbg!(&String::from_utf8_lossy(input));
    // read stream until header end
    // drop result for save memory
    many_till(take(1_usize).map(drop), count(line_ending, 2))(input)?;

    let (input, (command, headers)) = tuple((
        delimited(
            opt(complete(line_ending)),
            alpha1.map(String::from_utf8_lossy),
            line_ending,
        ), // command
        terminated(
            many0(parse_header), // header
            line_ending,
        ),
    ))(input)?;

    let (input, body) = match get_content_length(&headers) {
        None => take_until("\x00").map(map_empty_slice).parse(input)?,
        Some(length) => take(length).map(Some).parse(input)?,
    };

    let (input, _) = tuple((tag("\x00"), opt(complete(line_ending))))(input)?;

    Ok((
        input,
        Frame {
            command,
            headers,
            body: body.map(Cow::Borrowed),
        },
    ))
}

fn parse_header(input: &[u8]) -> IResult<&[u8], (String, String)> {
    complete(separated_pair(
        is_not(":\r\n").and_then(unescape),
        tag(":"),
        terminated(not_line_ending, line_ending).and_then(unescape),
    ))
    .parse(input)
}

fn unescape(input: &[u8]) -> IResult<&[u8], String> {
    let mut f = map_res(
        escaped_transform(
            take_while_m_n(1, 1, |c| c != b'\\'),
            '\\',
            alt((
                value("\\".as_bytes(), tag("\\")),
                value("\r".as_bytes(), tag("r")),
                value("\n".as_bytes(), tag("n")),
                value(":".as_bytes(), tag("c")),
            )),
        ),
        |o| String::from_utf8(o),
    );

    f.parse(input)
}

fn fetch_header(headers: &Vec<(String, String)>, key: &str) -> Option<String> {
    for (k, ref v) in headers {
        if &*k == key {
            return Some(v.clone());
        }
    }
    None
}

fn expect_header(headers: &Vec<(String, String)>, key: &str) -> Result<String> {
    fetch_header(headers, key).ok_or_else(|| anyhow!("Expected header '{}' missing", key))
}

impl<'a> TryFrom<Frame<'a>> for Message<ToServer> {
    type Error = anyhow::Error;

    fn try_from(
        Frame {
            command,
            ref headers,
            body,
        }: Frame<'a>,
    ) -> std::result::Result<Self, Self::Error> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use ToServer::*;
        let expect_keys: &[&str];
        let content = match command.to_uppercase().as_ref() {
            "STOMP" | "CONNECT" | "stomp" | "connect" => {
                expect_keys = &["accept-version", "host", "login", "passcode", "heart-beat"];
                let heartbeat = if let Some(hb) = fh(headers, "heart-beat") {
                    Some(parse_heartbeat(&hb)?)
                } else {
                    None
                };
                Connect {
                    accept_version: eh(headers, "accept-version")?,
                    host: eh(headers, "host")?,
                    login: fh(headers, "login"),
                    passcode: fh(headers, "passcode"),
                    heartbeat,
                }
            }
            "DISCONNECT" | "disconnect" => {
                expect_keys = &["receipt"];
                Disconnect {
                    receipt: fh(headers, "receipt"),
                }
            }
            "SEND" | "send" => {
                expect_keys = &[
                    "destination",
                    "transaction",
                    "content-length",
                    "content-type",
                ];
                Send {
                    destination: eh(headers, "destination")?,
                    transaction: fh(headers, "transaction"),
                    content_type: fh(headers, "content-type"),
                    body: body.map(|v| v.to_vec()),
                }
            }
            "SUBSCRIBE" | "subscribe" => {
                expect_keys = &["destination", "id", "ack"];
                Subscribe {
                    destination: eh(headers, "destination")?,
                    id: eh(headers, "id")?,
                    ack: match fh(headers, "ack").as_ref().map(|s| s.as_str()) {
                        Some("auto") => Some(AckMode::Auto),
                        Some("client") => Some(AckMode::Client),
                        Some("client-individual") => Some(AckMode::ClientIndividual),
                        Some(other) => bail!("Invalid ack mode: {}", other),
                        None => None,
                    },
                }
            }
            "UNSUBSCRIBE" | "unsubscribe" => {
                expect_keys = &["id"];
                Unsubscribe {
                    id: eh(headers, "id")?,
                }
            }
            "ACK" | "ack" => {
                expect_keys = &["id", "transaction"];
                Ack {
                    id: eh(headers, "id")?,
                    transaction: fh(headers, "transaction"),
                }
            }
            "NACK" | "nack" => {
                expect_keys = &["id", "transaction"];
                Nack {
                    id: eh(headers, "id")?,
                    transaction: fh(headers, "transaction"),
                }
            }
            "BEGIN" | "begin" => {
                expect_keys = &["transaction"];
                Begin {
                    transaction: eh(headers, "transaction")?,
                }
            }
            "COMMIT" | "commit" => {
                expect_keys = &["transaction"];
                Commit {
                    transaction: eh(headers, "transaction")?,
                }
            }
            "ABORT" | "abort" => {
                expect_keys = &["transaction"];
                Abort {
                    transaction: eh(headers, "transaction")?,
                }
            }
            other => bail!("Frame not recognized: {:?}", other),
        };
        let extra_headers: Vec<(String, String)> = extra_headers(headers, expect_keys);
        Ok(Message {
            content,
            extra_headers,
        })
    }
}

fn extra_headers(h: &Vec<(String, String)>, expect_keys: &[&str]) -> Vec<(String, String)> {
    h.iter()
        .filter_map(|(k, v)| {
            if expect_keys.contains(&k.as_str()) {
                None
            } else {
                Some((k.clone(), v.clone()))
            }
        })
        .collect()
}

impl<'a> TryFrom<Frame<'a>> for Message<FromServer> {
    type Error = anyhow::Error;

    fn try_from(
        Frame {
            command,
            ref headers,
            body,
        }: Frame<'a>,
    ) -> std::result::Result<Self, Self::Error> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use FromServer::{Connected, Error, Message as Msg, Receipt};
        let expect_keys: &[&str];

        let content = match command.to_uppercase().as_ref() {
            "CONNECTED" | "connected" => {
                expect_keys = &["version", "session", "server", "heart-beat"];
                Connected {
                    version: eh(headers, "version")?,
                    session: fh(headers, "session"),
                    server: fh(headers, "server"),
                    heartbeat: match fh(headers, "heart-beat") {
                        Some(hb) => Some(parse_heartbeat(&hb)?),
                        None => None,
                    },
                }
            }
            "MESSAGE" | "message" => {
                expect_keys = &[
                    "destination",
                    "message-id",
                    "subscription",
                    "content-length",
                    "content-type",
                ];
                Msg {
                    destination: eh(headers, "destination")?,
                    message_id: eh(headers, "message-id")?,
                    subscription: eh(headers, "subscription")?,
                    content_type: fh(headers, "content-type"),
                    body: body.map(|v| v.to_vec()),
                }
            }
            "RECEIPT" | "receipt" => {
                expect_keys = &["receipt-id"];
                Receipt {
                    receipt_id: eh(headers, "receipt-id")?,
                }
            }
            "ERROR" | "error" => {
                expect_keys = &["message", "content-length", "content-type"];
                Error {
                    message: fh(headers, "message"),
                    content_type: fh(headers, "content-type"),
                    body: body.map(|v| v.to_vec()),
                }
            }
            other => bail!("Frame not recognized: {:?}", other),
        };
        let extra_headers: Vec<(String, String)> = extra_headers(headers, expect_keys);
        Ok(Message {
            content,
            extra_headers,
        })
    }
}

fn get_content_length_header(body: &[u8]) -> Vec<u8> {
    format!("content-length:{}\n", body.len()).into_bytes()
}

fn parse_heartbeat(hb: &str) -> Result<(u32, u32)> {
    let mut split = hb.splitn(2, ',');
    let left = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    let right = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    Ok((left.parse()?, right.parse()?))
}

impl<'a> From<Message<FromServer>> for Frame<'a> {
    fn from(
        Message {
            content,
            extra_headers,
        }: Message<FromServer>,
    ) -> Self {
        match content {
            FromServer::Connected {
                version,
                session,
                server,
                heartbeat,
            } => Frame::new(
                "CONNECTED",
                vec![
                    ("version", version.into()),
                    ("heart-beat", heartbeat.map(|(v1, v2)| format!("{v1},{v2}"))),
                    ("session", session),
                    ("server", server),
                ],
                None,
                extra_headers,
            ),
            FromServer::Message {
                destination,
                message_id,
                subscription,
                body,
                content_type,
            } => Frame::new(
                "MESSAGE",
                vec![
                    ("subscription", subscription.into()),
                    ("message-id", message_id.into()),
                    ("destination", destination.into()),
                    ("content-type", content_type),
                ],
                body,
                extra_headers,
            ),
            FromServer::Receipt { receipt_id } => Frame::new(
                "RECEIPT",
                vec![("receipt-id", receipt_id)],
                None,
                extra_headers,
            ),
            FromServer::Error {
                message,
                body,
                content_type,
            } => Frame::new(
                "ERROR",
                vec![("message", message), ("content-type", content_type)],
                body,
                extra_headers,
            ),
        }
    }
}

impl<'a> From<Message<ToServer>> for Frame<'a> {
    fn from(
        Message {
            content,
            extra_headers,
        }: Message<ToServer>,
    ) -> Frame<'a> {
        use ToServer::*;
        match content {
            Connect {
                accept_version,
                host,
                login,
                passcode,
                heartbeat,
            } => Frame::new(
                "CONNECT",
                vec![
                    ("accept-version", accept_version.into()),
                    ("host", host.into()),
                    ("login", login),
                    ("heart-beat", heartbeat.map(|(v1, v2)| format!("{v1},{v2}"))),
                    ("passcode", passcode),
                ],
                None,
                extra_headers,
            ),
            Disconnect { receipt } => Frame::new(
                "DISCONNECT",
                vec![("receipt", receipt)],
                None,
                extra_headers,
            ),
            Subscribe {
                destination,
                id,
                ack,
            } => Frame::new(
                "SUBSCRIBE",
                vec![
                    ("destination", destination.into()),
                    ("id", id.into()),
                    (
                        "ack",
                        ack.map(|ack| {
                            match ack {
                                AckMode::Auto => "auto",
                                AckMode::Client => "client",
                                AckMode::ClientIndividual => "client-individual",
                            }
                            .to_string()
                        }),
                    ),
                ],
                None,
                extra_headers,
            ),
            Unsubscribe { id } => Frame::new("UNSUBSCRIBE", vec![("id", id)], None, extra_headers),
            Send {
                destination,
                transaction,
                body,
                content_type,
            } => Frame::new(
                "SEND",
                vec![
                    ("destination", destination.into()),
                    ("transaction", transaction),
                    ("content-type", content_type),
                ],
                body,
                extra_headers,
            ),
            Ack { id, transaction } => Frame::new(
                "ACK",
                vec![("id", id.into()), ("transaction", transaction)],
                None,
                extra_headers,
            ),
            Nack { id, transaction } => Frame::new(
                "NACK",
                vec![("id", id.into()), ("transaction", transaction)],
                None,
                extra_headers,
            ),
            Begin { transaction } => Frame::new(
                "BEGIN",
                vec![("transaction", transaction)],
                None,
                extra_headers,
            ),
            Commit { transaction } => Frame::new(
                "COMMIT",
                vec![("transaction", transaction)],
                None,
                extra_headers,
            ),
            Abort { transaction } => Frame::new(
                "ABORT",
                vec![("transaction", transaction)],
                None,
                extra_headers,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FromServer;
    use crate::Message;
    use pretty_assertions::{assert_eq, assert_matches};

    #[test]
    fn parse_and_serialize_connect() {
        let data = b"CONNECT
accept-version:1.2
host:datafeeds.here.co.uk
login:user
heart-beat:6,7
passcode:password\\c123\n\n\x00"
            .to_vec();
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command.as_ref(), "CONNECT");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"heart-beat", b"6,7"),
            (b"passcode", b"password:123"),
        ];
        let fh: Vec<_> = frame
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
            .collect();

        assert_eq!(fh, headers_expect);
        assert_eq!(frame.body, None);
        let stomp = Message::<ToServer>::try_from(frame).unwrap();

        assert_matches!( stomp.content, ToServer::Connect {
            heartbeat: Some((6,7)),
            login: Some(ref login),
            ..
        } if login == "user" );

        let mut buffer = BytesMut::new();
        Frame::from(stomp).serialize(&mut buffer);
        assert_eq!(&*buffer, &*data);

        dbg!(&buffer);

        assert_eq!(buffer.as_ref(), data)
    }

    #[test]
    fn parse_and_serialize_message() {
        let mut data = b"\nMESSAGE
destination:datafeeds.here.co.uk
message-id:12345
subscription:some-id
"
        .to_vec();
        let body = "this body contains \x00 nulls \n and \r\n newlines \x00 OK?";
        let rest = format!("content-length:{}\n\n{}\x00", body.len(), body);
        data.extend_from_slice(rest.as_bytes());
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command.as_bytes(), b"MESSAGE");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"datafeeds.here.co.uk"[..]),
            (b"message-id", b"12345"),
            (b"subscription", b"some-id"),
            (b"content-length", b"50"),
        ];
        let fh: Vec<_> = frame
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
            .collect();
        assert_eq!(fh, headers_expect);
        assert_eq!((&frame).body.as_ref().unwrap().as_ref(), (body.as_bytes()));
        Message::<FromServer>::try_from(frame).unwrap();
    }

    #[test]
    fn parse_and_serialize_message_with_body_start_with_newline() {
        let mut data = b"MESSAGE
destination:datafeeds.here.co.uk
message-id:12345
subscription:some-id"
            .to_vec();
        let body = "\n\n\nthis body contains  nulls \n and \r\n newlines OK?";
        let rest = format!("\n\n{}\x00\r\n", body);
        data.extend_from_slice(rest.as_bytes());
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command.as_bytes(), b"MESSAGE");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"datafeeds.here.co.uk"[..]),
            (b"message-id", b"12345"),
            (b"subscription", b"some-id"),
        ];
        let fh: Vec<_> = frame
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
            .collect();
        assert_eq!(fh, headers_expect);
        assert_eq!(frame.body.unwrap(), (body.as_bytes()));
    }

    #[test]
    fn parse_and_serialize_message_body_like_header() {
        let data = b"\nMESSAGE\r
destination:datafeeds.here.co.uk
message-id:12345
subscription:some-id\n\nsomething-like-header:1\x00\r\n"
            .to_vec();
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command.as_bytes(), b"MESSAGE");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (b"destination", b"datafeeds.here.co.uk"),
            (b"message-id", b"12345"),
            (b"subscription", b"some-id"),
        ];
        let fh: Vec<_> = frame
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
            .collect();
        assert_eq!(fh, headers_expect);
        assert_eq!(
            frame.body.as_ref().unwrap().as_ref(),
            ("something-like-header:1".as_bytes())
        );
        let msg = Message::<FromServer>::try_from(frame).unwrap();
        let mut bytes_mut = BytesMut::with_capacity(64);
        Frame::from(msg).serialize(&mut bytes_mut);

        dbg!(&bytes_mut);
    }

    #[test]
    fn parse_a_incomplete_message() {
        assert_matches!(
            parse_frame(b"\nMESSAG".as_ref()),
            Err(nom::Err::Incomplete(_))
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\n\n".as_ref()),
            Err(nom::Err::Incomplete(_))
        );

        assert_matches!(
            parse_frame(b"\nMESSAG\n\n\0".as_ref()),
            Ok((
                _,
                Frame {
                    ref command,
                    headers: _,
                    body: None
                }
            )) if command == "MESSAG"
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\r\ndestination:datafeeds.here.co.uk".as_ref()),
            Err(nom::Err::Incomplete(_))
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\r\ndestination:datafeeds.here.co.uk\n\n".as_ref()),
            Err(nom::Err::Incomplete(_))
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\r\nheader:da\\ctafeeds.here.co.uk\n\n\0".as_ref()),
            Ok((b"",Frame{ headers: ref a , .. })) if a[0].1 == "da:tafeeds.here.co.uk".to_string()
        );

        assert_matches!(
            Message::<ToServer>::try_from(
                parse_frame(b"\nSEND\r\ndestination:da\\ntafeedsuk\n\n\0".as_ref())
                    .unwrap()
                    .1
            ),
            Ok(Message::<ToServer> {
                content: ToServer::Send{
                   destination: ref a  ,
                    ..
                },
                ..
            }) if a == "da\ntafeedsuk"
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\r\ndestination:datafeeds.here.co.uk".as_ref()),
            Err(nom::Err::Incomplete(_))
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\r\ndestination:datafeeds.here.co.uk\n\n\0remain".as_ref()),
            Ok((b"remain", Frame { .. })),
            "stream with other after body end, should return remain text"
        );

        assert_matches!(
            parse_frame(b"\nMESSAGE\ncontent-length:10000\n\n\0remain".as_ref()),
            Err(nom::Err::Incomplete(_)),
            "content-length:10000, body size<10000, return incomplete"
        );
        assert_matches!(
            parse_frame(b"\nMESSAGE\ncontent-length:0\n\n\0remain".as_ref()),
            Ok((b"remain", Frame {   body:  Some(ref b), .. })) if b.len()==0,
            "empty body with content-length:0, body should be Some([])"
        );
        assert_matches!(
            parse_frame(b"\nMESSAGE\n\n\0remain".as_ref()),
            Ok((b"remain", Frame { body: None, .. })),
            "empty body without content-length header, body should be None"
        );
    }

    #[test]
    fn parse_and_serialize_message_header_value_with_colon() {
        let data = b"CONNECTED
server:ActiveMQ/6.0.0
heart-beat:0,0
session:ID:orbstack-45879-1702220142549-3:2
version:1.2

\0\n"
            .to_vec();
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command.as_bytes(), b"CONNECTED");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (b"server", b"ActiveMQ/6.0.0"),
            (b"heart-beat", b"0,0"),
            (b"session", b"ID:orbstack-45879-1702220142549-3:2"),
            (b"version", b"1.2"),
        ];
        let fh: Vec<_> = frame
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
            .collect();
        assert_eq!(fh, headers_expect);
        let _msg = Message::<FromServer>::try_from(frame).unwrap();
    }

    #[test]
    fn test_parser_header1() {
        let h = parse_frame(b"MESSAGE\nsubscription:11\nmessage-id:0.4.0\ndestination:now\\c Instant {\\n    tv_sec\\c 5740,\\n    tv_nsec\\c 164006416,\\n}\ncontent-type:application/json\nserver:tokio-stomp/0.4.0\n\nbody\0".as_ref());
        dbg!(&h);
        assert_matches!(h, Ok((b"", Frame{ body:Some(ref b) ,..})) if b.as_ref() == b"body");
    }
}
