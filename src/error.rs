use std::fmt;

/// Every failure maps to one of the documented exit codes.
#[derive(Debug)]
pub enum Error {
    /// Exit 1: provider or network failure.
    Provider(String),
    /// Exit 2: bad invocation.
    Usage(String),
    /// Exit 3: parse error, missing key, missing provider.
    Config(String),
    /// Exit 4: refused because of insecure config permissions.
    Refused(String),
    /// Already reported to the user; just exit with this code.
    Silent(u8),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn code(&self) -> u8 {
        match self {
            Error::Provider(_) => 1,
            Error::Usage(_) => 2,
            Error::Config(_) => 3,
            Error::Refused(_) => 4,
            Error::Silent(c) => *c,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Provider(m) | Error::Usage(m) | Error::Config(m) | Error::Refused(m) => {
                f.write_str(m)
            }
            Error::Silent(_) => Ok(()),
        }
    }
}

pub fn config_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Config(msg.into()))
}

pub fn usage_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Usage(msg.into()))
}
