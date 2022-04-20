// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// kani-verify-fail

// Check that `unchecked_sub` triggers overflow checks.
#![feature(core_intrinsics)]

#[kani::proof]
fn main() {
    let a: i32 = kani::any();
    let b: i32 = kani::any();
    unsafe { std::intrinsics::unchecked_sub(a, b) };
}
