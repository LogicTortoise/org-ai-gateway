//! Shared imports re-exported across the crate's modules. Each module does
//! `use crate::prelude::*;` to pull in the common external types plus the
//! crate's own widely-shared items, keeping per-module import noise low.
#![allow(unused_imports)]

pub(crate) use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

pub(crate) use axum::{
    extract::{
        ws::{Message as ClientWsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header::AUTHORIZATION, header::CONTENT_TYPE, HeaderMap, HeaderValue, Request, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
pub(crate) use chrono::{DateTime, NaiveDate, Utc};
pub(crate) use futures_util::{SinkExt, StreamExt};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::{json, Value};
pub(crate) use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::RwLock};
pub(crate) use tokio_tungstenite::{
    connect_async, tungstenite::client::IntoClientRequest, tungstenite::Message as UpstreamWsMessage,
};
pub(crate) use tracing::{error, info, warn};
pub(crate) use uuid::Uuid;

// Crate-internal: only the two truly ubiquitous modules. Everything else is
// imported explicitly at its use sites — re-exporting modules flat through the
// prelude marks every item as "used" and silently disables the compiler's
// dead-code detection for the whole crate.
pub(crate) use crate::models::*;
pub(crate) use crate::state::*;
