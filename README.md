## formal-os – build & run

This project uses bootloader v0.9 + bootimage to build a bootable disk image
and runs on QEMU as a BIOS-boot x86_64 environment.

### Build requirements
```bash
rustup component add rust-src
cargo install bootimage
```

### Build (kernel + bootloader)
```bash
cargo bootimage -p kernel --target x86_64-formal-os-local.json
```

The bootable image is generated at:

```bash
target/x86_64-formal-os-local/debug/bootimage-kernel.bin
```

### Run on QEMU
```bash
qemu-system-x86_64 \
-drive format=raw,file=target/x86_64-formal-os-local/debug/bootimage-kernel.bin \
-m 512M \
-serial stdio
```

### Automated scripts

For convenience:
```bash
scripts/build-kernel.sh
scripts/run-qemu-debug.sh
```