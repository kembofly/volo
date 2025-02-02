use std::{io, marker::PhantomData};

use http::{
    header::{CONTENT_TYPE, TE},
    HeaderValue,
};
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use motore::Service;
use volo::net::Address;

use super::connect::Connector;
use crate::{
    body::Body,
    client::Http2Config,
    codec::{
        compression::{CompressionEncoding, ACCEPT_ENCODING_HEADER, ENCODING_HEADER},
        decode::Kind,
    },
    context::{ClientContext, Config},
    Code, Request, Response, Status,
};

/// A simple wrapper of [`hyper::client::client`] that implements [`Service`]
/// to make outgoing requests.
pub struct ClientTransport<U> {
    http_client: http2::Builder<TokioExecutor>,
    connector: Connector,
    _marker: PhantomData<fn(U)>,
}

impl<U> Clone for ClientTransport<U> {
    fn clone(&self) -> Self {
        Self {
            http_client: self.http_client.clone(),
            connector: self.connector.clone(),
            _marker: self._marker,
        }
    }
}

impl<U> ClientTransport<U> {
    /// Creates a new [`ClientTransport`] by setting the underlying connection
    /// with the given config.
    pub fn new(http2_config: &Http2Config, rpc_config: &Config) -> Self {
        let config = volo::net::dial::Config::new(
            rpc_config.connect_timeout,
            rpc_config.read_timeout,
            rpc_config.write_timeout,
        );
        let mut http_client = http2::Builder::new(TokioExecutor::new());
        http_client
            .initial_stream_window_size(http2_config.init_stream_window_size)
            .initial_connection_window_size(http2_config.init_connection_window_size)
            .max_frame_size(http2_config.max_frame_size)
            .adaptive_window(http2_config.adaptive_window)
            .keep_alive_interval(http2_config.http2_keepalive_interval)
            .keep_alive_timeout(http2_config.http2_keepalive_timeout)
            .keep_alive_while_idle(http2_config.http2_keepalive_while_idle)
            .max_concurrent_reset_streams(http2_config.max_concurrent_reset_streams)
            .max_send_buf_size(http2_config.max_send_buf_size);
        let connector = Connector::new(Some(config));

        ClientTransport {
            http_client,
            connector,
            _marker: PhantomData,
        }
    }

    #[cfg(any(feature = "rustls", feature = "native-tls"))]
    pub fn new_with_tls(
        http2_config: &Http2Config,
        rpc_config: &Config,
        tls_config: volo::net::dial::ClientTlsConfig,
    ) -> Self {
        let config = volo::net::dial::Config::new(
            rpc_config.connect_timeout,
            rpc_config.read_timeout,
            rpc_config.write_timeout,
        );
        let mut http_client = http2::Builder::new(TokioExecutor::new());
        http_client
            .initial_stream_window_size(http2_config.init_stream_window_size)
            .initial_connection_window_size(http2_config.init_connection_window_size)
            .max_frame_size(http2_config.max_frame_size)
            .adaptive_window(http2_config.adaptive_window)
            .keep_alive_interval(http2_config.http2_keepalive_interval)
            .keep_alive_timeout(http2_config.http2_keepalive_timeout)
            .keep_alive_while_idle(http2_config.http2_keepalive_while_idle)
            .max_concurrent_reset_streams(http2_config.max_concurrent_reset_streams)
            .max_send_buf_size(http2_config.max_send_buf_size);
        let connector = Connector::new_with_tls(Some(config), tls_config);

        ClientTransport {
            http_client,
            connector,
            _marker: PhantomData,
        }
    }
}

impl<T, U> Service<ClientContext, Request<T>> for ClientTransport<U>
where
    T: crate::message::SendEntryMessage + Send + 'static,
    U: crate::message::RecvEntryMessage + 'static,
{
    type Response = Response<U>;

    type Error = Status;

    async fn call<'s, 'cx>(
        &'s self,
        cx: &'cx mut ClientContext,
        volo_req: Request<T>,
    ) -> Result<Self::Response, Self::Error> {
        let http_client = self.http_client.clone();
        // SAFETY: parameters controlled by volo-grpc are guaranteed to be valid.
        // get the call address from the context
        let target = cx.rpc_info.callee().address().ok_or_else(|| {
            io::Error::new(std::io::ErrorKind::InvalidData, "address is required")
        })?;

        let (metadata, extensions, message) = volo_req.into_parts();
        let path = cx.rpc_info.method();
        let rpc_config = cx.rpc_info.config();
        let accept_compressions = &rpc_config.accept_compressions;

        // select the compression algorithm with the highest priority by user's config
        let send_compression = rpc_config
            .send_compressions
            .as_ref()
            .map(|config| config[0]);

        let body = Body::new(message.into_body(send_compression));

        let mut req = http::Request::builder()
            .version(http::Version::HTTP_2)
            .method(http::Method::POST)
            .uri(build_uri(target.clone(), path))
            .extension(extensions)
            .body(body)
            .map_err(|err| Status::from_error(err.into()))?;
        *req.headers_mut() = metadata.into_headers();
        req.headers_mut()
            .insert(TE, HeaderValue::from_static("trailers"));
        req.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

        // insert compression headers
        if let Some(send_compression) = send_compression {
            req.headers_mut()
                .insert(ENCODING_HEADER, send_compression.into_header_value());
        }
        if let Some(accept_compressions) = accept_compressions {
            if !accept_compressions.is_empty() {
                if let Some(header_value) =
                    accept_compressions[0].into_accept_encoding_header_value(accept_compressions)
                {
                    req.headers_mut()
                        .insert(ACCEPT_ENCODING_HEADER, header_value);
                }
            }
        }

        // wrap io stream
        let io = TokioIo::new(
            self.connector
                .connect(target)
                .await
                .map_err(|err| Status::from_error(err.into()))?,
        );

        // call the service through hyper client
        let (mut sender, conn) = http_client
            .handshake(io)
            .await
            .map_err(|err| Status::from_error(err.into()))?;

        let resp = tokio::select! {
            resp = sender.send_request(req) => {
                resp.map_err(|err| Status::from_error(err.into()))?
            }
            status = conn => {
                status.map_err(|err| Status::from_error(err.into()))?;
                // **unreachable** because the connect cannot be broken without error before
                // receiving response.
                return Err(Status::new(Code::Internal, "Early broken of connection."));
            }
        };

        let status_code = resp.status();
        let headers = resp.headers();

        if let Some(status) = Status::from_header_map(headers) {
            if status.code() != Code::Ok {
                return Err(status);
            }
        }

        let accept_compression =
            CompressionEncoding::from_encoding_header(headers, &rpc_config.accept_compressions)?;

        let (parts, body) = resp.into_parts();

        let body = U::from_body(
            Some(path),
            body,
            Kind::Response(status_code),
            accept_compression,
        )?;
        let resp = hyper::Response::from_parts(parts, body);
        Ok(Response::from_http(resp))
    }
}

fn build_uri(addr: Address, path: &str) -> hyper::Uri {
    match addr {
        Address::Ip(ip) => hyper::Uri::builder()
            .scheme(http::uri::Scheme::HTTP)
            .authority(ip.to_string())
            .path_and_query(path)
            .build()
            .expect("fail to build ip uri"),
        #[cfg(target_family = "unix")]
        Address::Unix(unix) => hyper::Uri::builder()
            .scheme("http+unix")
            .authority(hex::encode(unix.to_string_lossy().as_bytes()))
            .path_and_query(path)
            .build()
            .expect("fail to build unix uri"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_build_uri_ip() {
        let addr = "127.0.0.1:8000".parse::<std::net::SocketAddr>().unwrap();
        let path = "/path?query=1";
        let uri = "http://127.0.0.1:8000/path?query=1"
            .parse::<hyper::Uri>()
            .unwrap();
        assert_eq!(super::build_uri(volo::net::Address::from(addr), path), uri);
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_build_uri_unix() {
        use std::borrow::Cow;

        let addr = "/tmp/rpc.sock".parse::<std::path::PathBuf>().unwrap();
        let path = "/path?query=1";
        let uri = "http+unix://2f746d702f7270632e736f636b/path?query=1"
            .parse::<hyper::Uri>()
            .unwrap();
        assert_eq!(
            super::build_uri(volo::net::Address::from(Cow::from(addr)), path),
            uri
        );
    }
}
