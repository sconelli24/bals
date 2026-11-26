use anyhow::{Result, bail};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyNameTextW, HOT_KEY_MODIFIERS, MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, MapVirtualKeyW, VK_OEM_5, VkKeyScanW,
};

/// Definición Win32 lista para pasar a `RegisterHotKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedHotkey {
    pub modifiers: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

/// Resultado de un ciclo de captura nativa de teclado al asignar un atajo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyCaptureEvent {
    Cancelled,
    Captured(String),
}

/// Parsea una cadena legible o canónica (ej. `"\\"`, `"F8"`, `"Alt+F10"`, `"Ctrl+Shift+S"`,
/// `"Numpad+"`, `"Ctrl+Numpad+"`, `"OEM_3"`, `"OEM_102"`, `"VK_E2"`)
/// en `(HOT_KEY_MODIFIERS, virtual_key)` para `RegisterHotKey`.
pub fn parse_hotkey(spec: &str) -> Result<ParsedHotkey> {
    let mut rest = spec.trim();
    if rest.is_empty() {
        bail!("El atajo de teclado está vacío");
    }

    let mut modifiers = MOD_NOREPEAT;

    // Extraer prefijos de modificadores (`Ctrl+`, `Alt+`, `Shift+`, `Win+`) sin romper teclas
    // que contienen `+` (como `"+"`, `"Ctrl++"`, `"Numpad+"` o `"Ctrl+Numpad+"`).
    loop {
        let Some(pos) = rest.find('+') else {
            break;
        };
        let prefix = rest[..pos].trim();
        let after = rest[pos + 1..].trim();
        if after.is_empty() {
            break;
        }
        match prefix.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                modifiers |= MOD_CONTROL;
                rest = after;
            }
            "alt" => {
                modifiers |= MOD_ALT;
                rest = after;
            }
            "shift" => {
                modifiers |= MOD_SHIFT;
                rest = after;
            }
            "win" | "super" => {
                modifiers |= MOD_WIN;
                rest = after;
            }
            _ => break,
        }
    }

    let key = rest.trim();
    if key.is_empty() {
        bail!("Falta la tecla principal en '{spec}'");
    }
    let vk = resolve_virtual_key(key)?;

    Ok(ParsedHotkey { modifiers, vk })
}

/// Convierte `(modifiers, vk)` capturados por Win32 en una especificación canónica con
/// garantía de round-trip exacto con `parse_hotkey()`.
pub fn format_hotkey(modifiers: HOT_KEY_MODIFIERS, vk: u32) -> Option<String> {
    if is_modifier_vk(vk) || vk < 0x08 || vk > 0xFE || vk == 0xE7 {
        return None;
    }

    let base = canonical_vk_name(vk);
    let mut parts: Vec<String> = Vec::with_capacity(5);
    if (modifiers.0 & MOD_CONTROL.0) != 0 {
        parts.push("Ctrl".to_string());
    }
    if (modifiers.0 & MOD_ALT.0) != 0 {
        parts.push("Alt".to_string());
    }
    if (modifiers.0 & MOD_SHIFT.0) != 0 {
        parts.push("Shift".to_string());
    }
    if (modifiers.0 & MOD_WIN.0) != 0 {
        parts.push("Win".to_string());
    }
    parts.push(base);
    Some(parts.join("+"))
}

/// Devuelve una etiqueta legible para mostrar en la UI y bandeja del sistema,
/// resolviendo teclas OEM (`OEM_1..8`, `OEM_102`, `OEM_PLUS`, etc.) al rótulo físico
/// del teclado activo en Windows (ej. `Ñ`, `|`, `°`, `<`, `+`, `´`, `¿`).
pub fn display_hotkey(spec: &str) -> String {
    let trimmed = spec.trim();
    if trimmed == "\\" {
        return "\\".to_string();
    }
    let Ok(parsed) = parse_hotkey(trimmed) else {
        return trimmed.to_string();
    };

    let mut parts: Vec<String> = Vec::with_capacity(5);
    if (parsed.modifiers.0 & MOD_CONTROL.0) != 0 {
        parts.push("Ctrl".to_string());
    }
    if (parsed.modifiers.0 & MOD_ALT.0) != 0 {
        parts.push("Alt".to_string());
    }
    if (parsed.modifiers.0 & MOD_SHIFT.0) != 0 {
        parts.push("Shift".to_string());
    }
    if (parsed.modifiers.0 & MOD_WIN.0) != 0 {
        parts.push("Win".to_string());
    }
    parts.push(display_vk_label(parsed.vk));
    parts.join("+")
}

/// Devuelve `true` si el Virtual-Key corresponde a una tecla modificadora pura.
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
    )
}

/// Toma una instantánea del estado actual de las 256 Virtual Keys para detectar solo
/// pulsaciones nuevas (flanco de subida) al iniciar el modo de asignación de tecla.
pub fn snapshot_key_states() -> [bool; 256] {
    let mut state = [false; 256];
    for (vk, slot) in state.iter_mut().enumerate() {
        *slot = is_vk_down(vk as i32);
    }
    state
}

/// Escanea el teclado vía Win32 (`GetAsyncKeyState`) buscando un flanco de subida de cualquier
/// tecla válida (incluyendo Numpad, teclas OEM de teclados ES/LatAm/ISO, teclas de función, etc.).
pub fn poll_pressed_hotkey(prev_state: &mut [bool; 256]) -> Option<HotkeyCaptureEvent> {
    let mut newly_pressed_vk: Option<u32> = None;

    for vk in 0x08u32..=0xFEu32 {
        let now_down = is_vk_down(vk as i32);
        let was_down = prev_state[vk as usize];
        prev_state[vk as usize] = now_down;

        if now_down && !was_down && !is_modifier_vk(vk) && vk != 0xE7 && newly_pressed_vk.is_none()
        {
            newly_pressed_vk = Some(vk);
        }
    }

    let vk = newly_pressed_vk?;
    if vk == 0x1B {
        // VK_ESCAPE cancela la asignación
        return Some(HotkeyCaptureEvent::Cancelled);
    }

    let mut modifiers = MOD_NOREPEAT;
    if is_vk_down(0x11) {
        modifiers |= MOD_CONTROL;
    }
    if is_vk_down(0x12) {
        modifiers |= MOD_ALT;
    }
    if is_vk_down(0x10) {
        modifiers |= MOD_SHIFT;
    }
    if is_vk_down(0x5B) || is_vk_down(0x5C) {
        modifiers |= MOD_WIN;
    }

    format_hotkey(modifiers, vk).map(HotkeyCaptureEvent::Captured)
}

#[inline]
fn is_vk_down(vk: i32) -> bool {
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

fn canonical_vk_name(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x41..=0x5A => ((vk as u8) as char).to_string(),
        0x30..=0x39 => ((vk as u8) as char).to_string(),
        0x60..=0x69 => format!("Numpad{}", vk - 0x60),
        0x6A => "Numpad*".to_string(),
        0x6B => "Numpad+".to_string(),
        0x6D => "Numpad-".to_string(),
        0x6E => "Numpad.".to_string(),
        0x6F => "Numpad/".to_string(),
        0x08 => "Backspace".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x13 => "Pause".to_string(),
        0x14 => "CapsLock".to_string(),
        0x20 => "Space".to_string(),
        0x21 => "PgUp".to_string(),
        0x22 => "PgDn".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2C => "PrintScreen".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x90 => "NumLock".to_string(),
        0x91 => "ScrollLock".to_string(),
        0xBA => "OEM_1".to_string(),
        0xBB => "OEM_PLUS".to_string(),
        0xBC => "OEM_COMMA".to_string(),
        0xBD => "OEM_MINUS".to_string(),
        0xBE => "OEM_PERIOD".to_string(),
        0xBF => "OEM_2".to_string(),
        0xC0 => "OEM_3".to_string(),
        0xDB => "OEM_4".to_string(),
        0xDC => "OEM_5".to_string(),
        0xDD => "OEM_6".to_string(),
        0xDE => "OEM_7".to_string(),
        0xDF => "OEM_8".to_string(),
        0xE2 => "OEM_102".to_string(),
        _ => format!("VK_{vk:02X}"),
    }
}

fn display_vk_label(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x41..=0x5A => ((vk as u8) as char).to_string(),
        0x30..=0x39 => ((vk as u8) as char).to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x6A => "Num *".to_string(),
        0x6B => "Num +".to_string(),
        0x6D => "Num -".to_string(),
        0x6E => "Num .".to_string(),
        0x6F => "Num /".to_string(),
        0x08 => "Bksp".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x13 => "Pause".to_string(),
        0x14 => "Caps".to_string(),
        0x20 => "Space".to_string(),
        0x21 => "PgUp".to_string(),
        0x22 => "PgDn".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2C => "PrtSc".to_string(),
        0x2D => "Ins".to_string(),
        0x2E => "Del".to_string(),
        0x90 => "NumLock".to_string(),
        0x91 => "ScrLk".to_string(),
        _ => win32_key_name(vk).unwrap_or_else(|| canonical_vk_name(vk)),
    }
}

/// Obtiene el rótulo físico de una tecla según el layout de teclado activo en Windows.
fn win32_key_name(vk: u32) -> Option<String> {
    let scancode = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
    if scancode == 0 {
        return None;
    }
    let lparam = (scancode << 16) as i32;
    let mut buf = [0u16; 64];
    let len = unsafe { GetKeyNameTextW(lparam, &mut buf) };
    if len <= 0 {
        return None;
    }
    let label = String::from_utf16_lossy(&buf[..len as usize]).trim().to_string();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

fn resolve_virtual_key(key: &str) -> Result<u32> {
    // Tecla por defecto elegida para Bals: barra invertida `\`
    // Si el teclado activo tiene `\` sin modificadores (layout US), usa ese VK exacto;
    // en teclados ES/LatAm donde `\` requiere AltGr, vinculamos directamente `VK_OEM_5`
    // (la tecla física de `\`/`º`/`|` a la izquierda del 1) para disparo de 1 solo toque en juego.
    if key == "\\" || key.eq_ignore_ascii_case("backslash") {
        let scan = unsafe { VkKeyScanW('\\' as u16) };
        if scan != -1 {
            let vk_low = (scan & 0xFF) as u32;
            let shift_state = (scan >> 8) & 0xFF;
            if shift_state == 0 && vk_low != 0 {
                return Ok(vk_low);
            }
        }
        return Ok(VK_OEM_5.0 as u32);
    }

    let upper = key.to_ascii_uppercase();

    // Teclas de función F1..F24
    if let Some(num_str) = upper.strip_prefix('F') {
        if let Ok(n) = num_str.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Ok(0x70 + (n - 1)); // VK_F1 = 0x70
            }
        }
    }

    // Fallback genérico `VK_XX` (hex) para cualquier Virtual-Key válida de teclado
    if let Some(hex_str) = upper.strip_prefix("VK_") {
        if let Ok(vk) = u32::from_str_radix(hex_str, 16) {
            if (0x08..=0xFE).contains(&vk) && !is_modifier_vk(vk) && vk != 0xE7 {
                return Ok(vk);
            }
        }
    }

    // Letras A-Z y dígitos 0-9
    if upper.len() == 1 {
        let b = upper.as_bytes()[0];
        if b.is_ascii_alphanumeric() {
            return Ok(b as u32);
        }
    }

    // Teclas especiales, Numpad y OEM
    match upper.as_str() {
        "BACKSPACE" | "BKSP" => Ok(0x08),
        "TAB" => Ok(0x09),
        "ENTER" | "RETURN" => Ok(0x0D),
        "PAUSE" => Ok(0x13),
        "CAPSLOCK" | "CAPS" => Ok(0x14),
        "SPACE" => Ok(0x20),
        "PGUP" | "PAGEUP" => Ok(0x21),
        "PGDN" | "PAGEDOWN" => Ok(0x22),
        "END" => Ok(0x23),
        "HOME" => Ok(0x24),
        "LEFT" | "ARROWLEFT" => Ok(0x25),
        "UP" | "ARROWUP" => Ok(0x26),
        "RIGHT" | "ARROWRIGHT" => Ok(0x27),
        "DOWN" | "ARROWDOWN" => Ok(0x28),
        "PRINTSCREEN" | "PRTSC" => Ok(0x2C),
        "INSERT" | "INS" => Ok(0x2D),
        "DELETE" | "DEL" => Ok(0x2E),
        "NUMLOCK" => Ok(0x90),
        "SCROLLLOCK" | "SCRLK" => Ok(0x91),
        "NUMPAD0" => Ok(0x60),
        "NUMPAD1" => Ok(0x61),
        "NUMPAD2" => Ok(0x62),
        "NUMPAD3" => Ok(0x63),
        "NUMPAD4" => Ok(0x64),
        "NUMPAD5" => Ok(0x65),
        "NUMPAD6" => Ok(0x66),
        "NUMPAD7" => Ok(0x67),
        "NUMPAD8" => Ok(0x68),
        "NUMPAD9" => Ok(0x69),
        "NUMPAD*" | "NUMPADMUL" | "NUMPADMULTIPLY" => Ok(0x6A),
        "NUMPAD+" | "NUMPADADD" => Ok(0x6B),
        "NUMPAD-" | "NUMPADSUB" | "NUMPADSUBTRACT" => Ok(0x6D),
        "NUMPAD." | "NUMPADDEC" | "NUMPADDECIMAL" => Ok(0x6E),
        "NUMPAD/" | "NUMPADDIV" | "NUMPADDIVIDE" => Ok(0x6F),
        "OEM_1" | ";" | "SEMICOLON" => Ok(0xBA),
        "OEM_PLUS" | "+" | "=" | "EQUALS" | "PLUS" => Ok(0xBB),
        "OEM_COMMA" | "," | "COMMA" => Ok(0xBC),
        "OEM_MINUS" | "-" | "MINUS" => Ok(0xBD),
        "OEM_PERIOD" | "." | "PERIOD" => Ok(0xBE),
        "OEM_2" | "/" | "SLASH" => Ok(0xBF),
        "OEM_3" | "`" | "TILDE" => Ok(0xC0),
        "OEM_4" | "[" | "LBRACKET" => Ok(0xDB),
        "OEM_5" => Ok(0xDC),
        "OEM_6" | "]" | "RBRACKET" => Ok(0xDD),
        "OEM_7" | "'" | "QUOTE" => Ok(0xDE),
        "OEM_8" => Ok(0xDF),
        "OEM_102" | "<" | ">" => Ok(0xE2),
        _ => bail!("Tecla no reconocida para hotkey: '{key}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_backslash_and_combos() {
        let bs = parse_hotkey("\\").unwrap();
        assert_eq!(bs.modifiers, MOD_NOREPEAT);
        assert_ne!(bs.vk, 0);

        let f8 = parse_hotkey("F8").unwrap();
        assert_eq!(f8.vk, 0x77);
        assert_eq!(f8.modifiers, MOD_NOREPEAT);

        let combo = parse_hotkey("Alt+F10").unwrap();
        assert_eq!(combo.vk, 0x79);
        assert_eq!(combo.modifiers, MOD_NOREPEAT | MOD_ALT);

        let combo2 = parse_hotkey("Ctrl+Shift+S").unwrap();
        assert_eq!(combo2.vk, b'S' as u32);
        assert_eq!(combo2.modifiers, MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT);
    }

    #[test]
    fn parses_plus_numpad_and_oem_keys() {
        let plus = parse_hotkey("+").unwrap();
        assert_eq!(plus.vk, 0xBB);
        assert_eq!(plus.modifiers, MOD_NOREPEAT);

        let ctrl_plus = parse_hotkey("Ctrl++").unwrap();
        assert_eq!(ctrl_plus.vk, 0xBB);
        assert_eq!(ctrl_plus.modifiers, MOD_NOREPEAT | MOD_CONTROL);

        let num_add = parse_hotkey("Numpad+").unwrap();
        assert_eq!(num_add.vk, 0x6B);
        assert_eq!(num_add.modifiers, MOD_NOREPEAT);

        let ctrl_num_add = parse_hotkey("Ctrl+Alt+Numpad+").unwrap();
        assert_eq!(ctrl_num_add.vk, 0x6B);
        assert_eq!(
            ctrl_num_add.modifiers,
            MOD_NOREPEAT | MOD_CONTROL | MOD_ALT
        );

        let oem102 = parse_hotkey("OEM_102").unwrap();
        assert_eq!(oem102.vk, 0xE2);

        let generic = parse_hotkey("Shift+VK_B3").unwrap();
        assert_eq!(generic.vk, 0xB3);
        assert_eq!(generic.modifiers, MOD_NOREPEAT | MOD_SHIFT);
    }

    #[test]
    fn format_and_parse_roundtrip_all_valid_vks() {
        let mod_combos = [
            MOD_NOREPEAT,
            MOD_NOREPEAT | MOD_CONTROL,
            MOD_NOREPEAT | MOD_ALT | MOD_SHIFT,
        ];
        for &mods in &mod_combos {
            for vk in 0x08u32..=0xFEu32 {
                if is_modifier_vk(vk) || vk == 0xE7 {
                    assert!(format_hotkey(mods, vk).is_none());
                    continue;
                }
                let spec = format_hotkey(mods, vk).expect("should format valid vk");
                let parsed = parse_hotkey(&spec)
                    .unwrap_or_else(|e| panic!("failed to parse '{spec}' (vk=0x{vk:02X}): {e}"));
                assert_eq!(parsed.vk, vk, "vk mismatch for '{spec}'");
                assert_eq!(parsed.modifiers, mods, "modifiers mismatch for '{spec}'");
            }
        }
    }
}
