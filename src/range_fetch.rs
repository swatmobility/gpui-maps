//! Byte-range reads as few, concurrent requests: what a Cloud-Optimised
//! GeoTIFF's internal tiles, a GeoParquet file's column chunks and a
//! FlatGeobuf's features have in common.
//!
//! [`coalesce`] merges the ranges one read needs into requests: two ranges
//! closer than `merge_gap` bytes become one request (the gap is read and
//! thrown away: a few KiB cost less than a round trip), and no request
//! grows past `max_request` bytes, so a large read still fans out into
//! several requests. [`join_bounded`] then runs those requests at most
//! [`MAX_CONCURRENT_RANGES`] at a time on the awaiting task -- no spawn, no
//! runtime, so the same code runs on tokio and in the browser.

/// How many range requests one read keeps in flight: enough to hide a
/// remote bucket's round trip behind the others, few enough that one read
/// does not monopolise a host's connection pool (a browser allows six per
/// origin over HTTP/1.1 and queues the rest).
pub const MAX_CONCURRENT_RANGES: usize = 8;

/// One wanted range of the file. `key` is the reader's own identity for
/// it (a COG's level, plane and tile index; a Parquet row group and column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange<K> {
    pub key: K,
    pub start: u64,
    pub len: u64,
}

impl<K> ByteRange<K> {
    /// One past the last byte.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// One request: `start..start + len`, covering `members`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeGroup<K> {
    pub start: u64,
    pub len: u64,
    pub members: Vec<ByteRange<K>>,
}

/// Merges `ranges` into requests: sorted by offset, a range joins the
/// current request when it starts within `merge_gap` bytes of its end and
/// the request would stay within `max_request` bytes (a single range
/// larger than that is a request of its own). Duplicate keys are fetched
/// once.
#[must_use]
pub fn coalesce<K: Copy + PartialEq>(ranges: &[ByteRange<K>], merge_gap: u64, max_request: u64) -> Vec<RangeGroup<K>> {
    let mut sorted: Vec<ByteRange<K>> = ranges.to_vec();
    sorted.sort_by_key(|r| (r.start, r.len));
    sorted.dedup_by(|a, b| a.key == b.key);
    let mut out: Vec<RangeGroup<K>> = Vec::new();
    for r in sorted {
        if let Some(group) = out.last_mut() {
            let end = group.start + group.len;
            let new_end = end.max(r.end());
            if r.start <= end.saturating_add(merge_gap) && new_end - group.start <= max_request {
                group.len = new_end - group.start;
                group.members.push(r);
                continue;
            }
        }
        out.push(RangeGroup { start: r.start, len: r.len, members: vec![r] });
    }
    out
}

/// Polls `futures` to completion on the current task, at most `limit` at
/// once, results in order: a dependency-free bounded `join_all` (the
/// requests overlap on the network while one task waits). A future not
/// yet started is not polled, so it has not begun its request.
pub async fn join_bounded<F: std::future::Future>(futures: Vec<F>, limit: usize) -> Vec<F::Output> {
    use std::pin::Pin;
    use std::task::Poll;
    let limit = limit.max(1);
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut active = 0;
        for (fut, slot) in futures.iter_mut().zip(out.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            if active == limit {
                break;
            }
            match fut.as_mut().poll(cx) {
                Poll::Ready(v) => *slot = Some(v),
                Poll::Pending => active += 1,
            }
        }
        if out.iter().all(Option::is_some) {
            Poll::Ready(out.iter_mut().map(|s| s.take().expect("ready")).collect())
        } else {
            Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At most `limit` futures are polled at once, and the results come
    /// back in the order the futures were given.
    #[test]
    fn join_bounded_keeps_order_and_the_limit() {
        use std::cell::Cell;
        use std::rc::Rc;
        let (now, most) = (Rc::new(Cell::new(0usize)), Rc::new(Cell::new(0usize)));
        let futures: Vec<_> = (0..20u32)
            .map(|i| {
                let (now, most) = (now.clone(), most.clone());
                async move {
                    now.set(now.get() + 1);
                    most.set(most.get().max(now.get()));
                    // Pending once, so the others get polled meanwhile.
                    let mut yielded = false;
                    std::future::poll_fn(|cx| {
                        if yielded {
                            std::task::Poll::Ready(())
                        } else {
                            yielded = true;
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                    now.set(now.get() - 1);
                    i * 2
                }
            })
            .collect();
        let got = pollster::block_on(join_bounded(futures, 8));
        assert_eq!(got, (0..20).map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(most.get(), 8);
    }
}
