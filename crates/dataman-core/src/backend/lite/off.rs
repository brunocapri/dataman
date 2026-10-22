//! A build without the `sqlite` feature: SQLite profiles load and show, connecting says why not.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};

use super::Ran;
use super::embed::{self, Columns};
use crate::config::ConnectionProfile;
use crate::result::{ColumnInfo, ErrorInfo, ResultSet};
use crate::server::{ServerExtension, ServerInfo};
use crate::tx::TxWatch;
use crate::value::Kind;

const OFF: &str = "this Dataman was built without SQLite";

#[derive(Debug)]
pub(crate) struct LiteError;

impl std::fmt::Display for LiteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(OFF)
    }
}

impl std::error::Error for LiteError {}

#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub(crate) struct Open {
    pub read_only: bool,
    pub agent: Option<Agent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agent {
    Read,
    Write,
}

/// Never made: `connect` always fails.
pub(crate) enum Client {}

pub(crate) async fn connect(_: &ConnectionProfile, _: Open, _: Option<&Arc<TxWatch>>) -> Result<Arc<Client>> {
    bail!(OFF)
}

impl Client {
    pub(crate) fn is_closed(&self) -> bool {
        match *self {}
    }

    pub(crate) fn canceller(&self) -> Canceller {
        match *self {}
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        match *self {}
    }

    pub(crate) async fn execute(&self, _: &str) -> Result<Option<u64>, LiteError> {
        match *self {}
    }

    pub(crate) async fn text_rows(&self, _: &str) -> Result<Vec<Vec<Option<String>>>, LiteError> {
        match *self {}
    }

    pub(crate) async fn prepare(&self, _: &str) -> Result<Vec<ColumnInfo>, LiteError> {
        match *self {}
    }

    pub(crate) async fn stream(&self, _: &str, _: &[Option<String>], _: Option<Vec<Kind>>) -> Result<(Columns, embed::Rows<LiteError>), LiteError> {
        match *self {}
    }
}

#[derive(Clone)]
pub(crate) enum Canceller {}

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        match *self {}
    }
}

pub(crate) async fn server_info(c: &Client, _: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    match *c {}
}

pub(crate) fn error_info(_: &LiteError) -> ErrorInfo {
    ErrorInfo { message: OFF.into(), ..Default::default() }
}

pub(crate) fn error_info_at(e: &LiteError, _: &str) -> ErrorInfo {
    error_info(e)
}

pub(crate) async fn run(
    c: &Arc<Client>,
    _: &Arc<ResultSet>,
    _: Instant,
    _: &(dyn Fn() + Sync),
    _: &(dyn Fn() -> bool + Sync),
    _: &(dyn Fn(&Arc<ResultSet>) -> Arc<ResultSet> + Sync),
) -> Ran {
    match **c {}
}
