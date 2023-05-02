use fred::{
  prelude::*,
  types::{RedisKey, RespVersion},
};
use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};
use tokio::time::sleep;

pub async fn should_invalidate_foo_resp3(client: RedisClient, _: RedisConfig) -> Result<(), RedisError> {
  if client.protocol_version() == RespVersion::RESP2 {
    return Ok(());
  }

  let key: RedisKey = "foo{1}".into();
  check_null!(client, "foo{1}");

  let invalidated = Arc::new(AtomicBool::new(false));
  let _invalidated = invalidated.clone();

  let mut invalidations = client.on_invalidation();
  tokio::spawn(async move {
    while let Ok(invalidation) = invalidations.recv().await {
      if invalidation.keys.contains(&key) {
        _invalidated.swap(true, Ordering::SeqCst);
      }
    }
  });

  let _ = client.start_tracking(None, false, false, false, false).await?;
  let _: () = client.get("foo{1}").await?;
  let _: () = client.incr("foo{1}").await?;

  let _: () = client.mget(vec!["bar{1}", "baz{1}"]).await?;
  let _: () = client.mset(vec![("bar{1}", 1), ("baz{1}", 1)]).await?;
  let _ = client.flushall(false).await?;

  sleep(Duration::from_secs(1)).await;
  if invalidated.load(Ordering::Acquire) {
    Ok(())
  } else {
    panic!("Failed to invalidate foo");
  }
}

pub async fn should_invalidate_foo_resp2_centralized(client: RedisClient, _: RedisConfig) -> Result<(), RedisError> {
  if client.protocol_version() == RespVersion::RESP3 || client.is_clustered() {
    return Ok(());
  }

  let key: RedisKey = "foo{1}".into();
  check_null!(client, "foo{1}");
  let subscriber = client.clone_new();
  let _ = subscriber.connect();
  let _ = subscriber.wait_for_connect().await?;

  let invalidated = Arc::new(AtomicBool::new(false));
  let _invalidated = invalidated.clone();

  let mut invalidations = subscriber.on_invalidation();
  tokio::spawn(async move {
    while let Ok(invalidation) = invalidations.recv().await {
      if invalidation.keys.contains(&key) {
        _invalidated.swap(true, Ordering::SeqCst);
      }
    }
  });
  let _ = subscriber.subscribe("__redis__:invalidate").await?;

  let (_, subscriber_id) = subscriber
    .connection_ids()
    .await
    .into_iter()
    .next()
    .expect("Failed to read subscriber connection ID");

  let _ = client
    .client_tracking("on", Some(subscriber_id), None, false, false, false, false)
    .await?;

  // verify that we get 2 keys in the invalidation message, or at least make sure that doesnt panic
  // in resp2 this might take some changes to the pubsub parser if it doesn't work with an array as the message type

  // check pubsub messages with one key
  let _: () = client.get("foo{1}").await?;
  let _: () = client.incr("foo{1}").await?;

  // check pubsub messages with an array of keys
  let _: () = client.mget(vec!["bar{1}", "baz{1}"]).await?;
  let _: () = client.mset(vec![("bar{1}", 1), ("baz{1}", 1)]).await?;
  // check pubsub messages with a null key
  let _ = client.flushall(false).await?;

  sleep(Duration::from_secs(1)).await;
  if invalidated.load(Ordering::Acquire) {
    Ok(())
  } else {
    panic!("Failed to invalidate foo");
  }
}