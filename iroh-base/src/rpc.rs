use std::fmt;

use serde::{Deserialize, Serialize};

/// A serializable error type for use in RPC responses.
#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub struct RpcError(serde_error::Error);

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        RpcError(serde_error::Error::new(&*e))
    }
}

impl From<std::io::Error> for RpcError {
    fn from(e: std::io::Error) -> Self {
        RpcError(serde_error::Error::new(&e))
    }
}

/// A serializable result type for use in RPC responses.
pub type RpcResult<T> = std::result::Result<T, RpcError>;
