use std::result;
use std::{io, ops::Range};

use thiserror::Error;

use crate::value::ValuePointer;

#[derive(Debug)]
pub struct InvalidValuePointerError {
    pub vptr: ValuePointer,
    pub kvlen: usize,
    pub range: Range<u32>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid Configuration: {0}")]
    Config(String),
    #[error("IO error: {0}")]
    Io(#[source] Box<io::Error>),
    #[error("Empty key")]
    EmptyKey,
    #[error("Too long: {0}")]
    TooLong(String),
    #[error("Invalid checksum")]
    InvalidChecksum(String),
    #[error("Invalid filename")]
    InvalidFilename(String),
    #[error("Invalid prost data: {0}")]
    Decode(#[source] Box<prost::DecodeError>),
    #[error("Invalid data: {0}")]
    VarDecode(&'static str),
    #[error("Error when reading table: {0}")]
    TableRead(String),
    #[error("Database Closed")]
    DBClosed,
    #[error("Error when reading from log: {0}")]
    LogRead(String),
    #[error("Invalid VP: {0:?}")]
    InvalidValuePointer(Box<InvalidValuePointerError>),
    #[error("Invalid Log Offset: {0} > {1}")]
    InvalidLogOffset(u32, u32),
    #[error("VLog Not Found: id={0}")]
    VlogNotFound(u32),
    #[error("Error when compaction: {0}")]
    CompactionError(String),
}

impl From<io::Error> for Error {
    #[inline]
    fn from(e: io::Error) -> Error {
        Error::Io(Box::new(e))
    }
}

impl From<InvalidValuePointerError> for Error {
    #[inline]
    fn from(e: InvalidValuePointerError) -> Error {
        Error::InvalidValuePointer(Box::new(e))
    }
}

impl From<prost::DecodeError> for Error {
    #[inline]
    fn from(e: prost::DecodeError) -> Error {
        Error::Decode(Box::new(e))
    }
}

pub type Result<T> = result::Result<T, Error>;
