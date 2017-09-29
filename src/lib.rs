// a slightly impressionistic interpretation of DPDK's ring library
// Anything in the header is marked #[inline]
// everything translated from a macro is #[inline(always)]
// block comments are from the original, line mine
#![cfg_attr(feature = "nightly", feature(attr_literals, repr_align, hint_core_should_pause, core_intrinsics))]
#![feature(manually_drop)]

use std::cell::UnsafeCell;
use std::iter::ExactSizeIterator;
use std::mem::{self, ManuallyDrop};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(feature = "nightly")]
use std::intrinsics;

//FIXME
type AtomicU32 = AtomicUsize;


#[cfg(feature = "nightly")]
macro_rules! unlikely {
    ($b: expr) => (unsafe{intrinsics::unlikely($b)});
}

#[cfg(feature = "nightly")]
macro_rules! likely {
    ($b: expr) => (unsafe{intrinsics::likely($b)});
}

#[cfg(not(feature = "nightly"))]
macro_rules! unlikely { ($b: expr) => ($b); }

#[cfg(not(feature = "nightly"))]
macro_rules! likely { ($b: expr) => ($b); }

#[repr(C)]
pub struct Ring<T: Send, P, C> {
    prod: Box<Prod<P>>,
    cons: Box<Cons<C>>,

    ring: Data<T>,
    _neither_send_not_sync: PhantomData<::std::cell::UnsafeCell<T>>,
}

struct Data<T>(Box<[ManuallyDrop<UnsafeCell<T>>]>);

struct Prod<P> {
    watermark: u32,   /**< Maximum items before EDQUOT. */
    mask: u32,        /**< Mask (size-1) of ring. */
    head: AtomicU32,  /**< Producer head. */
    tail: AtomicU32,  /**< Producer tail. */
    _align: [CacheAlign; 0],
    _pd: PhantomData<P>
}

struct Cons<C> {
    mask: u32,        /**< Mask (size-1) of ring. */
    head: AtomicU32,  /**< Consumer head. */
    tail: AtomicU32,  /**< Consumer tail. */
    _align: [CacheAlign; 0],
    _pd: PhantomData<C>
}

#[cfg_attr(feature = "nightly", repr(align(64)))]
#[derive(Default)]
struct CacheAlign();

pub enum SP {}
pub enum MP {}
pub enum SC {}
pub enum MC {}
enum Unknown {}

const RTE_RING_SZ_MASK: u32 = 0x0fffffff;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum EnqueueErr {
    NotEnoughRoom,
    ExceededQuota(usize),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum DequeueErr {
    NotEnoughInQueue,
    Empty,
    NothingToDo,
}

impl<T: Send, P, C> Ring<T, P, C> {

    fn with_capacity<'s>(capacity: u32) -> Result<Self, ()> {
        let count =
            if capacity.is_power_of_two() {
                (capacity + 1).checked_next_power_of_two().ok_or(())?
            } else {
                capacity.checked_next_power_of_two().ok_or(())?
            };
        Self::new(count)
    }

    // TODO currently there is no way to dealloc,
    //      maybe repurpose Prod.flags as an ARC? it looks ununsed
    fn new(count: u32) -> Result<Self, ()> {
        if !count.is_power_of_two() || count > RTE_RING_SZ_MASK {
            return Err(())
        }
        let ring = unsafe {
            let mut data = Vec::with_capacity(count as usize);
            data.set_len(count as usize);
            Data(data.into_boxed_slice())
        };
        let mask = count - 1;
        let watermark = count;
        Ok(Ring {
            prod: Box::new(Prod {
                mask,
                head: AtomicU32::new(0),
                tail: AtomicU32::new(0),
                watermark,
                _align: Default::default(),
                _pd: Default::default(),
            }),
            cons: Box::new(Cons {
                mask,
                head: AtomicU32::new(0),
                tail: AtomicU32::new(0),
                _align: Default::default(),
                _pd: Default::default(),
            }),
            _neither_send_not_sync: Default::default(),
            ring,
        })
    }

    fn split(self) -> (Arc<Ring<T, P, Unknown>>, Arc<Ring<T, Unknown, C>>) {
        let a: Arc<Ring<T, P, C>> = Arc::new(self);
        unsafe { (mem::transmute(a.clone()), mem::transmute(a)) }
    }

    fn free_count(&self) -> u32 {
        let prod_tail = self.prod.tail.load(Ordering::Relaxed) as u32;
        let cons_tail = self.cons.tail.load(Ordering::Relaxed) as u32;
        cons_tail.wrapping_sub(prod_tail).wrapping_sub(1) & self.prod.mask
    }

    fn used_count(&self) -> u32 {
        let prod_tail = self.prod.tail.load(Ordering::Relaxed) as u32;
        let cons_tail = self.cons.tail.load(Ordering::Relaxed) as u32;
        prod_tail.wrapping_sub(cons_tail) & self.prod.mask
    }

    fn is_full(&self) -> bool {
        let prod_tail = self.prod.tail.load(Ordering::Relaxed) as u32;
        let cons_tail = self.cons.tail.load(Ordering::Relaxed) as u32;
        cons_tail.wrapping_sub(prod_tail).wrapping_sub(1) & self.prod.mask == 0
    }

    fn is_empty(&self) -> bool {
        let prod_tail = self.prod.tail.load(Ordering::Relaxed) as u32;
        let cons_tail = self.cons.tail.load(Ordering::Relaxed) as u32;
        cons_tail == prod_tail
    }
}

impl<T: Send, C> Ring<T, MP, C> {
    fn enqueue<I>(&self, obj_table: I, all_or_none: bool) -> Result<usize, EnqueueErr>
    where I: ExactSizeIterator<Item=T> {
        if obj_table.len() == 0 { return Ok(0) }
        let (mut prod_head, mut prod_next, mut n);
        let mut free_entries: u32;

        let mask = self.prod.mask;

        'enqueue: loop {
            n = obj_table.len() as _;
            prod_head = self.prod.head.load(Ordering::Relaxed) as _;
            let cons_tail: u32 = self.cons.tail.load(Ordering::Relaxed) as _;
            /* The subtraction is done between two unsigned 32bits value
            * (the result is always modulo 32 bits even if we have
            * prod_head > cons_tail). So 'free_entries' is always between 0
            * and size(ring)-1. */
            free_entries = mask.wrapping_add(cons_tail.wrapping_sub(prod_head));
            if unlikely!(n > free_entries) {
                if all_or_none {
                    return Err(EnqueueErr::NotEnoughRoom)
                } else {
                    if unlikely!(free_entries == 0) {
                        return Ok(0)
                    }

                    n = free_entries
                }
            }

            prod_next = prod_head.wrapping_add(n);
            //TODO Ordering
            //     I believe the barriers below abrogate the need for a
            //     stronger ordering here
            let success = self.prod.head.compare_exchange(prod_head as _,
                          prod_next as _, Ordering::Relaxed, Ordering::Relaxed);

            if likely!(success.is_ok()) { break 'enqueue }
        }
        unsafe { self.enqueue_objs(prod_head, mask, n, obj_table) }
        smp_wmb();

        /* if we exceed the watermark */
        let ret = if unlikely!((mask + 1) - free_entries + n > self.prod.watermark) {
            if all_or_none {
                return Err(EnqueueErr::NotEnoughRoom)
            } else {
                return Err(EnqueueErr::ExceededQuota(n as _))
            }
        } else {
            //if all_or_none { Ok(0) } else { return Ok(n as _) }
            // On successful all_or_none put we return n instead of 0
            Ok(n as _)
        };

        /*
         * If there are other enqueues in progress that preceded us,
         * we need to wait for them to complete
         */
        while unlikely!(self.prod.tail.load(Ordering::Relaxed) != prod_head as _) {
           pause();
        }

        self.prod.tail.store(prod_next as _, Ordering::Relaxed);
        return ret
    }
}

impl<T: Send, C> Ring<T, SP, C> {
    fn enqueue<I>(&self, obj_table: I, all_or_none: bool) -> Result<usize, EnqueueErr>
    where I: ExactSizeIterator<Item=T> {
        let mask = self.prod.mask;

        let mut n = obj_table.len() as _;
        let prod_head: u32 = self.prod.head.load(Ordering::Relaxed) as _;
        let cons_tail: u32 = self.cons.tail.load(Ordering::Relaxed) as _;
        /* The subtraction is done between two unsigned 32bits value
        * (the result is always modulo 32 bits even if we have
        * prod_head > cons_tail). So 'free_entries' is always between 0
        * and size(ring)-1. */
        let free_entries: u32 = mask.wrapping_add(cons_tail.wrapping_sub(prod_head));
        if unlikely!(n > free_entries) {
            if all_or_none {
                return Err(EnqueueErr::NotEnoughRoom)
            } else {
                if unlikely!(free_entries == 0) {
                    /* No free entry available */
                    return Ok(0)
                }
                n = free_entries
            }
        }

        let prod_next = prod_head.wrapping_add(n);
        self.prod.head.store(prod_next as _, Ordering::Relaxed);

        /* write entries in ring */
        unsafe { self.enqueue_objs(prod_head, mask, n, obj_table) }
        smp_wmb();

        /* if we exceed the watermark */
        let ret = if unlikely!((mask + 1) - free_entries + n > self.prod.watermark) {
            if all_or_none {
                return Err(EnqueueErr::NotEnoughRoom)
            } else {
                return Err(EnqueueErr::ExceededQuota(n as _))
            }
        } else {
            //if all_or_none { Ok(0) } else { Ok(n as _) }
            // On successful all_or_none put we return n instead of 0
            Ok(n as _)
        };

        self.prod.tail.store(prod_next as _, Ordering::Relaxed);
        return ret
    }
}

impl<T: Send, P> Ring<T, P, MC> {
    fn dequeue<'a, 's: 'a, F, R>(&'s self, max: u32, all_or_none: bool, obj_table: F) -> Result<(usize, R), DequeueErr>
    where F: FnOnce(DequeIter<'a, T>) -> R {
        if max == 0 { return Err(DequeueErr::NothingToDo) }

        let (mut cons_head, mut cons_next);
        let mut entries: u32;
        let mut n;
        let mask = self.cons.mask;

        'dequeue: loop {
            n = max;
            cons_head = self.cons.head.load(Ordering::Relaxed) as u32;
            let prod_tail: u32 = self.prod.tail.load(Ordering::Relaxed) as _;
            /* The subtraction is done between two unsigned 32bits value
             * (the result is always modulo 32 bits even if we have
             * cons_head > prod_tail). So 'entries' is always between 0
             * and size(ring)-1. */
            entries = prod_tail.wrapping_sub(cons_head);

            /* Set the actual entries for dequeue */

            if n > entries {
                if all_or_none {
                    return Err(DequeueErr::NotEnoughInQueue)
                } else {
                    if unlikely!(entries == 0) {
                        return Err(DequeueErr::Empty)
                    }

                    n = entries
                }
            }

            cons_next = cons_head.wrapping_add(n);
            //TODO Ordering
            //     I believe the barriers below abrogate the need for a
            //     stronger ordering here
            let success = self.cons.head.compare_exchange(cons_head as _,
                          cons_next as _, Ordering::Relaxed, Ordering::Relaxed);
            if likely!(success.is_ok()) { break 'dequeue }
        }
        let ret = self.dequeue_objs(cons_head, mask, n, obj_table);
        smp_rmb();

        /*
         * If there are other enqueues in progress that preceded us,
         * we need to wait for them to complete
         */
        while unlikely!(self.cons.tail.load(Ordering::Relaxed) != cons_head as _) {
           pause();
        }

        self.cons.tail.store(cons_next as _, Ordering::Relaxed);
        return Ok((n as _, ret))
    }
}

impl<T: Send, P> Ring<T, P, SC> {
    fn dequeue<'a, 's: 'a, F, R>(&'s self, max: u32, all_or_none: bool, obj_table: F) -> Result<(usize, R), DequeueErr>
    where F: FnOnce(DequeIter<'a, T>) -> R {
        let mut n = max;
        let mask = self.cons.mask;

        let cons_head: u32 = self.cons.head.load(Ordering::Relaxed) as _;
        let prod_tail: u32 = self.prod.tail.load(Ordering::Relaxed) as _;
        /* The subtraction is done between two unsigned 32bits value
         * (the result is always modulo 32 bits even if we have
         * cons_head > prod_tail). So 'entries' is always between 0
         * and size(ring)-1. */
        let entries: u32 = prod_tail.wrapping_sub(cons_head);
        if n > entries {
            if all_or_none {
                return Err(DequeueErr::NotEnoughInQueue)
            } else {
                if unlikely!(entries == 0) {
                    return Err(DequeueErr::Empty)
                }

                n = entries
            }
        }

        let cons_next = cons_head.wrapping_add(n);
        self.cons.head.store(cons_next as _, Ordering::Relaxed);

        /* copy in table */
        let ret = self.dequeue_objs(cons_head, mask, n, obj_table);
        smp_rmb();

        self.cons.tail.store(cons_next as _, Ordering::Relaxed);
        return Ok((n as _, ret))
    }
}

impl<T: Send, P, C> Ring<T, P, C> {

    #[inline(always)]
    unsafe fn enqueue_objs<I>(&self, prod_head: u32, mask: u32, n: u32, objs: I)
    where I: ExactSizeIterator<Item=T> {
        let size: u32 = self.ring.size();
        let mut idx: u32 = prod_head & mask;
        //FIXME check branchiness
        for obj in objs.take(n as _) {
            //FIXME check if this adds too many branches
            if idx >= size { idx = 0 }
            self.ring.write(idx, obj);
            idx += 1;
        }
    }

    #[inline(always)]
    fn dequeue_objs<'a, 's: 'a, F, R>(
        &'s self, cons_head: u32, mask: u32, n: u32, objs: F
    ) -> R
    where F: FnOnce(DequeIter<'a, T>) -> R {
        let idx: u32 = cons_head & mask;
        let size: u32 = self.ring.size();
        //TODO does <= work as well as < ?
        if idx + n < size {
            objs(DequeIter::Contig(self.ring.slice(idx, n)))
        }
        else {
            let split = size - idx;
            let rem = n - split;
            let (before_split, after_split) = self.ring.split(idx, split, rem);
            objs(DequeIter::Split(before_split, after_split))
        }
    }

    // TODO move issues...
    #[cfg(False)]
    #[inline(always)]
    fn enqueue_actual(&self, prod_head: u32, mask: u32, obj_table: &[T], n: u32) {
        // This is a translation of ENQUEUE_PTRS made slightly ugly due to lack of stepsize or switch
        let size: u32 = self.prod.size;
        let mut idx: u32 = prod_head & mask;
        if mem::size_of::<T>() <= mem::size_of::<*mut u8>() {
            if idx + (n as u32) < size { //likely
                let mut i = 0;
                'unrolled: loop {
                    if i >= (n & !0x3) { break 'unrolled }
                    self.ring.write(idx, obj_table[i]);
                    self.ring.write(idx+1, obj_table[i+1]);
                    self.ring.write(idx+2, obj_table[i+2]);
                    self.ring.write(idx+3, obj_table[i+3]);
                    i += 4;
                    idx += 4;
                }
                'l1: loop {
                    'l2: loop {
                        match n & 3 {
                            3 => {
                                self.ring.write(idx, obj_table[i]);
                                idx +=1; i +=1;
                            }
                            2 => {}
                            1 => break 'l2,
                            _ => break 'l1,
                        }
                        self.ring.write(idx, obj_table[i]);
                        idx +=1; i +=1;
                        break 'l2
                    }
                    self.ring.write(idx, obj_table[i]);
                    idx +=1; i +=1;
                }
            } else {
                let mut i = 0;
                'tail: loop {
                    if idx >= size { break 'tail }
                    self.ring.write(idx, obj_table[i]);
                    i += 1;
                    idx += 1;
                }
                idx = 0;
                'head: loop {
                    if i >= n { break 'head }
                    self.ring.write(idx, obj_table[i]);
                    i += 1;
                    idx += 1;
                }
            }
        } else {
            let mut i = 0;
            'slow_tail: loop {
                if idx >= size { break 'slow_tail }
                self.ring.write(idx, obj_table[i]);
                i += 1;
                idx += 1;
            }
            idx = 0;
            'slow_head: loop {
                if i >= n { break 'slow_head }
                self.ring.write(idx, obj_table[i]);
                i += 1;
                idx += 1;
            }
        }
    }
}

pub enum DequeIter<'a, T: 'a> {
    Contig(&'a [ManuallyDrop<UnsafeCell<T>>]),
    Split(&'a [ManuallyDrop<UnsafeCell<T>>], &'a [ManuallyDrop<UnsafeCell<T>>]),
}

impl<'a, T> Iterator for DequeIter<'a, T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        use DequeIter::*;
        let (ret, remaining) = match self {
            &mut Contig(ref mut data) => //likely
                return if data.is_empty() { None }
                else {
                    let ret = unsafe {ptr::read(data[0].get())};
                    *data = &data[1..];
                    Some(ret)
                },
            &mut Split(ref mut before_split, ref mut after_split) => {
                let ret = unsafe {ptr::read(before_split[0].get())};
                *before_split = &before_split[1..];
                if !before_split.is_empty() {
                    return Some(ret)
                }
                (ret, *after_split)
            },
        };
        *self = DequeIter::Contig(remaining);
        Some(ret)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        use DequeIter::*;
        match self {
            &Contig(data) => (data.len(), Some(data.len())),
            &Split(before_split, after_split) => {
                let len = before_split.len() + after_split.len();
                (len, Some(len))
            },
        }
    }
}

impl<'a, T> std::iter::ExactSizeIterator for DequeIter<'a, T> {}

impl<T> Data<T> {
    #[inline(always)]
    unsafe fn write(&self, index: u32, data: T) {
        ptr::write(self.0[index as usize].get(), data)
    }

    #[inline(always)]
    fn slice(&self, index: u32, len: u32) -> &[ManuallyDrop<UnsafeCell<T>>] {
        &self.0[index as usize..index as usize + len as usize]
    }

    fn split(&self, index: u32, before_split: u32, after_split: u32)
    -> (&[ManuallyDrop<UnsafeCell<T>>], &[ManuallyDrop<UnsafeCell<T>>]) {
        let first_end = index as usize + before_split as usize;
        (&self.0[index as usize..first_end], &self.0[..after_split as usize])
    }

    #[inline(always)]
    fn size(&self) -> u32 {
        self.0.len() as u32
    }
}

//FIXME correct ordering
#[inline(always)]
fn smp_wmb() { atomic::fence(Ordering::Release) }

#[inline(always)]
fn smp_rmb() { atomic::fence(Ordering::Acquire) }

//FIXME use actual pause
//from parking_lot
#[inline(always)]
fn pause() {
    #[cfg(feature = "nightly")]
    {
        ::std::sync::atomic::hint_core_should_pause()
    }
    #[cfg(not(feature = "nightly"))]
    {
        for _ in 0..(4<<4) {
            atomic::fence(Ordering::SeqCst);
        }
    }
}

//TODO it might pay to split the ring implementation into sender/receiver parts
//     so we could use the one Sender<T, P> for both Receiver<T, MC> and Receiver<T, SC>
pub struct Sender<T: Send, P>(Arc<Ring<T, P, Unknown>>);
pub struct Receiver<T: Send, C>(Arc<Ring<T, Unknown, C>>);

unsafe impl<T: Send, P> Send for Sender<T, P> {}
unsafe impl<T: Send, C> Send for Receiver<T, C> {}

unsafe impl<T: Send> Sync for Sender<T, MP> {}
unsafe impl<T: Send> Sync for Receiver<T, MC> {}

impl<T: Send> Clone for Sender<T, MP> {
        fn clone(&self) -> Self {
            //TODO refcount?
            let &Sender(ref a) = self;
            Sender(a.clone())
    }
}

impl<T: Send> Clone for Receiver<T, MC> {
        fn clone(&self) -> Self {
            //TODO refcount?
            let &Receiver(ref a) = self;
            Receiver(a.clone())
    }
}

impl<T: Send, P> Sender<T, P> {
    pub fn to_multi(self) -> Sender<T, MP> {
        unsafe { mem::transmute(self) }
    }

    pub fn free_count(&self) -> u32 {
        self.0.free_count()
    }

    pub fn used_count(&self) -> u32 {
        self.0.used_count()
    }

    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: Send, C> Receiver<T, C> {
    pub fn to_multi(self) -> Receiver<T, MC> {
        unsafe { mem::transmute(self) }
    }

    pub fn free_count(&self) -> u32 {
        self.0.free_count()
    }

    pub fn used_count(&self) -> u32 {
        self.0.used_count()
    }

    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

macro_rules! impl_sender {
    ($($multi:ident),*) => {
        $(impl<T: Send> Sender<T, $multi> {
            pub fn enqueue_burst<I>(&self, obj_table: I, all_or_none: bool)
            -> Result<usize, EnqueueErr>
            where I: ExactSizeIterator<Item=T> {
                self.0.enqueue(obj_table, all_or_none)
            }

            pub fn try_send(&self, t: T) -> Result<(), EnqueueErr> {
                    self.enqueue_burst(::std::iter::once(t), true).map(|_| ())
            }

            pub fn send_from_slice(&self, data: &[T]) -> Result<usize, EnqueueErr>
            where T: Copy {
                self.enqueue_burst(data.iter().cloned(), false)
            }

            pub fn send_from_option_slice(&self, data: &mut [Option<T>])
            -> Result<usize, EnqueueErr> {
                let size = data.iter().take_while(|o| o.is_some()).count();
                self.enqueue_burst(data[..size].iter_mut().map(|o|o.take().unwrap()), false)
            }

            pub fn send_all(&self, data: &mut Vec<T>) -> Result<usize, EnqueueErr>
            where T: Copy {
                self.enqueue_burst(data.drain(..), false)
            }
        })*
    };
}

macro_rules! impl_receiver {
    ($($multi:ident),*) => {
        $(impl<T: Send> Receiver<T, $multi> {
            pub fn dequeue_burst<'a, 's: 'a, F, R>(
                &'s self, max: u32, all_or_none: bool, obj_table: F
            ) -> Result<(usize, R), DequeueErr>
            where F: FnOnce(DequeIter<'a, T>) -> R {
                self.0.dequeue(max, all_or_none, obj_table)
            }

            pub fn try_recv(&self) -> Result<T, DequeueErr> {
                self.dequeue_burst(1, true, |mut i| i.next().unwrap()).map(|(_, t)| t)
            }

            pub fn recv_to_slice(&self, slice: &mut [T]) -> Result<usize, DequeueErr>
            where T: Copy {
                let size = slice.len();
                self.dequeue_burst(size as u32, false, |i|
                    for (i, o) in i.enumerate() { slice[i] = o }
                ).map(|(s, _)| s)
            }

            pub fn recv_to_option_slice(&self, slice: &mut [Option<T>])
            -> Result<usize, DequeueErr>
            where T: Copy {
                let size = slice.iter().take_while(|o| o.is_none()).count();
                self.dequeue_burst(size as u32, false, |i|
                    for (i, o) in i.enumerate() { slice[i] = Some(o) }
                ).map(|(s, _)| s)
            }

            pub fn recv_burst(&self, vec: &mut Vec<T>, burst_size: u32)
            -> Result<usize, DequeueErr>
            where T: Copy {
                if vec.capacity() - vec.len() < burst_size as usize {
                    let additional = burst_size as usize - (vec.capacity() - vec.len());
                    vec.reserve_exact(additional)
                }
                self.dequeue_burst(burst_size as u32, false, |i|
                    for o in i { vec.push(o) }
                ).map(|(s, _)| s)
            }
        })*
    };
}

impl_sender!(MP, SP);
impl_receiver!(MC, SC);

macro_rules! fn_channel {
    () =>
        (pub fn channel<T: Send>(count: u32) -> Result<(Sender<T>, Receiver<T>), ()> {
            let ring = ::Ring::with_capacity(count)?;
            let (send, recv) = ring.split();
            Ok((::Sender(send), ::Receiver(recv)))
        });
}

pub mod mpmc {
    pub type Sender<T> = ::Sender<T, ::MP>;
    pub type Receiver<T> = ::Receiver<T, ::MC>;

    fn_channel!();
}

pub mod spmc {
    pub type Sender<T> = ::Sender<T, ::SP>;
    pub type Receiver<T> = ::Receiver<T, ::MC>;

    fn_channel!();
}

pub mod mpsc {
    pub type Sender<T> = ::Sender<T, ::MP>;
    pub type Receiver<T> = ::Receiver<T, ::SC>;

    fn_channel!();
}

pub mod spsc {
    pub type Sender<T> = ::Sender<T, ::SP>;
    pub type Receiver<T> = ::Receiver<T, ::SC>;

    fn_channel!();
}

#[cfg(False)]
#[test]
fn align() {
    use std::mem;
    assert!(mem::align_of::<Data<*mut u8>>() >= mem::align_of::<Data<*mut u8>>());
    assert!(mem::align_of::<Data<*mut u8>>() >= mem::align_of::<CacheAlign>());
        assert_eq!(mem::align_of::<Prod<()>>(), 64);
}
