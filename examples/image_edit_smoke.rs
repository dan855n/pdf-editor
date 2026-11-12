//! Smoke-test the image editing document operations against a generated PDF.

use image::RgbaImage;
use pdf_viewer::{OpenDocument, load_pdfium_bindings};
use pdfium_render::prelude::*;

fn main() {
    let pdfium: &'static Pdfium = Box::leak(Box::new(Pdfium::new(load_pdfium_bindings())));
    let path = std::env::temp_dir().join("pdf-editor-image-smoke.pdf");

    // Build a one-page PDF containing one red 100x50 pt image.
    {
        let mut doc = pdfium.create_new_pdf().expect("new pdf");
        let mut page = doc
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .expect("page");
        let red = image::DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            40,
            20,
            image::Rgba([255, 0, 0, 255]),
        ));
        let obj = PdfPageImageObject::new_with_size(
            &doc,
            &red,
            PdfPoints::new(100.0),
            PdfPoints::new(50.0),
        )
        .expect("image object");
        page.objects_mut().add_image_object(obj).expect("add image");
        doc.save_to_file(&path).expect("save seed pdf");
    }

    let mut open = OpenDocument::load(pdfium, &path).expect("open seed pdf");
    let page = 0;
    let object_index = open
        .doc
        .pages()
        .get(page as i32)
        .unwrap()
        .objects()
        .iter()
        .position(|o| o.as_image_object().is_some())
        .expect("one image object");

    let initial = open.image_object_matrix(page, object_index).unwrap();

    // Rotate around the visible centre.
    open.rotate_image_object(page, object_index, 90.0).unwrap();
    let rotated = open.image_object_matrix(page, object_index).unwrap();
    assert_ne!(rotated, initial, "rotate should change the matrix");

    // The visible bounds after a 90-degree clockwise rotation should be
    // [25, -25] .. [75, 75]: still centred on (50, 25), now 50x100.
    let rotated_bounds = open
        .doc
        .pages()
        .get(page as i32)
        .unwrap()
        .objects()
        .get(object_index)
        .unwrap()
        .bounds()
        .unwrap()
        .to_rect();
    assert!((rotated_bounds.left().value - 25.0).abs() < 0.1);
    assert!((rotated_bounds.bottom().value - (-25.0)).abs() < 0.1);
    assert!((rotated_bounds.right().value - 75.0).abs() < 0.1);
    assert!((rotated_bounds.top().value - 75.0).abs() < 0.1);

    // Translate.
    open.translate_image_object(page, object_index, 10.0, 20.0)
        .unwrap();
    let moved = open.image_object_matrix(page, object_index).unwrap();
    assert_ne!(moved, rotated, "move should change the matrix");

    // Replace the image data with a green square; matrix must stay unchanged.
    let green = image::DynamicImage::ImageRgba8(RgbaImage::from_pixel(
        30,
        30,
        image::Rgba([0, 255, 0, 255]),
    ));
    open.replace_image_object(page, object_index, &green)
        .unwrap();
    let replaced = open.image_object_matrix(page, object_index).unwrap();
    assert_eq!(replaced, moved, "replace must preserve the matrix");

    // Crop to a 50x50 pt rectangle starting at (20, 20).
    let crop_rect = PdfRect::new_from_values(20.0, 20.0, 70.0, 70.0);
    open.crop_image_object(page, object_index, crop_rect)
        .unwrap();
    let cropped = open.image_object_matrix(page, object_index).unwrap();
    assert_ne!(cropped, replaced, "crop should change the matrix");
    // The crop reset the matrix to an axis-aligned 50x50 placement at (20, 20).
    assert!((cropped.a() - 50.0).abs() < 0.01, "crop width");
    assert!((cropped.d() - 50.0).abs() < 0.01, "crop height");
    assert!((cropped.e() - 20.0).abs() < 0.01, "crop x");
    assert!((cropped.f() - 20.0).abs() < 0.01, "crop y");

    open.save().expect("save edited pdf");

    let _ = std::fs::remove_file(&path);
    println!(
        "image-edit smoke ok: initial={initial:?} rotated={rotated:?} moved={moved:?} cropped={cropped:?}"
    );
}
