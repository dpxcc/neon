//! This module implements the Generic Cell Rate Algorithm for a simplified
//! version of the Leaky Bucket rate limiting system.
//!
//! # Leaky Bucket
//!
//! If the bucket is full, no new requests are allowed and are throttled/errored.
//! If the bucket is partially full/empty, new requests are added to the bucket in
//! terms of "tokens".
//!
//! Over time, tokens are removed from the bucket, naturally allowing new requests at a steady rate.
//!
//! The bucket size tunes the burst support. The drain rate tunes the steady-rate requests per second.
//!
//! # [GCRA](https://en.wikipedia.org/wiki/Generic_cell_rate_algorithm)
//!
//! GCRA is a continuous rate leaky-bucket impl that stores minimal state and requires
//! no background jobs to drain tokens, as the design utilises timestamps to drain automatically over time.
//!
//! We store an "empty_at" timestamp as the only state. As time progresses, we will naturally approach
//! the empty state. The full-bucket state is calculated from `empty_at - config.bucket_width`.
//!
//! Another explaination can be found here: <https://brandur.org/rate-limiting>

use std::{
    cell::UnsafeCell,
    sync::Mutex,
    task::{Poll, Waker},
    time::Duration,
};

use pin_list::{Node, NodeData, PinList};
use tokio::time::Instant;

pub struct LeakyBucketConfig {
    /// This is the "time cost" of a single request unit.
    /// Should loosely represent how long it takes to handle a request unit in active resource time.
    /// Loosely speaking this is the inverse of the steady-rate requests-per-second
    pub cost: Duration,

    /// total size of the bucket
    pub bucket_width: Duration,
}

impl LeakyBucketConfig {
    pub fn new(rps: f64, bucket_size: f64) -> Self {
        let cost = Duration::from_secs_f64(rps.recip());
        let bucket_width = cost.mul_f64(bucket_size);
        Self { cost, bucket_width }
    }
}

pub struct LeakyBucketState {
    /// Bucket is represented by `allow_at..empty_at` where `allow_at = empty_at - config.bucket_width`.
    ///
    /// At any given time, `empty_at - now` represents the number of tokens in the bucket, multiplied by the "time_cost".
    /// Adding `n` tokens to the bucket is done by moving `empty_at` forward by `n * config.time_cost`.
    /// If `now < allow_at`, the bucket is considered filled and cannot accept any more tokens.
    /// Draining the bucket will happen naturally as `now` moves forward.
    ///
    /// Let `n` be some "time cost" for the request,
    /// If now is after empty_at, the bucket is empty and the empty_at is reset to now,
    /// If now is within the `bucket window + n`, we are within time budget.
    /// If now is before the `bucket window + n`, we have run out of budget.
    ///
    /// This is inspired by the generic cell rate algorithm (GCRA) and works
    /// exactly the same as a leaky-bucket.
    pub empty_at: Instant,
}

impl LeakyBucketState {
    pub fn with_initial_tokens(config: &LeakyBucketConfig, initial_tokens: f64) -> Self {
        LeakyBucketState {
            empty_at: Instant::now() + config.cost.mul_f64(initial_tokens),
        }
    }

    pub fn bucket_is_empty(&self, now: Instant) -> bool {
        // if self.end is after now, the bucket is not empty
        self.empty_at <= now
    }

    /// Immediately adds tokens to the bucket, if there is space.
    ///
    /// In a scenario where you are waiting for available rate,
    /// rather than just erroring immediately, `started` corresponds to when this waiting started.
    ///
    /// `n` is the number of tokens that will be filled in the bucket.
    ///
    /// # Errors
    ///
    /// If there is not enough space, no tokens are added. Instead, an error is returned with the time when
    /// there will be space again.
    pub fn add_tokens(
        &mut self,
        config: &LeakyBucketConfig,
        started: Instant,
        n: f64,
    ) -> Result<(), Instant> {
        let now = Instant::now();

        // invariant: started <= now
        debug_assert!(started <= now);

        // If the bucket was empty when we started our search,
        // we should update the `empty_at` value accordingly.
        // this prevents us from having negative tokens in the bucket.
        let mut empty_at = self.empty_at;
        if empty_at < started {
            empty_at = started;
        }

        let n = config.cost.mul_f64(n);
        let new_empty_at = empty_at + n;
        let allow_at = new_empty_at.checked_sub(config.bucket_width);

        //                     empty_at
        //          allow_at    |   new_empty_at
        //           /          |   /
        // -------o-[---------o-|--]---------
        //   now1 ^      now2 ^
        //
        // at now1, the bucket would be completely filled if we add n tokens.
        // at now2, the bucket would be partially filled if we add n tokens.

        match allow_at {
            Some(allow_at) if now < allow_at => Err(allow_at),
            _ => {
                self.empty_at = new_empty_at;
                Ok(())
            }
        }
    }
}

pub struct RateLimiter {
    config: LeakyBucketConfig,
    state: UnsafeCell<LeakyBucketState>,
    queue: Mutex<Queue>,
}

// SAFETY: To access the state in the UnsafeCell, you must be holding the RateToken.
unsafe impl Sync for RateLimiter {}

struct RateToken;

struct Queue {
    sleep_counter: u64,
    queue: PinList<RateLimitQueue>,
    token: Option<RateToken>,
}

impl Queue {
    /// returns the sleep_counter start value on await.
    /// sleep_counter end value can be found within the enqueued.
    fn wait(this: &Mutex<Self>) -> Enqueued<'_> {
        Enqueued {
            entry: pin_list::Node::new(),
            queue: this,
            token: None,
        }
    }
}

type RateLimitQueue = dyn pin_list::Types<
    Id = pin_list::id::DebugChecked,
    // the waker that lets us wake the next in the queue
    Protected = Waker,
    // the token that gives us access to the rate limit state
    Removed = RateToken,
    // the sleep count at the start of the enqueue
    Unprotected = u64,
>;

pin_project_lite::pin_project! {
    struct Enqueued<'a> {
        #[pin]
        entry: Node<RateLimitQueue>,
        token: Option<(RateToken, u64)>,
        queue: &'a Mutex<Queue>,
    }

    impl<'a> PinnedDrop for Enqueued<'a> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();

            #[allow(clippy::mut_mutex_lock, reason = "false positive")]
            let mut q = this.queue.lock().unwrap();

            let token = if let Some(init) = this.entry.initialized_mut() {
                let (data, _start_count) =  init.reset(&mut q.queue) ;
                match data {
                    // we were in the queue and are not holding any resources.
                    NodeData::Linked(_) => return,
                    // we were the head of the queue and were about to be the current leader
                    NodeData::Removed(token) => token
                }
            } else if let Some((token, sleep_counter)) = this.token.take() {
                // we were holding the lock, and are now releasing it.
                q.sleep_counter = sleep_counter;
                token
            } else {
                // we apparently didn't even get into the queue to begin with
                return;
            };

            // wake the next in the queue and give them the lock token
            match q.queue.cursor_front_mut().remove_current(token) {
                // wake up the next leader
                Ok(waker) => waker.wake(),
                // no tasks left in the queue. unlocked
                Err(token) => q.token = Some(token),
            }
        }
    }
}

impl std::future::Future for Enqueued<'_> {
    type Output = u64;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut node = this.entry;

        #[allow(clippy::mut_mutex_lock, reason = "false positive")]
        let mut q = this.queue.lock().unwrap();

        if let Some(init) = node.as_mut().initialized_mut() {
            // we are registered in the queue

            match init.take_removed(&q.queue) {
                // if we are removed from the queue, that means we are the new leader
                Ok((token, start_count)) => {
                    *this.token = Some((token, q.sleep_counter));
                    Poll::Ready(start_count)
                }
                // if we are not removed from the queue, that means we are still waiting
                // and had a spurious wake up
                Err(init) => {
                    init.protected_mut(&mut q.queue)
                        .unwrap()
                        .clone_from(cx.waker());
                    Poll::Pending
                }
            }
        } else {
            // we are not yet registered in the queue
            let start_count = q.sleep_counter;

            if let Some(token) = q.token.take() {
                // we are the first in the queue and it is not yet acquired
                *this.token = Some((token, q.sleep_counter));
                Poll::Ready(start_count)
            } else {
                // we push ourselves to the back of the queue
                q.queue.push_back(node, cx.waker().clone(), start_count);
                Poll::Pending
            }
        }
    }
}

impl RateLimiter {
    pub fn with_initial_tokens(config: LeakyBucketConfig, initial_tokens: f64) -> Self {
        RateLimiter {
            state: UnsafeCell::new(LeakyBucketState::with_initial_tokens(
                &config,
                initial_tokens,
            )),
            config,
            queue: Mutex::new(Queue {
                sleep_counter: 0,
                // SAFETY: we make sure to only interact with the same queue
                queue: PinList::new(unsafe { pin_list::id::DebugChecked::new() }),
                token: Some(RateToken),
            }),
        }
    }

    pub fn steady_rps(&self) -> f64 {
        self.config.cost.as_secs_f64().recip()
    }

    /// returns true if we did throttle
    pub async fn acquire(&self, count: usize) -> bool {
        let start = tokio::time::Instant::now();

        let mut entry = std::pin::pin!(Queue::wait(&self.queue));
        let start_count = entry.as_mut().await;
        let (_token, sleep_counter) = entry
            .project()
            .token
            .as_mut()
            .expect("token should be init if we returned from enqueued");

        loop {
            // SAFETY: we are holding the token.
            let res = unsafe {
                let state = &mut *self.state.get();
                state.add_tokens(&self.config, start, count as f64)
            };
            match res {
                Ok(()) => return start_count < *sleep_counter,
                Err(ready_at) => {
                    *sleep_counter += 1;
                    tokio::time::sleep_until(ready_at).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

    use super::{LeakyBucketConfig, LeakyBucketState};

    #[tokio::test(start_paused = true)]
    async fn check() {
        let config = LeakyBucketConfig {
            // average 100rps
            cost: Duration::from_millis(10),
            // burst up to 100 requests
            bucket_width: Duration::from_millis(1000),
        };

        let mut state = LeakyBucketState {
            empty_at: Instant::now(),
        };

        // supports burst
        {
            // should work for 100 requests this instant
            for _ in 0..100 {
                state.add_tokens(&config, Instant::now(), 1.0).unwrap();
            }
            let ready = state.add_tokens(&config, Instant::now(), 1.0).unwrap_err();
            assert_eq!(ready - Instant::now(), Duration::from_millis(10));
        }

        // doesn't overfill
        {
            // after 1s we should have an empty bucket again.
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(state.bucket_is_empty(Instant::now()));

            // after 1s more, we should not over count the tokens and allow more than 200 requests.
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..100 {
                state.add_tokens(&config, Instant::now(), 1.0).unwrap();
            }
            let ready = state.add_tokens(&config, Instant::now(), 1.0).unwrap_err();
            assert_eq!(ready - Instant::now(), Duration::from_millis(10));
        }

        // supports sustained rate over a long period
        {
            tokio::time::advance(Duration::from_secs(1)).await;

            // should sustain 100rps
            for _ in 0..2000 {
                tokio::time::advance(Duration::from_millis(10)).await;
                state.add_tokens(&config, Instant::now(), 1.0).unwrap();
            }
        }

        // supports requesting more tokens than can be stored in the bucket
        // we just wait a little bit longer upfront.
        {
            // start the bucket completely empty
            tokio::time::advance(Duration::from_secs(5)).await;
            assert!(state.bucket_is_empty(Instant::now()));

            // requesting 200 tokens of space should take 200*cost = 2s
            // but we already have 1s available, so we wait 1s from start.
            let start = Instant::now();

            let ready = state.add_tokens(&config, start, 200.0).unwrap_err();
            assert_eq!(ready - Instant::now(), Duration::from_secs(1));

            tokio::time::advance(Duration::from_millis(500)).await;
            let ready = state.add_tokens(&config, start, 200.0).unwrap_err();
            assert_eq!(ready - Instant::now(), Duration::from_millis(500));

            tokio::time::advance(Duration::from_millis(500)).await;
            state.add_tokens(&config, start, 200.0).unwrap();

            // bucket should be completely full now
            let ready = state.add_tokens(&config, Instant::now(), 1.0).unwrap_err();
            assert_eq!(ready - Instant::now(), Duration::from_millis(10));
        }
    }
}
