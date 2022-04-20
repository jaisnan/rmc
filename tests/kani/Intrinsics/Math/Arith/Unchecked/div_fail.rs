// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// kani-verify-fail

// Check that `unchecked_div` triggers overflow checks.
// Covers the case where `a == T::MIN && b == -1`.
#![feature(core_intrinsics)]

#[kani::proof]
fn main() {
    let a: i32 = i32::MIN;
    let b: i32 = -1;
    unsafe { std::intrinsics::unchecked_div(a, b) };
}
