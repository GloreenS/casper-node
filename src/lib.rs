pub mod backpressured;
pub mod chunked;
pub mod error;
pub mod length_prefixed;
pub mod mux;

use bytes::Buf;

/// A frame for stack allocated data.
#[derive(Debug)]
pub struct ImmediateFrame<A> {
    /// How much of the frame has been read.
    pos: usize,
    /// The actual value contained.
    value: A,
}

impl<A> ImmediateFrame<A> {
    #[inline]
    pub fn new(value: A) -> Self {
        Self { pos: 0, value }
    }
}

impl From<u8> for ImmediateFrame<[u8; 1]> {
    #[inline]
    fn from(value: u8) -> Self {
        ImmediateFrame::new(value.to_le_bytes())
    }
}

impl From<u16> for ImmediateFrame<[u8; 2]> {
    #[inline]
    fn from(value: u16) -> Self {
        ImmediateFrame::new(value.to_le_bytes())
    }
}

impl From<u32> for ImmediateFrame<[u8; 4]> {
    #[inline]
    fn from(value: u32) -> Self {
        ImmediateFrame::new(value.to_le_bytes())
    }
}

impl<A> Buf for ImmediateFrame<A>
where
    A: AsRef<[u8]>,
{
    fn remaining(&self) -> usize {
        // Does not overflow, as `pos` is  `< .len()`.

        self.value.as_ref().len() - self.pos
    }

    fn chunk(&self) -> &[u8] {
        // Safe access, as `pos` is guaranteed to be `< .len()`.
        &self.value.as_ref()[self.pos..]
    }

    fn advance(&mut self, cnt: usize) {
        // This is the only function modifying `pos`, upholding the invariant of it being smaller
        // than the length of the data we have.
        self.pos = (self.pos + cnt).min(self.value.as_ref().len());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        convert::Infallible,
        io::Read,
        ops::Deref,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use bytes::{Buf, Bytes};
    use futures::{future, stream, FutureExt, Sink, SinkExt};

    use crate::{
        chunked::{chunk_frame, SingleChunk},
        error::Error,
        length_prefixed::{frame_add_length_prefix, LengthPrefixedFrame},
    };

    /// Collects everything inside a `Buf` into a `Vec`.
    pub fn collect_buf<B: Buf>(buf: B) -> Vec<u8> {
        let mut vec = Vec::new();
        buf.reader()
            .read_to_end(&mut vec)
            .expect("reading buf should never fail");
        vec
    }

    /// Collects the contents of multiple `Buf`s into a single flattened `Vec`.
    pub fn collect_bufs<B: Buf, I: IntoIterator<Item = B>>(items: I) -> Vec<u8> {
        let mut vec = Vec::new();
        for buf in items.into_iter() {
            buf.reader()
                .read_to_end(&mut vec)
                .expect("reading buf should never fail");
        }
        vec
    }

    /// A sink for unit testing.
    ///
    /// All data sent to it will be written to a buffer immediately that can be read during
    /// operation. It is guarded by a lock so that only complete writes are visible.
    ///
    /// Additionally, a `Plug` can be inserted into the sink. While a plug is plugged in, no data
    /// can flow into the sink. In a similar manner, the sink can be clogged - while it is possible
    /// to start sending new data, it will not report being done until the clog is cleared.
    ///
    /// ```text
    ///   Item ->     (plugged?)             [             ...  ] -> (clogged?) -> done flushing
    ///    ^ Input     ^ Plug (blocks input)   ^ Buffer contents      ^ Clog, prevents flush
    /// ```
    ///
    /// This can be used to simulate a sink on a busy or slow TCP connection, for example.
    #[derive(Default, Debug)]
    pub struct TestingSink {
        /// The state of the plug.
        obstruction: Mutex<SinkObstruction>,
        /// Buffer storing all the data.
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl TestingSink {
        /// Creates a new testing sink.
        ///
        /// The sink will initially be unplugged.
        pub fn new() -> Self {
            TestingSink::default()
        }

        /// Inserts or removes the plug from the sink.
        pub fn set_plugged(&self, plugged: bool) {
            let mut guard = self.obstruction.lock().expect("could not lock plug");
            guard.plugged = plugged;

            // Notify any waiting tasks that there may be progress to be made.
            if !plugged {
                if let Some(ref waker) = guard.waker {
                    waker.wake_by_ref()
                }
            }
        }

        /// Inserts or removes the clog from the sink.
        pub fn set_clogged(&self, clogged: bool) {
            let mut guard = self.obstruction.lock().expect("could not lock plug");
            guard.clogged = clogged;

            // Notify any waiting tasks that there may be progress to be made.
            if !clogged {
                if let Some(ref waker) = guard.waker {
                    waker.wake_by_ref()
                }
            }
        }

        /// Determine whether the sink is plugged.
        ///
        /// Will update the local waker reference.
        pub fn is_plugged(&self, cx: &mut Context<'_>) -> bool {
            let mut guard = self.obstruction.lock().expect("could not lock plug");

            guard.waker = Some(cx.waker().clone());
            guard.plugged
        }

        /// Determine whether the sink is clogged.
        ///
        /// Will update the local waker reference.
        pub fn is_clogged(&self, cx: &mut Context<'_>) -> bool {
            let mut guard = self.obstruction.lock().expect("could not lock plug");

            guard.waker = Some(cx.waker().clone());
            guard.clogged
        }

        /// Returns a copy of the contents.
        pub fn get_contents(&self) -> Vec<u8> {
            Vec::clone(
                &self
                    .buffer
                    .lock()
                    .expect("could not lock test sink for copying"),
            )
        }

        /// Creates a new reference to the testing sink that also implements `Sink`.
        ///
        /// Internally, the reference has a static lifetime through `Arc` and can thus be passed
        /// on independently.
        pub fn into_ref(self: Arc<Self>) -> TestingSinkRef {
            TestingSinkRef(self)
        }

        /// Helper function for sink implementations, calling `poll_ready`.
        fn sink_poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            if self.is_plugged(cx) {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }

        /// Helper function for sink implementations, calling `start_end`.
        fn sink_start_send<F: Buf>(&self, item: F) -> Result<(), Infallible> {
            let mut guard = self.buffer.lock().expect("could not lock buffer");

            item.reader()
                .read_to_end(&mut guard)
                .expect("writing to vec should never fail");

            Ok(())
        }

        /// Helper function for sink implementations, calling `sink_poll_flush`.
        fn sink_poll_flush(&self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            // We're always done storing the data, but we pretend we need to do more if clogged.
            if self.is_clogged(cx) {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }

        /// Helper function for sink implementations, calling `sink_poll_close`.
        fn sink_poll_close(&self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            // Nothing to close, so this is essentially the same as flushing.
            self.sink_poll_flush(cx)
        }
    }

    /// A plug/clog inserted into the sink.
    #[derive(Debug, Default)]
    struct SinkObstruction {
        /// Whether or not the sink is plugged.
        plugged: bool,
        /// Whether or not the sink is clogged.
        clogged: bool,
        /// The waker of the last task to access the plug. Will be called when removing.
        waker: Option<Waker>,
    }

    macro_rules! sink_impl_fwd {
        ($ty:ty) => {
            impl<F: Buf> Sink<F> for $ty {
                type Error = Infallible;

                fn poll_ready(
                    self: Pin<&mut Self>,
                    cx: &mut Context<'_>,
                ) -> Poll<Result<(), Self::Error>> {
                    self.sink_poll_ready(cx)
                }

                fn start_send(self: Pin<&mut Self>, item: F) -> Result<(), Self::Error> {
                    self.sink_start_send(item)
                }

                fn poll_flush(
                    self: Pin<&mut Self>,
                    cx: &mut Context<'_>,
                ) -> Poll<Result<(), Self::Error>> {
                    self.sink_poll_flush(cx)
                }

                fn poll_close(
                    self: Pin<&mut Self>,
                    cx: &mut Context<'_>,
                ) -> Poll<Result<(), Self::Error>> {
                    self.sink_poll_close(cx)
                }
            }
        };
    }

    /// A reference to a testing sink that implements `Sink`.
    #[derive(Debug)]
    pub struct TestingSinkRef(Arc<TestingSink>);

    impl Deref for TestingSinkRef {
        type Target = TestingSink;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    sink_impl_fwd!(TestingSink);
    sink_impl_fwd!(&TestingSink);
    sink_impl_fwd!(TestingSinkRef);

    #[test]
    fn simple_lifecycle() {
        let mut sink = TestingSink::new();
        assert!(sink.send(&b"one"[..]).now_or_never().is_some());
        assert!(sink.send(&b"two"[..]).now_or_never().is_some());
        assert!(sink.send(&b"three"[..]).now_or_never().is_some());

        assert_eq!(sink.get_contents(), b"onetwothree");
    }

    #[test]
    fn plug_blocks_sink() {
        let sink = TestingSink::new();
        let mut sink_handle = &sink;

        sink.set_plugged(true);

        // The sink is plugged, so sending should fail. We also drop the future, causing the value
        // to be discarded.
        assert!(sink_handle.send(&b"dummy"[..]).now_or_never().is_none());
        assert!(sink.get_contents().is_empty());

        // Now stuff more data into the sink.
        let second_send = sink_handle.send(&b"second"[..]);
        sink.set_plugged(false);
        assert!(second_send.now_or_never().is_some());
        assert!(sink_handle.send(&b"third"[..]).now_or_never().is_some());
        assert_eq!(sink.get_contents(), b"secondthird");
    }

    #[test]
    fn clog_blocks_sink_completion() {
        let sink = TestingSink::new();
        let mut sink_handle = &sink;

        sink.set_clogged(true);

        // The sink is clogged, so sending should fail to complete, but it is written.
        assert!(sink_handle.send(&b"first"[..]).now_or_never().is_none());
        assert_eq!(sink.get_contents(), b"first");

        // Now stuff more data into the sink.
        let second_send = sink_handle.send(&b"second"[..]);
        sink.set_clogged(false);
        assert!(second_send.now_or_never().is_some());
        assert!(sink_handle.send(&b"third"[..]).now_or_never().is_some());
        assert_eq!(sink.get_contents(), b"firstsecondthird");
    }

    #[tokio::test]
    async fn ensure_sink_wakes_up_after_plugging_in() {
        let sink = Arc::new(TestingSink::new());

        sink.set_plugged(true);

        let sink_alt = sink.clone();

        let join_handle = tokio::spawn(async move {
            sink_alt.as_ref().send(&b"sample"[..]).await.unwrap();
        });

        tokio::task::yield_now().await;
        sink.set_plugged(false);

        // This will block forever if the other task is not woken up. To verify, comment out the
        // `Waker::wake_by_ref` call in the sink implementation.
        join_handle.await.unwrap();
    }

    /// Test an "end-to-end" instance of the assembled pipeline for sending.
    #[test]
    fn chunked_length_prefixed_sink() {
        let base_sink: Vec<LengthPrefixedFrame<SingleChunk>> = Vec::new();

        let length_prefixed_sink =
            base_sink.with(|frame| future::ready(frame_add_length_prefix(frame)));

        let mut chunked_sink = length_prefixed_sink.with_flat_map(|frame| {
            let chunk_iter = chunk_frame(frame, 5.try_into().unwrap()).expect("TODO: Handle error");
            stream::iter(chunk_iter.map(Result::<_, Error>::Ok))
        });

        let sample_data = Bytes::from(&b"QRSTUV"[..]);

        chunked_sink
            .send(sample_data)
            .now_or_never()
            .unwrap()
            .expect("send failed");

        let chunks: Vec<_> = chunked_sink
            .into_inner()
            .into_inner()
            .into_iter()
            .map(collect_buf)
            .collect();

        assert_eq!(
            chunks,
            vec![b"\x06\x00\x00QRSTU".to_vec(), b"\x02\x00\xffV".to_vec()]
        )
    }
}
