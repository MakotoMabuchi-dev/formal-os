/* formal-os/linker.ld */

OUTPUT_FORMAT("elf64-x86-64")
ENTRY(_start)

/* 2つの LOAD セグメントだけを作る */
PHDRS {
    text PT_LOAD FLAGS(5); /* R X */
    data PT_LOAD FLAGS(6); /* R W */
}

SECTIONS {
    /* カーネルを物理 1MiB から配置（典型的な場所） */
    . = 1M;

    /* コード + 読み取り専用データ */
    .text ALIGN(4K) : {
        *(.text .text.*)
        *(.rodata .rodata.*)
    } :text

    /* 書き換え可能なデータ類は別セグメントにして 4KiB アライン */
    .data ALIGN(4K) : {
        *(.data .data.*)
        *(.bss .bss.*)
        *(.got .got.*)
        *(COMMON)
    } :data

    /* 例外テーブルなど不要なものは捨てる */
    /DISCARD/ : {
        *(.eh_frame)
        *(.note .note.*)
    }
}
