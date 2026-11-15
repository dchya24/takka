use super::*;

// ── Dropdown trigger & accept logic ─────────────────────────

/// Discover skills and return (name, description) pairs for the skill dropdown.
pub(crate) fn discover_skill_pairs() -> Vec<(String, String)> {
    let _config_path = takka_core::Config::config_path();
    let config = takka_core::Config::load().unwrap_or_default();
    let discovery_config: takka_core::DiscoveryConfig =
        takka_core::DiscoveryConfig::from(&config.skills);
    let index = takka_core::discover_skills(&discovery_config);
    let mut skills: Vec<_> = index.all().into_iter().collect();
    skills.sort_by(|a, b| a.name().cmp(b.name()));
    skills
        .into_iter()
        .map(|s| (s.name().to_string(), s.description().to_string()))
        .collect()
}

/// Update dropdown state based on current input and cursor position.
///
/// An empty dropdown is closed immediately: it renders nothing, yet if it
/// stayed open it would intercept Enter / Up / Down and swallow those keys.
/// This happens in practice right after accepting a model — the input
/// becomes `/models <id> ` (trailing space) whose query `<id> ` matches no
/// model, so the next Enter, which should submit the `/models <id>`
/// command, would instead be consumed by the empty dropdown and the model
/// would never get selected.
pub(crate) fn update_dropdown(buffer: &InputBuffer, dropdown: &mut Option<Dropdown>) {
    let text = buffer.text();
    let cursor = buffer.cursor();

    let mut next: Option<Dropdown> = None;

    // 1) Check for `/` command trigger
    if text.starts_with('/') {
        let before_cursor = &text[..cursor];
        if !before_cursor.contains(' ') {
            let query = &text[1..cursor];
            // If the query exactly matches a skill command, open skills dropdown directly
            if query == "skills" || query == "skill" || query == "sk" {
                let skill_pairs = discover_skill_pairs();
                next = Some(Dropdown::new_skill(String::new(), skill_pairs));
            } else {
                next = Some(Dropdown::new(DropdownType::Command, query.to_string()));
            }
        } else if let Some(space_pos) = before_cursor.find(' ') {
            let cmd = &text[1..space_pos];
            // 1b) Check for `/models <partial>` model trigger
            if cmd == "models" || cmd == "m" {
                let query = &text[space_pos + 1..cursor];
                next = Some(Dropdown::new(DropdownType::Model, query.to_string()));
            }
            // 1c) Check for `/skill <partial>` skill trigger
            else if cmd == "skill" || cmd == "sk" {
                let query = &text[space_pos + 1..cursor];
                let skill_pairs = discover_skill_pairs();
                next = Some(Dropdown::new_skill(query.to_string(), skill_pairs));
            }
        }
    }

    // 2) Check for `@` file trigger
    if next.is_none() {
        if let Some(at_pos) = find_at_trigger(text, cursor) {
            let query = &text[at_pos + 1..cursor];
            next = Some(Dropdown::new(DropdownType::File, query.to_string()));
        }
    }

    // 3) Commit, closing empty dropdowns so they cannot swallow keys.
    *dropdown = next.filter(|dd| !dd.is_empty());
}

/// Find the byte position of the `@` trigger that the cursor is inside.
pub(crate) fn find_at_trigger(text: &str, cursor: usize) -> Option<usize> {
    let before_cursor = &text[..cursor];

    for (i, c) in before_cursor.char_indices().rev() {
        match c {
            '@' => {
                let at_start = i == 0;
                let after_space = i > 0 && text[..i].ends_with(char::is_whitespace);
                if at_start || after_space {
                    let after_at = &before_cursor[i + 1..];
                    if !after_at.contains(char::is_whitespace) {
                        return Some(i);
                    }
                }
                return None;
            }
            w if w.is_whitespace() => return None,
            _ => continue,
        }
    }
    None
}

/// Accept the currently selected dropdown item and insert into buffer.
///
/// Returns `Some(command)` when the input should be auto-submitted
/// (e.g. after selecting a skill from the skills dropdown), or `None`
/// when the user needs to press Enter again to confirm.
pub(crate) fn accept_dropdown(
    buffer: &mut InputBuffer,
    dropdown: &mut Option<Dropdown>,
) -> Option<String> {
    let dd = dropdown.take()?;

    let selected_text = dd.selected_item()?.to_string();

    let is_dir = selected_text.ends_with('/');

    match dd.dropdown_type {
        DropdownType::Command => {
            let cmd = selected_text.trim_start_matches('/');
            if cmd == "skill" || cmd == "sk" {
                // Skills dropdown is triggered separately — just fill /skill
                buffer.set_text("/skill ".to_string());
            } else {
                buffer.set_text(format!("/{} ", selected_text));
            }
        }
        DropdownType::File => {
            if let Some(at_pos) = find_at_trigger(buffer.text(), buffer.cursor()) {
                let suffix = if is_dir { "" } else { " " };
                let replacement = format!("{}{}", selected_text, suffix);
                buffer.replace_range(at_pos + 1, &replacement);
            }
        }
        DropdownType::Skill => {
            let skill_name = dd
                .get_skill_name(&selected_text)
                .unwrap_or_else(|| selected_text.clone());
            let cmd = format!("/skill {}", skill_name);
            buffer.set_text(cmd.clone());
            return Some(cmd);
        }
        DropdownType::Model => {
            // Auto-submit like the skill dropdown: selecting a model from
            // `/models <partial>` is always the whole command, so switch
            // to it immediately on Enter/Tab.
            //
            // Use the exact (provider, model id) carried by the dropdown
            // and emit a provider-qualified command. A plain id would be
            // re-resolved with a contains() search and could silently hit
            // a different provider that happens to share the display name
            // / id (e.g. "GLM-4.7" in two providers, or an aliased id).
            let (provider, model_id) = dd
                .selected_model_meta()
                .unwrap_or_else(|| (String::new(), selected_text.clone()));
            let cmd = if provider.is_empty() {
                format!("/models {}", model_id)
            } else {
                format!("/models {}/{}", provider, model_id)
            };
            buffer.set_text(cmd.clone());
            return Some(cmd);
        }
    }

    // For directories, re-trigger dropdown to show contents
    if is_dir {
        let mut new_dd = None;
        update_dropdown(buffer, &mut new_dd);
        *dropdown = new_dd;
    }
    None
}
