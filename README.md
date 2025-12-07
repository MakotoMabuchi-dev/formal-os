# formal-os

A minimal x86_64 OS kernel written in Rust.  
Boots successfully on QEMU using a custom target specification and the `bootloader` crate.

## Features
- Custom target (`x86_64-formal-os.json`)
- Rust `no_std` + `no_main` environment
- Bootloader integration (bootimage)
- Runs on QEMU (macOS native)

## How to build

```bash
rustup toolchain install nightly
rustup +nightly target add x86_64-unknown-none
rustup +nightly component add llvm-tools-preview rust-src

cargo +nightly bootimage
```

## How to run

```bash
qemu-system-x86_64 \
  -drive format=raw,file=target/x86_64-formal-os/debug/bootimage-kernel.bin \
  -m 512M \
  -serial stdio
```
