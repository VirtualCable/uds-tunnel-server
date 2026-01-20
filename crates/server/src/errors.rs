use std::{fmt, net::SocketAddr};

#[derive(Debug)]
pub struct ErrorWithAddres {
    pub src_ip: Option<SocketAddr>,
    pub message: String,
}

impl std::error::Error for ErrorWithAddres {}

impl ErrorWithAddres {
    pub fn new(src_ip: Option<SocketAddr>, message: &str) -> Self {
        ErrorWithAddres {
            src_ip,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for ErrorWithAddres {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SocketAddrError: {} {:?}", self.message, self.src_ip)
    }
}
