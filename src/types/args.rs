use std::{
  borrow::Cow,
  collections::{BTreeMap, HashMap, HashSet, VecDeque},
  convert::{TryFrom, TryInto},
  fmt,
  hash::{Hash, Hasher},
  iter::FromIterator,
  mem,
  ops::{Deref, DerefMut},
  str,
};

use bytes::Bytes;
use bytes_utils::Str;
use float_cmp::approx_eq;
use redis_protocol::resp2::types::NULL;
#[cfg(feature = "serde-json")]
use serde_json::Value;

use crate::{
  error::{RedisError, RedisErrorKind},
  interfaces::{ClientLike, Resp3Frame},
  protocol::{connection::OK, utils as protocol_utils},
  types::{FromRedis, FromRedisKey, Function, GeoPosition, GeoRadiusInfo, Server, XReadResponse, XReadValue, QUEUED},
  utils,
};

static_str!(TRUE_STR, "true");
static_str!(FALSE_STR, "false");

macro_rules! impl_string_or_number(
    ($t:ty) => {
        impl From<$t> for StringOrNumber {
            fn from(val: $t) -> Self {
                StringOrNumber::Number(val as i64)
            }
        }
    }
);

macro_rules! impl_from_str_for_redis_key(
    ($t:ty) => {
        impl From<$t> for RedisKey {
            fn from(val: $t) -> Self {
                RedisKey { key: val.to_string().into() }
            }
        }
    }
);

/// An argument representing a string or number.
#[derive(Clone, Debug)]
pub enum StringOrNumber {
  String(Str),
  Number(i64),
  Double(f64),
}

impl PartialEq for StringOrNumber {
  fn eq(&self, other: &Self) -> bool {
    match *self {
      StringOrNumber::String(ref s) => match *other {
        StringOrNumber::String(ref _s) => s == _s,
        _ => false,
      },
      StringOrNumber::Number(ref i) => match *other {
        StringOrNumber::Number(ref _i) => *i == *_i,
        _ => false,
      },
      StringOrNumber::Double(ref d) => match *other {
        StringOrNumber::Double(ref _d) => utils::f64_eq(*d, *_d),
        _ => false,
      },
    }
  }
}

impl Eq for StringOrNumber {}

impl StringOrNumber {
  /// An optimized way to convert from `&'static str` that avoids copying or moving the underlying bytes.
  pub fn from_static_str(s: &'static str) -> Self {
    StringOrNumber::String(utils::static_str(s))
  }

  pub(crate) fn into_arg(self) -> RedisValue {
    match self {
      StringOrNumber::String(s) => RedisValue::String(s),
      StringOrNumber::Number(n) => RedisValue::Integer(n),
      StringOrNumber::Double(f) => RedisValue::Double(f),
    }
  }
}

impl TryFrom<RedisValue> for StringOrNumber {
  type Error = RedisError;

  fn try_from(value: RedisValue) -> Result<Self, Self::Error> {
    let val = match value {
      RedisValue::String(s) => StringOrNumber::String(s),
      RedisValue::Integer(i) => StringOrNumber::Number(i),
      RedisValue::Double(f) => StringOrNumber::Double(f),
      RedisValue::Bytes(b) => StringOrNumber::String(Str::from_inner(b)?),
      _ => return Err(RedisError::new(RedisErrorKind::InvalidArgument, "")),
    };

    Ok(val)
  }
}

impl<'a> From<&'a str> for StringOrNumber {
  fn from(s: &'a str) -> Self {
    StringOrNumber::String(s.into())
  }
}

impl From<String> for StringOrNumber {
  fn from(s: String) -> Self {
    StringOrNumber::String(s.into())
  }
}

impl From<Str> for StringOrNumber {
  fn from(s: Str) -> Self {
    StringOrNumber::String(s)
  }
}

impl_string_or_number!(i8);
impl_string_or_number!(i16);
impl_string_or_number!(i32);
impl_string_or_number!(i64);
impl_string_or_number!(isize);
impl_string_or_number!(u8);
impl_string_or_number!(u16);
impl_string_or_number!(u32);
impl_string_or_number!(u64);
impl_string_or_number!(usize);

impl From<f32> for StringOrNumber {
  fn from(f: f32) -> Self {
    StringOrNumber::Double(f as f64)
  }
}

impl From<f64> for StringOrNumber {
  fn from(f: f64) -> Self {
    StringOrNumber::Double(f)
  }
}

/// A key in Redis.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RedisKey {
  key: Bytes,
}

impl RedisKey {
  /// Create a new `RedisKey` from static bytes without copying.
  pub fn from_static(b: &'static [u8]) -> Self {
    RedisKey {
      key: Bytes::from_static(b),
    }
  }

  /// Create a new `RedisKey` from a `&'static str` without copying.
  pub fn from_static_str(b: &'static str) -> Self {
    RedisKey {
      key: Bytes::from_static(b.as_bytes()),
    }
  }

  /// Read the key as a str slice if it can be parsed as a UTF8 string.
  pub fn as_str(&self) -> Option<&str> {
    str::from_utf8(&self.key).ok()
  }

  /// Read the key as a byte slice.
  pub fn as_bytes(&self) -> &[u8] {
    &self.key
  }

  /// Read the inner `Bytes` struct.
  pub fn inner(&self) -> &Bytes {
    &self.key
  }

  /// Read the key as a lossy UTF8 string with `String::from_utf8_lossy`.
  pub fn as_str_lossy(&self) -> Cow<str> {
    String::from_utf8_lossy(&self.key)
  }

  /// Convert the key to a UTF8 string, if possible.
  pub fn into_string(self) -> Option<String> {
    String::from_utf8(self.key.to_vec()).ok()
  }

  /// Read the inner bytes making up the key.
  pub fn into_bytes(self) -> Bytes {
    self.key
  }

  /// Parse and return the key as a `Str` without copying the inner contents.
  pub fn as_bytes_str(&self) -> Option<Str> {
    Str::from_inner(self.key.clone()).ok()
  }

  /// Hash the key to find the associated cluster [hash slot](https://redis.io/topics/cluster-spec#keys-distribution-model).
  pub fn cluster_hash(&self) -> u16 {
    redis_protocol::redis_keyslot(&self.key)
  }

  /// Read the `host:port` of the cluster node that owns the key if the client is clustered and the cluster state is
  /// known.
  pub fn cluster_owner<C>(&self, client: &C) -> Option<Server>
  where
    C: ClientLike,
  {
    if client.is_clustered() {
      let hash_slot = self.cluster_hash();
      client
        .inner()
        .with_cluster_state(|state| Ok(state.get_server(hash_slot).cloned()))
        .ok()
        .and_then(|server| server)
    } else {
      None
    }
  }

  /// Replace this key with an empty byte array, returning the bytes from the original key.
  pub fn take(&mut self) -> Bytes {
    self.key.split_to(self.key.len())
  }

  /// Attempt to convert the key to any type that implements [FromRedisKey](crate::types::FromRedisKey).
  ///
  /// See the [RedisValue::convert](crate::types::RedisValue::convert) documentation for more information.
  pub fn convert<K>(self) -> Result<K, RedisError>
  where
    K: FromRedisKey,
  {
    K::from_key(self)
  }
}

impl TryFrom<RedisValue> for RedisKey {
  type Error = RedisError;

  fn try_from(value: RedisValue) -> Result<Self, Self::Error> {
    let val = match value {
      RedisValue::String(s) => RedisKey { key: s.into_inner() },
      RedisValue::Integer(i) => RedisKey {
        key: i.to_string().into(),
      },
      RedisValue::Double(f) => RedisKey {
        key: f.to_string().into(),
      },
      RedisValue::Bytes(b) => RedisKey { key: b },
      RedisValue::Boolean(b) => match b {
        true => RedisKey {
          key: TRUE_STR.clone().into_inner(),
        },
        false => RedisKey {
          key: FALSE_STR.clone().into_inner(),
        },
      },
      RedisValue::Queued => utils::static_str(QUEUED).into(),
      _ => {
        return Err(RedisError::new(
          RedisErrorKind::InvalidArgument,
          "Cannot convert to key.",
        ))
      },
    };

    Ok(val)
  }
}

impl From<Bytes> for RedisKey {
  fn from(b: Bytes) -> Self {
    RedisKey { key: b }
  }
}

impl From<Box<[u8]>> for RedisKey {
  fn from(b: Box<[u8]>) -> Self {
    RedisKey { key: b.into() }
  }
}

impl<'a> From<&'a [u8]> for RedisKey {
  fn from(b: &'a [u8]) -> Self {
    RedisKey { key: b.to_vec().into() }
  }
}

impl From<String> for RedisKey {
  fn from(s: String) -> Self {
    RedisKey { key: s.into() }
  }
}

impl From<&str> for RedisKey {
  fn from(s: &str) -> Self {
    RedisKey {
      key: s.as_bytes().to_vec().into(),
    }
  }
}

impl From<&String> for RedisKey {
  fn from(s: &String) -> Self {
    RedisKey { key: s.clone().into() }
  }
}

impl From<Str> for RedisKey {
  fn from(s: Str) -> Self {
    RedisKey { key: s.into_inner() }
  }
}

impl From<&Str> for RedisKey {
  fn from(s: &Str) -> Self {
    RedisKey { key: s.inner().clone() }
  }
}

impl From<&RedisKey> for RedisKey {
  fn from(k: &RedisKey) -> RedisKey {
    k.clone()
  }
}

impl From<bool> for RedisKey {
  fn from(b: bool) -> Self {
    match b {
      true => RedisKey::from_static_str("true"),
      false => RedisKey::from_static_str("false"),
    }
  }
}

impl_from_str_for_redis_key!(u8);
impl_from_str_for_redis_key!(u16);
impl_from_str_for_redis_key!(u32);
impl_from_str_for_redis_key!(u64);
impl_from_str_for_redis_key!(u128);
impl_from_str_for_redis_key!(usize);
impl_from_str_for_redis_key!(i8);
impl_from_str_for_redis_key!(i16);
impl_from_str_for_redis_key!(i32);
impl_from_str_for_redis_key!(i64);
impl_from_str_for_redis_key!(i128);
impl_from_str_for_redis_key!(isize);
impl_from_str_for_redis_key!(f32);
impl_from_str_for_redis_key!(f64);

/// A map of `(RedisKey, RedisValue)` pairs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedisMap {
  pub(crate) inner: HashMap<RedisKey, RedisValue>,
}

impl RedisMap {
  /// Create a new empty map.
  pub fn new() -> Self {
    RedisMap { inner: HashMap::new() }
  }

  /// Replace the value an empty map, returning the original value.
  pub fn take(&mut self) -> Self {
    RedisMap {
      inner: std::mem::take(&mut self.inner),
    }
  }

  /// Read the number of (key, value) pairs in the map.
  pub fn len(&self) -> usize {
    self.inner.len()
  }

  /// Take the inner `HashMap`.
  pub fn inner(self) -> HashMap<RedisKey, RedisValue> {
    self.inner
  }
}

impl Deref for RedisMap {
  type Target = HashMap<RedisKey, RedisValue>;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl DerefMut for RedisMap {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.inner
  }
}

impl<'a> From<&'a RedisMap> for RedisMap {
  fn from(vals: &'a RedisMap) -> Self {
    vals.clone()
  }
}

impl<K, V> TryFrom<HashMap<K, V>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: HashMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisMap {
      inner: utils::into_redis_map(value.into_iter())?,
    })
  }
}

impl<K, V> TryFrom<BTreeMap<K, V>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: BTreeMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisMap {
      inner: utils::into_redis_map(value.into_iter())?,
    })
  }
}

impl<K, V> TryFrom<(K, V)> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from((key, value): (K, V)) -> Result<Self, Self::Error> {
    let mut inner = HashMap::with_capacity(1);
    inner.insert(to!(key)?, to!(value)?);
    Ok(RedisMap { inner })
  }
}

impl<K, V> TryFrom<Vec<(K, V)>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(values: Vec<(K, V)>) -> Result<Self, Self::Error> {
    let mut inner = HashMap::with_capacity(values.len());
    for (key, value) in values.into_iter() {
      inner.insert(to!(key)?, to!(value)?);
    }
    Ok(RedisMap { inner })
  }
}

impl<K, V> TryFrom<VecDeque<(K, V)>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(values: VecDeque<(K, V)>) -> Result<Self, Self::Error> {
    let mut inner = HashMap::with_capacity(values.len());
    for (key, value) in values.into_iter() {
      inner.insert(to!(key)?, to!(value)?);
    }
    Ok(RedisMap { inner })
  }
}

/// The kind of value from Redis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedisValueKind {
  Boolean,
  Integer,
  Double,
  String,
  Bytes,
  Null,
  Queued,
  Map,
  Array,
}

impl fmt::Display for RedisValueKind {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let s = match *self {
      RedisValueKind::Boolean => "Boolean",
      RedisValueKind::Integer => "Integer",
      RedisValueKind::Double => "Double",
      RedisValueKind::String => "String",
      RedisValueKind::Bytes => "Bytes",
      RedisValueKind::Null => "nil",
      RedisValueKind::Queued => "Queued",
      RedisValueKind::Map => "Map",
      RedisValueKind::Array => "Array",
    };

    write!(f, "{}", s)
  }
}

/// A value used in a Redis command.
#[derive(Clone, Debug)]
pub enum RedisValue {
  /// A boolean value.
  Boolean(bool),
  /// An integer value.
  Integer(i64),
  /// A double floating point number.
  Double(f64),
  /// A string value.
  String(Str),
  /// A byte array value.
  Bytes(Bytes),
  /// A `nil` value.
  Null,
  /// A special value used to indicate a MULTI block command was received by the server.
  Queued,
  /// A map of key/value pairs, primarily used in RESP3 mode.
  Map(RedisMap),
  /// An ordered list of values.
  ///
  /// In RESP2 mode the server usually sends map structures as an array of key/value pairs.
  Array(Vec<RedisValue>),
}

#[allow(clippy::match_like_matches_macro)]
impl PartialEq for RedisValue {
  fn eq(&self, other: &Self) -> bool {
    use RedisValue::*;

    match self {
      Boolean(ref s) => match other {
        Boolean(ref o) => *s == *o,
        _ => false,
      },
      Integer(ref s) => match other {
        Integer(ref o) => *s == *o,
        _ => false,
      },
      Double(ref s) => match other {
        Double(ref o) => approx_eq!(f64, *s, *o, ulps = 2),
        _ => false,
      },
      String(ref s) => match other {
        String(ref o) => s == o,
        _ => false,
      },
      Bytes(ref s) => match other {
        Bytes(ref o) => s == o,
        _ => false,
      },
      Null => match other {
        Null => true,
        _ => false,
      },
      Queued => match other {
        Queued => true,
        _ => false,
      },
      Map(ref s) => match other {
        Map(ref o) => s == o,
        _ => false,
      },
      Array(ref s) => match other {
        Array(ref o) => s == o,
        _ => false,
      },
    }
  }
}

impl Eq for RedisValue {}

impl RedisValue {
  /// Create a new `RedisValue::Bytes` from a static byte slice without copying.
  pub fn from_static(b: &'static [u8]) -> Self {
    RedisValue::Bytes(Bytes::from_static(b))
  }

  /// Create a new `RedisValue::String` from a static `str` without copying.
  pub fn from_static_str(s: &'static str) -> Self {
    RedisValue::String(utils::static_str(s))
  }

  /// Create a new `RedisValue` with the `OK` status.
  pub fn new_ok() -> Self {
    Self::from_static_str(OK)
  }

  /// Whether or not the value is a simple string OK value.
  pub fn is_ok(&self) -> bool {
    match *self {
      RedisValue::String(ref s) => *s == OK,
      _ => false,
    }
  }

  /// Attempt to convert the value into an integer, returning the original string as an error if the parsing fails.
  pub fn into_integer(self) -> Result<RedisValue, RedisValue> {
    match self {
      RedisValue::String(s) => match s.parse::<i64>() {
        Ok(i) => Ok(RedisValue::Integer(i)),
        Err(_) => Err(RedisValue::String(s)),
      },
      RedisValue::Integer(i) => Ok(RedisValue::Integer(i)),
      _ => Err(self),
    }
  }

  /// Read the type of the value without any associated data.
  pub fn kind(&self) -> RedisValueKind {
    match *self {
      RedisValue::Boolean(_) => RedisValueKind::Boolean,
      RedisValue::Integer(_) => RedisValueKind::Integer,
      RedisValue::Double(_) => RedisValueKind::Double,
      RedisValue::String(_) => RedisValueKind::String,
      RedisValue::Bytes(_) => RedisValueKind::Bytes,
      RedisValue::Null => RedisValueKind::Null,
      RedisValue::Queued => RedisValueKind::Queued,
      RedisValue::Map(_) => RedisValueKind::Map,
      RedisValue::Array(_) => RedisValueKind::Array,
    }
  }

  /// Check if the value is null.
  pub fn is_null(&self) -> bool {
    matches!(*self, RedisValue::Null)
  }

  /// Check if the value is an integer.
  pub fn is_integer(&self) -> bool {
    matches!(self, RedisValue::Integer(_))
  }

  /// Check if the value is a string.
  pub fn is_string(&self) -> bool {
    matches!(*self, RedisValue::String(_))
  }

  /// Check if the value is an array of bytes.
  pub fn is_bytes(&self) -> bool {
    matches!(*self, RedisValue::Bytes(_))
  }

  /// Whether or not the value is a boolean value or can be parsed as a boolean value.
  #[allow(clippy::match_like_matches_macro)]
  pub fn is_boolean(&self) -> bool {
    match *self {
      RedisValue::Boolean(_) => true,
      RedisValue::Integer(i) => match i {
        0 | 1 => true,
        _ => false,
      },
      RedisValue::String(ref s) => match s.as_bytes() {
        b"true" | b"false" | b"t" | b"f" | b"TRUE" | b"FALSE" | b"T" | b"F" | b"1" | b"0" => true,
        _ => false,
      },
      _ => false,
    }
  }

  /// Whether or not the inner value is a double or can be parsed as a double.
  pub fn is_double(&self) -> bool {
    match *self {
      RedisValue::Double(_) => true,
      RedisValue::String(ref s) => utils::redis_string_to_f64(s).is_ok(),
      _ => false,
    }
  }

  /// Check if the value is a `QUEUED` response.
  pub fn is_queued(&self) -> bool {
    matches!(*self, RedisValue::Queued)
  }

  /// Whether or not the value is an array or map.
  pub fn is_aggregate_type(&self) -> bool {
    matches!(*self, RedisValue::Array(_) | RedisValue::Map(_))
  }

  /// Whether or not the value is a `RedisMap`.
  ///
  /// See [is_maybe_map](Self::is_maybe_map) for a function that also checks for arrays that likely represent a map in
  /// RESP2 mode.
  pub fn is_map(&self) -> bool {
    matches!(*self, RedisValue::Map(_))
  }

  /// Whether or not the value is a `RedisMap` or an array with an even number of elements where each even-numbered
  /// element is not an aggregate type.
  ///
  /// RESP2 and RESP3 encode maps differently, and this function can be used to duck-type maps across protocol
  /// versions.
  pub fn is_maybe_map(&self) -> bool {
    match *self {
      RedisValue::Map(_) => true,
      RedisValue::Array(ref arr) => utils::is_maybe_array_map(arr),
      _ => false,
    }
  }

  /// Whether or not the value is an array.
  pub fn is_array(&self) -> bool {
    matches!(*self, RedisValue::Array(_))
  }

  /// Read and return the inner value as a `u64`, if possible.
  pub fn as_u64(&self) -> Option<u64> {
    match self {
      RedisValue::Integer(ref i) => {
        if *i >= 0 {
          Some(*i as u64)
        } else {
          None
        }
      },
      RedisValue::String(ref s) => s.parse::<u64>().ok(),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_u64())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(0),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  ///  Read and return the inner value as a `i64`, if possible.
  pub fn as_i64(&self) -> Option<i64> {
    match self {
      RedisValue::Integer(ref i) => Some(*i),
      RedisValue::String(ref s) => s.parse::<i64>().ok(),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_i64())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(0),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  ///  Read and return the inner value as a `usize`, if possible.
  pub fn as_usize(&self) -> Option<usize> {
    match self {
      RedisValue::Integer(i) => {
        if *i >= 0 {
          Some(*i as usize)
        } else {
          None
        }
      },
      RedisValue::String(ref s) => s.parse::<usize>().ok(),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_usize())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(0),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  ///  Read and return the inner value as a `f64`, if possible.
  pub fn as_f64(&self) -> Option<f64> {
    match self {
      RedisValue::Double(ref f) => Some(*f),
      RedisValue::String(ref s) => utils::redis_string_to_f64(s).ok(),
      RedisValue::Integer(ref i) => Some(*i as f64),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_f64())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(0.0),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  /// Read and return the inner `String` if the value is a string or scalar value.
  pub fn into_string(self) -> Option<String> {
    match self {
      RedisValue::Boolean(b) => Some(b.to_string()),
      RedisValue::Double(f) => Some(f.to_string()),
      RedisValue::String(s) => Some(s.to_string()),
      RedisValue::Bytes(b) => String::from_utf8(b.to_vec()).ok(),
      RedisValue::Integer(i) => Some(i.to_string()),
      RedisValue::Queued => Some(QUEUED.to_owned()),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          inner.pop().and_then(|v| v.into_string())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(String::new()),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  /// Read and return the inner data as a `Str` from the `bytes` crate.
  pub fn into_bytes_str(self) -> Option<Str> {
    match self {
      RedisValue::Boolean(b) => match b {
        true => Some(TRUE_STR.clone()),
        false => Some(FALSE_STR.clone()),
      },
      RedisValue::Double(f) => Some(f.to_string().into()),
      RedisValue::String(s) => Some(s),
      RedisValue::Bytes(b) => Str::from_inner(b).ok(),
      RedisValue::Integer(i) => Some(i.to_string().into()),
      RedisValue::Queued => Some(utils::static_str(QUEUED)),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          inner.pop().and_then(|v| v.into_bytes_str())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(Str::new()),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  /// Read the inner value as a `Str`.
  pub fn as_bytes_str(&self) -> Option<Str> {
    match self {
      RedisValue::Boolean(ref b) => match *b {
        true => Some(TRUE_STR.clone()),
        false => Some(FALSE_STR.clone()),
      },
      RedisValue::Double(ref f) => Some(f.to_string().into()),
      RedisValue::String(ref s) => Some(s.clone()),
      RedisValue::Bytes(ref b) => Str::from_inner(b.clone()).ok(),
      RedisValue::Integer(ref i) => Some(i.to_string().into()),
      RedisValue::Queued => Some(utils::static_str(QUEUED)),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner[0].as_bytes_str()
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(Str::new()),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  /// Read and return the inner `String` if the value is a string or scalar value.
  ///
  /// Note: this will cast integers and doubles to strings.
  pub fn as_string(&self) -> Option<String> {
    match self {
      RedisValue::Boolean(ref b) => Some(b.to_string()),
      RedisValue::Double(ref f) => Some(f.to_string()),
      RedisValue::String(ref s) => Some(s.to_string()),
      RedisValue::Bytes(ref b) => str::from_utf8(b).ok().map(|s| s.to_owned()),
      RedisValue::Integer(ref i) => Some(i.to_string()),
      RedisValue::Queued => Some(QUEUED.to_owned()),
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(String::new()),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  /// Read the inner value as a string slice.
  ///
  /// Null is returned as `"nil"` and scalar values are cast to a string.
  pub fn as_str(&self) -> Option<Cow<str>> {
    let s: Cow<str> = match *self {
      RedisValue::Double(ref f) => Cow::Owned(f.to_string()),
      RedisValue::Boolean(ref b) => Cow::Owned(b.to_string()),
      RedisValue::String(ref s) => Cow::Borrowed(s.deref()),
      RedisValue::Integer(ref i) => Cow::Owned(i.to_string()),
      RedisValue::Queued => Cow::Borrowed(QUEUED),
      RedisValue::Bytes(ref b) => return str::from_utf8(b).ok().map(Cow::Borrowed),
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Cow::Borrowed(""),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => return None,
      _ => return None,
    };

    Some(s)
  }

  /// Read the inner value as a string, using `String::from_utf8_lossy` on byte slices.
  pub fn as_str_lossy(&self) -> Option<Cow<str>> {
    let s: Cow<str> = match *self {
      RedisValue::Boolean(ref b) => Cow::Owned(b.to_string()),
      RedisValue::Double(ref f) => Cow::Owned(f.to_string()),
      RedisValue::String(ref s) => Cow::Borrowed(s.deref()),
      RedisValue::Integer(ref i) => Cow::Owned(i.to_string()),
      RedisValue::Queued => Cow::Borrowed(QUEUED),
      RedisValue::Bytes(ref b) => String::from_utf8_lossy(b),
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Cow::Borrowed(""),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => return None,
      _ => return None,
    };

    Some(s)
  }

  /// Read the inner value as an array of bytes, if possible.
  pub fn as_bytes(&self) -> Option<&[u8]> {
    match *self {
      RedisValue::String(ref s) => Some(s.as_bytes()),
      RedisValue::Bytes(ref b) => Some(b),
      RedisValue::Queued => Some(QUEUED.as_bytes()),
      _ => None,
    }
  }

  /// Attempt to convert the value to a `bool`.
  pub fn as_bool(&self) -> Option<bool> {
    match *self {
      RedisValue::Boolean(b) => Some(b),
      RedisValue::Integer(ref i) => match *i {
        0 => Some(false),
        1 => Some(true),
        _ => None,
      },
      RedisValue::String(ref s) => match s.as_bytes() {
        b"true" | b"TRUE" | b"t" | b"T" | b"1" => Some(true),
        b"false" | b"FALSE" | b"f" | b"F" | b"0" => Some(false),
        _ => None,
      },
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_bool())
        } else {
          None
        }
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Some(false),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => None,
      _ => None,
    }
  }

  /// Attempt to convert this value to a Redis map if it's an array with an even number of elements.
  pub fn into_map(self) -> Result<RedisMap, RedisError> {
    match self {
      RedisValue::Map(map) => Ok(map),
      RedisValue::Array(mut values) => {
        if values.len() % 2 != 0 {
          return Err(RedisError::new(
            RedisErrorKind::Unknown,
            "Expected an even number of elements.",
          ));
        }
        let mut inner = HashMap::with_capacity(values.len() / 2);
        while values.len() >= 2 {
          let value = values.pop().unwrap();
          let key: RedisKey = values.pop().unwrap().try_into()?;

          inner.insert(key, value);
        }

        Ok(RedisMap { inner })
      },
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Ok(RedisMap::new()),
      _ => Err(RedisError::new(RedisErrorKind::Unknown, "Could not convert to map.")),
    }
  }

  pub(crate) fn into_multiple_values(self) -> Vec<RedisValue> {
    match self {
      RedisValue::Array(values) => values,
      RedisValue::Map(map) => map
        .inner()
        .into_iter()
        .flat_map(|(k, v)| [RedisValue::Bytes(k.into_bytes()), v])
        .collect(),
      RedisValue::Null => Vec::new(),
      _ => vec![self],
    }
  }

  /// Convert the array value to a set, if possible.
  pub fn into_set(self) -> Result<HashSet<RedisValue>, RedisError> {
    match self {
      RedisValue::Array(values) => Ok(values.into_iter().collect()),
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Ok(HashSet::new()),
      _ => Err(RedisError::new_parse("Could not convert to set.")),
    }
  }

  /// Convert a `RedisValue` to `Vec<(RedisValue, f64)>`, if possible.
  pub fn into_zset_result(self) -> Result<Vec<(RedisValue, f64)>, RedisError> {
    protocol_utils::value_to_zset_result(self)
  }

  /// Convert this value to an array if it's an array or map.
  ///
  /// If the value is not an array or map this returns a single-element array containing the current value.
  pub fn into_array(self) -> Vec<RedisValue> {
    match self {
      RedisValue::Array(values) => values,
      RedisValue::Map(map) => {
        let mut out = Vec::with_capacity(map.len() * 2);

        for (key, value) in map.inner().into_iter() {
          out.push(key.into());
          out.push(value);
        }
        out
      },
      _ => vec![self],
    }
  }

  /// Convert the value to an array of bytes, if possible.
  pub fn into_owned_bytes(self) -> Option<Vec<u8>> {
    let v = match self {
      RedisValue::String(s) => s.to_string().into_bytes(),
      RedisValue::Bytes(b) => b.to_vec(),
      RedisValue::Queued => QUEUED.as_bytes().to_vec(),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          return inner.pop().and_then(|v| v.into_owned_bytes());
        } else {
          return None;
        }
      },
      RedisValue::Integer(i) => i.to_string().into_bytes(),
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Vec::new(),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => return None,
      _ => return None,
    };

    Some(v)
  }

  /// Convert the value into a `Bytes` view.
  pub fn into_bytes(self) -> Option<Bytes> {
    let v = match self {
      RedisValue::String(s) => s.inner().clone(),
      RedisValue::Bytes(b) => b,
      RedisValue::Queued => Bytes::from_static(QUEUED.as_bytes()),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          return inner.pop().and_then(|v| v.into_bytes());
        } else {
          return None;
        }
      },
      RedisValue::Integer(i) => i.to_string().into(),
      #[cfg(feature = "default-nil-types")]
      RedisValue::Null => Bytes::new(),
      #[cfg(not(feature = "default-nil-types"))]
      RedisValue::Null => return None,
      _ => return None,
    };

    Some(v)
  }

  /// Return the length of the inner array if the value is an array.
  pub fn array_len(&self) -> Option<usize> {
    match self {
      RedisValue::Array(ref a) => Some(a.len()),
      _ => None,
    }
  }

  /// Whether the value is an array with one element.
  pub(crate) fn is_single_element_vec(&self) -> bool {
    if let RedisValue::Array(ref d) = self {
      d.len() == 1
    } else {
      false
    }
  }

  /// Pop the first value in the inner array or return the original value.
  ///
  /// This uses unwrap. Use [is_single_element_vec] first.
  pub(crate) fn pop_or_take(self) -> Self {
    if let RedisValue::Array(mut values) = self {
      values.pop().unwrap()
    } else {
      self
    }
  }

  /// Flatten adjacent nested arrays to the provided depth.
  ///
  /// See the [XREAD](crate::interfaces::StreamsInterface::xread) documentation for an example of when this might be
  /// useful.
  pub fn flatten_array_values(self, depth: usize) -> Self {
    utils::flatten_nested_array_values(self, depth)
  }

  /// A utility function to convert the response from `XREAD` or `XREADGROUP` into a type with a less verbose type
  /// declaration.
  ///
  /// This function supports responses in both RESP2 and RESP3 formats.
  ///
  /// See the [XREAD](crate::interfaces::StreamsInterface::xread) (or `XREADGROUP`) documentation for more
  /// information.
  pub fn into_xread_response<K1, I, K2, V>(self) -> Result<XReadResponse<K1, I, K2, V>, RedisError>
  where
    K1: FromRedisKey + Hash + Eq,
    K2: FromRedisKey + Hash + Eq,
    I: FromRedis,
    V: FromRedis,
  {
    self.flatten_array_values(2).convert()
  }

  /// A utility function to convert the response from `XCLAIM`, etc into a type with a less verbose type declaration.
  ///
  /// This function supports responses in both RESP2 and RESP3 formats.
  pub fn into_xread_value<I, K, V>(self) -> Result<Vec<XReadValue<I, K, V>>, RedisError>
  where
    K: FromRedisKey + Hash + Eq,
    I: FromRedis,
    V: FromRedis,
  {
    self.flatten_array_values(1).convert()
  }

  /// A utility function to convert the response from `XAUTOCLAIM` into a type with a less verbose type declaration.
  ///
  /// This function supports responses in both RESP2 and RESP3 formats.
  ///
  /// Note: the new (as of Redis 7.x) return value containing message PIDs that were deleted from the PEL are dropped.
  /// Callers should use `xautoclaim` instead if this data is needed.
  pub fn into_xautoclaim_values<I, K, V>(self) -> Result<(String, Vec<XReadValue<I, K, V>>), RedisError>
  where
    K: FromRedisKey + Hash + Eq,
    I: FromRedis,
    V: FromRedis,
  {
    if let RedisValue::Array(mut values) = self {
      if values.len() == 3 {
        // convert the redis 7.x response format to the v6 format
        trace!("Removing the third message PID elements from XAUTOCLAIM response.");
        values.pop();
      }

      // unwrap checked above
      let entries = values.pop().unwrap();
      let cursor: String = values.pop().unwrap().convert()?;

      Ok((cursor, entries.flatten_array_values(1).convert()?))
    } else {
      Err(RedisError::new_parse("Expected array response."))
    }
  }

  /// Parse the value as the response from `FUNCTION LIST`, including only functions with the provided library `name`.
  pub fn as_functions(&self, name: &str) -> Result<Vec<Function>, RedisError> {
    utils::value_to_functions(self, name)
  }

  /// Convert the value into a `GeoPosition`, if possible.
  ///
  /// Null values are returned as `None` to work more easily with the result of the `GEOPOS` command.
  pub fn as_geo_position(&self) -> Result<Option<GeoPosition>, RedisError> {
    if self.is_null() {
      Ok(None)
    } else {
      GeoPosition::try_from(self.clone()).map(Some)
    }
  }

  /// Parse the value as the response to any of the relevant GEO commands that return an array of
  /// [GeoRadiusInfo](crate::types::GeoRadiusInfo) values, such as `GEOSEARCH`, GEORADIUS`, etc.
  pub fn into_geo_radius_result(
    self,
    withcoord: bool,
    withdist: bool,
    withhash: bool,
  ) -> Result<Vec<GeoRadiusInfo>, RedisError> {
    match self {
      RedisValue::Array(data) => data
        .into_iter()
        .map(|value| GeoRadiusInfo::from_redis_value(value, withcoord, withdist, withhash))
        .collect(),
      RedisValue::Null => Ok(Vec::new()),
      _ => Err(RedisError::new(RedisErrorKind::Parse, "Expected array.")),
    }
  }

  /// Replace this value with `RedisValue::Null`, returning the original value.
  pub fn take(&mut self) -> RedisValue {
    mem::replace(self, RedisValue::Null)
  }

  /// Attempt to convert this value to any value that implements the [FromRedis](crate::types::FromRedis) trait.
  pub fn convert<R>(self) -> Result<R, RedisError>
  where
    R: FromRedis,
  {
    R::from_value(self)
  }

  /// Whether or not the value can be hashed.
  ///
  /// Some use cases require using `RedisValue` types as keys in a `HashMap`, etc. Trying to do so with an aggregate
  /// type can panic, and this function can be used to more gracefully handle this situation.
  pub fn can_hash(&self) -> bool {
    matches!(
      self.kind(),
      RedisValueKind::String
        | RedisValueKind::Boolean
        | RedisValueKind::Double
        | RedisValueKind::Integer
        | RedisValueKind::Bytes
        | RedisValueKind::Null
        | RedisValueKind::Array
        | RedisValueKind::Queued
    )
  }

  /// Convert the value to JSON.
  #[cfg(feature = "serde-json")]
  #[cfg_attr(docsrs, doc(cfg(feature = "serde-json")))]
  pub fn into_json(self) -> Result<Value, RedisError> {
    Value::from_value(self)
  }
}

impl Hash for RedisValue {
  fn hash<H: Hasher>(&self, state: &mut H) {
    // used to prevent collisions between different types
    let prefix = match self.kind() {
      RedisValueKind::Boolean => b'B',
      RedisValueKind::Double => b'd',
      RedisValueKind::Integer => b'i',
      RedisValueKind::String => b's',
      RedisValueKind::Null => b'n',
      RedisValueKind::Queued => b'q',
      RedisValueKind::Array => b'a',
      RedisValueKind::Map => b'm',
      RedisValueKind::Bytes => b'b',
    };
    prefix.hash(state);

    match *self {
      RedisValue::Boolean(b) => b.hash(state),
      RedisValue::Double(f) => f.to_be_bytes().hash(state),
      RedisValue::Integer(d) => d.hash(state),
      RedisValue::String(ref s) => s.hash(state),
      RedisValue::Bytes(ref b) => b.hash(state),
      RedisValue::Null => NULL.hash(state),
      RedisValue::Queued => QUEUED.hash(state),
      RedisValue::Array(ref arr) => {
        for value in arr.iter() {
          value.hash(state);
        }
      },
      _ => panic!("Cannot hash aggregate value."),
    }
  }
}

impl From<u8> for RedisValue {
  fn from(d: u8) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<u16> for RedisValue {
  fn from(d: u16) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<u32> for RedisValue {
  fn from(d: u32) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i8> for RedisValue {
  fn from(d: i8) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i16> for RedisValue {
  fn from(d: i16) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i32> for RedisValue {
  fn from(d: i32) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i64> for RedisValue {
  fn from(d: i64) -> Self {
    RedisValue::Integer(d)
  }
}

impl From<f32> for RedisValue {
  fn from(f: f32) -> Self {
    RedisValue::Double(f as f64)
  }
}

impl From<f64> for RedisValue {
  fn from(f: f64) -> Self {
    RedisValue::Double(f)
  }
}

impl TryFrom<u64> for RedisValue {
  type Error = RedisError;

  fn try_from(d: u64) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as u64) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Unsigned integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl TryFrom<u128> for RedisValue {
  type Error = RedisError;

  fn try_from(d: u128) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as u128) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Unsigned integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl TryFrom<i128> for RedisValue {
  type Error = RedisError;

  fn try_from(d: i128) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as i128) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Signed integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl TryFrom<usize> for RedisValue {
  type Error = RedisError;

  fn try_from(d: usize) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as usize) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Unsigned integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl From<Str> for RedisValue {
  fn from(s: Str) -> Self {
    RedisValue::String(s)
  }
}

impl From<Bytes> for RedisValue {
  fn from(b: Bytes) -> Self {
    RedisValue::Bytes(b)
  }
}

impl From<&Box<[u8]>> for RedisValue {
  fn from(b: &Box<[u8]>) -> Self {
    b.into()
  }
}

impl From<Box<[u8]>> for RedisValue {
  fn from(b: Box<[u8]>) -> Self {
    RedisValue::Bytes(b.into())
  }
}

impl From<String> for RedisValue {
  fn from(d: String) -> Self {
    RedisValue::String(Str::from(d))
  }
}

impl<'a> From<&'a String> for RedisValue {
  fn from(d: &'a String) -> Self {
    RedisValue::String(Str::from(d))
  }
}

impl<'a> From<&'a str> for RedisValue {
  fn from(d: &'a str) -> Self {
    RedisValue::String(Str::from(d))
  }
}

impl<'a> From<&'a [u8]> for RedisValue {
  fn from(b: &'a [u8]) -> Self {
    RedisValue::Bytes(Bytes::from(b.to_vec()))
  }
}

impl From<bool> for RedisValue {
  fn from(d: bool) -> Self {
    RedisValue::Boolean(d)
  }
}

impl<T> TryFrom<Option<T>> for RedisValue
where
  T: TryInto<RedisValue>,
  T::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(d: Option<T>) -> Result<Self, Self::Error> {
    match d {
      Some(i) => to!(i),
      None => Ok(RedisValue::Null),
    }
  }
}

impl<'a, T, const N: usize> TryFrom<&'a [T; N]> for RedisValue
where
  T: TryInto<RedisValue> + Clone,
  T::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: &'a [T; N]) -> Result<Self, Self::Error> {
    let values = value
      .iter()
      .map(|v| v.clone().try_into().map_err(|e| e.into()))
      .collect::<Result<Vec<RedisValue>, RedisError>>()?;

    Ok(RedisValue::Array(values))
  }
}

impl<T, const N: usize> TryFrom<[T; N]> for RedisValue
where
  T: TryInto<RedisValue> + Clone,
  T::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: [T; N]) -> Result<Self, Self::Error> {
    let values = value
      .into_iter()
      .map(|v| v.try_into().map_err(|e| e.into()))
      .collect::<Result<Vec<RedisValue>, RedisError>>()?;

    Ok(RedisValue::Array(values))
  }
}

impl<T> TryFrom<Vec<T>> for RedisValue
where
  T: TryInto<RedisValue>,
  T::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
    let values = value
      .into_iter()
      .map(|v| v.try_into().map_err(|e| e.into()))
      .collect::<Result<Vec<RedisValue>, RedisError>>()?;

    Ok(RedisValue::Array(values))
  }
}

impl<T> TryFrom<VecDeque<T>> for RedisValue
where
  T: TryInto<RedisValue>,
  T::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: VecDeque<T>) -> Result<Self, Self::Error> {
    let values = value
      .into_iter()
      .map(|v| v.try_into().map_err(|e| e.into()))
      .collect::<Result<Vec<RedisValue>, RedisError>>()?;

    Ok(RedisValue::Array(values))
  }
}

impl<V> FromIterator<V> for RedisValue
where
  V: Into<RedisValue>,
{
  fn from_iter<I: IntoIterator<Item = V>>(iter: I) -> Self {
    RedisValue::Array(iter.into_iter().map(|v| v.into()).collect())
  }
}

impl<K, V> TryFrom<HashMap<K, V>> for RedisValue
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(d: HashMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisValue::Map(RedisMap {
      inner: utils::into_redis_map(d.into_iter())?,
    }))
  }
}

impl<K, V> TryFrom<BTreeMap<K, V>> for RedisValue
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(d: BTreeMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisValue::Map(RedisMap {
      inner: utils::into_redis_map(d.into_iter())?,
    }))
  }
}

impl From<RedisKey> for RedisValue {
  fn from(d: RedisKey) -> Self {
    RedisValue::Bytes(d.key)
  }
}

impl From<RedisMap> for RedisValue {
  fn from(m: RedisMap) -> Self {
    RedisValue::Map(m)
  }
}

impl From<()> for RedisValue {
  fn from(_: ()) -> Self {
    RedisValue::Null
  }
}

impl TryFrom<Resp3Frame> for RedisValue {
  type Error = RedisError;

  fn try_from(value: Resp3Frame) -> Result<Self, Self::Error> {
    protocol_utils::frame_to_results(value)
  }
}
