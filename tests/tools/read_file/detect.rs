
use super::*;

#[test]
fn extension_lowercases_and_ignores_dotfiles() {
    assert_eq!(extension("a/b/File.PNG"), "png");
    assert_eq!(extension("Cargo.toml"), "toml");
    assert_eq!(extension(".gitignore"), "");
    assert_eq!(extension("Makefile"), "");
}

#[test]
fn extension_detection_covers_every_advertised_kind() {
    assert_eq!(detect("a.png", b""), FileType::Image);
    assert_eq!(detect("a.jpg", b""), FileType::Image);
    assert_eq!(detect("a.jpeg", b""), FileType::Image);
    assert_eq!(detect("a.gif", b""), FileType::Image);
    assert_eq!(detect("a.webp", b""), FileType::Image);
    assert_eq!(detect("a.bmp", b""), FileType::Image);
    assert_eq!(detect("a.svg", b""), FileType::Svg);
    assert_eq!(detect("a.pdf", b""), FileType::Pdf);
    assert_eq!(detect("a.ipynb", b""), FileType::Notebook);
    assert_eq!(detect("a.rs", b""), FileType::Text);
    assert_eq!(detect("a.txt", b""), FileType::Text);
    assert_eq!(detect("a.ts", b""), FileType::Text);
}

#[test]
fn magic_recovers_a_mislabeled_or_extensionless_binary() {
    let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    assert_eq!(detect("blob", &png), FileType::Image);
    assert_eq!(detect("blob.dat", &png), FileType::Image);
    assert_eq!(detect("doc", b"%PDF-1.7\n..."), FileType::Pdf);
    assert_eq!(
        detect("jpg_blob", &[0xff, 0xd8, 0xff, 0xe0]),
        FileType::Image
    );
    let mut webp = Vec::from(*b"RIFF____WEBPVP8 ");
    webp.truncate(16);
    assert_eq!(detect("blob", &webp), FileType::Image);
}

#[test]
fn svg_magic_recovers_an_extensionless_svg() {
    assert_eq!(detect("icon", b"<svg xmlns=\"...\">"), FileType::Svg);
    assert_eq!(
        detect("icon", b"<?xml version=\"1.0\"?>\n<svg>"),
        FileType::Svg
    );
}

#[test]
fn image_mime_maps_the_extension_and_falls_back_to_octet_stream() {
    assert_eq!(image_mime("a.png"), "image/png");
    assert_eq!(image_mime("a.jpeg"), "image/jpeg");
    assert_eq!(image_mime("a.bmp"), "image/bmp");
    assert_eq!(image_mime("blob"), "application/octet-stream");
}
