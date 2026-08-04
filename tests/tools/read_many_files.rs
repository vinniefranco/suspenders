use super::*;
use crate::content::{ContentBlock, Modalities};
use tempfile::TempDir;

const ALL: Modalities = Modalities {
    image: true,
    pdf: true,
};

// A 1x1 transparent PNG (real magic bytes -> a base64 Image block).
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

// A minimal PDF header so detection returns Pdf and (with pdf modality) it rides
// as a native Document block without needing pdftotext.
const PDF_BYTES: &[u8] = b"%PDF-1.4\n1 0 obj\n<< >>\nendobj\ntrailer\n<< >>\n%%EOF\n";

fn file_spec(root: &std::path::Path, rel: &str) -> Spec {
    Spec {
        abs: root.join(rel),
        is_dir: false,
        display: rel.to_string(),
    }
}

fn dir_spec(root: &std::path::Path, rel: &str) -> Spec {
    Spec {
        abs: root.join(rel),
        is_dir: true,
        display: rel.to_string(),
    }
}

#[tokio::test]
async fn reads_a_text_file_as_a_text_block() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "line one\nline two").unwrap();

    let batch = read(&[file_spec(tmp.path(), "a.txt")], tmp.path(), ALL).await;
    assert_eq!(batch.blocks.len(), 1);
    assert!(
        matches!(&batch.blocks[0], ContentBlock::Text { text } if text.contains("line one") && text.contains("line two"))
    );
    assert_eq!(batch.reads.len(), 1);
    assert!(batch.reads[0].error.is_none());
}

#[tokio::test]
async fn reads_an_image_as_an_image_block() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("shot.png"), PNG_1X1).unwrap();

    let batch = read(&[file_spec(tmp.path(), "shot.png")], tmp.path(), ALL).await;
    assert_eq!(batch.blocks.len(), 1);
    assert!(matches!(
        &batch.blocks[0],
        ContentBlock::Image { mime, data } if mime == "image/png" && !data.is_empty()
    ));
}

#[tokio::test]
async fn reads_a_pdf_as_a_document_block() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("doc.pdf"), PDF_BYTES).unwrap();

    let batch = read(&[file_spec(tmp.path(), "doc.pdf")], tmp.path(), ALL).await;
    assert_eq!(batch.blocks.len(), 1);
    assert!(matches!(
        &batch.blocks[0],
        ContentBlock::Document { mime, data } if mime == "application/pdf" && !data.is_empty()
    ));
}

#[tokio::test]
async fn a_missing_file_is_an_error_read_with_no_blocks() {
    let tmp = TempDir::new().unwrap();
    let batch = read(&[file_spec(tmp.path(), "nope.txt")], tmp.path(), ALL).await;
    assert!(batch.blocks.is_empty());
    assert_eq!(batch.reads.len(), 1);
    assert!(batch.reads[0].error.is_some());
}

#[tokio::test]
async fn a_directory_spec_walks_and_reads_each_file() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("dir")).unwrap();
    std::fs::write(tmp.path().join("dir/a.txt"), "aaa").unwrap();
    std::fs::write(tmp.path().join("dir/b.txt"), "bbb").unwrap();

    let batch = read(&[dir_spec(tmp.path(), "dir")], tmp.path(), ALL).await;
    let joined: String = batch
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(joined.contains("aaa"));
    assert!(joined.contains("bbb"));
    // One directory read entry.
    assert_eq!(batch.reads.len(), 1);
    assert!(batch.reads[0].is_dir);
}

#[tokio::test]
async fn a_directory_walk_respects_gitignore() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("dir")).unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "dir/ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("dir/kept.txt"), "keep-me").unwrap();
    std::fs::write(tmp.path().join("dir/ignored.txt"), "hide-me").unwrap();

    let batch = read(&[dir_spec(tmp.path(), "dir")], tmp.path(), ALL).await;
    let joined: String = batch
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(joined.contains("keep-me"));
    assert!(!joined.contains("hide-me"));
}

#[tokio::test]
async fn an_image_blind_model_degrades_the_image_to_a_text_placeholder() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("shot.png"), PNG_1X1).unwrap();

    let batch = read(
        &[file_spec(tmp.path(), "shot.png")],
        tmp.path(),
        Modalities {
            image: false,
            pdf: false,
        },
    )
    .await;
    // Degrades to the verbatim placeholder Text block, not an Image block.
    assert!(
        matches!(&batch.blocks[0], ContentBlock::Text { text } if text.contains("Unsupported image"))
    );
}

#[tokio::test]
async fn specs_are_read_in_order() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("first.txt"), "FIRST").unwrap();
    std::fs::write(tmp.path().join("second.txt"), "SECOND").unwrap();

    let batch = read(
        &[
            file_spec(tmp.path(), "first.txt"),
            file_spec(tmp.path(), "second.txt"),
        ],
        tmp.path(),
        ALL,
    )
    .await;
    let texts: Vec<String> = batch
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts[0].contains("FIRST"));
    assert!(texts[1].contains("SECOND"));
}
