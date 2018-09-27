use std::fmt;

#[derive(Debug, PartialEq, Clone)]
pub struct ServerError(String);
impl From<String> for ServerError {
    fn from(s: String) -> Self {
        ServerError(s)
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ServerError").field(&self.0).finish()
    }
}