#![allow(clippy::redundant_pattern_matching)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::iter_kv_map)]
#![allow(clippy::len_without_is_empty)]
#![allow(clippy::vec_init_then_push)]
#![allow(clippy::while_let_on_iterator)]
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::new_without_default)]
#![cfg_attr(docsrs, deny(rustdoc::broken_intra_doc_links))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]
#![doc = include_str!("../README.md")]

#[macro_use]
extern crate async_trait;
#[macro_use]
extern crate log;

pub extern crate bytes;
pub extern crate bytes_utils;
#[cfg(feature = "enable-native-tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "enable-native-tls")))]
pub extern crate native_tls;
#[cfg(feature = "enable-rustls")]
#[cfg_attr(docsrs, doc(cfg(feature = "enable-rustls")))]
pub extern crate rustls;
#[cfg(feature = "enable-rustls")]
#[cfg_attr(docsrs, doc(cfg(feature = "enable-rustls")))]
pub extern crate rustls_native_certs;
#[cfg(feature = "serde-json")]
pub extern crate serde_json;
pub extern crate socket2;
#[cfg(feature = "codec")]
#[cfg_attr(docsrs, doc(cfg(feature = "codec")))]
pub extern crate tokio_util;
#[cfg(feature = "partial-tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "partial-tracing")))]
pub extern crate tracing;
#[cfg(any(feature = "full-tracing", feature = "partial-tracing"))]
extern crate tracing_futures;

#[macro_use]
mod macros;

mod commands;
mod modules;
mod protocol;
mod router;
mod trace;
mod utils;

/// Redis client implementations.
pub mod clients;
/// Error structs returned by Redis commands.
pub mod error;
/// Traits that implement portions of the Redis interface.
pub mod interfaces;
#[cfg(feature = "mocks")]
#[cfg_attr(docsrs, doc(cfg(feature = "mocks")))]
pub use modules::mocks;
/// An interface to run the `MONITOR` command.
#[cfg(feature = "monitor")]
#[cfg_attr(docsrs, doc(cfg(feature = "monitor")))]
pub mod monitor;
/// The structs and enums used by the Redis client.
pub mod types;

/// Codecs for use with the [tokio codec](https://docs.rs/tokio-util/latest/tokio_util/codec/index.html) interface.
#[cfg(feature = "codec")]
#[cfg_attr(docsrs, doc(cfg(feature = "codec")))]
pub mod codec {
  pub use super::protocol::public::*;
}

/// Utility functions used by the client that may also be useful to callers.
pub mod util {
  pub use crate::utils::{f64_to_redis_string, redis_string_to_f64, static_bytes, static_str};
  pub use redis_protocol::redis_keyslot;

  /// A convenience constant for `None` values used as generic arguments.
  ///
  /// Functions that take `Option<T>` as an argument often require the caller to use a turbofish when the
  /// variant is `None`. In many cases this constant can be used instead.
  // pretty much everything in this crate supports From<String>
  pub const NONE: Option<String> = None;

  /// Calculate the SHA1 hash output as a hex string. This is provided for clients that use the Lua interface to
  /// manage their own script caches.
  #[cfg(feature = "sha-1")]
  #[cfg_attr(docsrs, doc(cfg(feature = "sha-1")))]
  pub fn sha1_hash(input: &str) -> String {
    use sha1::Digest;

    let mut hasher = sha1::Sha1::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
  }
}

pub use crate::modules::globals;

/// Convenience module to import a `RedisClient`, all possible interfaces, error types, and common argument types or
/// return value types.
pub mod prelude {
  #[cfg(feature = "partial-tracing")]
  #[cfg_attr(docsrs, doc(cfg(feature = "partial-tracing")))]
  pub use crate::types::TracingConfig;

  pub use crate::{
    clients::{RedisClient, RedisPool},
    error::{RedisError, RedisErrorKind},
    interfaces::*,
    types::{
      Blocking,
      Builder,
      ConnectionConfig,
      Expiration,
      FromRedis,
      Options,
      PerformanceConfig,
      ReconnectPolicy,
      RedisConfig,
      RedisKey,
      RedisValue,
      RedisValueKind,
      ServerConfig,
      SetOptions,
      TcpConfig,
    },
  };

  #[cfg(any(feature = "enable-native-tls", feature = "enable-rustls"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "enable-rustls", feature = "enable-native-tls"))))]
  pub use crate::types::{TlsConfig, TlsConnector};
}
