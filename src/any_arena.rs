// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cell::{Cell, RefCell};
use std::cmp;
use std::intrinsics;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::slice;

use alloc::heap;
use alloc::raw_vec::RawVec;

/// A slower reflection-based arena that can allocate objects of any type.
///
/// This arena uses `RawVec<u8>` as a backing store to allocate objects from.
/// For each allocated object, the arena stores a pointer to the type descriptor
/// followed by the object (potentially with alignment padding after each
/// element). When the arena is destroyed, it iterates through all of its
/// chunks, and uses the tydesc information to trace through the objects,
/// calling the destructors on them. One subtle point that needs to be
/// addressed is how to handle panics while running the user provided
/// initializer function. It is important to not run the destructor on
/// uninitialized objects, but how to detect them is somewhat subtle. Since
/// `alloc()` can be invoked recursively, it is not sufficient to simply exclude
/// the most recent object. To solve this without requiring extra space, we
/// use the low order bit of the tydesc pointer to encode whether the object
/// it describes has been fully initialized.
///
/// As an optimization, objects with destructors are stored in different chunks
/// than objects without destructors. This reduces overhead when initializing
/// plain-old-data (`Copy` types) and means we don't need to waste time running
/// their destructors.
pub struct AnyArena<'longer_than_self> {
    // The heads are separated out from the list as a unbenchmarked
    // microoptimization, to avoid needing to case on the list to access a head.
    head: RefCell<Chunk>,
    copy_head: RefCell<Chunk>,
    chunks: RefCell<Vec<Chunk>>,
    _marker: PhantomData<*mut &'longer_than_self ()>,
}

impl<'longer_than_self> AnyArena<'longer_than_self> {
    /// Allocates a new AnyArena with 32 bytes preallocated.
    pub fn new() -> AnyArena<'longer_than_self> {
        AnyArena::new_with_size(32)
    }

    /// Allocates a new AnyArena with `initial_size` bytes preallocated.
    pub fn new_with_size(initial_size: usize) -> AnyArena<'longer_than_self> {
        AnyArena {
            head: RefCell::new(Chunk::new(initial_size, false)),
            copy_head: RefCell::new(Chunk::new(initial_size, true)),
            chunks: RefCell::new(Vec::new()),
            _marker: PhantomData,
        }
    }

    /// Allocates a new item in the arena, using `op` to initialize the value,
    /// and returns a reference to it.
    #[inline]
    pub fn alloc<T: 'longer_than_self, F>(&self, op: F) -> &mut T
        where F: FnOnce() -> T
    {
        unsafe {
            if intrinsics::needs_drop::<T>() {
                self.alloc_noncopy(op)
            } else {
                self.alloc_copy(op)
            }
        }
    }

    // Functions for the copyable part of the arena.

    #[inline]
    fn alloc_copy<T, F>(&self, op: F) -> &mut T
        where F: FnOnce() -> T
    {
        unsafe {
            let ptr = self.alloc_copy_inner(mem::size_of::<T>(), mem::align_of::<T>());
            let ptr = ptr as *mut T;
            ptr::write(&mut (*ptr), op());
            &mut *ptr
        }
    }

    #[inline]
    fn alloc_copy_inner(&self, n_bytes: usize, align: usize) -> *const u8 {
        let mut copy_head = self.copy_head.borrow_mut();
        let fill = copy_head.fill.get();
        let mut start = round_up(fill, align);
        let mut end = start + n_bytes;

        if end > copy_head.capacity() {
            if self.alloc_grow(&mut *copy_head, fill, end - fill) {
                // Continuing with a newly allocated chunk
                start = 0;
                end = n_bytes;
                copy_head.is_copy.set(true);
            }
        }

        copy_head.fill.set(end);

        unsafe { copy_head.as_ptr().offset(start as isize) }
    }

    // Functions for the non-copyable part of the arena.

    #[inline]
    fn alloc_noncopy<T, F>(&self, op: F) -> &mut T
        where F: FnOnce() -> T
    {
        unsafe {
            let tydesc = get_tydesc::<T>();
            let (ty_ptr, ptr) = self.alloc_noncopy_inner(mem::size_of::<T>(), mem::align_of::<T>());
            let ty_ptr = ty_ptr as *mut usize;
            let ptr = ptr as *mut T;
            // Write in our tydesc along with a bit indicating that it
            // has *not* been initialized yet.
            *ty_ptr = bitpack_tydesc_ptr(tydesc, false);
            // Actually initialize it
            ptr::write(&mut (*ptr), op());
            // Now that we are done, update the tydesc to indicate that
            // the object is there.
            *ty_ptr = bitpack_tydesc_ptr(tydesc, true);

            &mut *ptr
        }
    }

    #[inline]
    fn alloc_noncopy_inner(&self, n_bytes: usize, align: usize) -> (*const u8, *const u8) {
        let mut head = self.head.borrow_mut();
        let fill = head.fill.get();

        let mut tydesc_start = fill;
        let after_tydesc = fill + mem::size_of::<*const TyDesc>();
        let mut start = round_up(after_tydesc, align);
        let mut end = round_up(start + n_bytes, mem::align_of::<*const TyDesc>());

        if end > head.capacity() {
            if self.alloc_grow(&mut *head, tydesc_start, end - tydesc_start) {
                // Continuing with a newly allocated chunk
                tydesc_start = 0;
                start = round_up(mem::size_of::<*const TyDesc>(), align);
                end = round_up(start + n_bytes, mem::align_of::<*const TyDesc>());
            }
        }

        head.fill.set(end);

        unsafe {
            let buf = head.as_ptr();
            (buf.offset(tydesc_start as isize),
             buf.offset(start as isize))
        }
    }

    // Grows a given chunk and returns `false`, or replaces it with a bigger
    // chunk and returns `true`.
    // This method is shared by both parts of the arena.
    #[cold]
    fn alloc_grow(&self, head: &mut Chunk, used_cap: usize, n_bytes: usize) -> bool {
        if head.data.reserve_in_place(used_cap, n_bytes) {
            // In-place reallocation succeeded.
            false
        } else {
            // Allocate a new chunk.
            let new_min_chunk_size = cmp::max(n_bytes, head.capacity());
            let new_chunk = Chunk::new((new_min_chunk_size + 1).next_power_of_two(), false);
            let old_chunk = mem::replace(head, new_chunk);
            if old_chunk.fill.get() != 0 {
                self.chunks.borrow_mut().push(old_chunk);
            }
            true
        }
    }

    /// Allocates a slice of bytes of requested length. The bytes are not guaranteed to be zero
    /// if the arena has previously been cleared.
    ///
    /// # Panics
    ///
    /// Panics if the requested length is too large and causes overflow.
    pub fn alloc_bytes(&self, len: usize) -> &mut [u8] {
        unsafe {
            // Check for overflow.
            self.copy_head.borrow().fill.get().checked_add(len).expect("length overflow");
            let ptr = self.alloc_copy_inner(len, 1);
            intrinsics::assume(!ptr.is_null());
            slice::from_raw_parts_mut(ptr as *mut _, len)
        }
    }

    /// Clears the arena. Deallocates all but the longest chunk which may be reused.
    pub fn clear(&mut self) {
        unsafe {
            self.head.borrow().destroy();
            self.head.borrow().fill.set(0);
            self.copy_head.borrow().fill.set(0);
            for chunk in self.chunks.borrow().iter() {
                if !chunk.is_copy.get() {
                    chunk.destroy();
                }
            }
            self.chunks.borrow_mut().clear();
        }
    }
}

impl<'longer_than_self> Drop for AnyArena<'longer_than_self> {
    fn drop(&mut self) {
        unsafe {
            self.head.borrow().destroy();
            for chunk in self.chunks.borrow().iter() {
                if !chunk.is_copy.get() {
                    chunk.destroy();
                }
            }
        }
    }
}

struct Chunk {
    data: RawVec<u8>,
    /// Index of the first unused byte.
    fill: Cell<usize>,
    /// Indicates whether objects with destructors are stored in this chunk.
    is_copy: Cell<bool>,
}

impl Chunk {
    fn new(size: usize, is_copy: bool) -> Chunk {
        Chunk {
            data: RawVec::with_capacity(size),
            fill: Cell::new(0),
            is_copy: Cell::new(is_copy),
        }
    }

    fn capacity(&self) -> usize {
        self.data.cap()
    }

    unsafe fn as_ptr(&self) -> *const u8 {
        self.data.ptr()
    }

    // Walk down a chunk, running the destructors for any objects stored
    // in it.
    unsafe fn destroy(&self) {
        let mut idx = 0;
        let buf = self.as_ptr();
        let fill = self.fill.get();

        while idx < fill {
            let tydesc_data = buf.offset(idx as isize) as *const usize;
            let (tydesc, is_done) = un_bitpack_tydesc_ptr(*tydesc_data);
            let (size, align) = ((*tydesc).size, (*tydesc).align);

            let after_tydesc = idx + mem::size_of::<*const TyDesc>();

            let start = round_up(after_tydesc, align);

            if is_done {
                ((*tydesc).drop_glue)(buf.offset(start as isize) as *const i8);
            }

            // Find where the next tydesc lives
            idx = round_up(start + size, mem::align_of::<*const TyDesc>());
        }
    }
}

#[inline]
fn round_up(base: usize, align: usize) -> usize {
    (base.checked_add(align - 1)).unwrap() & !(align - 1)
}

// HACK(eddyb) TyDesc replacement using a trait object vtable.
// This could be replaced in the future with a custom DST layout,
// or `&'static (drop_glue, size, align)` created by a `const fn`.
// Requirements:
// * rvalue promotion (issue #1056)
// * mem::{size_of, align_of} must be const fns
struct TyDesc {
    drop_glue: fn(*const i8),
    size: usize,
    align: usize,
}

unsafe fn get_tydesc<T>() -> *const TyDesc {
    use std::raw::TraitObject;

    let ptr = &*(heap::EMPTY as *const T);

    // Can use any trait that is implemented for all types.
    let obj = mem::transmute::<&AllTypes, TraitObject>(ptr);
    obj.vtable as *const TyDesc
}

// We encode whether the object a tydesc describes has been
// initialized in the arena in the low bit of the tydesc pointer. This
// is necessary in order to properly do cleanup if a panic occurs
// during an initializer.
#[inline]
fn bitpack_tydesc_ptr(p: *const TyDesc, is_done: bool) -> usize {
    p as usize | (is_done as usize)
}
#[inline]
fn un_bitpack_tydesc_ptr(p: usize) -> (*const TyDesc, bool) {
    ((p & !1) as *const TyDesc, p & 1 == 1)
}

trait AllTypes {
    fn dummy(&self) {}
}

impl<T: ?Sized> AllTypes for T {}
