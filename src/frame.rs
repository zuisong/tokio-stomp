use anyhow::{anyhow, bail};
use bytes::{BufMut, BytesMut};
use nom::{
    bytes::streaming::{is_not, tag, take, take_until},
    character::streaming::{alpha1, line_ending, not_line_ending},
    combinator::{complete, opt},
    multi::{count, many0, many_till},
    sequence::{delimited, separated_pair, terminated, tuple},
    IResult, Parser,
};

use std::borrow::Cow;

use crate::{AckMode, FromServer, Message, Result, ToServer};

#[derive(Debug)]
pub(crate) struct Frame<'a> {
    command: &'a [u8],
    // TODO use ArrayVec to keep headers on the stack
    // (makes this object zero-allocation)
    headers: Vec<(&'a [u8], Cow<'a, [u8]>)>,
    body: Option<&'a [u8]>,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(
        command: &'a [u8],
        headers: &[(&'a [u8], Option<Cow<'a, [u8]>>)],
        body: Option<&'a [u8]>,
        extra_headers: &'a Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Frame<'a> {
        let mut headers: Vec<(&[u8], Cow<[u8]>)> = headers
            .iter()
            // filter out headers with None value
            .filter_map(|&(k, ref v)| v.as_ref().map(|i| (k, (&*i).clone())))
            .collect();
        let extra_headers: Vec<(&[u8], Cow<[u8]>)> = extra_headers
            .iter()
            .map(|(k, v)| (k.as_slice(), Cow::Borrowed(v.as_slice())))
            .collect();

        headers.extend(extra_headers);

        Frame {
            command,
            headers,
            body,
        }
    }

    pub(crate) fn serialize(&self, buffer: &mut BytesMut) {
        fn write_escaped(b: u8, buffer: &mut BytesMut) {
            match b {
                b'\r' => {
                    buffer.put_u8(b'\\');
                    buffer.put_u8(b'r')
                }
                b'\n' => {
                    buffer.put_u8(b'\\');
                    buffer.put_u8(b'n')
                }
                b':' => {
                    buffer.put_u8(b'\\');
                    buffer.put_u8(b'c')
                }
                b'\\' => {
                    buffer.put_u8(b'\\');
                    buffer.put_u8(b'\\')
                }
                b => buffer.put_u8(b),
            }
        }
        let requires = self.command.len()
            + self.body.map(|b| b.len() + 20).unwrap_or(0)
            + self
                .headers
                .iter()
                .fold(0, |acc, &(ref k, ref v)| acc + k.len() + v.len())
            + 30;
        if buffer.remaining_mut() < requires {
            buffer.reserve(requires);
        }
        buffer.put_slice(self.command);
        buffer.put_u8(b'\n');
        self.headers.iter().for_each(|&(key, ref val)| {
            for byte in key {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b':');
            for byte in val.iter() {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b'\n');
        });
        if let Some(body) = self.body {
            buffer.put_slice(&get_content_length_header(&body));
            buffer.put_u8(b'\n');
            buffer.put_slice(body);
        } else {
            buffer.put_u8(b'\n');
        }
        buffer.put_u8(b'\x00');
    }
}

// Nom definitions

fn get_content_length(headers: &[(&[u8], Cow<[u8]>)]) -> Option<usize> {
    for (name, value) in headers {
        if name == b"content-length" {
            return std::str::from_utf8(value)
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
        }
    }
    None
}

fn is_empty_slice(s: &[u8]) -> Option<&[u8]> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) fn parse_frame(input: &[u8]) -> IResult<&[u8], Frame> {
    // read stream until header end
    // drop result for save memory
    many_till(take(1_usize).map(drop), count(line_ending, 2))(input)?;

    let (input, (command, headers)) = tuple((
        delimited(opt(complete(line_ending)), alpha1, line_ending), // command
        terminated(
            many0(parse_header), // header
            line_ending,
        ),
    ))(input)?;

    let (input, body) = match get_content_length(&headers) {
        None => take_until("\x00").map(is_empty_slice).parse(input)?,
        Some(length) => take(length).map(Some).parse(input)?,
    };

    let (input, _) = tuple((tag("\x00"), opt(complete(line_ending))))(input)?;

    Ok((
        input,
        Frame {
            command,
            headers,
            body,
        },
    ))
}

fn parse_header(input: &[u8]) -> IResult<&[u8], (&[u8], Cow<[u8]>)> {
    complete(separated_pair(
        is_not(":\r\n"),
        tag(":"),
        terminated(not_line_ending, line_ending).map(Cow::Borrowed),
    ))
    .parse(input)
}

fn fetch_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Option<String> {
    let kk = key.as_bytes();
    for &(k, ref v) in headers {
        if &*k == kk {
            return String::from_utf8(v.to_vec()).ok();
        }
    }
    None
}

fn expect_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Result<String> {
    fetch_header(headers, key).ok_or_else(|| anyhow!("Expected header '{}' missing", key))
}

impl<'a> TryFrom<&'a Frame<'a>> for Message<ToServer> {
    type Error = anyhow::Error;

    fn try_from(frame: &'a Frame<'a>) -> std::result::Result<Self, Self::Error> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use ToServer::*;
        let h = &frame.headers;
        let expect_keys: &[&[u8]];
        let content = match frame.command {
            b"STOMP" | b"CONNECT" | b"stomp" | b"connect" => {
                expect_keys = &[
                    b"accept-version",
                    b"host",
                    b"login",
                    b"passcode",
                    b"heart-beat",
                ];
                let heartbeat = if let Some(hb) = fh(h, "heart-beat") {
                    Some(parse_heartbeat(&hb)?)
                } else {
                    None
                };
                Connect {
                    accept_version: eh(h, "accept-version")?,
                    host: eh(h, "host")?,
                    login: fh(h, "login"),
                    passcode: fh(h, "passcode"),
                    heartbeat,
                }
            }
            b"DISCONNECT" | b"disconnect" => {
                expect_keys = &[b"receipt"];
                Disconnect {
                    receipt: fh(h, "receipt"),
                }
            }
            b"SEND" | b"send" => {
                expect_keys = &[
                    b"destination",
                    b"transaction",
                    b"content-length",
                    b"content-type",
                ];
                Send {
                    destination: eh(h, "destination")?,
                    transaction: fh(h, "transaction"),
                    content_type: fh(h, "content-type"),
                    body: frame.body.map(|v| v.to_vec()),
                }
            }
            b"SUBSCRIBE" | b"subscribe" => {
                expect_keys = &[b"destination", b"id", b"ack"];
                Subscribe {
                    destination: eh(h, "destination")?,
                    id: eh(h, "id")?,
                    ack: match fh(h, "ack").as_ref().map(|s| s.as_str()) {
                        Some("auto") => Some(AckMode::Auto),
                        Some("client") => Some(AckMode::Client),
                        Some("client-individual") => Some(AckMode::ClientIndividual),
                        Some(other) => bail!("Invalid ack mode: {}", other),
                        None => None,
                    },
                }
            }
            b"UNSUBSCRIBE" | b"unsubscribe" => {
                expect_keys = &[b"id"];
                Unsubscribe { id: eh(h, "id")? }
            }
            b"ACK" | b"ack" => {
                expect_keys = &[b"id", b"transaction"];
                Ack {
                    id: eh(h, "id")?,
                    transaction: fh(h, "transaction"),
                }
            }
            b"NACK" | b"nack" => {
                expect_keys = &[b"id", b"transaction"];
                Nack {
                    id: eh(h, "id")?,
                    transaction: fh(h, "transaction"),
                }
            }
            b"BEGIN" | b"begin" => {
                expect_keys = &[b"transaction"];
                Begin {
                    transaction: eh(h, "transaction")?,
                }
            }
            b"COMMIT" | b"commit" => {
                expect_keys = &[b"transaction"];
                Commit {
                    transaction: eh(h, "transaction")?,
                }
            }
            b"ABORT" | b"abort" => {
                expect_keys = &[b"transaction"];
                Abort {
                    transaction: eh(h, "transaction")?,
                }
            }
            other => bail!("Frame not recognized: {:?}", String::from_utf8_lossy(other)),
        };
        let extra_headers = h
            .iter()
            .filter_map(|&(k, ref v)| {
                if !expect_keys.contains(&k) {
                    Some((k.to_vec(), (&*v).to_vec()))
                } else {
                    None
                }
            })
            .collect();
        Ok(Message {
            content,
            extra_headers,
        })
    }
}

impl<'a> TryFrom<&'a Frame<'a>> for Message<FromServer> {
    type Error = anyhow::Error;

    fn try_from(frame: &'a Frame<'a>) -> std::result::Result<Self, Self::Error> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use FromServer::{Connected, Error, Message as Msg, Receipt};
        let h = &frame.headers;
        let expect_keys: &[&[u8]];
        let content = match frame.command {
            b"CONNECTED" | b"connected" => {
                expect_keys = &[b"version", b"session", b"server", b"heart-beat"];
                Connected {
                    version: eh(h, "version")?,
                    session: fh(h, "session"),
                    server: fh(h, "server"),
                    heartbeat: match fh(h, "heart-beat") {
                        Some(hb) => Some(parse_heartbeat(&hb)?),
                        None => None,
                    },
                }
            }
            b"MESSAGE" | b"message" => {
                expect_keys = &[
                    b"destination",
                    b"message-id",
                    b"subscription",
                    b"content-length",
                    b"content-type",
                ];
                Msg {
                    destination: eh(h, "destination")?,
                    message_id: eh(h, "message-id")?,
                    subscription: eh(h, "subscription")?,
                    content_type: fh(h, "content-type"),
                    body: frame.body.map(|v| v.to_vec()),
                }
            }
            b"RECEIPT" | b"receipt" => {
                expect_keys = &[b"receipt-id"];
                Receipt {
                    receipt_id: eh(h, "receipt-id")?,
                }
            }
            b"ERROR" | b"error" => {
                expect_keys = &[b"message", b"content-length", b"content-type"];
                Error {
                    message: fh(h, "message"),
                    content_type: fh(h, "content-type"),
                    body: frame.body.map(|v| v.to_vec()),
                }
            }
            other => bail!("Frame not recognized: {:?}", String::from_utf8_lossy(other)),
        };
        let extra_headers = h
            .iter()
            .filter_map(|&(k, ref v)| {
                if !expect_keys.contains(&k) {
                    Some((k.to_vec(), (&*v).to_vec()))
                } else {
                    None
                }
            })
            .collect();
        Ok(Message {
            content,
            extra_headers,
        })
    }
}

fn opt_str_to_bytes(s: &Option<String>) -> Option<Cow<[u8]>> {
    s.as_ref().map(|v| Cow::Borrowed(v.as_bytes()))
}

fn str_to_bytes(s: &String) -> Option<Cow<[u8]>> {
    Some(Cow::Borrowed(s.as_bytes()))
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

impl<'a> From<&'a Message<FromServer>> for Frame<'a> {
    fn from(value: &'a Message<FromServer>) -> Self {
        match &value.content {
            FromServer::Connected {
                ref version,
                ref session,
                ref server,
                ref heartbeat,
            } => Frame::new(
                b"CONNECTED",
                &[
                    (b"version", str_to_bytes(version)),
                    (
                        b"heart-beat",
                        heartbeat.map(|(v1, v2)| Cow::Owned(format!("{},{}", v1, v2).into())),
                    ),
                    (b"session", opt_str_to_bytes(session)),
                    (b"server", opt_str_to_bytes(server)),
                ],
                None,
                &value.extra_headers,
            ),
            FromServer::Message {
                ref destination,
                ref message_id,
                ref subscription,
                ref body,
                ref content_type,
            } => Frame::new(
                b"MESSAGE",
                &[
                    (b"subscription", str_to_bytes(subscription)),
                    (b"message-id", str_to_bytes(message_id)),
                    (b"destination", str_to_bytes(destination)),
                    (b"content-type", opt_str_to_bytes(content_type)),
                ],
                body.as_ref().map(|v| v.as_ref()),
                &value.extra_headers,
            ),
            FromServer::Receipt { receipt_id } => Frame::new(
                b"RECEIPT",
                &[(b"receipt-id", str_to_bytes(receipt_id))],
                None,
                &value.extra_headers,
            ),
            FromServer::Error {
                ref message,
                ref body,
                ref content_type,
            } => Frame::new(
                b"ERROR",
                &[
                    (b"message", opt_str_to_bytes(message)),
                    (b"content-type", opt_str_to_bytes(content_type)),
                ],
                body.as_ref().map(|v| v.as_ref()),
                &value.extra_headers,
            ),
        }
    }
}

impl<'a> From<&'a Message<ToServer>> for Frame<'a> {
    fn from(msg: &'a Message<ToServer>) -> Frame<'a> {
        use Cow::*;
        use ToServer::*;
        match &msg.content {
            Connect {
                ref accept_version,
                ref host,
                ref login,
                ref passcode,
                ref heartbeat,
            } => Frame::new(
                b"CONNECT",
                &[
                    (b"accept-version", str_to_bytes(accept_version)),
                    (b"host", str_to_bytes(host)),
                    (b"login", opt_str_to_bytes(login)),
                    (
                        b"heart-beat",
                        heartbeat.map(|(v1, v2)| Owned(format!("{},{}", v1, v2).into())),
                    ),
                    (b"passcode", opt_str_to_bytes(passcode)),
                ],
                None,
                &msg.extra_headers,
            ),
            Disconnect { ref receipt } => Frame::new(
                b"DISCONNECT",
                &[(b"receipt", opt_str_to_bytes(&receipt))],
                None,
                &msg.extra_headers,
            ),
            Subscribe {
                ref destination,
                ref id,
                ref ack,
            } => Frame::new(
                b"SUBSCRIBE",
                &[
                    (b"destination", str_to_bytes(destination)),
                    (b"id", str_to_bytes(id)),
                    (
                        b"ack",
                        ack.map(|ack| match ack {
                            AckMode::Auto => Borrowed(&b"auto"[..]),
                            AckMode::Client => Borrowed(&b"client"[..]),
                            AckMode::ClientIndividual => Borrowed(&b"client-individual"[..]),
                        }),
                    ),
                ],
                None,
                &msg.extra_headers,
            ),
            Unsubscribe { ref id } => Frame::new(
                b"UNSUBSCRIBE",
                &[(b"id", str_to_bytes(id))],
                None,
                &msg.extra_headers,
            ),
            Send {
                ref destination,
                ref transaction,
                ref body,
                ref content_type,
            } => Frame::new(
                b"SEND",
                &[
                    (b"destination", str_to_bytes(destination)),
                    (b"transaction", opt_str_to_bytes(transaction)),
                    (b"content-type", opt_str_to_bytes(content_type)),
                ],
                body.as_ref().map(|v| v.as_ref()),
                &msg.extra_headers,
            ),
            Ack {
                ref id,
                ref transaction,
            } => Frame::new(
                b"ACK",
                &[
                    (b"id", str_to_bytes(id)),
                    (b"transaction", opt_str_to_bytes(transaction)),
                ],
                None,
                &msg.extra_headers,
            ),
            Nack {
                ref id,
                ref transaction,
            } => Frame::new(
                b"NACK",
                &[
                    (b"id", str_to_bytes(id)),
                    (b"transaction", opt_str_to_bytes(transaction)),
                ],
                None,
                &msg.extra_headers,
            ),
            Begin { ref transaction } => Frame::new(
                b"BEGIN",
                &[(b"transaction", str_to_bytes(transaction))],
                None,
                &msg.extra_headers,
            ),
            Commit { ref transaction } => Frame::new(
                b"COMMIT",
                &[(b"transaction", str_to_bytes(transaction))],
                None,
                &msg.extra_headers,
            ),
            Abort { ref transaction } => Frame::new(
                b"ABORT",
                &[(b"transaction", str_to_bytes(transaction))],
                None,
                &msg.extra_headers,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::{assert_eq, assert_matches};

    #[test]
    fn parse_and_serialize_connect() {
        let data = b"CONNECT
accept-version:1.2
host:datafeeds.here.co.uk
login:user
heart-beat:6,7
passcode:password\n\n\x00"
            .to_vec();
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command, b"CONNECT");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"heart-beat", b"6,7"),
            (b"passcode", b"password"),
        ];
        let fh: Vec<_> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        assert_eq!(fh, headers_expect);
        assert_eq!(frame.body, None);
        let stomp = Message::<ToServer>::try_from(&frame).unwrap();
        let mut buffer = BytesMut::new();
        Frame::from(&stomp).serialize(&mut buffer);
        assert_eq!(&*buffer, &*data);
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
        assert_eq!(frame.command, b"MESSAGE");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"datafeeds.here.co.uk"[..]),
            (b"message-id", b"12345"),
            (b"subscription", b"some-id"),
            (b"content-length", b"50"),
        ];
        let fh: Vec<_> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        assert_eq!(fh, headers_expect);
        assert_eq!(frame.body, Some(body.as_bytes()));
        Message::<FromServer>::try_from(&frame).unwrap();
        // TODO to_frame for FromServer
        // let roundtrip = stomp.to_frame().serialize();
        // assert_eq!(roundtrip, data);
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
        assert_eq!(frame.command, b"MESSAGE");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"datafeeds.here.co.uk"[..]),
            (b"message-id", b"12345"),
            (b"subscription", b"some-id"),
        ];
        let fh: Vec<_> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        assert_eq!(fh, headers_expect);
        assert_eq!(frame.body, Some(body.as_bytes()));
        Message::<FromServer>::try_from(&frame).unwrap();
        // TODO to_frame for FromServer
        // let roundtrip = stomp.to_frame().serialize();
        // assert_eq!(roundtrip, data);
    }

    #[test]
    fn parse_and_serialize_message_body_like_header() {
        let data = b"\nMESSAGE\r
destination:datafeeds.here.co.uk
message-id:12345
subscription:some-id\n\nsomething-like-header:1\x00\r\n"
            .to_vec();
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command, b"MESSAGE");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (b"destination", b"datafeeds.here.co.uk"),
            (b"message-id", b"12345"),
            (b"subscription", b"some-id"),
        ];
        let fh: Vec<_> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        assert_eq!(fh, headers_expect);
        assert_eq!(frame.body, Some("something-like-header:1".as_bytes()));
        Message::<FromServer>::try_from(&frame).unwrap();
        // TODO to_frame for FromServer
        // let roundtrip = stomp.to_frame().serialize();
        // assert_eq!(roundtrip, data);
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
                    command: b"MESSAG",
                    headers: _,
                    body: None
                }
            ))
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
            Ok((b"remain", Frame { body: Some([]), .. })),
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
        let data = b"
CONNECTED
server:ActiveMQ/6.0.0
heart-beat:0,0
session:ID:orbstack-45879-1702220142549-3:2
version:1.2

\0\n"
            .to_vec();
        let (_, frame) = parse_frame(&data).unwrap();
        assert_eq!(frame.command, b"CONNECTED");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (b"server", b"ActiveMQ/6.0.0"),
            (b"heart-beat", b"0,0"),
            (b"session", b"ID:orbstack-45879-1702220142549-3:2"),
            (b"version", b"1.2"),
        ];
        let fh: Vec<_> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        assert_eq!(fh, headers_expect);
        Message::<FromServer>::try_from(&frame).unwrap();
    }
}
