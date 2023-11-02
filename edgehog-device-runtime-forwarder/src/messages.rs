// Copyright 2024 SECO Mind Srl
// SPDX-License-Identifier: Apache-2.0

//! Internal Rust representation of protobuf structures.
//!
//! The structures belonging to this module are used to serialize/deserialize to/from the protobuf
//! data representation.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::num::TryFromIntError;
use std::ops::Not;
use std::str::FromStr;

use thiserror::Error as ThisError;
use tokio_tungstenite::tungstenite::{Error as TungError, Message as TungMessage};
use tracing::{debug, error, instrument, warn};
use url::ParseError;

use edgehog_device_forwarder_proto as proto;
use edgehog_device_forwarder_proto::{
    http::Message as ProtobufHttpMessage,
    http::Request as ProtobufHttpRequest,
    http::Response as ProtobufHttpResponse,
    message::Protocol as ProtobufProtocol,
    prost::{self, Message as ProstMessage},
    web_socket::Close as ProtobufWsClose,
    web_socket::Message as ProtobufWsMessage,
    Http as ProtobufHttp, WebSocket as ProtobufWebSocket,
};

/// Errors occurring while handling [`protobuf`](https://protobuf.dev/overview/) messages
#[derive(displaydoc::Display, ThisError, Debug)]
#[non_exhaustive]
pub enum ProtocolError {
    /// Failed to serialize into Protobuf.
    Encode(#[from] prost::EncodeError),
    /// Failed to deserialize from Protobuf.
    Decode(#[from] prost::DecodeError),
    /// Empty fields.
    Empty,
    /// Reqwest error.
    Reqwest(#[from] reqwest::Error),
    /// Error parsing URL.
    ParseUrl(#[from] ParseError),
    /// Wrong HTTP method field.
    InvalidHttpMethod(#[from] http::method::InvalidMethod),
    /// Invalid Uri.
    InvalidUri(#[from] http::uri::InvalidUri),
    /// Http error.
    Http(#[from] http::Error),
    /// Invalid HTTP status code
    InvalidStatusCode(#[from] http::status::InvalidStatusCode),
    /// Error while parsing Headers.
    ParseHeaders(#[from] http::header::ToStrError),
    /// Invalid port number.
    InvalidPortNumber(#[from] TryFromIntError),
    /// Wrong HTTP method field, `{0}`.
    WrongHttpMethod(String),
    /// Error performing exponential backoff when trying to connect with TTYD, {0}
    WebSocketConnect(#[from] TungError),
    /// Received a wrong WebSocket frame.
    WrongWsFrame,
}

/// Requests Id.
#[derive(Default, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct Id(Vec<u8>);

impl Debug for Id {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Id({})", hex::encode(&self.0))
    }
}

impl Display for Id {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(&self.0))
    }
}

impl TryFrom<Vec<u8>> for Id {
    type Error = ProtocolError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ProtocolError::Empty);
        }

        Ok(Self(value))
    }
}

/// [`protobuf`](https://protobuf.dev/overview/) message internal representation.
///
/// It contains the actually supported protocols.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ProtoMessage {
    Http(Http),
    WebSocket(WebSocket),
}

impl ProtoMessage {
    /// Encode [`ProtoMessage`] struct into the corresponding [`protobuf`](https://protobuf.dev/overview/) version.
    pub(crate) fn encode(self) -> Result<Vec<u8>, ProtocolError> {
        let protocol = ProtobufProtocol::from(self);

        let msg = proto::Message {
            protocol: Some(protocol),
        };

        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf)?;

        Ok(buf)
    }

    /// Decode a [`protobuf`](https://protobuf.dev/overview/) message into a [`ProtoMessage`] struct.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let msg = proto::Message::decode(bytes).map_err(ProtocolError::from)?;
        Self::try_from(msg)
    }

    /// Convert a Tungstenite frame into a ProtoMessage
    pub(crate) fn try_from_tung(
        socket_id: Id,
        tung_msg: TungMessage,
    ) -> Result<Self, ProtocolError> {
        Ok(Self::WebSocket(WebSocket {
            socket_id,
            message: WebSocketMessage::try_from(tung_msg)?,
        }))
    }

    /// Return the internal websocket message if it matches the type.
    pub(crate) fn into_ws(self) -> Option<WebSocket> {
        match self {
            ProtoMessage::Http(_) => None,
            ProtoMessage::WebSocket(ws) => Some(ws),
        }
    }

    /// Return the internal http message if it matches the type.
    #[cfg(test)]
    pub(crate) fn into_http(self) -> Option<Http> {
        match self {
            ProtoMessage::Http(http) => Some(http),
            ProtoMessage::WebSocket(_) => None,
        }
    }
}

impl TryFrom<proto::Message> for ProtoMessage {
    type Error = ProtocolError;

    fn try_from(value: proto::Message) -> Result<Self, Self::Error> {
        let proto::Message { protocol } = value;

        let protocol = protocol.ok_or(ProtocolError::Empty)?;

        protocol.try_into()
    }
}

impl TryFrom<ProtobufProtocol> for ProtoMessage {
    type Error = ProtocolError;

    fn try_from(value: ProtobufProtocol) -> Result<Self, Self::Error> {
        let protocol = match value {
            ProtobufProtocol::Http(http) => ProtoMessage::Http(http.try_into()?),
            ProtobufProtocol::Ws(ws) => ProtoMessage::WebSocket(ws.try_into()?),
        };

        Ok(protocol)
    }
}

impl From<ProtoMessage> for ProtobufProtocol {
    fn from(protocol: ProtoMessage) -> Self {
        match protocol {
            ProtoMessage::Http(http) => {
                let proto_http = ProtobufHttp::from(http);
                ProtobufProtocol::Http(proto_http)
            }
            ProtoMessage::WebSocket(ws) => {
                let proto_ws = ProtobufWebSocket::from(ws);

                ProtobufProtocol::Ws(proto_ws)
            }
        }
    }
}

/// Http message.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Http {
    /// Unique ID.
    pub(crate) request_id: Id,
    /// Http message type.
    pub(crate) http_msg: HttpMessage,
}

impl Http {
    pub(crate) fn new(request_id: Id, http_msg: HttpMessage) -> Self {
        Self {
            request_id,
            http_msg,
        }
    }
}

impl TryFrom<ProtobufHttp> for Http {
    type Error = ProtocolError;

    fn try_from(value: ProtobufHttp) -> Result<Self, Self::Error> {
        let ProtobufHttp {
            request_id,
            message,
        } = value;

        let request_id = request_id.try_into()?;

        message
            .ok_or(ProtocolError::Empty)
            .and_then(|msg| match msg {
                ProtobufHttpMessage::Request(req) => req.try_into().map(HttpMessage::Request),
                ProtobufHttpMessage::Response(res) => res.try_into().map(HttpMessage::Response),
            })
            .map(|http_msg: HttpMessage| Http {
                request_id,
                http_msg,
            })
    }
}

impl From<Http> for ProtobufHttp {
    fn from(value: Http) -> Self {
        let message = match value.http_msg {
            HttpMessage::Request(req) => {
                let proto_req = ProtobufHttpRequest::from(req);
                ProtobufHttpMessage::Request(proto_req)
            }
            HttpMessage::Response(res) => {
                let proto_res = ProtobufHttpResponse::from(res);
                ProtobufHttpMessage::Response(proto_res)
            }
        };

        Self {
            request_id: value.request_id.0,
            message: Some(message),
        }
    }
}

/// Http protocol message types.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HttpMessage {
    Request(HttpRequest),
    Response(HttpResponse),
}

impl HttpMessage {
    pub(crate) fn into_req(self) -> Option<HttpRequest> {
        match self {
            HttpMessage::Request(req) => Some(req),
            HttpMessage::Response(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_res(self) -> Option<HttpResponse> {
        match self {
            HttpMessage::Request(_) => None,
            HttpMessage::Response(res) => Some(res),
        }
    }
}

/// HTTP request fields.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct HttpRequest {
    pub(crate) method: http::Method,
    pub(crate) path: String,
    pub(crate) query_string: String,
    pub(crate) headers: http::HeaderMap,
    pub(crate) body: Vec<u8>,
    /// Port on the device to which the request will be sent.
    pub(crate) port: u16,
}

impl HttpRequest {
    /// Create a [`RequestBuilder`](reqwest::RequestBuilder) from an HTTP request message.
    pub(crate) fn request_builder(self) -> Result<reqwest::RequestBuilder, ProtocolError> {
        let url_str = format!(
            "http://localhost:{}/{}?{}",
            self.port, self.path, self.query_string
        );
        let url = url::Url::parse(&url_str)?;
        let method = http::method::Method::from_str(self.method.as_str())?;

        let http_builder = reqwest::Client::new()
            .request(method, url)
            .headers(self.headers)
            .body(self.body);

        Ok(http_builder)
    }

    /// Check if the HTTP request contains an "Upgrade" header.
    pub(crate) fn is_upgrade(&self) -> bool {
        static WEBSOCKET_UPGRADE: http::HeaderValue = http::HeaderValue::from_static("websocket");

        self.headers
            .get_all(http::header::UPGRADE)
            .iter()
            .any(|v| v == WEBSOCKET_UPGRADE)
    }

    /// Convert an [`HttpRequest`] into an [`http::Request`](http::Request)
    #[instrument(skip_all)]
    pub(crate) fn upgrade(mut self) -> Result<http::Request<()>, ProtocolError> {
        let uri: http::Uri = format!(
            "ws://localhost:{}/{}?{}",
            self.port, self.path, self.query_string
        )
        .parse()?;

        // remove unsupported websocket headers
        self.remove_unsupported_ws_ext();

        // add method
        let req = http::request::Builder::new().uri(uri).method(self.method);

        // add the headers to the request
        let req = self
            .headers
            .into_iter()
            .fold(req, |req, (key, val)| match key {
                Some(key) => req.header(key, val),
                None => req,
            });

        // the body of an upgrade request should be empty.
        if !self.body.is_empty() {
            warn!(
                "HTTP upgrade request contains non-empty body, {:?}",
                self.body
            );
        }

        req.body(()).map_err(ProtocolError::from)
    }

    /// Remove unsupported websocket headers.
    #[instrument(skip_all)]
    fn remove_unsupported_ws_ext(&mut self) {
        // TODO: at the moment TTYD permessage-deflate extension is not supported by tungstenite. We should filter the supported ones implemented in tungstenite
        if let Some(extensions) = self.headers.remove("sec-websocket-extensions") {
            debug!(
                "websocket extensions removed: {}",
                String::from_utf8_lossy(extensions.as_bytes())
            );
        }
    }
}

impl TryFrom<ProtobufHttpRequest> for HttpRequest {
    type Error = ProtocolError;
    fn try_from(value: ProtobufHttpRequest) -> Result<Self, Self::Error> {
        let ProtobufHttpRequest {
            path,
            method,
            query_string,
            headers,
            body,
            port,
        } = value;
        Ok(Self {
            path,
            method: method.as_str().try_into()?,
            query_string,
            headers: (&headers).try_into()?,
            body,
            port: port.try_into()?,
        })
    }
}

impl From<HttpRequest> for ProtobufHttpRequest {
    fn from(http_req: HttpRequest) -> Self {
        Self {
            path: http_req.path,
            method: http_req.method.as_str().to_string(),
            query_string: http_req.query_string,
            headers: headermap_to_hashmap(&http_req.headers),
            body: http_req.body,
            port: http_req.port.into(),
        }
    }
}

/// HTTP response fields.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct HttpResponse {
    pub(crate) status_code: http::StatusCode,
    pub(crate) headers: http::HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl HttpResponse {
    /// Return the status code of the HTTP response.
    pub(crate) fn status(&self) -> u16 {
        self.status_code.as_u16()
    }

    /// Create an [`HttpResponse`] message from a [`reqwest`] response.
    pub(crate) async fn from_reqw_response(
        http_res: reqwest::Response,
    ) -> Result<Self, reqwest::Error> {
        let status_code = http_res.status();
        let headers = http_res.headers().clone();
        let body = http_res.bytes().await?.into();

        Ok(Self {
            status_code,
            headers,
            body,
        })
    }
}

impl TryFrom<ProtobufHttpResponse> for HttpResponse {
    type Error = ProtocolError;
    fn try_from(value: ProtobufHttpResponse) -> Result<Self, Self::Error> {
        let ProtobufHttpResponse {
            status_code,
            headers,
            body,
        } = value;

        Ok(Self {
            status_code: http::StatusCode::from_u16(status_code.try_into()?)?,
            headers: (&headers).try_into()?,
            body,
        })
    }
}

impl From<HttpResponse> for ProtobufHttpResponse {
    fn from(http_res: HttpResponse) -> Self {
        Self {
            status_code: http_res.status_code.as_u16().into(),
            headers: headermap_to_hashmap(&http_res.headers),
            body: http_res.body,
        }
    }
}

impl TryFrom<http::Response<Option<Vec<u8>>>> for HttpResponse {
    type Error = ProtocolError;

    fn try_from(mut value: http::Response<Option<Vec<u8>>>) -> Result<Self, Self::Error> {
        let status_code = value.status();
        let headers = value.headers().clone();
        let body = value.body_mut().take().unwrap_or_default();

        Ok(Self {
            status_code,
            headers,
            body,
        })
    }
}

/// WebSocket request fields.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct WebSocket {
    pub(crate) socket_id: Id,
    pub(crate) message: WebSocketMessage,
}

impl TryFrom<ProtobufWebSocket> for WebSocket {
    type Error = ProtocolError;

    fn try_from(value: ProtobufWebSocket) -> Result<Self, Self::Error> {
        let proto::WebSocket { socket_id, message } = value;

        let Some(msg) = message else {
            return Err(Self::Error::Empty);
        };

        let message = match msg {
            ProtobufWsMessage::Text(data) => WebSocketMessage::text(data),
            ProtobufWsMessage::Binary(data) => WebSocketMessage::binary(data),
            ProtobufWsMessage::Ping(data) => WebSocketMessage::ping(data),
            ProtobufWsMessage::Pong(data) => WebSocketMessage::pong(data),
            ProtobufWsMessage::Close(close) => WebSocketMessage::close(
                close.code.try_into()?,
                close.reason.is_empty().not().then_some(close.reason),
            ),
        };

        Ok(Self {
            socket_id: Id::try_from(socket_id)?,
            message,
        })
    }
}

impl From<WebSocket> for ProtobufWebSocket {
    fn from(ws: WebSocket) -> Self {
        let ws_message = match ws.message {
            WebSocketMessage::Text(data) => ProtobufWsMessage::Text(data),
            WebSocketMessage::Binary(data) => ProtobufWsMessage::Binary(data),
            WebSocketMessage::Ping(data) => ProtobufWsMessage::Ping(data),
            WebSocketMessage::Pong(data) => ProtobufWsMessage::Pong(data),
            WebSocketMessage::Close { code, reason } => ProtobufWsMessage::Close(ProtobufWsClose {
                code: code.into(),
                reason: reason.unwrap_or_default(),
            }),
        };

        proto::WebSocket {
            socket_id: ws.socket_id.0,
            message: Some(ws_message),
        }
    }
}

/// [`WebSocket`] message type.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close { code: u16, reason: Option<String> },
}

impl WebSocketMessage {
    /// Create a text frame.
    pub(crate) fn text(data: String) -> Self {
        Self::Text(data)
    }

    /// Create a binary frame.
    pub(crate) fn binary(data: Vec<u8>) -> Self {
        Self::Binary(data)
    }

    /// Create a ping frame.
    pub(crate) fn ping(data: Vec<u8>) -> Self {
        Self::Ping(data)
    }

    /// Create a pong frame.
    pub(crate) fn pong(data: Vec<u8>) -> Self {
        Self::Pong(data)
    }

    /// Create a close frame.
    pub(crate) fn close(code: u16, reason: Option<String>) -> Self {
        Self::Close { code, reason }
    }
}

impl TryFrom<TungMessage> for WebSocketMessage {
    type Error = ProtocolError;

    fn try_from(tung_msg: TungMessage) -> Result<Self, Self::Error> {
        let msg = match tung_msg {
            TungMessage::Text(data) => WebSocketMessage::text(data),
            TungMessage::Binary(data) => WebSocketMessage::binary(data),
            TungMessage::Ping(data) => WebSocketMessage::ping(data),
            TungMessage::Pong(data) => WebSocketMessage::pong(data),
            TungMessage::Close(data) => {
                // instead of returning an error, here i build a default close frame in case no frame is passed
                let (code, reason) = match data {
                    Some(close_frame) => {
                        let code = close_frame.code.into();
                        let reason = Some(close_frame.reason.into_owned());
                        (code, reason)
                    }
                    None => (1000, None),
                };

                WebSocketMessage::close(code, reason)
            }
            TungMessage::Frame(_) => {
                error!("this kind of message should not be sent");
                return Err(ProtocolError::WrongWsFrame);
            }
        };

        Ok(msg)
    }
}

impl From<WebSocketMessage> for TungMessage {
    fn from(value: WebSocketMessage) -> Self {
        match value {
            WebSocketMessage::Text(data) => Self::Text(data),
            WebSocketMessage::Binary(data) => Self::Binary(data),
            WebSocketMessage::Ping(data) => Self::Ping(data),
            WebSocketMessage::Pong(data) => Self::Pong(data),
            WebSocketMessage::Close { code, reason } => {
                Self::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: code.into(),
                    reason: Cow::Owned(reason.unwrap_or_default()),
                }))
            }
        }
    }
}

/// Convert a [`HeaderMap`] containing all HTTP headers into a [`HashMap`].
pub(crate) fn headermap_to_hashmap<'a, I>(headers: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (&'a http::HeaderName, &'a http::HeaderValue)>,
{
    headers
        .into_iter()
        .map(|(name, val)| {
            (
                name.to_string(),
                String::from_utf8_lossy(val.as_bytes()).into(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_message_req() -> HttpMessage {
        HttpMessage::Request(HttpRequest {
            method: http::Method::GET,
            path: String::new(),
            query_string: String::new(),
            headers: http::HeaderMap::new(),
            body: Vec::new(),
            port: 0,
        })
    }

    fn empty_http(id: &[u8]) -> Http {
        Http {
            request_id: Id::try_from(id.to_vec()).unwrap(),
            http_msg: http_message_req(),
        }
    }

    fn empty_protobuf_http(id: &[u8]) -> ProtobufHttp {
        ProtobufHttp {
            request_id: id.to_vec(),
            message: Some(ProtobufHttpMessage::Request(ProtobufHttpRequest {
                body: Vec::new(),
                headers: HashMap::new(),
                query_string: String::new(),
                path: String::new(),
                method: "GET".to_string(),
                port: 0,
            })),
        }
    }

    fn empty_protobuf_ws(id: &[u8]) -> ProtobufWebSocket {
        ProtobufWebSocket {
            socket_id: id.to_vec(),
            message: Some(ProtobufWsMessage::Binary(b"test_data".to_vec())),
        }
    }

    #[test]
    fn test_id() {
        // test empty ID
        assert!(matches!(
            Id::try_from(Vec::new()),
            Err(ProtocolError::Empty)
        ));

        let id_binary = b"test_id".to_vec();
        let id = Id::try_from(id_binary.clone()).unwrap();

        // test Display
        let display_id = format!("{id}");
        let res = hex::decode(display_id).unwrap();

        assert_eq!(res, id_binary);

        // test Debug
        let debug_id = format!("{id:?}");

        assert_eq!(debug_id, format!("Id({id})"));
    }

    #[test]
    fn test_from_protobuf_protocol() {
        // test WebSocket match case
        let id = b"test_id".to_vec();
        let proto = ProtobufProtocol::Ws(empty_protobuf_ws(&id));
        let res = ProtoMessage::try_from(proto).unwrap();

        let exp = ProtoMessage::WebSocket(WebSocket {
            socket_id: Id::try_from(id).unwrap(),
            message: WebSocketMessage::Binary(b"test_data".to_vec()),
        });

        assert_eq!(res, exp);
    }

    #[test]
    fn test_try_from_protobuf_http() {
        // test response ok
        let protobuf_msg = ProtobufHttp {
            request_id: b"test_id".to_vec(),
            message: Some(ProtobufHttpMessage::Response(ProtobufHttpResponse {
                body: Vec::new(),
                headers: HashMap::new(),
                status_code: 200,
            })),
        };

        assert!(Http::try_from(protobuf_msg).is_ok());

        // test missing message
        let protobuf_msg = ProtobufHttp {
            request_id: b"test_id".to_vec(),
            message: None,
        };

        assert!(matches!(
            Http::try_from(protobuf_msg),
            Err(ProtocolError::Empty)
        ));
    }

    #[test]
    fn test_from_http() {
        let msg = empty_http(b"test_id");

        let expected = empty_protobuf_http(b"test_id");

        assert_eq!(ProtobufHttp::from(msg), expected);
    }

    #[test]
    fn test_status() {
        let http_res = HttpResponse {
            status_code: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Vec::new(),
        };

        assert_eq!(200, http_res.status());
    }

    #[test]
    fn test_try_from_protobuf_websocket() {
        // empty ws message
        let protobuf_msg = ProtobufWebSocket {
            socket_id: b"test_id".to_vec(),
            message: None,
        };

        assert!(matches!(
            WebSocket::try_from(protobuf_msg),
            Err(ProtocolError::Empty)
        ));

        // empty ID message
        let protobuf_msg = ProtobufWebSocket {
            socket_id: Vec::new(),
            message: Some(ProtobufWsMessage::Binary(Vec::new())),
        };

        assert!(matches!(
            WebSocket::try_from(protobuf_msg),
            Err(ProtocolError::Empty)
        ));

        // check all variants
        let protobuf_msgs = [
            (
                ProtobufWsMessage::Text(String::new()),
                WebSocketMessage::Text(String::new()),
            ),
            (
                ProtobufWsMessage::Binary(Vec::new()),
                WebSocketMessage::Binary(Vec::new()),
            ),
            (
                ProtobufWsMessage::Ping(Vec::new()),
                WebSocketMessage::Ping(Vec::new()),
            ),
            (
                ProtobufWsMessage::Pong(Vec::new()),
                WebSocketMessage::Pong(Vec::new()),
            ),
            (
                ProtobufWsMessage::Close(ProtobufWsClose {
                    code: 1000,
                    reason: String::new(),
                }),
                WebSocketMessage::Close {
                    code: 1000,
                    reason: None,
                },
            ),
        ]
        .map(|(case, exp)| {
            (
                ProtobufWebSocket {
                    socket_id: b"test_id".to_vec(),
                    message: Some(case),
                },
                WebSocket {
                    socket_id: Id::try_from(b"test_id".to_vec()).unwrap(),
                    message: exp,
                },
            )
        });

        for (case, exp) in protobuf_msgs {
            assert_eq!(WebSocket::try_from(case).unwrap(), exp);
        }
    }

    #[test]
    fn test_from_websocket() {
        // check all variants
        let proto_msgs = [
            (
                WebSocketMessage::Text(String::new()),
                ProtobufWsMessage::Text(String::new()),
            ),
            (
                WebSocketMessage::Binary(Vec::new()),
                ProtobufWsMessage::Binary(Vec::new()),
            ),
            (
                WebSocketMessage::Ping(Vec::new()),
                ProtobufWsMessage::Ping(Vec::new()),
            ),
            (
                WebSocketMessage::Pong(Vec::new()),
                ProtobufWsMessage::Pong(Vec::new()),
            ),
            (
                WebSocketMessage::Close {
                    code: 1000,
                    reason: None,
                },
                ProtobufWsMessage::Close(ProtobufWsClose {
                    code: 1000,
                    reason: String::new(),
                }),
            ),
        ]
        .map(|(case, exp)| {
            (
                WebSocket {
                    socket_id: Id::try_from(b"test_id".to_vec()).unwrap(),
                    message: case,
                },
                ProtobufWebSocket {
                    socket_id: b"test_id".to_vec(),
                    message: Some(exp),
                },
            )
        });

        for (case, exp) in proto_msgs {
            assert_eq!(ProtobufWebSocket::from(case), exp);
        }
    }
}
