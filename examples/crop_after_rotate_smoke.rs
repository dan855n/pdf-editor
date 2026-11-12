//! Crop a rotated red image and verify the cropped bitmap is not blank.
use image::RgbaImage;
use pdf_viewer::{OpenDocument, load_pdfium_bindings};
use pdfium_render::prelude::*;

fn main() {
    let pdfium: &'static Pdfium = Box::leak(Box::new(Pdfium::new(load_pdfium_bindings())));
    let path = std::env::temp_dir().join("pdf-editor-crop-rot-smoke.pdf");

    {
        let mut doc = pdfium.create_new_pdf().unwrap();
        let mut page = doc
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .unwrap();
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
        .unwrap();
        page.objects_mut().add_image_object(obj).unwrap();
        doc.save_to_file(&path).unwrap();
    }

    let mut open = OpenDocument::load(pdfium, &path).unwrap();
    let object_index = open
        .doc
        .pages()
        .get(0)
        .unwrap()
        .objects()
        .iter()
        .position(|o| o.as_image_object().is_some())
        .unwrap();

    // Move the image so that after rotation it stays on-page, then rotate.
    open.translate_image_object(0, object_index, 100.0, 100.0)
        .unwrap();
    open.rotate_image_object(0, object_index, 90.0).unwrap();

    let bounds = open
        .doc
        .pages()
        .get(0)
        .unwrap()
        .objects()
        .get(object_index)
        .unwrap()
        .bounds()
        .unwrap()
        .to_rect();
    println!(
        "before crop bounds: l={} b={} r={} t={}",
        bounds.left().value,
        bounds.bottom().value,
        bounds.right().value,
        bounds.top().value
    );

    // Crop a 40x40 rect inside the rotated visible bounds.
    let l = bounds.left().value + 5.0;
    let b = bounds.bottom().value + 5.0;
    let r = l + 40.0;
    let t = b + 40.0;
    open.crop_image_object(0, object_index, PdfRect::new_from_values(b, l, t, r))
        .unwrap();

    let page = open.doc.pages().get(0).unwrap();
    let obj = page.objects().get(object_index).unwrap();
    let img = obj.as_image_object().unwrap();
    let raw = img.get_raw_image().unwrap();
    let rgba = raw.to_rgba8();
    let (w, h) = rgba.dimensions();
    println!("after crop raw dims: {}x{}", w, h);
    let p = rgba.get_pixel(w / 2, h / 2);
    println!("center pixel: {:?}", p);
    assert!(
        p[0] > 200 && p[1] < 60 && p[2] < 60,
        "center should be red, got {:?}",
        p
    );
    let _ = std::fs::remove_file(&path);
    println!("crop-after-rotate smoke ok");
}
