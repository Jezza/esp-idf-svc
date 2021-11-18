use core::ptr;
use core::marker::PhantomData;

extern crate alloc;
use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;

use log::info;

use crate::private::cstr::CString;

use embedded_svc::http::server::{
    HandlerRegistration, InlineHandler, InlineResponse, Registry, Request, SendError,
};
use embedded_svc::http::*;
use embedded_svc::io::{Read, Write};

use esp_idf_sys::*;

use crate::private::common::Newtype;

#[derive(Copy, Clone, Debug)]
pub struct Configuration {
    pub http_port: u16,
    pub https_port: u16,
}

impl Default for Configuration {
    fn default() -> Self {
        Configuration {
            http_port: 80,
            https_port: 443,
        }
    }
}

impl From<&Configuration> for Newtype<httpd_config_t> {
    fn from(conf: &Configuration) -> Self {
        Self(httpd_config_t {
            task_priority: 5,
            stack_size: if conf.https_port != 0 { 10240 } else { 4096 },
            core_id: std::i32::MAX,
            server_port: conf.http_port,
            ctrl_port: 32768,
            max_open_sockets: if conf.https_port != 0 { 4 } else { 7 },
            max_uri_handlers: 8,
            max_resp_headers: 8,
            backlog_conn: 5,
            lru_purge_enable: conf.https_port != 0,
            recv_wait_timeout: 5,
            send_wait_timeout: 5,
            global_user_ctx: ptr::null_mut(),
            global_user_ctx_free_fn: None,
            global_transport_ctx: ptr::null_mut(),
            global_transport_ctx_free_fn: None,
            open_fn: None,
            close_fn: None,
            uri_match_fn: None,
        })
    }
}

impl From<Method> for Newtype<c_types::c_uint> {
    fn from(method: Method) -> Self {
        Self(match method {
            Method::Get => esp_idf_sys::http_method_HTTP_GET,
            Method::Post => esp_idf_sys::http_method_HTTP_POST,
            Method::Delete => esp_idf_sys::http_method_HTTP_DELETE,
            Method::Head => esp_idf_sys::http_method_HTTP_HEAD,
            Method::Put => esp_idf_sys::http_method_HTTP_PUT,
            Method::Connect => esp_idf_sys::http_method_HTTP_CONNECT,
            Method::Options => esp_idf_sys::http_method_HTTP_OPTIONS,
            Method::Trace => esp_idf_sys::http_method_HTTP_TRACE,
            Method::Copy => esp_idf_sys::http_method_HTTP_COPY,
            Method::Lock => esp_idf_sys::http_method_HTTP_LOCK,
            Method::MkCol => esp_idf_sys::http_method_HTTP_MKCOL,
            Method::Move => esp_idf_sys::http_method_HTTP_MOVE,
            Method::Propfind => esp_idf_sys::http_method_HTTP_PROPFIND,
            Method::Proppatch => esp_idf_sys::http_method_HTTP_PROPPATCH,
            Method::Search => esp_idf_sys::http_method_HTTP_SEARCH,
            Method::Unlock => esp_idf_sys::http_method_HTTP_UNLOCK,
            Method::Bind => esp_idf_sys::http_method_HTTP_BIND,
            Method::Rebind => esp_idf_sys::http_method_HTTP_REBIND,
            Method::Unbind => esp_idf_sys::http_method_HTTP_UNBIND,
            Method::Acl => esp_idf_sys::http_method_HTTP_ACL,
            Method::Report => esp_idf_sys::http_method_HTTP_REPORT,
            Method::MkActivity => esp_idf_sys::http_method_HTTP_MKACTIVITY,
            Method::Checkout => esp_idf_sys::http_method_HTTP_CHECKOUT,
            Method::Merge => esp_idf_sys::http_method_HTTP_MERGE,
            Method::MSearch => esp_idf_sys::http_method_HTTP_MSEARCH,
            Method::Notify => esp_idf_sys::http_method_HTTP_NOTIFY,
            Method::Subscribe => esp_idf_sys::http_method_HTTP_SUBSCRIBE,
            Method::Unsubscribe => esp_idf_sys::http_method_HTTP_UNSUBSCRIBE,
            Method::Patch => esp_idf_sys::http_method_HTTP_PATCH,
            Method::Purge => esp_idf_sys::http_method_HTTP_PURGE,
            Method::MkCalendar => esp_idf_sys::http_method_HTTP_MKCALENDAR,
            Method::Link => esp_idf_sys::http_method_HTTP_LINK,
            Method::Unlink => esp_idf_sys::http_method_HTTP_UNLINK,
        })
    }
}

pub struct EspHttpServer {
    sd: esp_idf_sys::httpd_handle_t,
    registrations: Vec<(CString, esp_idf_sys::httpd_uri_t)>,
}

impl EspHttpServer {
    pub fn new(conf: &Configuration) -> Result<Self, EspError> {
        let config: Newtype<esp_idf_sys::httpd_config_t> = conf.into();

        let mut handle: esp_idf_sys::httpd_handle_t = ptr::null_mut();
        let handle_ref = &mut handle;

        esp!(unsafe { esp_idf_sys::httpd_start(handle_ref, &config.0 as *const _) })?;

        info!("Started Httpd server with config {:?}", conf);

        Ok(EspHttpServer {
            sd: handle,
            registrations: vec![],
        })
    }

    fn unregister(&mut self, uri: CString, conf: esp_idf_sys::httpd_uri_t) -> Result<(), EspError> {
        unsafe {
            esp!(esp_idf_sys::httpd_unregister_uri_handler(
                self.sd,
                uri.as_ptr() as _,
                conf.method
            ))?;

            let _drop = Box::from_raw(conf.user_ctx as *mut _);
        };

        info!(
            "Unregistered Httpd server handler {:?} for URI \"{}\"",
            conf.method,
            uri.to_str().unwrap()
        );

        Ok(())
    }

    fn stop(&mut self) -> Result<(), EspError> {
        if !self.sd.is_null() {
            while !self.registrations.is_empty() {
                let (uri, registration) = self.registrations.pop().unwrap();

                self.unregister(uri, registration)?;
            }

            esp!(unsafe { esp_idf_sys::httpd_stop(self.sd) })?;

            self.sd = ptr::null_mut();
        }

        info!("Httpd server stopped");

        Ok(())
    }

    unsafe extern "C" fn handle(raw_req: *mut httpd_req_t) -> c_types::c_int {
        let handler = ((*raw_req).user_ctx
            as *mut Box<
                dyn Fn(EspHttpRequest, EspHttpResponse) -> Result<(), EspError>,
            >)
            .as_ref()
            .unwrap();

        let request = EspHttpRequest {
            raw_req,
            _data: PhantomData,
        };

        let response = EspHttpResponse {
            raw_req,
            status: 200,
            status_message: None,
            headers: BTreeMap::new(),
        };

        info!("About to handle query string {:?}", request.query_string());

        if let Err(_err) = handler(request, response) {
            // TODO
            return 0;
        }

        0

        // let mut idf_request_response = IdfRequest(rd, PhantomData);

        // log!(
        //     if err { Level::Warn } else { Level::Info },
        //     "Request handled with status {} ({:?})",
        //     &response.status,
        //     &response.status_message
        // );

        // match idf_request_response.send(response) {
        //     Result::Ok(_) => esp_idf_sys::ESP_OK as _,
        //     Result::Err(_) => esp_idf_sys::ESP_FAIL as _,
        // }
    }
}

impl Drop for EspHttpServer {
    fn drop(&mut self) {
        self.stop().expect("Unable to stop the server cleanly");
    }
}

impl Registry for EspHttpServer {
    type Request<'a> = EspHttpRequest<'a>;
    type Response<'a> = EspHttpResponse<'a>;
    type Error = EspError;

    fn set_inline_handler<'a, F>(&mut self, handler: HandlerRegistration<F>) -> Result<&mut Self, Self::Error>
    where
        F: InlineHandler<'a, Self::Request<'a>, Self::Response<'a>>
    {
        let c_str = CString::new(handler.uri()).unwrap();
        let method = handler.method();

        let conf = esp_idf_sys::httpd_uri_t {
            uri: c_str.as_ptr() as _,
            method: Newtype::<c_types::c_uint>::from(method).0,
            user_ctx: Box::into_raw(Box::new(handler.handler())) as *mut _,
            handler: Some(EspHttpServer::handle),
        };

        esp!(unsafe { esp_idf_sys::httpd_register_uri_handler(self.sd, &conf) })?;

        info!(
            "Registered Httpd server handler {:?} for URI \"{}\"",
            method,
            c_str.to_str().unwrap()
        );

        self.registrations.push((c_str, conf));

        Ok(self)
    }
}

pub struct EspHttpRequest<'a> {
    raw_req: *mut httpd_req_t,
    _data: PhantomData<&'a httpd_req_t>,
}

impl<'a> Request<'a> for EspHttpRequest<'a> {
    fn query_string(&self) -> Cow<'a, str> {
        unsafe {
            match esp_idf_sys::httpd_req_get_url_query_len(self.raw_req) as usize {
                0 => "".into(),
                len => {
                    // TODO: Would've been much more effective, if ESP-IDF was capable of returning a
                    // pointer to the header value that is in the scratch buffer
                    //
                    // Check if we can implement it ourselves vy traversing the scratch buffer manually

                    let mut buf: vec::Vec<u8> = Vec::with_capacity(len + 1);

                    esp_nofail!(esp_idf_sys::httpd_req_get_url_query_str(
                        self.raw_req,
                        buf.as_mut_ptr() as *mut _,
                        (len + 1) as esp_idf_sys::size_t
                    ));

                    buf.set_len(len + 1);

                    // TODO: Replace with a proper conversion from ISO-8859-1 to UTF8
                    String::from_utf8_lossy(&buf[..len]).into_owned().into()
                }
            }
        }
    }
}

impl<'a> Read for EspHttpRequest<'a> {
    type Error = EspError;

    fn do_read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        unsafe {
            let len = esp_idf_sys::httpd_req_recv(
                self.raw_req,
                buf.as_mut_ptr() as *mut _,
                buf.len() as esp_idf_sys::size_t,
            );

            if len < 0 {
                esp!(len)?;
            }

            Ok(len as usize)
        }
    }
}

impl<'a> Headers<'a> for EspHttpRequest<'a> {
    fn header(&self, name: impl AsRef<str>) -> Option<Cow<'a, str>> {
        let c_name = CString::new(name.as_ref()).unwrap();

        unsafe {
            match esp_idf_sys::httpd_req_get_hdr_value_len(self.raw_req, c_name.as_ptr() as _) as usize {
                0 => None,
                len => {
                    // TODO: Would've been much more effective, if ESP-IDF was capable of returning a
                    // pointer to the header value that is in the scratch buffer
                    //
                    // Check if we can implement it ourselves vy traversing the scratch buffer manually

                    let mut buf: vec::Vec<u8> = Vec::with_capacity(len + 1);

                    esp_nofail!(esp_idf_sys::httpd_req_get_hdr_value_str(
                        self.raw_req,
                        c_name.as_ptr() as _,
                        buf.as_mut_ptr() as *mut _,
                        (len + 1) as esp_idf_sys::size_t
                    ));

                    buf.set_len(len + 1);

                    // TODO: Replace with a proper conversion from ISO-8859-1 to UTF8
                    Some(String::from_utf8_lossy(&buf[..len]).into_owned().into())
                }
            }
        }
    }
}

pub struct EspHttpResponse<'a> {
    raw_req: *mut httpd_req_t,
    status: u16,
    status_message: Option<Cow<'a, str>>,
    headers: BTreeMap<Cow<'a, str>, Cow<'a, str>>,
}

impl <'a> EspHttpResponse<'a> {
    fn send_headers(&self) -> Result<(CString, Vec<(CString, CString)>, Option<CString>), EspError> {
        // TODO: Would be much more effective if we are serializing the status line and headers directly
        // Consider implement this, based on http_resp_send() - even though that would require implementing
        // chunking in Rust

        let status = if let Some(ref status_message) = self.status_message {
            format!("{} {}", self.status, status_message)
        } else {
            self.status.to_string()
        };

        let c_status = CString::new(status.as_str()).unwrap();

        esp!(unsafe { esp_idf_sys::httpd_resp_set_status(self.raw_req, c_status.as_ptr() as _) })?;

        let mut c_headers: Vec<(CString, CString)> = vec![];
        let mut c_content_type: Option<CString> = None;
        let mut content_len: Option<usize> = None; // TODO: Use it

        for (key, value) in &self.headers {
            if key.as_ref().eq_ignore_ascii_case("Content-Type") {
                c_content_type = Some(CString::new(value.as_ref()).unwrap());
            } else if key.as_ref().eq_ignore_ascii_case("Content-Length") {
                content_len = Some(
                    value
                        .as_ref()
                        .parse::<usize>()
                        .map_err(|_| EspError::from(ESP_ERR_INVALID_ARG as _).unwrap())?,
                );
            } else {
                c_headers.push((
                    CString::new(key.as_ref()).unwrap(),
                    // TODO: Replace with a proper conversion from UTF8 to ISO-8859-1
                    CString::new(value.as_ref()).unwrap(),
                ))
            }
        }

        if let Some(c_content_type) = c_content_type.as_ref() {
            esp!(unsafe {
                esp_idf_sys::httpd_resp_set_type(self.raw_req, c_content_type.as_ptr())
            })?
        }

        for (c_field, c_value) in &c_headers {
            esp!(unsafe {
                esp_idf_sys::httpd_resp_set_hdr(
                    self.raw_req,
                    c_field.as_ptr() as _,
                    c_value.as_ptr() as _,
                )
            })?;
        }

        Ok((c_status, c_headers, c_content_type))
    }
}

impl<'a> SendStatus<'a> for EspHttpResponse<'a> {
    fn set_status(&mut self, status: u16) -> &mut Self {
        self.status = status;
        self
    }

    fn set_status_message<M>(&mut self, message: M) -> &mut Self
    where
        M: Into<Cow<'a, str>>,
    {
        self.status_message = Some(message.into());
        self
    }
}

impl<'a> SendHeaders<'a> for EspHttpResponse<'a> {
    fn set_header<H, V>(&mut self, name: H, value: V) -> &mut Self
    where
        H: Into<Cow<'a, str>>,
        V: Into<Cow<'a, str>>,
    {
        *self.headers.entry(name.into()).or_insert(Cow::Borrowed("")) = value.into();
        self
    }
}

impl<'a> InlineResponse<'a> for EspHttpResponse<'a> {
    type Write = Self;
    type Error = EspError;

    #[cfg(feature = "std")]
    fn send<E: std::error::Error + Send + Sync + 'static>(
        self,
        request: impl Request<'a>,
        f: impl FnOnce(&mut Self::Write) -> Result<(), SendError<Self::Error, E>>,
    ) -> Result<(), SendError<Self::Error, E>>
    where
        Self: Sized,
    {
        let _headers = self.send_headers().map_err(SendError::SendError)?;

        f(&mut self)?;

        esp!(unsafe {
            esp_idf_sys::httpd_resp_send_chunk(self.raw_req, core::ptr::null() as *const _, 0)
        }).map_err(SendError::SendError)?;

        Ok(())
    }

    #[cfg(not(feature = "std"))]
    fn send<E: fmt::Display + fmt::Debug>(
        self,
        request: impl Request<'a>,
        f: impl FnOnce(&mut Self::Write) -> Result<(), SendError<Self::Error, E>>,
    ) -> Result<(), SendError<Self::Error, E>>
    where
        Self: Sized,
    {
        let _headers = self.send_headers().map_err(SendError::SendError)?;

        f(&mut self)?;

        esp!(unsafe {
            esp_idf_sys::httpd_resp_send_chunk(self.raw_req, core::ptr::null() as *const _, 0)
        }).map_err(SendError::SendError)?;

        Ok(())
    }
}

impl<'a> Write for EspHttpResponse<'a> {
    type Error = EspError;

    fn do_write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if !buf.is_empty() {
            esp!(unsafe {
                esp_idf_sys::httpd_resp_send_chunk(
                    self.raw_req,
                    buf.as_ptr() as *const _,
                    buf.len() as esp_idf_sys::ssize_t,
                )
            })?;
        }

        Ok(buf.len())
    }
}
