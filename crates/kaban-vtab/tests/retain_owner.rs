//! `retain_owner` (spec 11 PC-8): the attached owner lives exactly as long as the bytes that
//! carry it, clones and slices included, and the bytes are the input's own.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use kaban_vtab::retain_owner;
use ownedbytes::OwnedBytes;

/// An owner that sets its flag when it is dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// An owner and the flag its drop sets.
fn owner() -> (DropFlag, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    (DropFlag(Arc::clone(&dropped)), dropped)
}

#[test]
fn the_owner_is_dropped_only_after_the_last_clone_and_slice() {
    // Arrange
    let (owner, dropped) = owner();
    let bytes = retain_owner(OwnedBytes::new(b"0123456789".to_vec()), owner);
    let clone = bytes.clone();
    let slice = bytes.slice(2..5);

    // Act / Assert: each holder keeps the owner alive
    drop(bytes);
    assert!(
        !dropped.load(Ordering::Acquire),
        "the owner was dropped while a clone and a slice lived"
    );
    drop(clone);
    assert!(
        !dropped.load(Ordering::Acquire),
        "the owner was dropped while a slice lived"
    );
    assert_eq!(slice.as_slice(), b"234");
    drop(slice);
    assert!(
        dropped.load(Ordering::Acquire),
        "the owner outlived the last holder"
    );
}

#[test]
fn the_returned_bytes_are_the_input_bytes_uncopied() {
    // Arrange
    let input = OwnedBytes::new((0..=255_u8).collect::<Vec<_>>());

    // Act
    let bytes = retain_owner(input.clone(), ());

    // Assert
    assert_eq!(bytes.as_slice(), input.as_slice());
    assert_eq!(bytes.as_slice().as_ptr(), input.as_slice().as_ptr());
}

#[test]
fn dropping_the_last_holder_on_another_thread_releases_the_owner() {
    // Arrange: only a slice is left, handed to another thread
    let (owner, dropped) = owner();
    let bytes = retain_owner(OwnedBytes::new(vec![7_u8; 64]), owner);
    let slice = bytes.slice(8..16);
    drop(bytes);
    assert!(
        !dropped.load(Ordering::Acquire),
        "the owner was dropped while a slice lived"
    );
    let (done, finished) = channel();

    // Act
    std::thread::spawn(move || {
        drop(slice);
        done.send(()).unwrap();
    });

    // Assert
    finished
        .recv_timeout(Duration::from_secs(5))
        .expect("the other thread did not drop the slice within 5 s");
    assert!(
        dropped.load(Ordering::Acquire),
        "the owner outlived the last holder dropped on another thread"
    );
}
