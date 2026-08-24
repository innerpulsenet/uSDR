//! Pager message formatting, hospital/nurse-call decoding, and CAD dispatch parser.
//!
//! Enriches raw POCSAG and FLEX alphanumeric and numeric pages into structured,
//! human-readable formats with protocol, sender, callback, and location fields.

/// Decodes and enriches raw pager text into clean, structured human-readable text.
pub fn parse_pager_text(raw_text: &str, function: Option<u8>) -> String {
    let clean = clean_control_chars(raw_text.trim());
    if clean.is_empty() {
        return match function {
            Some(0) => "[Tone Only: Emergency / Alert]".to_string(),
            Some(1) => "[Tone Only: Priority Alert]".to_string(),
            Some(2) => "[Tone Only: Information Alert]".to_string(),
            Some(3) => "[Tone Only: Routine Alert]".to_string(),
            _ => "[Tone Only]".to_string(),
        };
    }

    // 1. Hospital / Nurse Call / Zetron Priority Pattern
    // e.g. UU4732U8(35405)72U41 or U102(4401)3B or 4732U8(35405)72U41
    if let Some(formatted) = try_parse_hospital_nurse_call(&clean) {
        return formatted;
    }

    // 2. Public Safety CAD Dispatch Format
    // e.g. "CALL: STRUCTURE FIRE ADDR: 123 MAIN ST UNITS: E1, L1 BOX: 14-2"
    if let Some(formatted) = try_parse_cad_dispatch(&clean) {
        return formatted;
    }

    // 3. Medical Emergency & Telemetry Alarms
    if let Some(formatted) = try_parse_medical_alert(&clean) {
        return formatted;
    }

    // 4. 911 / Emergency Numeric Pager Codes
    if let Some(formatted) = try_parse_911_numeric(&clean) {
        return formatted;
    }

    // 5. General Delimited Numeric Pages (e.g. *123*456*789#)
    if let Some(formatted) = try_parse_delimited_numeric(&clean) {
        return formatted;
    }

    // 6. Unstructured numeric/BCD pages: digits plus the POCSAG numeric
    // symbols. Tag them so they are clearly pages, not undecoded noise.
    if clean
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '*' | 'U' | ' ' | '-' | ')' | '('))
    {
        return format!("[Numeric Page] {clean}");
    }

    clean
}

/// Cleans unprintable control characters and protocol wrappers (<STX>, <ETX>, [MSG]).
fn clean_control_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x02' || c == '\x03' || c == '\x04' || c == '\x00' {
            continue;
        }
        if c == '[' && chars.clone().take(4).collect::<String>() == "MSG]" {
            for _ in 0..4 {
                chars.next();
            }
            continue;
        }
        if c == '<' && chars.clone().take(4).collect::<String>() == "STX>" {
            for _ in 0..4 {
                chars.next();
            }
            continue;
        }
        if c == '<' && chars.clone().take(4).collect::<String>() == "ETX>" {
            for _ in 0..4 {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }

    out.trim().to_string()
}

/// Parses healthcare nurse call and hospital telemetry formats:
/// e.g. `UU4732U8(35405)72U41` -> `[URGENT] Unit/Sender: 4732 (Code 8) | Callback/Ext: 35405 | Location/Action: Room 72, Priority Code 41`
fn try_parse_hospital_nurse_call(s: &str) -> Option<String> {
    // Check if string contains parentheses for extension/callback
    let paren_open = s.find('(')?;
    let paren_close = s.find(')')?;
    if paren_close <= paren_open {
        return None;
    }

    let before = &s[..paren_open];
    let ext = &s[paren_open + 1..paren_close];
    let after = &s[paren_close + 1..];

    if ext.is_empty()
        || (!ext
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ' '))
    {
        return None;
    }

    // Parse prefix & before fields
    let (urg_prefix, rest_before) = if before.starts_with("UU") || before.starts_with("uu") {
        (Some("[URGENT]"), &before[2..])
    } else if before.starts_with('U') || before.starts_with('u') {
        (Some("[ALERT]"), &before[1..])
    } else {
        (None, before)
    };

    // Check for unit and code in before field: e.g. "4732U8" or "4732"
    let (unit, code) = if let Some(u_pos) = rest_before.rfind(|c| c == 'U' || c == 'u') {
        let u = &rest_before[..u_pos];
        let cd = &rest_before[u_pos + 1..];
        if !u.is_empty() && !cd.is_empty() {
            (u, Some(cd))
        } else if !u.is_empty() {
            (u, None)
        } else {
            (rest_before, None)
        }
    } else {
        (rest_before, None)
    };

    if unit.is_empty() && before.is_empty() {
        return None;
    }

    // Parse after fields: room and action code (e.g. "72U41" or "72" or "ICU-4U9")
    let (room, action_code) = if let Some(u_pos) = after.rfind(|c| c == 'U' || c == 'u') {
        let rm = &after[..u_pos];
        let act = &after[u_pos + 1..];
        if !act.is_empty() {
            (rm, Some(act))
        } else {
            (rm, None)
        }
    } else {
        (after, None)
    };

    let mut parts = Vec::new();

    let mut unit_part = String::new();
    if !unit.is_empty() {
        unit_part.push_str(&format!("Unit/Sender: {unit}"));
        if let Some(cd) = code {
            unit_part.push_str(&format!(" (Code {cd})"));
        }
    } else if let Some(cd) = code {
        unit_part.push_str(&format!("Code: {cd}"));
    }
    if !unit_part.is_empty() {
        parts.push(unit_part);
    }

    parts.push(format!("Callback/Ext: {ext}"));

    if !room.is_empty() || action_code.is_some() {
        let mut loc_part = String::new();
        if !room.is_empty() && action_code.is_some() {
            loc_part.push_str(&format!(
                "Location/Action: Room {room}, Priority Code {}",
                action_code.unwrap()
            ));
        } else if !room.is_empty() {
            loc_part.push_str(&format!("Location: Room {room}"));
        } else if let Some(act) = action_code {
            loc_part.push_str(&format!("Priority/Action Code: {act}"));
        }
        parts.push(loc_part);
    }

    let header = if let Some(tag) = urg_prefix {
        format!("{tag} ")
    } else {
        String::new()
    };

    let body_parts: Vec<String> = parts.into_iter().filter(|p| !p.starts_with('[')).collect();
    if body_parts.is_empty() {
        return None;
    }

    Some(format!("{}{}", header, body_parts.join(" | ")))
}

/// Parses Computer-Aided Dispatch (CAD) public safety messages.
fn try_parse_cad_dispatch(s: &str) -> Option<String> {
    let upper = s.to_uppercase();
    let cad_keywords = [
        "CALL:",
        "NATURE:",
        "TYPE:",
        "ADDR:",
        "LOC:",
        "LOCATION:",
        "CROSS:",
        "XST:",
        "X-ST:",
        "UNITS:",
        "UNIT:",
        "BOX:",
        "INC:",
        "INCIDENT:",
        "MAP:",
        "GRID:",
        "DETAILS:",
        "NARR:",
    ];

    let mut matched_count = 0;
    for kw in &cad_keywords {
        if upper.contains(kw) {
            matched_count += 1;
        }
    }

    if matched_count < 2 {
        return None;
    }

    let mut formatted_fields = Vec::new();
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut cur_key = String::new();
    let mut cur_val = Vec::new();

    for token in tokens {
        let token_upper = token.to_uppercase();
        let is_kw = cad_keywords
            .iter()
            .any(|&k| token_upper == k || token_upper == format!("{k}:"));

        if is_kw {
            if !cur_key.is_empty() && !cur_val.is_empty() {
                formatted_fields.push(format!("{}: {}", cur_key, cur_val.join(" ")));
                cur_val.clear();
            }
            cur_key = token.trim_end_matches(':').to_string();
        } else if !cur_key.is_empty() {
            cur_val.push(token);
        }
    }

    if !cur_key.is_empty() && !cur_val.is_empty() {
        formatted_fields.push(format!("{}: {}", cur_key, cur_val.join(" ")));
    }

    if formatted_fields.is_empty() {
        return None;
    }

    Some(format!("[CAD] {}", formatted_fields.join(" | ")))
}

/// Detects critical hospital codes and emergency telemetry alarms.
fn try_parse_medical_alert(s: &str) -> Option<String> {
    let upper = s.to_uppercase();

    let alerts = [
        ("CODE BLUE", "[CODE BLUE - CARDIAC/RESP]"),
        ("CODE RED", "[CODE RED - FIRE]"),
        ("CODE PINK", "[CODE PINK - INFANT ABDUCTION]"),
        ("CODE ADAM", "[CODE ADAM - CHILD ABDUCTION]"),
        ("CODE BLACK", "[CODE BLACK - BOMB/SEVERE WEATHER]"),
        ("CODE SILVER", "[CODE SILVER - ACTIVE THREAT/WEAPON]"),
        ("STAT ", "[STAT - IMMEDIATE]"),
        ("RAPID RESPONSE", "[RAPID RESPONSE TEAM]"),
        ("TRAUMA STAT", "[TRAUMA ACTIVATION]"),
        ("TRAUMA ALERT", "[TRAUMA ALERT]"),
        ("STEMI ALERT", "[CARDIAC STEMI ALERT]"),
        ("STROKE ALERT", "[STROKE ALERT]"),
        ("SEPSIS ALERT", "[SEPSIS PROTOCOL]"),
        ("BED ALARM", "[BED ALARM - FALL RISK]"),
        ("FALL ALARM", "[FALL ALARM]"),
        ("BATH ALARM", "[BATHROOM NURSE CALL]"),
        ("ASYSTOLE", "[CRITICAL TELEMETRY: ASYSTOLE]"),
        ("VFIB", "[CRITICAL TELEMETRY: VFIB]"),
        ("VTAC", "[CRITICAL TELEMETRY: VTAC]"),
        ("SPO2 LOW", "[CRITICAL TELEMETRY: LOW SPO2]"),
        ("HR HIGH", "[CRITICAL TELEMETRY: HIGH HR]"),
    ];

    for (kw, tag) in &alerts {
        if upper.contains(kw) && !upper.starts_with(tag) {
            return Some(format!("{tag} {s}"));
        }
    }

    None
}

/// Parses 911 emergency numeric pager codes: e.g. `911*5551234` or `5551234*911`
fn try_parse_911_numeric(s: &str) -> Option<String> {
    let trimmed = s.trim_matches(|c| c == '*' || c == '#' || c == '-' || c == ' ');
    if trimmed.contains("911")
        && trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || c == '*' || c == '-' || c == '/')
    {
        let parts: Vec<&str> = trimmed
            .split(|c| c == '*' || c == '-' || c == '/')
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() > 1 {
            return Some(format!("[911 EMERGENCY] Callback: {}", parts.join(" · ")));
        } else if trimmed == "911" || trimmed == "911911" {
            return Some("[911 EMERGENCY ALERT]".to_string());
        }
    }
    None
}

/// Formats numeric sub-fields delimited by `*` or `#` (e.g. `*123*456*789#`).
fn try_parse_delimited_numeric(s: &str) -> Option<String> {
    if (s.starts_with('*') || s.ends_with('#') || s.contains('*'))
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '*' || c == '#' || c == '-' || c == ' ')
    {
        let fields: Vec<&str> = s
            .split(|c| c == '*' || c == '#' || c == '-')
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() >= 2 {
            return Some(format!("[Numeric Fields] {}", fields.join(" · ")));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urgent_nurse_call() {
        let raw = "UU4732U8(35405)72U41";
        let res = parse_pager_text(raw, None);
        assert_eq!(
            res,
            "[URGENT] Unit/Sender: 4732 (Code 8) | Callback/Ext: 35405 | Location/Action: Room 72, Priority Code 41"
        );
    }

    #[test]
    fn parses_nurse_call_variations() {
        let raw1 = "U102(4401)3B";
        assert_eq!(
            parse_pager_text(raw1, None),
            "[ALERT] Unit/Sender: 102 | Callback/Ext: 4401 | Location: Room 3B"
        );

        let raw2 = "4732U8(35405)72U41";
        assert_eq!(
            parse_pager_text(raw2, None),
            "Unit/Sender: 4732 (Code 8) | Callback/Ext: 35405 | Location/Action: Room 72, Priority Code 41"
        );
    }

    #[test]
    fn parses_cad_dispatch() {
        let raw = "CALL: STRUCTURE FIRE ADDR: 123 MAIN ST XST: OAK / PINE UNITS: E1, T1 BOX: 14-2";
        let res = parse_pager_text(raw, None);
        assert_eq!(
            res,
            "[CAD] CALL: STRUCTURE FIRE | ADDR: 123 MAIN ST | XST: OAK / PINE | UNITS: E1, T1 | BOX: 14-2"
        );
    }

    #[test]
    fn parses_medical_alerts() {
        let raw = "CODE BLUE RM 412 ICU-3";
        let res = parse_pager_text(raw, None);
        assert!(res.starts_with("[CODE BLUE - CARDIAC/RESP]"));
    }

    #[test]
    fn parses_911_numeric() {
        let raw = "911*5551234";
        let res = parse_pager_text(raw, None);
        assert_eq!(res, "[911 EMERGENCY] Callback: 911 · 5551234");
    }

    #[test]
    fn parses_tone_only() {
        assert_eq!(
            parse_pager_text("", Some(0)),
            "[Tone Only: Emergency / Alert]"
        );
        assert_eq!(parse_pager_text("", Some(1)), "[Tone Only: Priority Alert]");
    }
}
