// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    features::Gso,
    message::Message,
    socket::{ring::Consumer, task::events},
};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use futures::ready;

pub use events::TxEvents as Events;

pub trait Socket<T: Message> {
    type Error;

    fn send(
        &mut self,
        cx: &mut Context,
        entries: &mut [T],
        events: &mut Events,
    ) -> Result<(), Self::Error>;
}

pub struct Sender<T: Message, S: Socket<T>> {
    ring: Consumer<T>,
    /// Implementation of a socket that transmits filled slots in the ring buffer
    tx: S,
    /// The number of messages that have been transmitted but not yet released to the producer.
    ///
    /// This value is to avoid calling `release` too much and excessively waking up the producer.
    pending: u32,
    events: Events,
}

impl<T, S> Sender<T, S>
where
    T: Message + Unpin,
    S: Socket<T> + Unpin,
{
    #[inline]
    pub fn new(ring: Consumer<T>, tx: S, gso: Gso) -> Self {
        Self {
            ring,
            tx,
            pending: 0,
            events: Events::new(gso),
        }
    }

    #[inline]
    fn poll_ring(&mut self, watermark: u32, cx: &mut Context) -> Poll<Result<(), ()>> {
        loop {
            let count = match self.ring.poll_acquire(watermark, cx) {
                Poll::Ready(count) => count,
                Poll::Pending if self.pending == 0 => {
                    return if !self.ring.is_open() {
                        Err(()).into()
                    } else {
                        Poll::Pending
                    };
                }
                Poll::Pending => 0,
            };

            // if the number of free slots increased since last time then yield
            if count > self.pending {
                return Ok(()).into();
            }

            // If there is no additional capacity available (i.e. we have filled all slots),
            // then release those filled slots for the consumer to read from. Once
            // the consumer reads, we will have spare capacity to populate again.
            self.release();
        }
    }

    #[inline]
    fn release(&mut self) {
        let to_release = core::mem::take(&mut self.pending);
        self.ring.release(to_release);
    }
}

impl<T, S> Future for Sender<T, S>
where
    T: Message + Unpin,
    S: Socket<T> + Unpin,
{
    type Output = Option<S::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();

        while !this.events.take_blocked() {
            if ready!(this.poll_ring(u32::MAX, cx)).is_err() {
                return None.into();
            }

            // slice the ring data by the number of items we've already received
            let entries = &mut this.ring.data()[this.pending as usize..];

            // perform the send syscall
            match this.tx.send(cx, entries, &mut this.events) {
                Ok(()) => {
                    // increment the number of received messages
                    this.pending += this.events.take_count() as u32
                }
                Err(err) => return Some(err).into(),
            }
        }

        // release any of the messages we wrote back to the consumer
        this.release();

        Poll::Pending
    }
}
