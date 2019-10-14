//! A bounded, single-producer/single-consumer queue.
//!
//! # Examples
//!
//! ```
//! use crossbeam_queue::spsc;
//!
//! let (mut p, mut c) = spsc::with_capacity(2);
//!
//! assert!(p.try_push(1).is_ok());
//! assert!(p.try_push(2).is_ok());
//! assert!(p.try_push(3).is_err());
//!
//! assert_eq!(c.try_pop(), Ok(1));
//! assert_eq!(c.try_pop(), Ok(2));
//! assert!(c.try_pop().is_err());
//! ```

use std::{
    cell::{Cell, UnsafeCell},
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[cfg(feature = "std")]
use std::mem;

use crossbeam_utils::CachePadded;

/// The underlying buffer of the queue
#[derive(Debug)]
pub struct Buffer<T> {
    /// The number of slots in the buffer
    pub size: usize,

    /// Pointer to the raw, uninitialized memory
    pub data: *mut T,
}

impl<T> Buffer<T> {
    #[inline]
    unsafe fn slot(&self, offset: usize) -> *mut T {
        debug_assert!(offset < self.size);
        self.data.add(offset)
    }
}

#[cfg(feature = "std")]
impl<T> Buffer<T> {
    // Cache line pad for both the beginning and end of the buffer
    // to avoid false sharing with adjacent allocations
    fn data_cache_pad() -> usize {
        1 + (mem::size_of::<CachePadded<u8>>() - 1) / mem::size_of::<T>().max(1)
    }

    fn capacity(size: usize) -> usize {
        // Insert or account for the implicit cache line pads at the
        // beginning and end of the data!
        size + 2 * Self::data_cache_pad()
    }

    fn alloc(size: usize) -> Self {
        assert!(size > 0, "empty buffer");
        let data = {
            let capacity = Self::capacity(size);
            assert!(size < capacity, "size/capacity overflow");
            let mut v = Vec::<T>::with_capacity(capacity);
            let ptr = v.as_mut_ptr();
            mem::forget(v);
            // Skip the inserted cache line pad at the beginning of the data!
            unsafe { ptr.add(Self::data_cache_pad()) }
        };
        Self { data, size }
    }

    fn free(&mut self) {
        assert!(self.size > 0, "double free");
        let capacity = Self::capacity(self.size);
        // Account for the inserted cache line pad at the beginning of
        // the data!
        let offset = -(Self::data_cache_pad() as isize);
        unsafe {
            let ptr = self.data.offset(offset);
            Vec::from_raw_parts(ptr, 0, capacity);
        }
        // Free the uninitialized memory, i.e. don't drop the elements
        self.size = 0;
    }
}

/// The shared representation of a bounded, single-producer/single-consumer
/// queue.
#[derive(Debug)]
struct Shared<T> {
    /// Keeps track of the next slot that is writable.
    /// UnsafeCell is needed to allow accessing the underlying value
    /// without synchronization from the producer as the only writer.
    head: CachePadded<UnsafeCell<AtomicUsize>>,

    /// Keeps track of the first slot that is readable.
    /// UnsafeCell is needed to allow accessing the underlying value
    /// without synchronization from the consumer as the only writer.
    tail: CachePadded<UnsafeCell<AtomicUsize>>,

    /// The internal slot buffer.
    buffer: Buffer<T>,

    /// Flag that indicated if the buffer's memory needs to be freed.
    free_buffer: bool,
}

impl<T> Shared<T> {
    #[inline]
    fn head(&self) -> &AtomicUsize {
        // Safely access the contents of the UnsafeCell through a shared reference
        unsafe { &*self.head.get() }
    }

    #[inline]
    unsafe fn head_unsync(&self) -> usize {
        // Directly read the atomic value without synchronization
        *(*self.head.get()).get_mut()
    }

    #[inline]
    fn tail(&self) -> &AtomicUsize {
        // Safely access the contents of the UnsafeCell through a shared reference
        unsafe { &*self.tail.get() }
    }

    #[inline]
    unsafe fn tail_unsync(&self) -> usize {
        // Directly read the atomic value without synchronization
        *(*self.tail.get()).get_mut()
    }

    #[inline]
    fn capacity(&self) -> usize {
        // One slot must remain empty between writer head and reader tail
        debug_assert!(self.buffer.size > 0);
        self.buffer.size - 1
    }

    #[inline]
    fn readable_capacity(&self, head: usize, tail: usize) -> usize {
        // (head + self.buffer.size - tail) % self.buffer.size
        let capacity = if head >= tail {
            head - tail
        } else {
            self.buffer.size - (tail - head)
        };
        debug_assert!(capacity <= self.capacity());
        capacity
    }

    #[inline]
    fn writable_capacity(&self, head: usize, tail: usize) -> usize {
        // (tail + (size - 1) - head) % size
        let capacity = if tail > head {
            (tail - head) - 1
        } else {
            (self.buffer.size - 1) - (head - tail)
        };
        debug_assert!(capacity <= self.capacity());
        capacity
    }

    #[inline]
    fn debug_assert_capacity(&self, head: usize, tail: usize) {
        debug_assert_eq!(
            self.capacity(),
            self.readable_capacity(head, tail) + self.writable_capacity(head, tail)
        );
    }

    #[inline]
    fn skip(&self, pos: usize, n: usize) -> (usize, (usize, usize)) {
        debug_assert!(pos < self.buffer.size);
        debug_assert!(n < self.buffer.size);
        let tail_size = self.buffer.size - pos;
        let (skip_pos, (n1, n2)) = if n < tail_size {
            (pos + n, (n, 0))
        } else {
            (n - tail_size, (tail_size, n - tail_size))
        };
        debug_assert!(skip_pos < self.buffer.size);
        debug_assert!(n1 < self.buffer.size);
        debug_assert!(n2 < self.buffer.size);
        debug_assert_eq!(n, n1 + n2);
        debug_assert!(n1 > 0 || n2 == 0);
        debug_assert!(skip_pos >= n1 || skip_pos == n2);
        (skip_pos, (n1, n2))
    }

    /// Splits the shared queue into producer/consumer sides.
    fn split(self) -> (Producer<T>, Consumer<T>) {
        let head = self.head().load(Ordering::Relaxed);
        let tail = self.tail().load(Ordering::Relaxed);

        let shared = Arc::new(self);

        let producer = Producer {
            shared: shared.clone(),
            cached_tail: Cell::new(tail),
        };

        let consumer = Consumer {
            shared,
            cached_head: Cell::new(head),
        };

        (producer, consumer)
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let head = self.head().load(Ordering::Relaxed);
        let mut tail = self.tail().load(Ordering::Relaxed);

        // Loop over all slots that hold a value and drop them
        while tail != head {
            unsafe {
                self.buffer.slot(tail).drop_in_place();
            }
            tail = self.skip(tail, 1).0;
        }

        // Finally, free the buffer if it has been allocated upon construction
        #[cfg(feature = "std")]
        {
            if self.free_buffer {
                self.buffer.free();
            }
        }
        #[cfg(not(feature = "std"))]
        {
            debug_assert!(!self.free_buffer);
        }
    }
}


/// Constructs a bounded, single-producer/single-consumer queue with
/// the specified capacity.
///
/// The queue will buffer up to `capacity` elements at once. The internal
/// buffer required for this capacity is allocated upon construction.
///
/// Returns the producer and the consumer side of the queue.
#[cfg(feature = "std")]
pub fn with_capacity<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let size = capacity + 1; // one free empty slot

    Shared {
        head: CachePadded::new(UnsafeCell::new(AtomicUsize::new(0))),
        tail: CachePadded::new(UnsafeCell::new(AtomicUsize::new(0))),
        buffer: Buffer::alloc(size),
        free_buffer: true,
    }.split()
}

/// Constructs a bounded, single-producer/single-consumer queue on
/// the given buffer.
///
/// The buffer must not be empty. One slot of the buffer is needed
/// as a separator between occupied and free slots, i.e. the actual
/// capacity will be `buffer.size - 1`.
///
/// The caller is responsible for managing the (uninitialized) memory
/// that is backing up the buffer! The memory must only be freed after
/// the queue has been dropped. A typical use case for this function
/// is to operate on statically allocated memory that does never need
/// to be freed.
///
/// Returns the producer and the consumer side of the queue.
pub fn new<T, D>(buffer: Buffer<T>) -> (Producer<T>, Consumer<T>) {
    assert!(
        buffer.size > 0,
        "at least one free empty slot in the buffer is required"
    );
    Shared {
        head: CachePadded::new(UnsafeCell::new(AtomicUsize::new(0))),
        tail: CachePadded::new(UnsafeCell::new(AtomicUsize::new(0))),
        buffer,
        free_buffer: false,
    }.split()
}

/// Error results that might occur when trying to pop values from a queue.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TryPopError {
    /// The queue is empty.
    Empty,
}

impl fmt::Display for TryPopError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TryPopError::Empty => f.write_str("queue is empty"),
        }
    }
}

impl Error for TryPopError {}

/// Error results that might occur when trying to push values into a queue.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TryPushError<T> {
    /// The value that could not be written because the queue is full.
    Full(T),
}

impl<T> fmt::Display for TryPushError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TryPushError::Full(_) => f.write_str("queue is full"),
        }
    }
}

impl<T: Send + fmt::Debug> Error for TryPushError<T> {}

/// The producer side of a bounded, single-producer/single-consumer queue.
///
/// Writes to the head of the queue.
#[derive(Debug)]
pub struct Producer<T> {
    /// The shared representation of the queue.
    shared: Arc<Shared<T>>,

    /// An exclusive copy of the shared tail for quick access.
    ///
    /// This value can become stale and then needs to be resynchronized
    /// with the shared tail that is updated by the consumer.
    cached_tail: Cell<usize>,
}

unsafe impl<T: Send> Send for Producer<T> {}

impl<T> Producer<T> {
    #[inline]
    fn head(&self) -> usize {
        // Unsynchronized access of the underlying atomic value is
        // permitted, because the producer is the only writer!!
        unsafe { self.shared.head_unsync() }
    }

    #[inline]
    fn tail(&self) -> usize {
        self.cached_tail.get()
    }

    /// Refreshes the cached tail.
    #[inline]
    fn refresh_tail(&self) -> usize {
        let tail = self.shared.tail().load(Ordering::Acquire);
        debug_assert!(tail < self.shared.buffer.size);
        self.cached_tail.set(tail);
        tail
    }

    /// Updates both the shared and the cached head.
    #[inline]
    fn update_head(&mut self, head: usize) {
        self.shared.head().store(head, Ordering::Release);
    }

    #[inline]
    unsafe fn slot(&self, offset: usize) -> *mut T {
        self.shared.buffer.slot(offset)
    }

    fn acquire_writable(&self, min_count: usize) -> (usize, usize) {
        let head = self.head();
        let mut tail = self.tail();
        self.shared.debug_assert_capacity(head, tail);
        let mut writable = self.shared.writable_capacity(head, tail);
        if writable < min_count {
            tail = self.refresh_tail();
            self.shared.debug_assert_capacity(head, tail);
            debug_assert!(writable <= self.shared.writable_capacity(head, tail));
            writable = self.shared.writable_capacity(head, tail);
        }
        debug_assert!(writable < self.shared.buffer.size);
        (head, writable)
    }

    /// Returns the capacity of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc;
    ///
    /// let (p, c) = spsc::with_capacity::<i32>(100);
    ///
    /// assert_eq!(c.capacity(), 100);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
    }

    /// Returns how many slots are immediately available for writing.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc;
    ///
    /// let (mut p, mut c) = spsc::with_capacity(1);
    /// assert_eq!(p.try_push(10), Ok(()));
    /// assert_eq!(c.try_pop(), Ok(10));
    ///
    /// // Producer has not been refreshed yet
    /// assert_eq!(p.peek(), 0);
    /// ```
    #[inline]
    pub fn peek(&self) -> usize {
        self.acquire_writable(0).1
    }

    /// Returns how many slots are available for writing given a minimum amount.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc;
    ///
    /// let (mut p, mut c) = spsc::with_capacity(1);
    /// assert_eq!(p.try_push(10), Ok(()));
    /// assert_eq!(c.try_pop(), Ok(10));
    ///
    /// // Producer has not been refreshed yet
    /// assert_eq!(p.peek_min(0), 0);
    /// // Enforce refresh of producer
    /// assert_eq!(p.peek_min(1), 1);
    /// assert_eq!(p.peek_min(2), 1);
    /// ```
    #[inline]
    pub fn peek_min(&self, min_count: usize) -> usize {
        self.acquire_writable(min_count).1
    }

    /// Returns how many slots are actually available for writing.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc;
    ///
    /// let (mut p, mut c) = spsc::with_capacity(1);
    /// assert_eq!(p.try_push(10), Ok(()));
    /// assert_eq!(c.try_pop(), Ok(10));
    ///
    /// assert_eq!(p.peek_max(), 1);
    /// ```
    #[inline]
    pub fn peek_max(&self) -> usize {
        self.peek_min(self.shared.buffer.size)
    }

    /// Attempts to push an element into the queue.
    ///
    /// If the queue is full, the element is returned back as an error.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc::{self, TryPushError};
    ///
    /// let (mut p, _) = spsc::with_capacity(1);
    ///
    /// assert_eq!(p.try_push(10), Ok(()));
    /// assert_eq!(p.try_push(20), Err(TryPushError::Full(20)));
    /// ```
    pub fn try_push(&mut self, value: T) -> Result<(), TryPushError<T>> {
        let (mut head, writable) = self.acquire_writable(1);
        if writable == 0 {
            return Err(TryPushError::Full(value));
        }

        // Write the value into the head slot
        unsafe {
            self.slot(head).write(value);
        }
        head = self.shared.skip(head, 1).0;

        // Move the head one slot forward
        self.update_head(head);

        Ok(())
    }

    /// Obtains two contiguous slices for writing.
    ///
    /// Returns a pair of two contiguous slices with uninitialized slots for
    /// writing. The two (possibly empty) slices represent all free slots that
    /// are immediately available for writing. The first slice has to be filled
    /// before starting to write into the second slice.
    ///
    /// The parameter `min_count` is a hint for the desired number of free slots.
    /// It affects if coordination with the consumer is required. For `min_count = 0`
    /// only the free slots currently known to the producer are returned, not taking
    /// into account any slots that have been freed by the consumer in the meanwhile.
    ///
    /// Either the second or both slices might be empty. If the first slice is empty
    /// then the queue is full.
    pub unsafe fn writable_slices_raw(&mut self, min_count: usize) -> (&mut [T], &mut [T]) {
        let (head, writable) = self.acquire_writable(min_count);
        let (_, (n1, n2)) = self.shared.skip(head, writable);
        let s1 = std::slice::from_raw_parts_mut(self.slot(head), n1);
        let s2 = std::slice::from_raw_parts_mut(self.slot(0), n2);
        debug_assert!(!s1.is_empty() || s2.is_empty());
        (s1, s2)
    }

    /// Advances the write position.
    ///
    /// The caller is responsible that the corresponding slots have been
    /// initialized properly upon writing.
    pub fn skip_writable(&mut self, count_write: usize) {
        debug_assert!(count_write <= self.peek());
        let head = self.shared.skip(self.head(), count_write).0;
        self.update_head(head);
    }
}

impl<T: Copy + Default> Producer<T> {
    /// Obtains two contiguous slices for writing.
    ///
    /// See also: [`writable_slices_raw`](#method.writable_slices_raw)
    #[inline]
    pub fn writable_slices(&mut self, min_count: usize) -> (&mut [T], &mut [T]) {
        unsafe { self.writable_slices_raw(min_count) }
    }

    /// Copies from a slice into the queue.
    ///
    /// Copy as many elements as possible from the the given slice
    /// into the queue.
    ///
    /// Returns how many elements have actually been copied, i.e. the
    /// contents of the slice may have only been copied partially.
    pub fn copy_from_slice(&mut self, src: &[T]) -> usize {
        let (dst1, dst2) = self.writable_slices(src.len());
        let len1 = dst1.len().min(src.len());
        let len2 = dst2.len().min(src.len() - len1);
        let len = len1 + len2;
        dst1.copy_from_slice(&src[..len1]);
        dst2.copy_from_slice(&src[len1..len]);
        self.skip_writable(len);
        len
    }

    /// Copies from a slice into the queue (blocking).
    ///
    /// Copy all elements from the given slice into the queue.
    ///
    /// This function blocks by looping until all elements from
    /// the slice have been copied into the queue!
    pub fn copy_from_slice_blocking(&mut self, src: &[T]) {
        let mut write_len = 0;
        while write_len < src.len() {
            write_len += self.copy_from_slice(&src[write_len..]);
        }
    }
}

/// The consumer side of a bounded, single-producer/single-consumer queue.
///
/// Reads from the tail of the queue.
#[derive(Debug)]
pub struct Consumer<T> {
    /// The shared representation of the queue.
    shared: Arc<Shared<T>>,

    /// An exclusive copy of the shared head for quick access.
    ///
    /// This value can become stale and then needs to be resynchronized
    /// with the shared head that is updated by the producer.
    cached_head: Cell<usize>,
}

unsafe impl<T: Send> Send for Consumer<T> {}

impl<T> Consumer<T> {
    #[inline]
    fn head(&self) -> usize {
        self.cached_head.get()
    }

    #[inline]
    fn tail(&self) -> usize {
        // Unsynchronized access of the underlying atomic value is
        // permitted, because the consumer is the only writer!!
        unsafe { self.shared.tail_unsync() }
    }

    /// Refreshes the cached head.
    #[inline]
    fn refresh_head(&self) -> usize {
        let head = self.shared.head().load(Ordering::Acquire);
        debug_assert!(head < self.shared.buffer.size);
        self.cached_head.set(head);
        head
    }

    /// Updates both the shared and the cached head.
    #[inline]
    fn update_tail(&mut self, tail: usize) {
        self.shared.tail().store(tail, Ordering::Release);
    }

    #[inline]
    unsafe fn slot(&self, offset: usize) -> *const T {
        self.shared.buffer.slot(offset)
    }

    fn acquire_readable(&self, min_count: usize) -> (usize, usize) {
        let mut head = self.head();
        let tail = self.tail();
        self.shared.debug_assert_capacity(head, tail);
        let mut readable = self.shared.readable_capacity(head, tail);
        if readable < min_count {
            head = self.refresh_head();
            self.shared.debug_assert_capacity(head, tail);
            debug_assert!(readable <= self.shared.readable_capacity(head, tail));
            readable = self.shared.readable_capacity(head, tail);
        }
        debug_assert!(readable < self.shared.buffer.size);
        (tail, readable)
    }

    /// Returns the capacity of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc;
    ///
    /// let (p, c) = spsc::with_capacity::<i32>(100);
    ///
    /// assert_eq!(c.capacity(), 100);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
    }

    /// Returns how many slots are immediately available for reading.
    #[inline]
    pub fn peek(&self) -> usize {
        self.acquire_readable(0).1
    }

    /// Returns how many slots are available for reading given a minimum amount.
    #[inline]
    pub fn peek_min(&self, min_count: usize) -> usize {
        self.acquire_readable(min_count).1
    }

    /// Returns how many slots are actually available for reading.
    #[inline]
    pub fn peek_max(&self) -> usize {
        self.peek_min(self.shared.buffer.size)
    }

    /// Attempts to pop an element from the queue.
    ///
    /// If the queue is empty, an error is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc::{self, TryPopError};
    ///
    /// let (mut p, mut c) = spsc::with_capacity(1);
    /// assert_eq!(p.try_push(10), Ok(()));
    ///
    /// assert_eq!(c.try_pop(), Ok(10));
    /// assert_eq!(c.try_pop(), Err(TryPopError::Empty));
    /// ```
    pub fn try_pop(&mut self) -> Result<T, TryPopError> {
        let (mut tail, readable) = self.acquire_readable(1);
        if readable == 0 {
            return Err(TryPopError::Empty);
        }

        // Read the value from the tail slot
        let value = unsafe { self.slot(tail).read() };
        tail = self.shared.skip(tail, 1).0;

        // Move the tail one slot forward
        self.update_tail(tail);

        Ok(value)
    }

    /// Obtains two contiguous slices for reading raw elements.
    ///
    /// The two (possibly empty) slices represent all occupied slots that are
    /// immediately available for reading. The first slice has to be read before
    /// starting to read from the second slice. If the first slice is empty then
    /// the queue is empty.
    ///
    /// The parameter `min_count` is a hint for the desired number of occupied slots.
    /// It affects if coordination with the producer is required. For `min_count = 0`
    /// only the occupied slots currently known to the consumer are returned, not
    /// taking into account any slots that have been written by the producer meanwhile.
    pub unsafe fn readable_slices_raw(&self, min_count: usize) -> (&[T], &[T]) {
        let (tail, readable) = self.acquire_readable(min_count);
        let (_, (n1, n2)) = self.shared.skip(tail, readable);
        let s1 = std::slice::from_raw_parts(self.slot(tail), n1);
        let s2 = std::slice::from_raw_parts(self.slot(0), n2);
        debug_assert!(!s1.is_empty() || s2.is_empty());
        (s1, s2)
    }

    /// Advances the read position.
    ///
    /// Skips the given number of readable slots without dropping the
    /// contained values.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_queue::spsc;
    ///
    /// let (mut p, mut c) = spsc::with_capacity(2);
    /// assert_eq!(p.try_push(1), Ok(()));
    /// assert_eq!(p.try_push(2), Ok(()));
    /// assert_eq!(c.try_pop(), Ok(1));
    /// assert_eq!(p.try_push(3), Ok(()));
    /// assert_eq!(c.try_pop(), Ok(2));
    ///
    /// // Wrap around after reaching internal buffer size 3 = 2 + 1
    /// assert_eq!(p.try_push(4), Ok(()));
    ///
    /// let (s1, s2) = c.readable_slices(2);
    /// assert_eq!(s1, &[3]);
    /// assert_eq!(s2, &[4]);
    ///
    /// c.skip_readable(1);
    /// assert_eq!(p.try_push(5), Ok(()));
    ///
    /// let (s1, s2) = c.readable_slices(2);
    /// assert_eq!(s1, &[4, 5]);
    /// assert_eq!(s2, &[]);
    ///
    /// c.skip_readable(2);
    ///
    /// let (s1, s2) = c.readable_slices(1);
    /// assert_eq!(s1, &[]);
    /// assert_eq!(s2, &[]);
    /// ```
    pub fn skip_readable(&mut self, read_count: usize) {
        debug_assert!(read_count <= self.peek());
        let tail = self.shared.skip(self.tail(), read_count).0;
        self.update_tail(tail);
    }
}

impl<T: Copy> Consumer<T> {
    /// Obtains two contiguous slices for reading.
    ///
    /// See also: [`readable_slices_raw`](#method.readable_slices_raw)
    #[inline]
    pub fn readable_slices(&self, min_count: usize) -> (&[T], &[T]) {
        unsafe { self.readable_slices_raw(min_count) }
    }

    /// Copies from the queue into a slice.
    ///
    /// Copy available elements from the queue into the given slice.
    ///
    /// Returns how many elements have actually been copied, i.e. the
    /// slice may only be filled partially.
    pub fn copy_into_slice(&mut self, dst: &mut [T]) -> usize {
        let (src1, src2) = self.readable_slices(dst.len());
        let len1 = src1.len().min(dst.len());
        let len2 = src2.len().min(dst.len() - len1);
        let len = len1 + len2;
        dst.copy_from_slice(&src1[..len1]);
        dst[len1..].copy_from_slice(&src2[..len2]);
        self.skip_readable(len);
        len
    }

    /// Copies from the queue into a slice (blocking).
    ///
    /// Copy elements from the queue into the given slice exhaustively.
    ///
    /// This function blocks by looping until the whole slice has been
    /// filled with elements from the queue!
    pub fn copy_into_slice_blocking(&mut self, dst: &mut [T]) {
        let mut read_len = 0;
        while read_len < dst.len() {
            read_len += self.copy_into_slice(&mut dst[read_len..]);
        }
    }

    /// Clears the queue.
    ///
    /// Discard all buffered elements.
    ///
    /// Returns the number if dropped elements.
    pub fn clear(&mut self) -> usize {
        let drop_count = self.peek_max();
        self.skip_readable(drop_count);
        drop_count
    }
}
