# embedded-sdmmc 0.8.2 (local patch)

Upstream: <https://github.com/rust-embedded-community/embedded-sdmmc-rs> tag `v0.8.2`.

License: MIT OR Apache-2.0. See `LICENSE-MIT` and `LICENSE-APACHE`.

Two behaviour changes, both for volumes produced by `mkfs.vfat -F 32`:

- A volume with `BPB_FATSz16 == 0`, a non-zero `BPB_FATSz32`, and a zero root-entry count is FAT32 even when the cluster count is below 65525. Upstream classifies by cluster count alone, then treats the root directory as a zero-length FAT16 root, so creating a file returns `NotEnoughSpace`.
- A FAT32 info sector whose signatures do not match is treated as "free cluster count unknown". Mounting continues and cluster allocation scans the FAT.
