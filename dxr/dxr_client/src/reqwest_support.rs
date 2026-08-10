#[cfg(feature = "multicall")]
use std::collections::HashMap;
use std::fmt::Debug;

use std::path::Path;

use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use log::error;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tokio::net::UnixStream;
use url::Url;

use dxr::{DxrError, Fault, FaultResponse, MethodCall, MethodResponse, TryFromValue, TryToParams};
#[cfg(feature = "multicall")]
use dxr::Value;

use crate::{Call, DEFAULT_USER_AGENT};

/// Error type for XML-RPC clients based on [`reqwest`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// Error variant for XML-RPC server faults.
    #[error("{}", fault)]
    Fault {
        /// Fault returned by the server.
        #[from]
        fault: Fault,
    },
    /// Error variant for XML-RPC errors.
    #[error("{}", error)]
    RPC {
        /// XML-RPC parsing error.
        #[from]
        error: DxrError,
    },
    /// Error variant for networking errors.
    #[error("{}", error)]
    Net {
        /// Networking error returned by [`reqwest`].
        #[from]
        error: reqwest::Error,
    },
}

/// Builder that takes parameters for constructing a [`Client`] based on [`reqwest::Client`].
#[derive(Debug)]
pub struct ClientBuilder {
    url: Url,
    headers: HeaderMap,
    user_agent: Option<&'static str>,
}

impl ClientBuilder {
    /// Constructor for [`ClientBuilder`] from the URL of the XML-RPC server.
    ///
    /// This also sets up the default `Content-Type: text/xml` HTTP header for XML-RPC requests.
    pub fn new(url: Url) -> Self {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/xml"));

        ClientBuilder {
            url,
            headers: default_headers,
            user_agent: None,
        }
    }

    /// Method for overriding the default User-Agent header.
    pub fn user_agent(mut self, user_agent: &'static str) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    /// Method for providing additional custom HTTP headers.
    ///
    /// Using [`HeaderName`] constants for the header name is recommended. The [`HeaderValue`]
    /// argument needs to be parsed (probably from a string) with [`HeaderValue::from_str`] to
    /// ensure their value is valid.
    pub fn add_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Build the [`Client`] by setting up and initializing the internal [`reqwest::Client`].
    ///
    /// If no custom value was provided for `User-Agent`, the default value
    /// ([`DEFAULT_USER_AGENT`]) will be used.
    pub fn build(self) -> Client {
        let user_agent = self.user_agent.unwrap_or(DEFAULT_USER_AGENT);

        let builder = self.add_header(USER_AGENT, HeaderValue::from_static(user_agent));

        let client = reqwest::Client::builder()
            .default_headers(builder.headers)
            .build()
            .expect("Failed to initialize reqwest client.");

        Client {
            url: builder.url,
            client,
        }
    }
}

/// # XML-RPC client implementation
///
/// This type provides a very simple XML-RPC client implementation based on [`reqwest`]. Initialize
/// the [`Client`], submit a [`Call`], get a result (or a fault).
#[derive(Debug)]
pub struct Client {
    url: Url,
    client: reqwest::Client,
}

impl Client {
    /// Constructor for a [`Client`] from a [`reqwest::Client`] that was already initialized.
    pub fn with_client(url: Url, client: reqwest::Client) -> Self {
        Client { url, client }
    }

    /// Asynchronous method for handling remote procedure calls with XML-RPC.
    ///
    /// Fault responses from the XML-RPC server are transparently converted into [`Fault`] errors.
    /// Invalid XML-RPC responses or faults will result in an appropriate [`DxrError`].
    pub async fn call<P: TryToParams, R: TryFromValue>(&self, call: Call<'_, P, R>) -> Result<R, ClientError> {
        // serialize XML-RPC method call
        let request = call.as_xml_rpc()?;
        let body = request_to_body(&request)?;

        let response = match self.url.clone().scheme() {
            "unix" => {
                match send_scgi_request(self.url.path(), &body).await {
                    Ok(response) => response,
                    Err(e) => {
                        error!("Failed to talk to the rTorrent socket: {e}");
                        return Err(ClientError::Fault {
                            fault: Fault::new(1, format!("Failed to reach rTorrent socket: {e}")),
                        });
                    }
                }
            }
            _ => {
                // let request = self.client.post(self.url.clone()).body(body).build()?;
                let request = match self.client.post(self.url.clone()).body(body).build() {
                    Ok(request) => request,
                    Err(e) => {
                        eprintln!("Failed to build the request: {:?}", e);
                        return Err(ClientError::Net { error: e });
                    }
                };
                self.client.execute(request).await?.text().await?
            }
        };
        // construct request and send to server

        /// Send an XML-RPC body to an rTorrent SCGI unix socket and return the
        /// response payload.
        ///
        /// SCGI is a netstring of NUL-separated header key/value pairs followed
        /// by the body:
        ///
        ///     <len>:CONTENT_LENGTH\0<n>\0SCGI\01\0...\0,<body>
        ///
        /// This used to go through the `tokio-scgi` crate, which was last
        /// released in 2021 and pinned `tokio-util` to 0.6, blocking that whole
        /// branch of the tree from being updated. The encoding is a dozen lines,
        /// so it is done here instead and the dependency is gone.
        ///
        /// The previous response handling also did `s.split("<?xml")[1]`, which
        /// panicked on any response that was not XML -- an SCGI error reply, for
        /// instance. Splitting on the header/body boundary is both correct and
        /// payload-agnostic.
        async fn send_scgi_request(socket_path: &str, body: &str) -> std::io::Result<String> {
            let headers = format!(
                // CONTENT_LENGTH must come first per the SCGI spec.
                "CONTENT_LENGTH\0{}\0SCGI\x001\0REQUEST_METHOD\0POST\0REQUEST_URI\0/RPC\0",
                body.len()
            );

            let mut request = Vec::with_capacity(headers.len() + body.len() + 16);
            request.extend_from_slice(format!("{}:", headers.len()).as_bytes());
            request.extend_from_slice(headers.as_bytes());
            request.push(b',');
            request.extend_from_slice(body.as_bytes());

            let mut stream = UnixStream::connect(Path::new(socket_path)).await?;
            stream.write_all(&request).await?;
            stream.flush().await?;

            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await?;

            let text = String::from_utf8_lossy(&raw).into_owned();

            // rTorrent replies with HTTP-style headers followed by the payload.
            let payload = match text.find("\r\n\r\n") {
                Some(idx) => &text[idx + 4..],
                None => match text.find("\n\n") {
                    Some(idx) => &text[idx + 2..],
                    // No header block at all: treat the whole reply as payload
                    // rather than guessing.
                    None => text.as_str(),
                },
            };

            Ok(payload.to_string())
        }

        // deserialize XML-RPC method response
        let contents = response;
        let result = response_to_result(&contents)?;

        // extract return value
        Ok(R::try_from_value(&result.inner())?)
    }

    /// Asynchronous method for handling "system.multicall" calls.
    ///
    /// *Note*: This method does not check if the number of method calls matches the number of
    /// returned results.
    #[cfg(feature = "multicall")]
    pub async fn multicall<P: TryToParams>(
        &self,
        call: Call<'_, P, Vec<Value>>,
    ) -> Result<Vec<Result<Value, Fault>>, ClientError> {
        let response = self.call(call).await?;

        let mut results = Vec::new();
        for result in response {
            // return values for successful calls are arrays that contain a single value
            if let Ok((value, )) = <(Value, )>::try_from_value(&result) {
                results.push(Ok(value));
            };

            // return values for failed calls are structs with two members
            if let Ok(mut value) = <HashMap<String, Value>>::try_from_value(&result) {
                let code = match value.remove("faultCode") {
                    Some(code) => code,
                    None => return Err(DxrError::missing_field("Fault", "faultCode").into()),
                };

                let string = match value.remove("faultString") {
                    Some(string) => string,
                    None => return Err(DxrError::missing_field("Fault", "faultString").into()),
                };

                // The value might still contain other struct fields:
                // Rather than return an error because they are unexpected, they are ignored,
                // since the required "faultCode" and "faultString" members were present.

                let fault = Fault::new(i32::try_from_value(&code)?, String::try_from_value(&string)?);
                results.push(Err(fault));
            }
        }

        Ok(results)
    }
}

fn request_to_body(call: &MethodCall) -> Result<String, DxrError> {
    let body = [
        r#"<?xml version="1.0"?>"#,
        dxr::serialize_xml(&call)
            .map_err(|error| DxrError::invalid_data(error.to_string()))?
            .as_str(),
        "",
    ]
        .join("\n");

    Ok(body)
}

fn response_to_result(contents: &str) -> Result<MethodResponse, ClientError> {
    // need to check for FaultResponse first:
    // - a missing <params> tag is ambiguous (can be either an empty response, or a fault response)
    // - a present <fault> tag is unambiguous
    let error2 = match dxr::deserialize_xml(contents) {
        Ok(fault) => {
            let response: FaultResponse = fault;
            return match Fault::try_from(response) {
                // server fault: return Fault
                Ok(fault) => Err(fault.into()),
                // malformed server fault: return DxrError
                Err(error) => Err(error.into()),
            };
        }
        Err(error) => error.to_string(),
    };

    let error1 = match dxr::deserialize_xml(contents) {
        Ok(response) => return Ok(response),
        Err(error) => error.to_string(),
    };

    // log errors if the contents could not be deserialized as either response or fault
    log::debug!("Failed to deserialize response as either value or fault.");
    log::debug!("Response failed with: {}; Fault failed with: {}", error1, error2);

    // malformed response: return DxrError::InvalidData
    Err(DxrError::invalid_data(contents.to_owned()).into())
}
