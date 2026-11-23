//! HTTP integration suite: shared harness in child modules, tests by area.

#[path = "http/checks.rs"]
mod checks;
#[path = "http/fixtures.rs"]
mod fixtures;
#[path = "http/harness.rs"]
mod harness;
#[path = "http/scenarios.rs"]
mod scenarios;
mod support;

pub(crate) use std::fs;
pub(crate) use std::io::{Read, Write};
pub(crate) use std::net::TcpStream;
pub(crate) use std::time::{Duration, Instant};

pub(crate) use ed25519_dalek::SigningKey;
pub(crate) use serde_json::{json, Value};

pub(crate) use checks::*;
pub(crate) use fixtures::*;
pub(crate) use harness::*;
pub(crate) use scenarios::*;

#[path = "http/auth.rs"]
mod auth;
#[path = "http/backup_rules.rs"]
mod backup_rules;
#[path = "http/config.rs"]
mod config;
#[path = "http/control.rs"]
mod control;
#[path = "http/install.rs"]
mod install;
#[path = "http/lifecycle.rs"]
mod lifecycle;
#[path = "http/logs.rs"]
mod logs;
#[path = "http/mcp.rs"]
mod mcp;
#[path = "http/protocol.rs"]
mod protocol;
#[path = "http/settings.rs"]
mod settings;
#[path = "http/static_site.rs"]
mod static_site;
#[path = "http/transfer.rs"]
mod transfer;
#[path = "http/ui.rs"]
mod ui;
#[path = "http/upgrade.rs"]
mod upgrade;
