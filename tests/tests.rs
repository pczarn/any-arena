// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![feature(test)]

extern crate any_arena;
extern crate test;

use std::cell::Cell;
use std::rc::Rc;
use self::test::Bencher;

use any_arena::AnyArena;

#[allow(dead_code)]
#[derive(Debug, Eq, PartialEq)]
struct Point {
    x: i32,
    y: i32,
    z: i32,
}

#[bench]
pub fn bench_arena_copy(b: &mut Bencher) {
    let arena = AnyArena::new();
    b.iter(|| arena.alloc(|| Point { x: 1, y: 2, z: 3 }))
}

#[allow(dead_code)]
struct Noncopy {
    string: String,
    array: Vec<i32>,
}

#[test]
pub fn test_arena_zero_sized() {
    let arena = AnyArena::new();
    let mut points = vec![];
    for _ in 0..1000 {
        for _ in 0..100 {
            arena.alloc(|| ());
        }
        let point = arena.alloc(|| Point { x: 1, y: 2, z: 3 });
        points.push(point);
    }
    for point in &points {
        assert_eq!(**point, Point { x: 1, y: 2, z: 3 });
    }
}

#[test]
pub fn test_arena_clear() {
    let mut arena = AnyArena::new();
    for _ in 0..10 {
        arena.clear();
        for _ in 0..10000 {
            arena.alloc(|| Point { x: 1, y: 2, z: 3 });
            arena.alloc(|| {
                Noncopy {
                    string: "hello world".to_string(),
                    array: vec![],
                }
            });
        }
    }
}

#[test]
pub fn test_arena_alloc_bytes() {
    let arena = AnyArena::new();
    for i in 0..10000 {
        arena.alloc(|| Point { x: 1, y: 2, z: 3 });
        for byte in arena.alloc_bytes(i % 42).iter_mut() {
            *byte = i as u8;
        }
    }
}

#[test]
fn test_arena_destructors() {
    let arena = AnyArena::new();
    for i in 0..10 {
        // AnyArena allocate something with drop glue to make sure it
        // doesn't leak.
        arena.alloc(|| Rc::new(i));
        // Allocate something with funny size and alignment, to keep
        // things interesting.
        arena.alloc(|| [0u8, 1u8, 2u8]);
    }
}

#[test]
#[should_panic]
fn test_arena_destructors_fail() {
    let arena = AnyArena::new();
    // Put some stuff in the arena.
    for i in 0..10 {
        // AnyArena allocate something with drop glue to make sure it
        // doesn't leak.
        arena.alloc(|| Rc::new(i));
        // Allocate something with funny size and alignment, to keep
        // things interesting.
        arena.alloc(|| [0u8, 1, 2]);
    }
    // Now, panic while allocating
    arena.alloc::<Rc<i32>, _>(|| {
        panic!();
    });
}

// Drop tests

struct DropCounter<'a> {
    count: &'a Cell<u32>,
}

impl<'a> Drop for DropCounter<'a> {
    fn drop(&mut self) {
        self.count.set(self.count.get() + 1);
    }
}

#[test]
fn test_arena_drop_count() {
    let counter = Cell::new(0);
    {
        let arena = AnyArena::new();
        for _ in 0..100 {
            // Allocate something with drop glue to make sure it doesn't leak.
            arena.alloc(|| DropCounter { count: &counter });
            // Allocate something with funny size and alignment, to keep
            // things interesting.
            arena.alloc(|| [0u8, 1u8, 2u8]);
        }
        // dropping
    };
    assert_eq!(counter.get(), 100);
}

#[test]
fn test_arena_drop_on_clear() {
    let counter = Cell::new(0);
    for i in 0..10 {
        let mut arena = AnyArena::new();
        for _ in 0..100 {
            // Allocate something with drop glue to make sure it doesn't leak.
            arena.alloc(|| DropCounter { count: &counter });
            // Allocate something with funny size and alignment, to keep
            // things interesting.
            arena.alloc(|| [0u8, 1u8, 2u8]);
        }
        arena.clear();
        assert_eq!(counter.get(), i * 100 + 100);
    }
}

thread_local! {
    static DROP_COUNTER: Cell<u32> = Cell::new(0)
}

struct SmallDroppable;

impl Drop for SmallDroppable {
    fn drop(&mut self) {
        DROP_COUNTER.with(|c| c.set(c.get() + 1));
    }
}

#[test]
fn test_arena_drop_small_count() {
    DROP_COUNTER.with(|c| c.set(0));
    {
        let arena = AnyArena::new();
        for _ in 0..10 {
            for _ in 0..10 {
                // Allocate something with drop glue to make sure it doesn't leak.
                arena.alloc(|| SmallDroppable);
            }
            // Allocate something with funny size and alignment, to keep
            // things interesting.
            arena.alloc(|| [0u8, 1u8, 2u8]);
        }
        // dropping
    };
    assert_eq!(DROP_COUNTER.with(|c| c.get()), 100);
}

#[bench]
pub fn bench_arena_noncopy(b: &mut Bencher) {
    let arena = AnyArena::new();
    b.iter(|| {
        arena.alloc(|| {
            Noncopy {
                string: "hello world".to_string(),
                array: vec![1, 2, 3, 4, 5],
            }
        })
    })
}
