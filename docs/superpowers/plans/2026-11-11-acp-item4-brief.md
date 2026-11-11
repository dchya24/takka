# Brief — Item 4: laporan implementasi T-03/T-04 + bukti konformansi ACP v1 (docs lane)

Repo: `/Works/Development/self-project/agentic` (branch `feature/acp-sessions-t03-t07`).
**Lingkup: HANYA file di `docs/superpowers/plans/` dan `scripts/`.**
JANGAN menyentuh file Rust mana pun (`takka-core/`, `takka-runtime/`, `takka-cli/`) —
lane lain sedang mengompilasi/mengubahnya. JANGAN commit, JANGAN patch `.github/`.

Keluaran: dua file dokumen + satu skrip konformansi.

## 1. `docs/superpowers/plans/2026-11-11-acp-t03-t04-report.md`

Subagent lane T-03/T-04 sebelumnya berhenti sebelum menulis laporannya. Rekonstruksi
laporan itu dari **bukti kode**, bukan dari ingatan. Bahan baca:

- `docs/superpowers/specs/2026-11-10-takka-acp-agent-design.md` (D6/D7/D9/D11, §5,
  §6 baris T-03/T-04)
- `docs/superpowers/plans/2026-11-11-acp-t03-t04-brief.md` (apa yang diminta)
- `takka-runtime/src/acp.rs` (implementasi akhirnya: `AcpConfig`, `serve_over`,
  `AcpService`, registry sesi, `session_new`, `session_prompt`, `boot_engine`,
  `prompt_content`, `session_mode_state`)
- Test yang relevan di `mod tests` `acp.rs` (nama test T-03/T-04) dan
  `takka-runtime/tests/acp_smoke.rs`
- `git log -1 --stat` untuk daftar file yang berubah di commit `beb052e`

Format: tabel "apa yang diminta vs apa yang diimplementasikan vs bukti (nama test)",
keputusan desain yang terlihat di kode, hasil `cargo test -p takka-runtime` (jalankan
sendiri: `timeout 600 cargo test -p takka-runtime 2>&1 | tail -20` — read-only, aman),
deviasi dari brief, kekhawatiran. Nyatakan eksplisit: "dokumen ini direkonstruksi
setelah lane T-03/T-04 selesai; kode dan test adalah sumber kebenarannya."

## 2. `scripts/acp-conformance.sh` + hasil run

Spec §6 menyebut "Zed ACP Test Compatibility Kit". Cek dulu apakah kit itu benar-benar
tersedia (npm `@zed-industries/agent-client-protocol-tck`, crate
`agent-client-protocol-tck`) — sudah dicoba dan **keduanya tidak ada** di registry;
kalau memang tidak tersedia, JANGAN memaksakan install pihak ketiga. Ganti dengan
skrip konformansi buatan sendiri yang menguji kontrak v1 secara end-to-end terhadap
binary asli.

Skrip (POSIX `sh`, boleh `bash` shebang `#!/usr/bin/env bash`, `set -eu`):

- Bangun binary: `cargo build -p takka-runtime` (jangan `cargo install`).
- Siapkan config hermetik di `mktemp` dir: provider `openai-compatible`,
  `api_base: http://127.0.0.1:9`, api_key dummy, satu model. Tidak boleh menyentuh
  `~/.config/takka/config.json`.
- Jalankan `target/debug/takka-runtime --acp` dengan stdin/stdout pipa, env
  `TAKKA_TEST_ACP_CONFIG=<tmp>/config.json` (pin test-only yang sudah ada di
  `serve()`), lalu kirim frame JSONL berurutan dan periksa balasan:
  1. `initialize` (protocolVersion 2) → `protocolVersion: 1`, `agentCapabilities`,
     `agentInfo.name == "takka"`.
  2. `session/new` dengan `cwd` = direktori kerja proses → `sessionId`,
     `modes.currentModeId == "default"`, `availableModes` berisi
     `default|plan|yolo`.
  3. `session/new` dengan `cwd` berbeda (mis. `/`) → error dengan data yang memuat
     "restart the agent from project directory".
  4. `session/prompt` pada sesi valid → selesai cepat dengan `error` (turn gagal
     bersih karena provider unreachable) ATAU `result.stopReason` — terima yang
     mana pun yang jujur, tapi WAJIB tidak menggantung (pakai timeout per langkah).
  5. `session/prompt` kedua pada sesi yang sama → tetap dijawab (tidak poisoned).
  6. method tak dikenal → `-32601`.
  7. tutup stdin → proses keluar dengan exit code 0 dalam batas waktu.
- Setiap langkah dengan timeout (mis. `timeout 15`) supaya skrip tidak bisa hang;
  cetak `PASS`/`FAIL` per langkah dan ringkasan; exit non-zero bila ada yang FAIL.
- Skrip harus jalan di Linux/macOS. Untuk perbedaan tool (`timeout` absen di macOS
  bawaan), pakai fungsi pembungkus yang mendeteksi `gtimeout`/`timeout`, atau
  pendekatan baca berbatas waktu lain — jelaskan di komentar.
- **Catatan penting saat menulis skrip:** `serve()` membaca `TAKKA_TEST_ACP_CONFIG`
  dan `--acp-max-sessions` sudah ada; verifikasi dengan membaca `main.rs` dan
  `acp.rs` (baca saja, jangan edit). Kalau nama env var berbeda, sesuaikan.

Jalankan skripnya (`bash scripts/acp-conformance.sh`) dan **tempel output aslinya**
ke laporan. Kalau ada langkah yang gagal, JANGAN memperbaiki kode Rust — catat
temuannya sebagai kegagalan/temuan penting di laporan (lane integrator yang
memperbaiki). Jangan meninggalkan proses runtime yang menggantung (pastikan ditutup).

## 3. `docs/superpowers/plans/2026-11-11-acp-conformance-report.md`

Isi: status ketersediaan TCK resmi (dengan bukti perintah yang kamu jalankan dan
keluarannya), isi/cakupan skrip konformansi, output run asli (tempel), hasil per
langkah, temuan/penyimpangan dari spec ACP v1, dan rekomendasi. Nyatakan jelas bahwa
ini pengganti TCK resmi, bukan TCK resmi.

## Verifikasi

- `bash -n scripts/acp-conformance.sh` bersih; skrip executable (`chmod +x`).
- Skrip dijalankan sekali, output ditempel ke laporan.
- `git status --short` menunjukkan hanya path di `docs/` dan `scripts/` yang kamu
  sentuh (selain perubahan lane lain yang bukan milikmu).

## Laporan akhir ke saya

Sebutkan: status, path file, ringkasan hasil konformansi per langkah, dan temuan
apa pun yang butuh perbaikan kode.
