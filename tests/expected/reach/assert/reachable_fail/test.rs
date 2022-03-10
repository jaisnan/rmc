// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// kani-flags: --assertion-reach-checks --output-format regular --no-default-checks

fn main() {
    let x = 5;
    if kani::any() {
        assert!(x != 5);
    }
}
