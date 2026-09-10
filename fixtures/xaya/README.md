# Xaya publication fixture

`xaya_monitor_evidence.csv` retains the complete canonical row for child height
901 from Research revision `e09f52b11c207a6a226ea5531e093ab80ca6a5fd`.
The source artifact is `results/monitor-evidence/xaya_monitor_evidence.csv`,
SHA-256 `caefbd64b03ab3d5578ec485a8c3713fb4b935b03152c91778c781f02bfb8ea0`.

All 20,841 rows in that pinned artifact have an 80-byte child header with zero
header `nBits` and a non-zero external `child_nbits`. The fixture checks that
the importer accepts the real representation and preserves its parent-work
verdict. Synthetic fixtures cover contradictory and missing fields.
