use super::*;

#[test]
fn every_ansi_16_name_parses_to_its_variant() {
    let names: [(&str, Color); 16] = [
        ("black", Color::Black),
        ("red", Color::Red),
        ("green", Color::Green),
        ("yellow", Color::Yellow),
        ("blue", Color::Blue),
        ("magenta", Color::Magenta),
        ("cyan", Color::Cyan),
        ("gray", Color::Gray),
        ("dark_gray", Color::DarkGray),
        ("light_red", Color::LightRed),
        ("light_green", Color::LightGreen),
        ("light_yellow", Color::LightYellow),
        ("light_blue", Color::LightBlue),
        ("light_magenta", Color::LightMagenta),
        ("light_cyan", Color::LightCyan),
        ("white", Color::White),
    ];
    for (name, expected) in names {
        assert_eq!(name.parse(), Ok(expected), "{name}");
    }
}

#[test]
fn hex_parses_case_insensitively() {
    assert_eq!("#b9d7b4".parse(), Ok(Color::Rgb(185, 215, 180)));
    assert_eq!("#B9D7B4".parse(), Ok(Color::Rgb(185, 215, 180)));
    assert_eq!("#000000".parse(), Ok(Color::Rgb(0, 0, 0)));
    assert_eq!("#FFffFF".parse(), Ok(Color::Rgb(255, 255, 255)));
}

#[test]
fn bad_hex_is_rejected_with_the_expected_shape() {
    // "#+1+2+3" and "#-0-0-0" pin the from_str_radix sign hazard: a
    // pairwise u8 parse accepts "+1" as 1, so these MUST reject.
    for bad in [
        "#12345", "#1234567", "#12345g", "#", "#ééé", "#+1+2+3", "#-0-0-0",
    ] {
        let err = bad.parse::<Color>().unwrap_err();
        assert_eq!(
            err,
            format!("\"{bad}\" is not a valid hex color: expected \"#rrggbb\"")
        );
    }
}

#[test]
fn an_unknown_name_is_rejected_listing_the_vocabulary() {
    let err = "mauve".parse::<Color>().unwrap_err();
    assert!(err.contains("\"mauve\" is not a color"), "{err}");
    assert!(err.contains("#rrggbb"), "{err}");
    assert!(err.contains("dark_gray"), "the full name list is in: {err}");
    // Names are exact: no case-folding, no hyphens (strictness, ADR-0038).
    assert!("Cyan".parse::<Color>().is_err());
    assert!("dark-gray".parse::<Color>().is_err());
    assert!("".parse::<Color>().is_err());
}
