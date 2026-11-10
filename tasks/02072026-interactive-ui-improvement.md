# Interactive Mode UI Improvement

**Tanggal:** 02 Juli 2026  
**Status:** 🚀 In Progress - Phase 1-2 Complete  
**Priority:** High  
**Estimated Effort:** 2-3 weeks  

> **⚠️ Audit status (2026-11-10, takka v0.6.0)** — dokumen ini sudah sebagian besar
> basi. Yang SUDAH landed sejalan dengan bentuknya: dropdown completion untuk
> `@` file, `/skill`, dan slash command (story 3.1, buffer-agnostic; interactive
> + TUI), spinner/status line hidup, live streaming, usage nyata, toast/widget
> stack baru (`widgets/toast.rs`, `widgets/spinner.rs`), serta verifikasi
> overhaul. Yang BELUM ada dan tetap sebagai idea terbuka (prioritas rendah,
> tidak terjadwalkan): theme system (kustom + predefined), error styling dengan
> context/suggestion, fuzzy-match highlighting, dan perf-gate dropdown proyek
> besar. Checkbox `- [ ]` internal (test checklist dsb.) menandai rencana saat
> itu, bukan queue kerja aktif — jangan digunakan sebagai backlog. Daftar gap
> aktif kini ada di `docs/tui-feature-gap.md` (audit baru) + `docs/ROADMAP.md`.

---

## 📋 Overview

Dokumen ini berisi rencana improvement untuk user interface interactive mode pada `agentic-cli`. Tujuan utama adalah meningkatkan user experience, visual feedback, dan usability dari interactive REPL.

---

## 🎯 Goals

1. Meningkatkan visual feedback untuk semua user actions
2. Menambahkan fuzzy search highlighting pada dropdown
3. Implementasi loading states dan progress indicators
4. Menambahkan toast notification system
5. Meningkatkan responsiveness dan performance

---

## 📊 Current State Analysis

### ✅ Sudah Ada
- [x] Dropdown completion untuk `/` commands dan `@` files
- [x] Syntax highlighting untuk input
- [x] History navigation (↑/↓)
- [x] Session management (save/load/resume)
- [x] Statistics display
- [x] Ratatui-based rendering dengan inline mode
- [x] Raw mode key capture
- [x] Transient rendering untuk prompt

### ✅ Sudah Diimplementasi (Phase 1-2)
- [x] Fuzzy search highlighting di dropdown
- [x] Loading spinner/progress indicator
- [x] Toast notification system
- [x] Multi-line input support
- [x] Real-time token usage counter
- [x] Keyboard shortcuts hint di status bar

### ❌ Belum Ada / Perlu Improvement (Phase 3)
- [ ] Theme customization
- [ ] Better error styling dengan context

---

## 📝 Task List

### Phase 1: Visual Feedback (Week 1)

#### Task 1.1: Fuzzy Search Highlighting
**File:** `src/input_renderer.rs`, `src/tui/dropdown.rs`  
**Priority:** 🔴 High  
**Effort:** 3-4 hours  

**Description:**
Tambahkan highlighting untuk karakter yang match dengan query di dropdown items.

**Implementation:**
```rust
fn render_fuzzy_match(text: &str, query: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let query_lower = query.to_lowercase();
    let mut qi = 0;
    let query_chars: Vec<char> = query_lower.chars().collect();
    
    for c in text.chars() {
        if qi < query_chars.len() && c.to_lowercase().next() == Some(query_chars[qi]) {
            spans.push(Span::styled(
                c.to_string(),
                Style::default()
                    .fg(Color::Rgb(255, 215, 0))
                    .add_modifier(Modifier::BOLD),
            ));
            qi += 1;
        } else {
            spans.push(Span::raw(c.to_string()));
        }
    }
    Line::from(spans)
}
```

**Acceptance Criteria:**
- [x] Karakter yang match di-highlight dengan warna berbeda
- [x] Highlight berfungsi untuk command, file, dan model dropdown
- [x] Case-insensitive matching
- [x] Tidak mengganggu selection highlight

**Status:** ✅ Completed

---

#### Task 1.2: Loading Spinner
**File:** `src/widgets/spinner.rs`, `src/interactive.rs`  
**Priority:** 🔴 High  
**Effort:** 2-3 hours  

**Description:**
Implementasi loading spinner yang muncul saat processing message ke AI.

**Implementation:**
```rust
pub struct Spinner {
    frames: Vec<&'static str>,
    current: usize,
    message: String,
}

impl Spinner {
    pub fn new(message: &str) -> Self {
        Self {
            frames: vec!["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            current: 0,
            message: message.to_string(),
        }
    }
    
    pub fn tick(&mut self) {
        self.current = (self.current + 1) % self.frames.len();
    }
    
    pub fn render(&self) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                self.frames[self.current],
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(" "),
            Span::styled(
                &self.message,
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    }
}
```

**Acceptance Criteria:**
- [x] Spinner muncul saat mengirim message ke AI
- [x] Animasi berputar dengan smooth
- [x] Bisa di-cancel dengan Ctrl+C
- [x] Menghilang setelah response diterima

**Status:** ✅ Completed

---

#### Task 1.3: Toast Notification System
**File:** `src/widgets/toast.rs`, `src/interactive.rs`  
**Priority:** 🟡 Medium  
**Effort:** 3-4 hours  

**Description:**
Implementasi toast notification untuk feedback actions (success, error, warning, info).

**Implementation:**
```rust
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub created_at: Instant,
    pub duration: Duration,
}

impl Toast {
    pub fn render(&self) -> Line<'static> {
        let (icon, color) = match self.level {
            ToastLevel::Info => ("ℹ️", Color::Rgb(52, 152, 219)),
            ToastLevel::Success => ("✅", Color::Rgb(46, 204, 113)),
            ToastLevel::Warning => ("⚠️", Color::Rgb(241, 196, 15)),
            ToastLevel::Error => ("❌", Color::Rgb(231, 76, 60)),
        };
        
        Line::from(vec![
            Span::styled(format!(" {} ", icon), Style::default()),
            Span::styled(&self.message, Style::default().fg(color)),
        ])
    }
    
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.duration
    }
}
```

**Acceptance Criteria:**
- [x] Toast muncul untuk success actions (session saved, model switched, etc.)
- [x] Toast muncul untuk errors dengan styling berbeda
- [x] Toast auto-dismiss setelah 3-5 detik
- [x] Tidak mengganggu input area

**Status:** ✅ Completed

---

### Phase 2: Input Enhancements (Week 2)

#### Task 2.1: Multi-line Input Support
**File:** `src/input_buffer.rs`, `src/interactive.rs`  
**Priority:** 🟡 Medium  
**Effort:** 6-8 hours  

**Description:**
Tambahkan support untuk multi-line input menggunakan Shift+Enter atau Alt+Enter.

**Key Changes:**
- InputBuffer perlu support multiple lines
- Render perlu handle line wrapping
- Submit tetap dengan Enter biasa
- Visual indicator untuk multi-line mode

**Acceptance Criteria:**
- [x] Shift+Enter menambah baris baru
- [x] Enter biasa tetap submit
- [x] Visual indicator saat multi-line active
- [x] Scrolling untuk input panjang
- [x] Backspace di awal line merge dengan line sebelumnya

**Status:** ✅ Completed

---

#### Task 2.2: Real-time Token Counter
**File:** `src/interactive.rs`, `src/widgets/components.rs`  
**Priority:** 🟡 Medium  
**Effort:** 2-3 hours  

**Description:**
Tambahkan real-time token usage counter di status bar yang update setiap response.

**Implementation:**
```rust
fn render_token_counter(stats: &SessionStats) -> Line<'static> {
    let in_tok = stats.total_input_tokens();
    let out_tok = stats.total_output_tokens();
    let total = in_tok + out_tok;
    
    Line::from(vec![
        Span::styled("📊 ", Style::default()),
        Span::styled(
            format_tokens(in_tok),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" ↑ ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            format_tokens(out_tok),
            Style::default().fg(Color::Red),
        ),
        Span::styled(" ↓ ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            format!("({})", format_tokens(total)),
            Style::default().fg(Color::Yellow),
        ),
    ])
}
```

**Acceptance Criteria:**
- [x] Token counter update setiap response
- [x] Format yang readable (1.2K, 3.5M, etc.)
- [x] Visual distinction antara input dan output tokens
- [x] Total tokens terlihat jelas

**Status:** ✅ Completed

---

#### Task 2.3: Keyboard Shortcuts Hint
**File:** `src/interactive.rs`  
**Priority:** 🟢 Low  
**Effort:** 1-2 hours  

**Description:**
Tambahkan hint keyboard shortcuts di status bar atau footer.

**Implementation:**
```rust
fn render_shortcuts_hint() -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled("Tab", Style::default()
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)),
        Span::styled(" accept ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("↑↓", Style::default()
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)),
        Span::styled(" navigate ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("Esc", Style::default()
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)),
        Span::styled(" close ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("Ctrl+C", Style::default()
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)),
        Span::styled(" cancel ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("Ctrl+D", Style::default()
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)),
        Span::styled(" exit", Style::default().add_modifier(Modifier::DIM)),
    ])
}
```

**Acceptance Criteria:**
- [x] Hint muncul di bawah input area
- [x] Tidak terlalu mengganggu
- [x] Bisa di-dismiss atau auto-hide
- [x] Update berdasarkan context (dropdown open/closed)

**Status:** ✅ Completed

---

### Phase 3: Polish & Performance (Week 3)

#### Task 3.1: Theme Customization
**File:** `src/config.rs`, `src/theme.rs`  
**Priority:** 🟢 Low  
**Effort:** 4-6 hours  

**Description:**
Tambahkan support untuk customizable color themes.

**Implementation:**
```rust
pub struct Theme {
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub text: Color,
    pub text_dim: Color,
    pub background: Color,
    pub border: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            primary: Color::Rgb(64, 224, 208),
            secondary: Color::Rgb(255, 105, 180),
            accent: Color::Rgb(255, 215, 0),
            success: Color::Rgb(46, 204, 113),
            warning: Color::Rgb(241, 196, 15),
            error: Color::Rgb(231, 76, 60),
            text: Color::Rgb(220, 220, 230),
            text_dim: Color::Rgb(100, 100, 120),
            background: Color::Reset,
            border: Color::Rgb(60, 60, 80),
        }
    }
}
```

**Acceptance Criteria:**
- [ ] Bisa load theme dari config file
- [ ] Predefined themes (default, dark, light, monokai, etc.)
- [ ] Custom theme bisa di-define di config
- [ ] Fallback ke default jika theme invalid

---

#### Task 3.2: Dropdown Performance Optimization
**File:** `src/tui/dropdown.rs`  
**Priority:** 🟢 Low  
**Effort:** 3-4 hours  

**Description:**
Optimasi performance untuk dropdown dengan banyak items (terutama file dropdown di project besar).

**Key Changes:**
- Lazy loading untuk file tree
- Caching hasil scan
- Debounced input untuk search
- Limit visible items dengan virtual scrolling

**Acceptance Criteria:**
- [ ] File dropdown tetap responsive di project besar (>1000 files)
- [ ] Search tidak lag saat mengetik cepat
- [ ] Memory usage tetap reasonable
- [ ] Cache invalidasi yang benar

---

#### Task 3.3: Better Error Display
**File:** `src/widgets/components.rs`, `src/interactive.rs`  
**Priority:** 🟡 Medium  
**Effort:** 2-3 hours  

**Description:**
Improve error display dengan context dan suggested actions.

**Implementation:**
```rust
pub fn error_with_context(message: &str, context: &str, suggestion: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("❌ ", Style::default()),
            Span::styled(message, Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::raw("   "),
            Span::styled(context, Style::default()
                .fg(Color::Rgb(180, 180, 200))
                .add_modifier(Modifier::DIM)),
        ]),
        Line::from(vec![
            Span::raw("   "),
            Span::styled("💡 ", Style::default()),
            Span::styled(suggestion, Style::default()
                .fg(Color::Yellow)),
        ]),
    ]
}
```

**Acceptance Criteria:**
- [ ] Error messages lebih informative
- [ ] Ada context tentang apa yang salah
- [ ] Ada suggestion untuk fix
- [ ] Styling yang jelas dan mudah dibaca

---

## 📁 Files to Modify

| File | Changes |
|------|---------|
| `src/input_renderer.rs` | Fuzzy search highlighting |
| `src/tui/dropdown.rs` | Dropdown rendering improvements |
| `src/widgets/spinner.rs` | New file: Loading spinner |
| `src/widgets/toast.rs` | New file: Toast notifications |
| `src/widgets/mod.rs` | Register new widgets |
| `src/widgets/components.rs` | Error display improvements |
| `src/input_buffer.rs` | Multi-line input support |
| `src/interactive.rs` | Integrate all improvements |
| `src/config.rs` | Theme configuration |
| `src/theme.rs` | New file: Theme definitions |

---

## 🧪 Testing Plan

### Unit Tests
- [ ] Fuzzy match highlighting logic
- [ ] Toast expiration logic
- [ ] Multi-line buffer operations
- [ ] Theme parsing

### Integration Tests
- [ ] Dropdown rendering dengan highlights
- [ ] Spinner lifecycle
- [ ] Toast display dan dismiss
- [ ] Keyboard shortcuts behavior

### Manual Testing
- [ ] Test di terminal sizes berbeda (80x24, 120x40, etc.)
- [ ] Test dengan dark dan light terminal themes
- [ ] Test performance dengan project besar
- [ ] Test semua keyboard shortcuts

---

## 📅 Timeline

| Week | Tasks | Deliverables |
|------|-------|--------------|
| **Week 1** | Task 1.1, 1.2, 1.3 | Fuzzy highlight, Spinner, Toast |
| **Week 2** | Task 2.1, 2.2, 2.3 | Multi-line, Token counter, Shortcuts |
| **Week 3** | Task 3.1, 3.2, 3.3 | Theme, Performance, Error display |

---

## 🔗 Dependencies

- `ratatui` - UI framework (sudah ada)
- `crossterm` - Terminal manipulation (sudah ada)
- Tidak ada dependency baru yang diperlukan

---

## 📚 References

- [Ratatui Documentation](https://docs.rs/ratatui)
- [Crossterm Documentation](https://docs.rs/crossterm)
- Current implementation: `src/interactive.rs`
- Widget system: `src/widgets/`

---

## ✅ Success Criteria

1. **Visual Feedback** - Semua user actions punya visual feedback yang jelas
2. **Performance** - UI tetap responsive di project besar
3. **Usability** - Keyboard shortcuts intuitive dan discoverable
4. **Consistency** - Styling konsisten di semua komponen
5. **Accessibility** - Mudah dibaca di berbagai terminal themes

---

## 📝 Notes

- Prioritaskan Phase 1 karena impact paling besar
- Multi-line input bisa di-scope down jika terlalu kompleks
- Theme customization bisa di-defer ke versi berikutnya
- Pastikan backward compatibility dengan session files yang sudah ada

---

**Last Updated:** 02 Juli 2026  
**Author:** AI Assistant  
**Review Status:** Pending Review